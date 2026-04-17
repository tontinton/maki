use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

pub(crate) fn create_log_table(lua: &Lua, plugin: Arc<str>) -> LuaResult<Table> {
    let t = lua.create_table()?;

    let mk = |lua: &Lua, plugin: Arc<str>, level: &'static str| {
        lua.create_function(move |_, msg: String| {
            let plugin = &plugin;
            match level {
                "debug" => tracing::debug!(plugin = %plugin, "{}", msg),
                "info" => tracing::info!(plugin = %plugin, "{}", msg),
                "warn" => tracing::warn!(plugin = %plugin, "{}", msg),
                "error" => tracing::error!(plugin = %plugin, "{}", msg),
                _ => unreachable!(),
            }
            Ok(())
        })
    };

    t.set("debug", mk(lua, Arc::clone(&plugin), "debug")?)?;
    t.set("info", mk(lua, Arc::clone(&plugin), "info")?)?;
    t.set("warn", mk(lua, Arc::clone(&plugin), "warn")?)?;
    t.set("error", mk(lua, plugin, "error")?)?;

    Ok(t)
}
