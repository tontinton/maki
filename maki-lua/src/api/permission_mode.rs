//! `maki.api.register_permission_mode`: permission levels a plugin adds to
//! the ones a host offers. The host treats the id as an opaque string, lists
//! the modes with [`list`] and tells the owner about a pick with [`select`].

use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table};

use crate::api::util::convert::TableExt;
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{plugin_loaded, run_command_scoped, strip_traceback};

const REGISTER_PERMISSION_MODE: &str = "register_permission_mode";
const BLANK_ID_ERR: &str = "register_permission_mode: 'id' must not be blank";
const UNKNOWN_MODE_ERR: &str = "unknown permission mode";

/// What a host shows for a mode: the id it hands back on a pick, and the
/// label and help text a user reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

struct ModeEntry {
    plugin: Arc<str>,
    info: PermissionModeInfo,
    on_select: Function,
}

/// In registration order, which is the order a host lists them in.
#[derive(Default)]
pub(crate) struct PermissionModeStore(Vec<ModeEntry>);

pub(crate) fn clear_plugin_permission_modes(lua: &Lua, plugin: &str) {
    if let Some(mut store) = lua.app_data_mut::<PermissionModeStore>() {
        store.0.retain(|entry| entry.plugin.as_ref() != plugin);
    }
}

pub(crate) fn list(lua: &Lua) -> Vec<PermissionModeInfo> {
    lua.app_data_ref::<PermissionModeStore>()
        .map(|store| store.0.iter().map(|entry| entry.info.clone()).collect())
        .unwrap_or_default()
}

/// Calls the owner's `on_select(session, selected)` and waits for it. The
/// error is the plugin's own, so a host can show why a pick did not take.
pub(crate) async fn select(
    lua: &Lua,
    id: &str,
    session: &str,
    selected: bool,
) -> Result<(), String> {
    let on_select = lua
        .app_data_ref::<PermissionModeStore>()
        .and_then(|store| {
            store
                .0
                .iter()
                .find(|entry| entry.info.id == id)
                .map(|entry| entry.on_select.clone())
        })
        .ok_or_else(|| format!("{UNKNOWN_MODE_ERR} '{id}'"))?;
    run_command_scoped(lua, 0, on_select.call_async::<()>((session, selected)))
        .await
        .map_err(|e| {
            let error = strip_traceback(&e);
            tracing::warn!(mode = id, error = %error, "permission mode handler failed");
            error
        })
}

/// Adds a permission level that hosts with a permission picker list next to
/// their own, like the `Permissions` option an ACP client shows. A host that
/// has no picker ignores it.
///
/// Choosing the mode calls `on_select(session, true)`. Choosing another level
/// afterwards, or the session ending, calls `on_select(session, false)`, so
/// the handler is where the plugin switches its behavior on and off. What the
/// mode does is up to the plugin, typically by layering `permission.prompt`.
/// The host only knows the id, so pick one that is unlikely to collide.
/// Registering an id again replaces the earlier mode, and the mode goes away
/// when your plugin unloads.
///
/// Throws when `id` is blank or `on_select` is missing.
///
/// @param spec table `{ id, name?, description?, on_select }`. `id` (string)
///   is what the host stores and hands back. `name` (string) is the label,
///   default the id. `description` (string) is the help text. `on_select`
///   (function) is called with the session id (string) and whether the mode
///   was selected (boolean).
/// @return
/// @example
/// maki.api.register_permission_mode({
///   id = "judge",
///   name = "Judge",
///   description = "A model answers permission prompts",
///   on_select = function(session, selected)
///     enabled = selected
///   end,
/// })
#[lua_fn(guard = SessionSettings)]
fn register_permission_mode(lua: &Lua, #[ctx] plugin: Arc<str>, spec: Table) -> LuaResult<()> {
    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::runtime(BLANK_ID_ERR))?;
    if id.trim().is_empty() {
        return Err(mlua::Error::runtime(BLANK_ID_ERR));
    }
    let on_select: Function = spec.get("on_select").map_err(|_| {
        mlua::Error::runtime(format!(
            "{REGISTER_PERMISSION_MODE}: missing 'on_select' function"
        ))
    })?;
    let info = PermissionModeInfo {
        name: spec.opt("name")?.unwrap_or_else(|| id.clone()),
        description: spec.opt("description")?,
        id,
    };
    // A handler from a load that is already gone would outlive its plugin.
    if !plugin_loaded(lua, &plugin) {
        tracing::debug!(mode = %info.id, plugin = %plugin, "permission mode dropped: plugin unloaded");
        return Ok(());
    }
    if let Some(mut store) = lua.app_data_mut::<PermissionModeStore>() {
        let entry = ModeEntry {
            plugin,
            info,
            on_select,
        };
        match store.0.iter_mut().find(|e| e.info.id == entry.info.id) {
            Some(existing) => *existing = entry,
            None => store.0.push(entry),
        }
    }
    Ok(())
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_permission_mode_methods(perms: &PluginPermissions, plugin: Arc<str>), DOCS [
        register_permission_mode(perms, plugin),
    ]
}
