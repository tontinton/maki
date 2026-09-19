pub(crate) mod agent;
pub(crate) mod r#async;
pub(crate) mod autocmd;
pub(crate) mod base64;
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
pub(crate) mod plan;
pub(crate) mod provider;
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
use crate::plugin_permissions::{NetEgress, Permission, PluginPermissions};
use maki_providers::plugin::DeclAuthority;

/// Who a `maki` global belongs to: the name everything it registers is filed
/// under, and whether that code shipped inside the binary. One value, so a
/// call site cannot hand over the name and forget the authority behind it.
#[derive(Clone)]
pub(crate) struct Owner {
    pub name: Arc<str>,
    pub authority: DeclAuthority,
}

pub(crate) fn create_maki_global(
    lua: &Lua,
    pending: PendingTools,
    pending_rules: PendingRules,
    owner: Owner,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    permissions: &PluginPermissions,
    opts: PluginOpts,
) -> LuaResult<Table> {
    let Owner {
        name: plugin,
        authority,
    } = owner;
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
    slot::add_slot_methods(&api, lua, Arc::clone(&plugin), permissions.clone())?;
    maki.set("api", api)?;
    maki.set("env", env::create_env_table(lua, permissions)?)?;
    maki.set(
        "fs",
        fs::create_fs_table(lua, permissions, Arc::clone(&plugin))?,
    )?;
    maki.set("log", log::create_log_table(lua, Arc::clone(&plugin))?)?;
    maki.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    maki.set("uv", uv::create_uv_table(lua, permissions)?)?;
    maki.set("base64", base64::create_base64_table(lua)?)?;
    maki.set("image", image::create_image_table(lua)?)?;
    maki.set("json", json::create_json_table(lua)?)?;
    maki.set("yaml", yaml::create_yaml_table(lua)?)?;
    // One egress value shared by the two namespaces that can open a socket, so
    // a provider registered through `maki.provider` is reachable from
    // `maki.net` without the manifest naming an origin only maki resolves.
    let egress = NetEgress::new(permissions.net_hosts());
    maki.set(
        "net",
        net::create_net_table(lua, permissions, egress.clone())?,
    )?;
    maki.set("plan", plan::create_plan_table(lua, ui_action_tx.clone())?)?;
    maki.set(
        "provider",
        provider::create_provider_namespace(
            lua,
            permissions,
            Arc::clone(&plugin),
            egress,
            authority,
        )?,
    )?;
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
