use std::io;
use std::path::Path;

use mlua::{Error as LuaError, Function, IntoLuaMulti, Lua, Result as LuaResult};
use semver::Version;
use tracing::warn;

use crate::error::PluginError;

pub use maki_config::Permission;

pub(crate) const MANIFEST_FILE: &str = "plugin.toml";
const MIN_MAKI_VERSION: &str = "min_maki_version";
const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct PluginPermissions {
    allowed: [bool; Permission::COUNT],
}

impl PluginPermissions {
    pub fn trusted() -> Self {
        Self {
            allowed: [true; Permission::COUNT],
        }
    }

    pub fn denied() -> Self {
        Self {
            allowed: [false; Permission::COUNT],
        }
    }

    /// Builds a set from the names an approval records.
    ///
    /// A name this build does not know is ignored rather than treated as a
    /// grant, so an approval file written by a newer maki cannot widen what an
    /// older one allows.
    pub fn from_approved<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::denied();
        for name in names {
            if let Some(perm) = Permission::from_key(name) {
                out.set(perm, true);
            }
        }
        out
    }

    pub fn is_allowed(&self, perm: Permission) -> bool {
        self.allowed[perm as usize]
    }

    /// Layering a call whose reach nobody declared takes full trust, since no
    /// narrower price would be honest.
    pub fn holds_all(&self) -> bool {
        self.allowed.iter().all(|&allowed| allowed)
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let perms = manifest.get("permissions");
        let mut allowed = [true; Permission::COUNT];
        for &perm in Permission::ALL {
            allowed[perm as usize] = perms
                .and_then(|p| p.get(perm.manifest_key()))
                .and_then(toml::Value::as_bool)
                .unwrap_or(true);
        }
        Self { allowed }
    }

    pub fn set(&mut self, perm: Permission, value: bool) {
        self.allowed[perm as usize] = value;
    }

    pub fn guard<F, A, R>(&self, perm: Permission, lua: &Lua, f: F) -> LuaResult<Function>
    where
        F: Fn(&Lua, A) -> LuaResult<R> + Send + 'static,
        A: mlua::FromLuaMulti,
        R: IntoLuaMulti,
    {
        if self.is_allowed(perm) {
            lua.create_function(f)
        } else {
            lua.create_function(move |_, _: mlua::MultiValue| -> LuaResult<mlua::Value> {
                Err(denied_error(perm))
            })
        }
    }

    pub fn guard_async<F, Fut, A, R>(
        &self,
        perm: Permission,
        lua: &Lua,
        f: F,
    ) -> LuaResult<Function>
    where
        F: Fn(Lua, A) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = LuaResult<R>> + Send + 'static,
        A: mlua::FromLuaMulti,
        R: IntoLuaMulti,
    {
        if self.is_allowed(perm) {
            lua.create_async_function(f)
        } else {
            lua.create_function(move |_, _: mlua::MultiValue| -> LuaResult<mlua::Value> {
                Err(denied_error(perm))
            })
        }
    }
}

/// What a package's `plugin.toml` asks for.
///
/// Deny by default: an omitted key is *not requested*. That is the opposite of
/// [`PluginPermissions::from_manifest`], which stays permissive for a local
/// `init.lua` the user wrote themselves. The two parsers are separate because
/// the inputs differ in trust, not in shape: a package manifest arrives with
/// downloaded code and must not be able to widen its own access.
///
/// This is a distinct type so an effective grant can never be built by
/// accident from a request alone.
#[derive(Debug, Clone)]
pub struct Requested(PluginPermissions);

impl Requested {
    pub fn none() -> Self {
        Self(PluginPermissions::denied())
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let perms = manifest.get("permissions");
        let mut allowed = [false; Permission::COUNT];
        for &perm in Permission::ALL {
            allowed[perm as usize] = perms
                .and_then(|p| p.get(perm.manifest_key()))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
        }
        Self(PluginPermissions { allowed })
    }

    pub fn is_requested(&self, perm: Permission) -> bool {
        self.0.is_allowed(perm)
    }

    pub fn names(&self) -> Vec<String> {
        Permission::ALL
            .iter()
            .filter(|permission| self.is_requested(**permission))
            .map(|permission| permission.to_string())
            .collect()
    }

    /// Code whose files nobody fetched gets what it asks for: a package the
    /// user installed by hand, or a plugin bundled into the binary. Only a
    /// package maki downloaded has to be intersected with an approval.
    pub fn granted(self) -> PluginPermissions {
        self.0
    }

    /// Effective permissions for a managed package: the request and the user's
    /// approval must agree.
    pub fn intersect(&self, approved: &PluginPermissions) -> PluginPermissions {
        let mut out = PluginPermissions::denied();
        for &perm in Permission::ALL {
            out.set(perm, self.0.is_allowed(perm) && approved.is_allowed(perm));
        }
        out
    }
}

/// Reads a package's requested permissions.
///
/// Only an absent manifest means "requests nothing". A manifest that exists but
/// cannot be read or parsed is an error, because silently treating it as empty
/// would load the package and then fail every guarded call it makes, which
/// reports the typo as a permission problem instead of a syntax one.
pub(crate) fn load_requested_permissions(
    plugin_dir: &Path,
) -> Result<Requested, crate::error::PluginError> {
    let manifest_path = plugin_dir.join(MANIFEST_FILE);
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Requested::none()),
        Err(source) => {
            return Err(crate::error::PluginError::Io {
                path: manifest_path,
                source,
            });
        }
    };
    requested_permissions_from_text(&content, &manifest_path)
}

pub(crate) fn requested_permissions_from_text(
    content: &str,
    manifest_path: &Path,
) -> Result<Requested, PluginError> {
    toml::from_str::<toml::Value>(content)
        .map(|value| Requested::from_manifest(&value))
        .map_err(|error| PluginError::PackageManifest {
            path: manifest_path.to_path_buf(),
            message: error.to_string(),
        })
}

pub(crate) fn denied_error(perm: Permission) -> LuaError {
    let msg = format!(
        "permission denied: '{perm}' not granted for this plugin (grant it in {MANIFEST_FILE} next to the plugin file)"
    );
    warn!(permission = %perm, "{msg}");
    LuaError::runtime(msg)
}

pub(crate) fn load_plugin_permissions(plugin_dir: Option<&Path>) -> PluginPermissions {
    load_plugin_manifest(plugin_dir)
        .as_ref()
        .map_or_else(PluginPermissions::denied, PluginPermissions::from_manifest)
}

/// Host-side gate, run before any Lua from `plugin_dir` reaches the runtime.
/// An `Err` means the directory is refused; the startup path turns it into a
/// warning and skips the plugin, so one bad `min_maki_version` cannot keep
/// Maki from booting.
pub(crate) fn check_plugin_compatibility(
    plugin: &str,
    plugin_dir: Option<&Path>,
) -> Result<(), PluginError> {
    let Some(manifest) = load_plugin_manifest(plugin_dir) else {
        return Ok(());
    };
    let Some(required) = manifest.get(MIN_MAKI_VERSION) else {
        return Ok(());
    };
    check_minimum_version(plugin, required, RUNTIME_VERSION)
}

fn load_plugin_manifest(plugin_dir: Option<&Path>) -> Option<toml::Value> {
    let dir = plugin_dir?;
    let manifest_path = dir.join(MANIFEST_FILE);
    match std::fs::read_to_string(&manifest_path) {
        Ok(content) => match toml::from_str::<toml::Value>(&content) {
            Ok(manifest) => Some(manifest),
            Err(e) => {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "invalid {MANIFEST_FILE}, denying all permissions"
                );
                None
            }
        },
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                warn!(
                    dir = %dir.display(),
                    "no {MANIFEST_FILE} next to plugin; all permissions denied. Create one \
                     (even an empty file) next to it to grant permissions"
                );
            } else {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "cannot read {MANIFEST_FILE}, denying all permissions"
                );
            }
            None
        }
    }
}

fn check_minimum_version(
    plugin: &str,
    required: &toml::Value,
    running: &str,
) -> Result<(), PluginError> {
    let required = required
        .as_str()
        .ok_or_else(|| PluginError::InvalidMinimumVersionType {
            plugin: plugin.to_owned(),
        })?;
    let required =
        Version::parse(required).map_err(|source| PluginError::InvalidMinimumVersion {
            plugin: plugin.to_owned(),
            version: required.to_owned(),
            source,
        })?;
    let running = Version::parse(running).map_err(|source| PluginError::InvalidRuntimeVersion {
        version: running.to_owned(),
        source,
    })?;
    if required > running {
        return Err(PluginError::MakiVersionTooOld {
            plugin: plugin.to_owned(),
            required,
            running,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::*;

    const PLUGIN: &str = "test-plugin";

    fn assert_denied(permissions: &PluginPermissions) {
        for &permission in Permission::ALL {
            assert!(
                !permissions.is_allowed(permission),
                "{permission} should be denied"
            );
        }
    }

    #[test]
    fn trusted_allows_everything() {
        let p = PluginPermissions::trusted();
        for &perm in Permission::ALL {
            assert!(p.is_allowed(perm), "{perm} should be allowed");
        }
    }

    #[test]
    fn denied_blocks_everything() {
        let p = PluginPermissions::denied();
        assert_denied(&p);
    }

    #[test]
    fn from_manifest_partial() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = false
            net = false
            "#,
        )
        .unwrap();
        let p = PluginPermissions::from_manifest(&val);
        assert!(!p.is_allowed(Permission::FsRead));
        assert!(p.is_allowed(Permission::FsWrite));
        assert!(!p.is_allowed(Permission::Net));
        assert!(p.is_allowed(Permission::Run));
        assert!(p.is_allowed(Permission::Env));
    }

    #[test]
    fn from_manifest_missing_section() {
        let val: toml::Value = toml::from_str("[package]\nname = \"test\"").unwrap();
        let p = PluginPermissions::from_manifest(&val);
        for &perm in Permission::ALL {
            assert!(p.is_allowed(perm), "{perm} should default to allowed");
        }
    }

    #[test]
    fn set_modifies_single_permission() {
        let mut p = PluginPermissions::trusted();
        p.set(Permission::Net, false);
        p.set(Permission::Run, false);
        assert!(p.is_allowed(Permission::FsRead));
        assert!(p.is_allowed(Permission::FsWrite));
        assert!(!p.is_allowed(Permission::Net));
        assert!(!p.is_allowed(Permission::Run));
        assert!(p.is_allowed(Permission::Env));
    }

    #[test]
    fn guard_allowed_calls_inner() {
        let lua = Lua::new();
        let perms = PluginPermissions::trusted();
        let func = perms
            .guard(Permission::FsRead, &lua, |_, ()| Ok(42))
            .unwrap();
        let result: i32 = func.call(()).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn requested_denies_omitted_keys() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            net = true
            "#,
        )
        .unwrap();
        let req = Requested::from_manifest(&val);
        assert!(req.is_requested(Permission::Net));
        for &perm in Permission::ALL {
            if perm != Permission::Net {
                assert!(!req.is_requested(perm), "{perm} must not be requested");
            }
        }
    }

    #[test]
    fn requested_names_are_the_approval_keys() {
        let value: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = true
            run = true
            "#,
        )
        .unwrap();

        assert_eq!(
            Requested::from_manifest(&value).names(),
            ["fs_read".to_owned(), "run".to_owned()]
        );
    }

    /// The legacy parser stays permissive; only the package parser is strict.
    /// A manifest with no `[permissions]` section proves the two differ.
    #[test]
    fn requested_and_legacy_parsers_disagree_by_design() {
        let val: toml::Value = toml::from_str("[package]\nname = \"p\"").unwrap();
        let legacy = PluginPermissions::from_manifest(&val);
        let requested = Requested::from_manifest(&val);
        for &perm in Permission::ALL {
            assert!(legacy.is_allowed(perm), "legacy stays permissive");
            assert!(!requested.is_requested(perm), "package requests nothing");
        }
    }

    #[test]
    fn intersect_needs_both_request_and_approval() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            net = true
            run = true
            "#,
        )
        .unwrap();
        let requested = Requested::from_manifest(&val);

        let mut approved = PluginPermissions::denied();
        approved.set(Permission::Net, true);
        approved.set(Permission::FsRead, true);

        let effective = requested.intersect(&approved);
        assert!(
            effective.is_allowed(Permission::Net),
            "requested + approved"
        );
        assert!(!effective.is_allowed(Permission::Run), "not approved");
        assert!(!effective.is_allowed(Permission::FsRead), "not requested");
        assert!(!effective.is_allowed(Permission::Env), "neither");
    }

    #[test]
    fn manual_install_grants_what_it_requests() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = true
            "#,
        )
        .unwrap();
        let granted = Requested::from_manifest(&val).granted();
        assert!(granted.is_allowed(Permission::FsRead));
        assert!(!granted.is_allowed(Permission::Net));
    }

    #[test]
    fn guard_denied_returns_error() {
        let lua = Lua::new();
        let perms = PluginPermissions::denied();
        let func = perms
            .guard(Permission::FsRead, &lua, |_, ()| Ok(42))
            .unwrap();
        let err = func.call::<i32>(()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("permission denied"));
        assert!(msg.contains("fs_read"));
    }

    #[test_case("1.2.2", "1.2.3", true; "lower")]
    #[test_case("1.2.3", "1.2.3", true; "equal")]
    #[test_case("1.2.3-alpha.1", "1.2.3-alpha.2", true; "older_prerelease")]
    #[test_case("1.2.3-alpha.2", "1.2.3-alpha.1", false; "newer_prerelease")]
    #[test_case("1.2.4", "1.2.3", false; "higher")]
    fn minimum_version_uses_semver_precedence(required: &str, running: &str, compatible: bool) {
        let required = toml::Value::String(required.to_owned());
        let result = check_minimum_version(PLUGIN, &required, running);
        assert_eq!(result.is_ok(), compatible);
        if !compatible {
            assert!(matches!(result, Err(PluginError::MakiVersionTooOld { .. })));
        }
    }

    #[test]
    fn minimum_version_requires_a_plain_semver_string() {
        let wrong_type = check_minimum_version(PLUGIN, &toml::Value::Integer(1), RUNTIME_VERSION);
        assert!(matches!(
            wrong_type,
            Err(PluginError::InvalidMinimumVersionType { .. })
        ));

        for version in ["not-a-version", "v1.2.3"] {
            let value = toml::Value::String(version.to_owned());
            assert!(matches!(
                check_minimum_version(PLUGIN, &value, RUNTIME_VERSION),
                Err(PluginError::InvalidMinimumVersion { .. })
            ));
        }

        let value = toml::Value::String("1.2.3".to_owned());
        assert!(matches!(
            check_minimum_version(PLUGIN, &value, "invalid"),
            Err(PluginError::InvalidRuntimeVersion { .. })
        ));
    }

    #[test]
    fn manifest_rejects_an_invalid_declared_minimum() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_MAKI_VERSION} = 1\n"),
        )
        .unwrap();
        assert!(matches!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())),
            Err(PluginError::InvalidMinimumVersionType { .. })
        ));

        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_MAKI_VERSION} = \"v1.2.3\"\n"),
        )
        .unwrap();
        assert!(matches!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())),
            Err(PluginError::InvalidMinimumVersion { .. })
        ));
    }

    #[test]
    fn missing_valid_and_malformed_manifests_keep_existing_defaults() {
        assert_denied(&load_plugin_permissions(None));

        let dir = tempfile::tempdir().unwrap();
        assert_denied(&load_plugin_permissions(Some(dir.path())));

        fs::write(dir.path().join(MANIFEST_FILE), "").unwrap();
        let permissions = load_plugin_permissions(Some(dir.path()));
        for &permission in Permission::ALL {
            assert!(permissions.is_allowed(permission));
        }

        fs::write(dir.path().join(MANIFEST_FILE), "not = [valid").unwrap();
        assert_denied(&load_plugin_permissions(Some(dir.path())));
        assert!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())).is_ok(),
            "an unparseable manifest has no floor to enforce"
        );
    }

    #[test]
    fn one_manifest_provides_compatibility_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_MAKI_VERSION} = {RUNTIME_VERSION:?}\n\n[permissions]\nnet = false\n"),
        )
        .unwrap();

        check_plugin_compatibility(PLUGIN, Some(dir.path())).unwrap();
        let permissions = load_plugin_permissions(Some(dir.path()));
        assert!(permissions.is_allowed(Permission::FsRead));
        assert!(!permissions.is_allowed(Permission::Net));
    }
}
