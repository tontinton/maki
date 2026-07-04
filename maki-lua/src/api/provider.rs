use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::provider_hooks::{ProviderHookStore, next_hook_id};
use crate::plugin_permissions::{Permission, PluginPermissions};
use maki_agent::agent::{REQUEST_STAGE, RESPONSE_END_STAGE};

const MISSING_CALLBACK_MSG: &str = "callback must be a function";
const STORE_NOT_INITIALIZED_MSG: &str = "provider hook store not initialized";

fn register_stage(
    lua: &Lua,
    t: &Table,
    plugin: Arc<str>,
    stage: &'static str,
    fname: &str,
) -> LuaResult<()> {
    let p = Arc::clone(&plugin);
    t.set(
        fname,
        lua.create_function(move |lua, opts: Table| {
            let slug_filter: Option<String> = opts.get("slug").ok();
            let callback: mlua::Function = opts
                .get("callback")
                .map_err(|_| mlua::Error::runtime(MISSING_CALLBACK_MSG))?;
            let id = next_hook_id();
            let key = lua.create_registry_value(callback)?;
            let mut store = lua
                .app_data_mut::<ProviderHookStore>()
                .ok_or_else(|| mlua::Error::runtime(STORE_NOT_INITIALIZED_MSG))?;
            store.register(id, stage.to_owned(), key, Arc::clone(&p), slug_filter);
            Ok(id)
        })?,
    )?;
    Ok(())
}

fn register_del(lua: &Lua, t: &Table) -> LuaResult<()> {
    t.set(
        "del_provider_hook",
        lua.create_function(|lua, id: u64| {
            let keys = lua
                .app_data_mut::<ProviderHookStore>()
                .map(|mut store| store.remove(id))
                .unwrap_or_default();
            for key in keys {
                let _ = lua.remove_registry_value(key);
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

fn denied_hook(lua: &Lua, perm: Permission) -> LuaResult<mlua::Function> {
    lua.create_function(move |_, _: mlua::MultiValue| -> LuaResult<mlua::Value> {
        Err(mlua::Error::runtime(format!(
            "permission denied: '{}' not granted for this plugin",
            perm
        )))
    })
}

pub(crate) fn create_provider_table(
    lua: &Lua,
    plugin: Arc<str>,
    perms: &PluginPermissions,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    let perm = Permission::ProviderHooks;
    if perms.is_allowed(perm) {
        register_stage(
            lua,
            &t,
            Arc::clone(&plugin),
            REQUEST_STAGE,
            "register_request_hook",
        )?;
        register_stage(
            lua,
            &t,
            Arc::clone(&plugin),
            RESPONSE_END_STAGE,
            "register_response_hook",
        )?;
        register_del(lua, &t)?;
    } else {
        t.set("register_request_hook", denied_hook(lua, perm)?)?;
        t.set("register_response_hook", denied_hook(lua, perm)?)?;
        t.set("del_provider_hook", denied_hook(lua, perm)?)?;
    }
    Ok(t)
}
