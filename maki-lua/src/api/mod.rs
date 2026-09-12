pub(crate) mod agent;
pub(crate) mod r#async;
pub(crate) mod autocmd;
pub(crate) mod base64;
pub(crate) mod completer;
pub(crate) mod env;
pub(crate) mod r#fn;
pub(crate) mod fs;
pub(crate) mod image;
pub(crate) mod interpreter;
pub(crate) mod json;
pub(crate) mod keymap;
pub(crate) mod log;
pub(crate) mod model;
pub(crate) mod net;
pub(crate) mod options;
pub(crate) mod pack;
pub(crate) mod session;
pub(crate) mod slot;
pub(crate) mod split;
pub(crate) mod task;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod top;
pub(crate) mod treesitter;
pub(crate) mod ui;
pub(crate) mod util;
pub(crate) mod uv;
pub(crate) mod yaml;

use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::options::PluginOpts;
use crate::api::tool::{PendingRules, PendingTools};
use crate::api::util::command::UiAction;
use crate::plugin_permissions::{Permission, PluginPermissions};

pub(crate) fn create_maki_global(
    lua: &Lua,
    pending: PendingTools,
    pending_rules: PendingRules,
    plugin: Arc<str>,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    permissions: &PluginPermissions,
    opts: PluginOpts,
) -> LuaResult<Table> {
    let maki = lua.create_table()?;

    let api = tool::create_api_table(
        lua,
        pending,
        pending_rules,
        permissions.clone(),
        Arc::clone(&plugin),
        opts,
        ui_action_tx.clone(),
    )?;
    autocmd::add_autocmd_methods(&api, lua, Arc::clone(&plugin))?;
    slot::add_slot_methods(&api, lua, Arc::clone(&plugin))?;
    completer::add_completer_methods(&api, lua, Arc::clone(&plugin))?;
    maki.set("api", api)?;
    maki.set("env", env::create_env_table(lua, permissions)?)?;
    maki.set("fs", fs::create_fs_table(lua, permissions)?)?;
    maki.set("log", log::create_log_table(lua, Arc::clone(&plugin))?)?;
    maki.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    maki.set("uv", uv::create_uv_table(lua, permissions)?)?;
    maki.set("base64", base64::create_base64_table(lua)?)?;
    maki.set("image", image::create_image_table(lua)?)?;
    maki.set("json", json::create_json_table(lua)?)?;
    maki.set("yaml", yaml::create_yaml_table(lua)?)?;
    maki.set("net", net::create_net_table(lua, permissions)?)?;
    maki.set("text", text::create_text_table(lua)?)?;
    maki.set(
        "session",
        session::create_session_table(lua, ui_action_tx.clone())?,
    )?;
    maki.set(
        "model",
        model::create_model_table(lua, ui_action_tx.clone())?,
    )?;
    maki.set("task", task::create_task_table(lua, ui_action_tx.clone())?)?;
    maki.set(
        "ui",
        ui::create_ui_table(lua, ui_action_tx.clone(), Arc::clone(&plugin))?,
    )?;
    maki.set(
        "fn",
        r#fn::create_fn_table(
            lua,
            Arc::clone(&plugin),
            permissions,
            permissions.is_allowed(Permission::FsWrite),
            ui_action_tx.clone(),
        )?,
    )?;
    split::split__register(&maki, lua)?;
    top::add_top_methods(&maki, lua, Arc::clone(&plugin))?;
    maki.set("async", r#async::create_async_table(lua)?)?;
    maki.set(
        "interpreter",
        interpreter::create_interpreter_table(lua, permissions)?,
    )?;
    maki.set("agent", agent::create_agent_table(lua)?)?;
    maki.set(
        "keymap",
        keymap::create_keymap_table(lua, Arc::clone(&plugin))?,
    )?;
    pack::add_packadd(lua, &maki)?;
    maki.set("pack", pack::create_pack_read_table(lua)?)?;

    // `notify` sits on the metatable's `__index` rather than on the table
    // itself, because Lua only fires `__newindex` for keys missing from the
    // raw table. That is what gives `maki.notify = fn` somewhere to be caught
    // and routed into the one shared slot, instead of quietly shadowing notify
    // for the assigning plugin alone.
    let index = lua.create_table()?;
    top::notify__register(&index, lua, ui_action_tx, Arc::clone(&plugin))?;
    let notify_router = lua.create_function(
        move |lua, (t, k, v): (Table, String, Value)| match k.as_str() {
            "notify" => top::install_notify_handler(lua, Arc::clone(&plugin), v),
            _ => t.raw_set(k, v),
        },
    )?;
    let meta = lua.create_table()?;
    meta.set("__index", index)?;
    meta.set("__newindex", notify_router)?;
    maki.set_metatable(Some(meta))?;

    Ok(maki)
}
