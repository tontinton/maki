use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::Lua;

fn log_emit(plugin: &str, level: &str, msg: &str) {
    match level {
        "debug" => tracing::debug!(plugin = %plugin, "{}", msg),
        "info" => tracing::info!(plugin = %plugin, "{}", msg),
        "warn" => tracing::warn!(plugin = %plugin, "{}", msg),
        "error" => tracing::error!(plugin = %plugin, "{}", msg),
        _ => unreachable!(),
    }
}

/// Emit a DEBUG-level log message. Useful for development and troubleshooting.
/// The message is tagged with the plugin name automatically.
///
/// @param msg string Message to log.
/// @example
/// maki.log.debug("loaded " .. #items .. " items")
#[lua_fn]
fn debug(_lua: &Lua, #[ctx] plugin: Arc<str>, msg: String) -> mlua::Result<()> {
    log_emit(&plugin, "debug", &msg);
    Ok(())
}

/// Emit an INFO-level log message. Good for normal operational events.
///
/// @param msg string Message to log.
/// @example
/// maki.log.info("plugin initialized")
#[lua_fn]
fn info(_lua: &Lua, #[ctx] plugin: Arc<str>, msg: String) -> mlua::Result<()> {
    log_emit(&plugin, "info", &msg);
    Ok(())
}

/// Emit a WARN-level log message. Use for recoverable problems.
///
/// @param msg string Message to log.
/// @example
/// maki.log.warn("config file missing, using defaults")
#[lua_fn]
fn warn(_lua: &Lua, #[ctx] plugin: Arc<str>, msg: String) -> mlua::Result<()> {
    log_emit(&plugin, "warn", &msg);
    Ok(())
}

/// Emit an ERROR-level log message. Use for failures that need attention.
///
/// @param msg string Message to log.
/// @example
/// maki.log.error("failed to connect to API")
#[lua_fn]
fn error(_lua: &Lua, #[ctx] plugin: Arc<str>, msg: String) -> mlua::Result<()> {
    log_emit(&plugin, "error", &msg);
    Ok(())
}

lua_table! {
    /// Structured logging for plugins.
    ///
    /// Each call emits a tracing event tagged with the calling plugin's name.
    /// Messages show up in maki's log output, which you can view with `maki --log`.
    ///
    /// ```lua
    /// maki.log.info("ready")
    /// maki.log.warn("something looks off")
    /// ```
    "maki.log" => pub(crate) fn create_log_table(plugin: Arc<str>), DOCS [
        debug(plugin), info(plugin), warn(plugin), error(plugin),
    ]
}
