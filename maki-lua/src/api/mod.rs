mod agent;
mod r#async;
pub(crate) mod autocmd;
pub(crate) mod env;
pub(crate) mod r#fn;
pub(crate) mod fs;
pub(crate) mod json;
pub(crate) mod keymap;
pub(crate) mod log;
pub(crate) mod net;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod treesitter;
pub(crate) mod ui;
pub(crate) mod util;
pub(crate) mod uv;
pub(crate) mod yaml;

use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::tool::PendingTools;
use crate::api::util::command::UiAction;
use crate::plugin_permissions::PluginPermissions;

pub(crate) fn create_maki_global(
    lua: &Lua,
    pending: PendingTools,
    plugin: Arc<str>,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    permissions: &PluginPermissions,
) -> LuaResult<Table> {
    let maki = lua.create_table()?;

    let api = tool::create_api_table(lua, pending, Arc::clone(&plugin))?;
    autocmd::add_autocmd_methods(&api, lua, Arc::clone(&plugin))?;
    maki.set("api", api)?;
    maki.set("env", env::create_env_table(lua, permissions)?)?;
    maki.set("fs", fs::create_fs_table(lua, permissions)?)?;
    maki.set("log", log::create_log_table(lua, Arc::clone(&plugin))?)?;
    maki.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    maki.set("uv", uv::create_uv_table(lua, permissions)?)?;
    maki.set("json", json::create_json_table(lua)?)?;
    maki.set("yaml", yaml::create_yaml_table(lua)?)?;
    maki.set("net", net::create_net_table(lua, permissions)?)?;
    maki.set("text", text::create_text_table(lua)?)?;
    maki.set(
        "ui",
        ui::create_ui_table(lua, ui_action_tx, Arc::clone(&plugin))?,
    )?;
    maki.set("fn", r#fn::create_fn_table(lua, permissions)?)?;
    maki.set("async", r#async::create_async_table(lua)?)?;
    agent::register(lua, &maki)?;
    maki.set(
        "keymap",
        keymap::create_keymap_table(lua, Arc::clone(&plugin))?,
    )?;

    Ok(maki)
}
