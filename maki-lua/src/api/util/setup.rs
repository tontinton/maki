use std::sync::{Arc, Mutex};

use maki_config::RawConfig;
use mlua::{Function, Lua, LuaSerdeExt, Result as LuaResult};

use crate::api::split::split__doc;
use crate::docs::{DocKind, FnDoc, ModuleDoc, ParamDoc};

pub(crate) type ConfigStore = Arc<Mutex<Option<RawConfig>>>;

const DOUBLE_SETUP_MSG: &str = "maki.setup() already called in this init.lua";

pub(crate) const DOCS: ModuleDoc = ModuleDoc {
    name: "maki",
    kind: DocKind::Table,
    desc: "The global entry point. Every API lives under this table.",
    fns: &[
        FnDoc {
            name: "setup",
            args: "{config}",
            desc: "Apply your personal configuration. This is only available inside \
`init.lua` (not in plugins) and can be called at most once. The table \
accepts the same keys as the Configuration reference.",
            params: &[ParamDoc {
                name: "{config}",
                ty: "table",
                desc: "Configuration table.",
            }],
            returns: "",
            example: "maki.setup({\n\
  model = \"opus\",\n\
  keymaps = false,\n\
})",
        },
        split__doc,
    ],
};

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
