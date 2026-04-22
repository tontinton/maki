pub(crate) mod buf;
pub(crate) mod ctx;
pub(crate) mod fs;
pub(crate) mod json;
pub(crate) mod log;
pub(crate) mod net;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod treesitter;
pub(crate) mod uv;

use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::buf::{BufHandle, BufferStore};
use crate::api::tool::PendingTools;

pub(crate) fn create_maki_global(
    lua: &Lua,
    pending: PendingTools,
    fs_roots: Arc<[PathBuf]>,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let maki = lua.create_table()?;

    maki.set("api", tool::create_api_table(lua, pending)?)?;
    maki.set("fs", fs::create_fs_table(lua, fs_roots)?)?;
    maki.set("log", log::create_log_table(lua, plugin)?)?;
    maki.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    maki.set("uv", uv::create_uv_table(lua)?)?;
    maki.set("json", json::create_json_table(lua)?)?;
    maki.set("net", net::create_net_table(lua)?)?;
    maki.set("text", text::create_text_table(lua)?)?;
    maki.set("ui", create_ui_table(lua)?)?;

    Ok(maki)
}

fn create_ui_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;
    t.set(
        "buf",
        lua.create_function(|lua, ()| {
            let mut store = lua
                .app_data_mut::<BufferStore>()
                .ok_or_else(|| mlua::Error::runtime("buffer store not initialized"))?;
            let id = store.create();
            Ok(BufHandle(id))
        })?,
    )?;
    Ok(t)
}
