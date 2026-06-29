use std::sync::{Arc, Mutex};

use maki_config::RawConfig;
use mlua::{Function, Lua, LuaSerdeExt, Result as LuaResult};

pub(crate) type ConfigStore = Arc<Mutex<Option<RawConfig>>>;

const DOUBLE_SETUP_MSG: &str = "maki.setup() already called in this init.lua";

pub(crate) fn create_setup_fn(lua: &Lua, store: ConfigStore) -> LuaResult<Function> {
    lua.create_function(move |lua, table: mlua::Value| {
        let raw: RawConfig = lua
            .from_value(table)
            .map_err(|e| mlua::Error::runtime(e.to_string()))?;
        let mut guard = store.lock().unwrap();
        if guard.is_some() {
            return Err(mlua::Error::runtime(DOUBLE_SETUP_MSG));
        }
        *guard = Some(raw);
        Ok(())
    })
}
