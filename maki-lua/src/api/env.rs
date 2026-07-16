use maki_lua_macro::{lua_fn, lua_table};
use mlua::Lua;

use crate::plugin_permissions::PluginPermissions;

/// Return the directory where maki stores runtime state (sessions, auth tokens, etc.).
/// Typically something like `~/.local/state/maki`.
///
/// @return (string?) State directory path, or nil if it cannot be determined.
/// @example
/// local dir = maki.env.state_dir()
#[lua_fn(guard = Env)]
fn state_dir(_lua: &Lua) -> mlua::Result<Option<String>> {
    Ok(maki_storage::paths::state_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from)))
}

/// Return the directory where maki looks for user configuration files.
/// Typically something like `~/.config/maki`.
///
/// @return (string?) Config directory path, or nil if it cannot be determined.
/// @example
/// local dir = maki.env.config_dir()
#[lua_fn(guard = Env)]
fn config_dir(_lua: &Lua) -> mlua::Result<Option<String>> {
    Ok(maki_storage::paths::config_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from)))
}

/// Return the legacy config path (`~/.maki`), if it exists on disk.
/// Useful for migration logic. Returns nil when there is no legacy directory.
///
/// @return (string?) Legacy directory path, or nil if not present.
#[lua_fn(guard = Env)]
fn legacy_dir(_lua: &Lua) -> mlua::Result<Option<String>> {
    Ok(maki_storage::paths::legacy_home_dir().and_then(|p| p.to_str().map(String::from)))
}

lua_table! {
    /// Paths to maki's own directories (config, state, legacy).
    ///
    /// Use these to locate config files or persistent state without hard-coding paths.
    ///
    /// ```lua
    /// local cfg = maki.env.config_dir()
    /// ```
    "maki.env" => pub(crate) fn create_env_table(perms: &PluginPermissions), DOCS [
        state_dir(perms), config_dir(perms), legacy_dir(perms),
    ]
}
