use std::sync::LazyLock;
use std::time::Instant;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult};

use crate::plugin_permissions::PluginPermissions;

/// Epoch for `hrtime`. Like libuv's, it is arbitrary: only differences
/// between two readings mean anything.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Return the current working directory as an absolute path. Like `vim.uv.cwd`.
///
/// @return (string?) Current working directory, or nil if it cannot be determined.
/// @example
/// local cwd = maki.uv.cwd()
/// if cwd then print("working in: " .. cwd) end
#[lua_fn(guard = FsRead)]
fn cwd(_lua: &Lua) -> LuaResult<Option<String>> {
    Ok(std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from)))
}

/// Return the current user's home directory. Like `vim.uv.os_homedir`.
///
/// @return (string?) Home directory path, or nil if it cannot be determined.
/// @example
/// local home = maki.uv.os_homedir() -- e.g. "/home/user"
#[lua_fn(guard = FsRead)]
fn os_homedir(_lua: &Lua) -> LuaResult<Option<String>> {
    Ok(maki_storage::paths::home().and_then(|p| p.to_str().map(String::from)))
}

/// Look up the environment variable {name}. Like `vim.uv.os_getenv`.
/// Returns nil when the variable is not set.
///
/// @param name string Name of the environment variable.
/// @return (string?) Variable value, or nil if not set.
/// @example
/// local editor = maki.uv.os_getenv("EDITOR") or "vi"
#[lua_fn(guard = Env)]
fn os_getenv(_lua: &Lua, name: String) -> LuaResult<Option<String>> {
    Ok(std::env::var(&name).ok())
}

/// Return a monotonic clock reading in nanoseconds. Like `vim.uv.hrtime`.
/// The epoch is arbitrary, so this is only useful for measuring how much
/// time passed between two readings; unlike `os.time` it never jumps when
/// the wall clock is adjusted.
///
/// @return (integer) Nanoseconds since an unspecified, fixed point in time.
/// @example
/// local start = maki.uv.hrtime()
/// local elapsed_ms = (maki.uv.hrtime() - start) / 1e6
#[lua_fn]
fn hrtime(_lua: &Lua) -> LuaResult<u64> {
    // u64 nanoseconds covers 584 years of uptime, so the cast cannot wrap in
    // any process that could still be running.
    Ok(EPOCH.elapsed().as_nanos() as u64)
}

lua_table! {
    /// System and environment utilities, modelled after `vim.uv`.
    ///
    /// Provides access to the working directory, home directory, and environment
    /// variables. None of these functions throw.
    ///
    /// Filesystem location queries (`cwd`, `os_homedir`) need `fs_read`, while
    /// `os_getenv` reads the process environment, where secrets live, so it needs
    /// `env`.
    ///
    /// ```lua
    /// local home = maki.uv.os_homedir()
    /// ```
    "maki.uv" => pub(crate) fn create_uv_table(perms: &PluginPermissions), DOCS [
        cwd(perms), os_homedir(perms), os_getenv(perms), hrtime,
    ]
}
