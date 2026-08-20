use std::fmt;
use std::io;
use std::path::Path;

use mlua::{Error as LuaError, Function, IntoLuaMulti, Lua, Result as LuaResult};
use tracing::warn;

const MANIFEST_FILE: &str = "plugin.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    FsRead,
    FsWrite,
    Net,
    Run,
    Env,
}

impl Permission {
    pub(crate) const ALL: [Permission; 5] = [
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Net,
        Permission::Run,
        Permission::Env,
    ];

    /// Parses the name used in `plugin.toml` and in the approval store.
    ///
    /// Both use one spelling on purpose. If an approval were recorded under a
    /// different name from the request, `intersect` would silently never
    /// match, and every managed package would run with nothing granted.
    pub fn from_key(key: &str) -> Option<Self> {
        Permission::ALL
            .into_iter()
            .find(|p| p.manifest_key() == key)
    }

    pub(crate) const fn manifest_key(self) -> &'static str {
        match self {
            Permission::FsRead => "fs_read",
            Permission::FsWrite => "fs_write",
            Permission::Net => "net",
            Permission::Run => "run",
            Permission::Env => "env",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.manifest_key())
    }
}

#[derive(Debug, Clone)]
pub struct PluginPermissions {
    allowed: [bool; 5],
}

impl PluginPermissions {
    pub fn trusted() -> Self {
        Self { allowed: [true; 5] }
    }

    pub fn denied() -> Self {
        Self {
            allowed: [false; 5],
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

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let perms = manifest.get("permissions");
        let mut allowed = [true; 5];
        for perm in Permission::ALL {
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
        let mut allowed = [false; 5];
        for perm in Permission::ALL {
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
            .into_iter()
            .filter(|permission| self.is_requested(*permission))
            .map(|permission| permission.to_string())
            .collect()
    }

    pub fn is_granted_by(&self, permissions: &PluginPermissions) -> bool {
        Permission::ALL
            .into_iter()
            .all(|permission| !self.is_requested(permission) || permissions.is_allowed(permission))
    }

    /// A package the user installed by hand gets what it asks for. They placed
    /// the files, which is the same trust already given to a local `init.lua`.
    /// Only a package maki fetched has to be intersected with an approval.
    pub fn granted_for_manual_install(self) -> PluginPermissions {
        self.0
    }

    /// Effective permissions for a managed package: the request and the user's
    /// approval must agree.
    pub fn intersect(&self, approved: &PluginPermissions) -> PluginPermissions {
        let mut out = PluginPermissions::denied();
        for perm in Permission::ALL {
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
    toml::from_str::<toml::Value>(&content)
        .map(|value| Requested::from_manifest(&value))
        .map_err(|e| crate::error::PluginError::PackageManifest {
            path: manifest_path,
            message: e.to_string(),
        })
}

fn denied_error(perm: Permission) -> LuaError {
    let msg = format!(
        "permission denied: '{perm}' not granted for this plugin (grant it in {MANIFEST_FILE} next to the plugin file)"
    );
    warn!(permission = %perm, "{msg}");
    LuaError::runtime(msg)
}

pub(crate) fn load_plugin_permissions(plugin_dir: Option<&Path>) -> PluginPermissions {
    let Some(dir) = plugin_dir else {
        return PluginPermissions::denied();
    };
    let manifest_path = dir.join(MANIFEST_FILE);
    match std::fs::read_to_string(&manifest_path) {
        Ok(content) => match toml::from_str::<toml::Value>(&content) {
            Ok(val) => PluginPermissions::from_manifest(&val),
            Err(e) => {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "invalid {MANIFEST_FILE}, denying all permissions"
                );
                PluginPermissions::denied()
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
            PluginPermissions::denied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for perm in Permission::ALL {
            assert!(p.is_allowed(perm), "{perm} should default to allowed");
        }
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
        for perm in Permission::ALL {
            if perm != Permission::Net {
                assert!(!req.is_requested(perm), "{perm} must not be requested");
            }
        }
    }

    /// The legacy parser stays permissive; only the package parser is strict.
    /// A manifest with no `[permissions]` section proves the two differ.
    #[test]
    fn requested_and_legacy_parsers_disagree_by_design() {
        let val: toml::Value = toml::from_str("[package]\nname = \"p\"").unwrap();
        let legacy = PluginPermissions::from_manifest(&val);
        let requested = Requested::from_manifest(&val);
        for perm in Permission::ALL {
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
        let granted = Requested::from_manifest(&val).granted_for_manual_install();
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

    #[test]
    fn every_requested_permission_must_be_granted() {
        let manifest: toml::Value =
            toml::from_str("[permissions]\nnet = true\nrun = true\n").unwrap();
        let requested = Requested::from_manifest(&manifest);
        let mut granted = PluginPermissions::denied();
        granted.set(Permission::Net, true);

        assert!(!requested.is_granted_by(&granted));
        granted.set(Permission::Run, true);
        assert!(requested.is_granted_by(&granted));
    }
}
