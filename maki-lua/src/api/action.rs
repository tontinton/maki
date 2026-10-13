use std::collections::BTreeMap;
use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table};
use strum::VariantNames;

use crate::api::keymap::KeymapWriter;
use crate::api::slot::parse_capability;
use crate::api::util::command::{BuiltinAction, UiAction, ui_send};
use crate::api::util::pair::{Pair, err_pair, pair};
use crate::plugin_permissions::PluginReach;
use crate::runtime::{ConfigScope, any_plugin_loaded, plugin_loaded, strip_traceback};

pub(crate) const UNKNOWN_ACTION_ERR: &str = "unknown action";
pub(crate) const ACTION_NAME_ERR: &str = "register_action: the name must be";
const USER_PLUGIN_ERR: &str =
    "register_action: a plugin named 'user' cannot register actions, `user.` belongs to init.lua";
const ACTION_FAILED_ERR: &str = "failed";
const OWNER_NOT_LOADED_ERR: &str = "no loaded plugin provides action";
const USER_PREFIX: &str = "user";
const REGISTER_ACTION: &str = "register_action";
const CALLERS: &str = "callers";

/// A plugin action, as the help screen lists it.
#[derive(Clone, Debug)]
pub struct ActionInfo {
    pub name: Arc<str>,
    pub plugin: Arc<str>,
}

struct ActionEntry {
    plugin: Arc<str>,
    /// What a caller has to hold: the owner's whole reach, which the callback
    /// runs with, or the part of it the owner declared.
    reach: PluginReach,
    callback: Function,
}

/// Ordered by name, which is the order the help screen lists them in.
#[derive(Default)]
pub(crate) struct ActionStore(BTreeMap<Arc<str>, ActionEntry>);

fn publish(lua: &Lua) {
    let Some(store) = lua.app_data_ref::<ActionStore>() else {
        return;
    };
    let infos = store
        .0
        .iter()
        .map(|(name, entry)| ActionInfo {
            name: Arc::clone(name),
            plugin: Arc::clone(&entry.plugin),
        })
        .collect();
    if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
        writer.publish_actions(infos);
    }
}

pub(crate) fn clear_plugin_actions(lua: &Lua, plugin: &str) {
    if let Some(mut store) = lua.app_data_mut::<ActionStore>() {
        store.0.retain(|_, entry| entry.plugin.as_ref() != plugin);
    }
    publish(lua);
}

/// The prefix {plugin}'s action names start with. Both `init.lua` files
/// share one, so a user's actions keep their names when they move from one
/// file to the other. A plugin named `user` has none: the keys a user
/// composes in `init.lua` call `user.` actions, and a plugin must not be
/// able to take them over.
fn action_prefix(plugin: &str) -> Option<&str> {
    if plugin == ConfigScope::Global.label() || plugin == ConfigScope::Project.label() {
        Some(USER_PREFIX)
    } else {
        (plugin != USER_PREFIX).then_some(plugin)
    }
}

/// Whether {name} is {plugin}'s prefix, a dot, and a name of its own.
fn owns(plugin: &str, name: &str) -> bool {
    action_prefix(plugin)
        .and_then(|prefix| name.strip_prefix(prefix))
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|rest| !rest.is_empty())
}

/// Runs right here inside the caller's task, with no trip to the UI. The
/// function closes over its own plugin's env, so it already runs as that
/// plugin for `require`, for `maki` and for the watchdog's blame.
///
/// When the action fails, that is not the caller's bug. So it answers
/// `nil, err` and the caller's handler can go on to its fallback. Any value
/// but `nil` and `false` counts as handled, the same as in a Lua `if`.
///
/// A name no loaded plugin claims answers `nil, err` too: the plugin may be
/// disabled, missing or unloaded, and a key composed in `init.lua` should
/// still work without it. A name its loaded owner never registered is a
/// typo, and throws.
///
/// A {caller}, the reach of plugin {plugin}, that falls short of what the
/// action needs, checked the way `maki.api.run_command` checks a plugin's
/// command, answers `nil, err` too.
/// The callback reads whatever the caller set up, like the input and its
/// selection, so the caller could otherwise steer the owner's grants. The
/// refusal is flashed as well: a key composed in `init.lua` goes on to its
/// fallback, and the user would never learn which grant was missing.
pub(crate) async fn run_plugin_action(
    lua: &Lua,
    tx: Option<&flume::Sender<UiAction>>,
    name: &str,
    plugin: &str,
    caller: &PluginReach,
) -> LuaResult<Pair<bool>> {
    let entry = lua.app_data_ref::<ActionStore>().and_then(|store| {
        store
            .0
            .get(name)
            .map(|entry| (entry.callback.clone(), caller.first_missing(&entry.reach)))
    });
    let Some((callback, missing)) = entry else {
        return if name.contains('.') && !any_plugin_loaded(lua, |plugin| owns(plugin, name)) {
            Ok(err_pair(format!("{OWNER_NOT_LOADED_ERR} '{name}'")))
        } else {
            Err(mlua::Error::runtime(unknown_action(lua, name)))
        };
    };
    if let Some(missing) = missing {
        let refusal = missing.refusal(name);
        tracing::warn!(action = name, caller = plugin, "{refusal}");
        let _ = ui_send(tx, UiAction::Flash(refusal.clone()));
        return Ok(err_pair(refusal));
    }
    Ok(pair(callback.call_async::<bool>(()).await.map_err(|e| {
        let error = strip_traceback(&e);
        tracing::warn!(action = name, error = %error, "plugin action failed");
        format!("action '{name}' {ACTION_FAILED_ERR}: {error}")
    })))
}

fn unknown_action(lua: &Lua, name: &str) -> String {
    let mut known = BuiltinAction::VARIANTS.join(", ");
    if let Some(store) = lua.app_data_ref::<ActionStore>() {
        for action in store.0.keys() {
            known.push_str(", ");
            known.push_str(action);
        }
    }
    format!("{UNKNOWN_ACTION_ERR} '{name}' (known: {known})")
}

/// Registers {callback} as an action that `maki.ui.action(name)` runs. This
/// is how a user's `init.lua` reaches your plugin, since `require` only sees
/// the calling plugin's own files.
///
/// {name} is your plugin's name, a dot, and a name of your choice, like
/// `"completion.accept"`. In the global or project `init.lua` the prefix is
/// `user`, like `"user.paste_plain"`. A plugin named `user` cannot register
/// actions, so no plugin can replace the ones a user's keys call.
///
/// The callback runs as your plugin, inside the caller's key handler. Return
/// `true` when it handled the request, and `false` or nothing when it did not,
/// so the caller can try something else. `maki.ui.action` returns that value.
/// Registering a name again replaces the callback, so the project `init.lua`
/// wins over the global one. The action goes away when your plugin unloads.
///
/// The callback runs with all your plugin's permissions, so by default a
/// caller must hold all of them too. `opts.capability` lowers that to the
/// permissions you list, and `{}` lets any caller in. The callback still runs
/// with all of yours and acts on what the caller set up, like the input
/// text. Choose a capability that makes the action safe for any caller
/// holding only those permissions. `completion.accept` writes file paths
/// into the input, so a caller must hold `fs_read`. A caller that falls
/// short gets `nil` and an error naming the missing permission, which the
/// status bar shows too.
///
/// Also bind a default key to the action, for users who never compose keys.
/// Plugins load after `init.lua`, so your binding shadows a user's binding
/// on the same key. Pick a key that a user who composes keys is unlikely to
/// bind, like an Alt chord, over `<Tab>` or `<CR>`.
///
/// Throws when {name} has the wrong prefix, or `capability` lists a
/// permission your plugin does not hold.
///
/// @param name string `"<plugin>.<name>"`, or `"user.<name>"` in `init.lua`.
/// @param callback function Called with no arguments. Return `true` when handled.
/// @param opts table|nil `{ capability = { "fs_read", ... } }`: what a caller must hold. Default: every permission your plugin holds.
/// @return
/// @example
/// -- Inserts a file path, so a caller must be allowed to read files itself:
/// maki.api.register_action("completion.accept", function()
///   if not Menu.open() then return false end
///   Menu.accept()
///   return true
/// end, { capability = { "fs_read" } })
#[lua_fn]
fn register_action(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] reach: PluginReach,
    name: String,
    callback: Function,
    opts: Option<Table>,
) -> LuaResult<()> {
    let Some(prefix) = action_prefix(&plugin) else {
        return Err(mlua::Error::runtime(USER_PLUGIN_ERR));
    };
    if !owns(&plugin, &name) {
        return Err(mlua::Error::runtime(format!(
            "{ACTION_NAME_ERR} '{prefix}.<name>', got '{name}'"
        )));
    }
    let needed = parse_capability(
        opts.as_ref(),
        REGISTER_ACTION,
        &name,
        CALLERS,
        reach.permissions(),
    )?;
    let reach = match needed {
        Some(perms) => reach.only(&perms),
        None => reach,
    };
    // A handler from a load that is already gone would leave an action that
    // nothing ever takes away.
    if !plugin_loaded(lua, &plugin) {
        tracing::debug!(action = %name, plugin = %plugin, "action dropped: plugin unloaded");
        return Ok(());
    }
    if let Some(mut store) = lua.app_data_mut::<ActionStore>() {
        store.0.insert(
            Arc::from(name),
            ActionEntry {
                plugin,
                reach,
                callback,
            },
        );
    }
    publish(lua);
    Ok(())
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_action_methods(plugin: Arc<str>, reach: PluginReach), DOCS [
        register_action(plugin, reach),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use maki_agent::tools::ToolRegistry;
    use test_case::test_case;

    use super::*;
    use crate::PluginHost;
    use crate::plugin_permissions::{Missing, Permission, PluginPermissions};

    const OWNER: &str = "owner";
    const CALLER: &str = "caller";
    const ACTION: &str = "owner.act";
    const USER_ACTION: &str = "user.act";
    const GLOBAL_INIT: &str = "global/init.lua";
    const PROJECT_INIT: &str = "project/init.lua";
    const USER_PLUGIN: &str = "user";
    const RAISED: &str = "boom";

    fn new_host() -> PluginHost {
        PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
    }

    fn register(name: &str, body: &str) -> String {
        format!("maki.api.register_action('{name}', function() {body} end)\n")
    }

    fn register_with(name: &str, opts: &str) -> String {
        format!("maki.api.register_action('{name}', function() return true end, {opts})\n")
    }

    fn holding(perms: &[Permission]) -> PluginPermissions {
        let mut permissions = PluginPermissions::denied();
        for &perm in perms {
            permissions.set(perm, true);
        }
        permissions
    }

    fn all_but(perm: Permission) -> PluginPermissions {
        let mut permissions = PluginPermissions::trusted();
        permissions.set(perm, false);
        permissions
    }

    /// Goes through `maki.ui.action` from another plugin, the same way a
    /// user's `init.lua` would.
    fn call(host: &PluginHost, ok: &str, err: Option<&str>) {
        call_named(host, ACTION, ok, err);
    }

    fn call_named(host: &PluginHost, name: &str, ok: &str, err: Option<&str>) {
        call_as(host, PluginPermissions::trusted(), name, ok, err);
    }

    fn call_as(
        host: &PluginHost,
        permissions: PluginPermissions,
        name: &str,
        ok: &str,
        err: Option<&str>,
    ) {
        let err_check = match err {
            Some(want) => {
                format!("assert(err and err:find([==[{want}]==], 1, true), tostring(err))")
            }
            None => "assert(err == nil, err)".into(),
        };
        let source = format!(
            "local ok, err = maki.ui.action('{name}')\n\
             assert(ok == {ok}, 'got ' .. tostring(ok))\n\
             {err_check}"
        );
        host.load_source_with_permissions(CALLER, &source, permissions)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    fn listed(host: &PluginHost) -> Vec<(String, String)> {
        host.keymap_reader()
            .actions()
            .iter()
            .map(|a| (a.name.to_string(), a.plugin.to_string()))
            .collect()
    }

    #[test_case("return true",                "true",  None         ; "handled")]
    #[test_case("return false",               "false", None         ; "declined")]
    #[test_case("",                           "false", None         ; "nothing_returned_is_declined")]
    #[test_case(&format!("error('{RAISED}')"), "nil",   Some(RAISED) ; "raised_is_an_error_pair")]
    fn a_plugin_action_answers_with_its_result(body: &str, ok: &str, err: Option<&str>) {
        let host = new_host();
        host.load_source(OWNER, &register(ACTION, body)).unwrap();
        call(&host, ok, err);
    }

    /// The owner here holds everything. Naming nothing charges callers all of
    /// it, naming permissions charges exactly those, and an empty list lets
    /// anyone run the action. The refusal names the first one missing.
    #[test_case("nil",                             PluginPermissions::denied(),                 Some(Permission::ALL[0]) ; "closed_refuses_a_caller_without_grants")]
    #[test_case("nil",                             all_but(Permission::Env),                    Some(Permission::Env)    ; "closed_refuses_a_caller_short_of_one")]
    #[test_case("nil",                             PluginPermissions::trusted(),                None                     ; "closed_runs_for_a_caller_holding_all")]
    #[test_case("{ capability = {} }",             PluginPermissions::denied(),                 None                     ; "open_runs_for_a_caller_without_grants")]
    #[test_case("{ capability = { 'fs_read' } }", holding(&[Permission::FsRead]),              None                     ; "declared_runs_for_a_caller_holding_just_those")]
    #[test_case("{ capability = { 'fs_read' } }", all_but(Permission::FsRead),                 Some(Permission::FsRead) ; "declared_refuses_a_caller_without_them")]
    fn a_plugin_action_needs_what_its_owner_declared(
        opts: &str,
        caller: PluginPermissions,
        missing: Option<Permission>,
    ) {
        let host = new_host();
        host.load_source(OWNER, &register_with(ACTION, opts))
            .unwrap();
        let refusal = missing.map(|perm| Missing::Permission(perm).refusal(ACTION));
        let ok = if refusal.is_some() { "nil" } else { "true" };

        call_as(&host, caller, ACTION, ok, refusal.as_deref());
    }

    /// A price in a grant the owner does not hold would be a toll on reach
    /// it has no claim to.
    #[test]
    fn register_action_refuses_to_name_a_permission_its_owner_lacks() {
        let err = new_host()
            .load_source_with_permissions(
                OWNER,
                &register_with(ACTION, "{ capability = { 'net' } }"),
                holding(&[Permission::FsRead]),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(REGISTER_ACTION) && err.contains(CALLERS),
            "{err}"
        );
    }

    #[test_case(OWNER, "other.act"               ; "another_plugins_prefix")]
    #[test_case(OWNER, "owner."                  ; "no_name_after_the_dot")]
    #[test_case(OWNER, "owneract"                ; "no_dot")]
    #[test_case(OWNER, USER_ACTION               ; "the_user_prefix_from_a_plugin")]
    #[test_case(GLOBAL_INIT, "global/init.lua.act" ; "init_lua_by_its_plugin_name")]
    fn register_action_refuses_a_name_the_plugin_does_not_own(plugin: &str, name: &str) {
        let err = new_host()
            .load_source(plugin, &register(name, ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains(ACTION_NAME_ERR), "{err}");
    }

    /// A plugin named `user` would otherwise own `user.`, and could register
    /// or replace the actions the keys composed in `init.lua` call.
    #[test_case(USER_ACTION ; "an_init_lua_name")]
    #[test_case("other.act" ; "any_other_name")]
    fn a_plugin_named_user_cannot_register_actions(name: &str) {
        let host = new_host();
        host.load_source(GLOBAL_INIT, &register(USER_ACTION, "return true"))
            .unwrap();

        let err = host
            .load_source(USER_PLUGIN, &register(name, "return false"))
            .unwrap_err()
            .to_string();

        assert!(err.contains(USER_PLUGIN_ERR), "{err}");
        call_named(&host, USER_ACTION, "true", None);
    }

    /// Once the owner is gone its actions are a runtime gap rather than a
    /// typo, so asking for one answers a pair instead of throwing into the
    /// caller's handler.
    #[test]
    fn an_action_is_replaced_by_name_and_goes_on_unload() {
        let host = new_host();
        let source = register(ACTION, "return false") + &register(ACTION, "return true");
        host.load_source(OWNER, &source).unwrap();
        assert_eq!(listed(&host), [(ACTION.to_owned(), OWNER.to_owned())]);
        call(&host, "true", None);

        host.unload(OWNER).unwrap();

        assert!(listed(&host).is_empty());
        call(&host, "nil", Some(OWNER_NOT_LOADED_ERR));
    }

    #[test_case("file_pickr" ; "a_misspelled_builtin")]
    #[test_case("owner.nope" ; "a_name_its_loaded_plugin_never_registered")]
    fn an_unknown_action_throws(name: &str) {
        let host = new_host();
        host.load_source(OWNER, &register(ACTION, "")).unwrap();
        let err = host
            .load_source(CALLER, &format!("maki.ui.action('{name}')"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(UNKNOWN_ACTION_ERR), "{err}");
    }

    /// Both `init.lua` files register under `user.`, and the project one
    /// runs second, so its action replaces the global one like a second
    /// registration from one plugin would.
    #[test]
    fn init_lua_registers_under_user_and_the_project_file_wins() {
        let host = new_host();
        host.load_source(GLOBAL_INIT, &register(USER_ACTION, "return false"))
            .unwrap();
        call_named(&host, USER_ACTION, "false", None);

        host.load_source(PROJECT_INIT, &register(USER_ACTION, "return true"))
            .unwrap();

        call_named(&host, USER_ACTION, "true", None);
        assert_eq!(
            listed(&host),
            [(USER_ACTION.to_owned(), PROJECT_INIT.to_owned())]
        );
    }
}
