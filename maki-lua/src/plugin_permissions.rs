use std::fmt;
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
    const ALL: [Permission; 5] = [
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Net,
        Permission::Run,
        Permission::Env,
    ];

    fn manifest_key(self) -> &'static str {
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

fn denied_error(perm: Permission) -> LuaError {
    LuaError::runtime(format!(
        "permission denied: '{}' not granted for this plugin",
        perm
    ))
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
        Err(_) => PluginPermissions::denied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_allows_everything() {
        let p = PluginPermissions::trusted();
        for perm in Permission::ALL {
            assert!(p.is_allowed(perm), "{perm} should be allowed");
        }
    }

    #[test]
    fn denied_blocks_everything() {
        let p = PluginPermissions::denied();
        for perm in Permission::ALL {
            assert!(!p.is_allowed(perm), "{perm} should be denied");
        }
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
        for perm in Permission::ALL {
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
}
