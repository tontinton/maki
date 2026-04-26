pub(crate) mod buf;
pub(crate) mod ctx;
pub(crate) mod fn_api;
pub(crate) mod fs;
pub(crate) mod json;
pub(crate) mod log;
pub(crate) mod net;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod treesitter;
pub(crate) mod ui;
pub(crate) mod uv;

use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::tool::PendingTools;

pub(crate) fn create_maki_global(
    lua: &Lua,
    pending: PendingTools,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let maki = lua.create_table()?;

    maki.set("api", tool::create_api_table(lua, pending)?)?;
    maki.set("fs", fs::create_fs_table(lua)?)?;
    maki.set("log", log::create_log_table(lua, plugin)?)?;
    maki.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    maki.set("uv", uv::create_uv_table(lua)?)?;
    maki.set("json", json::create_json_table(lua)?)?;
    maki.set("net", net::create_net_table(lua)?)?;
    maki.set("text", text::create_text_table(lua)?)?;
    maki.set("ui", ui::create_ui_table(lua)?)?;
    maki.set("fn", fn_api::create_fn_table(lua)?)?;

    Ok(maki)
}
