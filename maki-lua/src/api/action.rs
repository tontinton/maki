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
use crate::runtime::{ConfigScope, Narrowed, any_plugin_loaded, plugin_loaded, strip_traceback};

pub(crate) const UNKNOWN_ACTION_ERR: &str = "unknown action";
const USER_PLUGIN_ERR: &str =
    "register_action: a plugin named 'user' cannot register actions, `user.` belongs to init.lua";
const ACTION_FAILED_ERR: &str = "failed";
const OWNER_NOT_LOADED_ERR: &str = "no loaded plugin provides action";
const USER_PREFIX: &str = "user";
const REGISTER_ACTION: &str = "register_action";
const ACTION_NAME_ERR: &str = "an action name must be non-empty and have no `.`";

/// A plugin action, as the help screen lists it.
#[derive(Clone, Debug)]
pub struct ActionInfo {
    pub name: Arc<str>,
    pub plugin: Arc<str>,
}

struct ActionEntry {
    plugin: Arc<str>,
    /// The price: the owner's whole reach, or the part of it the owner
    /// declared, as far as the code registering it could use. A caller has to
    /// hold it, and the callback holds no more.
    reach: PluginReach,
    callback: Function,
}

/// Ordered by name, which is the order the help screen lists them in. A name
/// holds one entry per load that registered it, which only the two
/// `init.lua` files can share, so each of them can reload or unload without
/// touching the other's.
#[derive(Default)]
pub(crate) struct ActionStore(BTreeMap<Arc<str>, Vec<ActionEntry>>);

impl ActionStore {
    /// The project `init.lua` wins over the global one.
    fn get(&self, name: &str) -> Option<&ActionEntry> {
        self.0
            .get(name)?
            .iter()
            .max_by_key(|entry| entry.plugin.as_ref() == ConfigScope::Project.label())
    }
}

fn publish(lua: &Lua) {
    let Some(store) = lua.app_data_ref::<ActionStore>() else {
        return;
    };
    let infos = store
        .0
        .keys()
        .filter_map(|name| {
            Some(ActionInfo {
                name: Arc::clone(name),
                plugin: Arc::clone(&store.get(name)?.plugin),
            })
        })
        .collect();
    if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
        writer.publish_actions(infos);
    }
}

pub(crate) fn clear_plugin_actions(lua: &Lua, plugin: &str) {
    if let Some(mut store) = lua.app_data_mut::<ActionStore>() {
        store.0.retain(|_, entries| {
            entries.retain(|entry| entry.plugin.as_ref() != plugin);
            !entries.is_empty()
        });
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
/// The refusal is flashed as well: a key composed in `init.lua` goes on to
/// its fallback, and the user would never learn which grant was missing.
///
/// The callback reads whatever the caller set up, like the input and its
/// selection, so it runs under a [`Ceiling`] of the price alone, also when
/// the owner calls it. Grants of the owner beyond it would be the caller's
/// to steer.
///
/// [`Ceiling`]: crate::plugin_permissions::Ceiling
pub(crate) async fn run_plugin_action(
    lua: &Lua,
    tx: Option<&flume::Sender<UiAction>>,
    name: &str,
    plugin: &str,
    caller: &PluginReach,
) -> LuaResult<Pair<bool>> {
    let entry = lua.app_data_ref::<ActionStore>().and_then(|store| {
        store
            .get(name)
            .map(|entry| (entry.callback.clone(), entry.reach.clone()))
    });
    let Some((callback, price)) = entry else {
        let owned_by = |owner: &str| {
            action_prefix(owner)
                .and_then(|prefix| name.strip_prefix(prefix))
                .is_some_and(|rest| rest.starts_with('.'))
        };
        return if name.contains('.') && !any_plugin_loaded(lua, owned_by) {
            Ok(err_pair(format!("{OWNER_NOT_LOADED_ERR} '{name}'")))
        } else {
            Err(mlua::Error::runtime(unknown_action(lua, name)))
        };
    };
    if let Some(missing) = caller.now(lua).first_missing(&price) {
        let refusal = missing.refusal(name);
        tracing::warn!(action = name, caller = plugin, "{refusal}");
        let _ = ui_send(tx, UiAction::Flash(refusal.clone()));
        return Ok(err_pair(refusal));
    }
    let call = callback.call_async::<bool>(());
    Ok(pair(
        Narrowed::to(lua, price.permissions(), call)
            .await
            .map_err(|e| {
                let error = strip_traceback(&e);
                tracing::warn!(action = name, error = %error, "plugin action failed");
                format!("action '{name}' {ACTION_FAILED_ERR}: {error}")
            }),
    ))
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

/// Registers {callback} as an action that `maki.ui.action` runs. This is
/// how a user's `init.lua` reaches your plugin, since `require` only sees
/// the calling plugin's own files.
///
/// The action is named after your plugin: `register_action("accept", ...)`
/// in a plugin installed as `completion` registers `"completion.accept"`. In
/// the global or project `init.lua` the prefix is `user`, like
/// `"user.paste_plain"`. A plugin named `user` cannot register actions, so no
/// plugin can replace the ones a user's keys call.
///
/// The callback runs as your plugin, inside the caller's key handler, and
/// `maki.ui.action` returns its value. Return `true` when it handled the
/// request, and `false` or nothing so the caller can try something else.
/// Registering a name again replaces the callback, and the action goes away
/// when your plugin unloads. When both `init.lua` files register a name, the
/// project one wins until it is gone.
///
/// `opts.capability` is the price of the action: the permissions a caller
/// must hold. Without it, the price is every permission your plugin holds.
/// With `{}`, any caller gets in, and the callback holds no permission. A
/// caller that falls short gets `nil` and an error naming the missing
/// permission.
///
/// The price also caps the callback, whoever calls it, so list everything
/// it uses. A call that needs a permission outside the price fails as it
/// would in a plugin without that permission. The cap covers the tasks,
/// timers and jobs the callback starts, and the tools, permission rules,
/// slot layers and actions it registers. Keymaps and autocmds it sets run
/// later with all your grants. Code of other plugins that runs meanwhile,
/// like their autocmds, keeps its own grants. For example,
/// `completion.accept` writes file paths into the input that Enter then
/// sends, so it lists `fs_read`, `input`, `session_read`, `agent` and
/// `session_write`.
///
/// Also bind a default key for users who do not
/// [compose keys](/docs/keybindings/#composing-keys), preferably an Alt chord,
/// since your binding [shadows theirs](/docs/keybindings/#overriding-keybindings)
/// on the same key.
///
/// Throws when `capability` lists a permission your plugin does not hold, or
/// when {name} is empty or has a `.`.
///
/// @param name string The name after the prefix, like `"accept"`.
/// @param callback function Called with no arguments. Return `true` when handled.
/// @param opts table|nil `{ capability = { "fs_read", ... } }`: what a caller must hold. Default: every permission your plugin holds.
/// @return (string) The full name to pass to `maki.ui.action`, like `"completion.accept"`.
/// @example
/// -- Writes a file path into the input, so a caller must hold what that takes:
/// maki.api.register_action("accept", function()
///   if not Menu.open() then return false end
///   Menu.accept()
///   return true
/// end, { capability = { "fs_read", "input", "session_read", "agent", "session_write" } })
#[lua_fn]
fn register_action(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] reach: PluginReach,
    name: String,
    callback: Function,
    opts: Option<Table>,
) -> LuaResult<String> {
    let Some(prefix) = action_prefix(&plugin) else {
        return Err(mlua::Error::runtime(USER_PLUGIN_ERR));
    };
    // A package name can hold dots too, so a dotted name could pose as
    // another plugin's action, like `user.paste` registering `plain`.
    if name.is_empty() || name.contains('.') {
        return Err(mlua::Error::runtime(ACTION_NAME_ERR));
    }
    let name = format!("{prefix}.{name}");
    let reach = reach.now(lua);
    let needed = parse_capability(opts.as_ref(), REGISTER_ACTION, &name, reach.permissions())?;
    let reach = match needed {
        Some(perms) => reach.only(&perms),
        None => reach,
    };
    // A handler from a load that is already gone would leave an action that
    // nothing ever takes away.
    if !plugin_loaded(lua, &plugin) {
        tracing::debug!(action = %name, plugin = %plugin, "action dropped: plugin unloaded");
        return Ok(name);
    }
    if let Some(mut store) = lua.app_data_mut::<ActionStore>() {
        let entries = store.0.entry(Arc::from(name.as_str())).or_default();
        entries.retain(|entry| entry.plugin != plugin);
        entries.push(ActionEntry {
            plugin,
            reach,
            callback,
        });
    }
    publish(lua);
    Ok(name)
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
    use crate::api::slot::UNGRANTED_PRICE_ERR;
    use crate::plugin_permissions::{Missing, OUTSIDE_CEILING, Permission, PluginPermissions};

    const OWNER: &str = "owner";
    const CALLER: &str = "caller";
    const SHORT_NAME: &str = "act";
    const ACTION: &str = "owner.act";
    const USER_ACTION: &str = "user.act";
    const GLOBAL_INIT: &str = "global/init.lua";
    const PROJECT_INIT: &str = "project/init.lua";
    const USER_PLUGIN: &str = "user";
    const RAISED: &str = "boom";

    fn new_host() -> PluginHost {
        PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
    }

    fn register(body: &str) -> String {
        format!("maki.api.register_action('{SHORT_NAME}', function() {body} end)\n")
    }

    fn register_with(opts: &str) -> String {
        format!("maki.api.register_action('{SHORT_NAME}', function() return true end, {opts})\n")
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
        host.load_source(OWNER, &register(body)).unwrap();
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
        host.load_source(OWNER, &register_with(opts)).unwrap();
        let refusal = missing.map(|perm| Missing::Permission(perm).refusal(ACTION));
        let ok = if refusal.is_some() { "nil" } else { "true" };

        call_as(&host, caller, ACTION, ok, refusal.as_deref());
    }

    /// The callback runs with only the price of its owner's grants, so a
    /// caller cannot steer the rest. The owner here holds everything.
    #[test_case("{ capability = { 'fs_read' } }", "maki.session.prompt('hi')", Some(OUTSIDE_CEILING) ; "a_guarded_call_outside_the_price")]
    #[test_case("{ capability = { 'fs_read' } }", "maki.session.notify('hi', { session = 'x', wake = true })", Some(OUTSIDE_CEILING) ; "a_checked_option_outside_the_price")]
    #[test_case("{ capability = {} }", "maki.ui.input_edit({ start = 0, stop = 0, text = 'x', version = 0, session_id = 'x' })", Some(OUTSIDE_CEILING) ; "an_input_edit_from_an_open_action")]
    #[test_case("{ capability = { 'fs_read' } }", "maki.ui.action('undo')", Some(OUTSIDE_CEILING) ; "an_editing_builtin_outside_the_price")]
    #[test_case("{ capability = {} }", "maki.api.register_action('more', function() end, { capability = { 'fs_read' } })", Some(UNGRANTED_PRICE_ERR) ; "an_action_priced_above_the_price")]
    #[test_case("{ capability = { 'fs_read' } }", "maki.fs.read('/nonexistent')", None ; "a_call_inside_the_price")]
    #[test_case("nil", "maki.fs.read('/nonexistent')", None ; "an_undeclared_price_is_every_grant")]
    fn a_plugin_action_holds_only_its_price(opts: &str, call: &str, err: Option<&str>) {
        let host = new_host();
        let source = format!(
            "maki.api.register_action('{SHORT_NAME}', function() {call} return true end, {opts})\n"
        );
        host.load_source(OWNER, &source).unwrap();
        let ok = if err.is_some() { "nil" } else { "true" };

        call_as(&host, PluginPermissions::trusted(), ACTION, ok, err);
    }

    /// The price binds the owner's code alone. A handler of another plugin
    /// that the callback fires keeps that plugin's grants, and shows it by
    /// registering an action only a successful read reaches.
    #[test]
    fn another_plugins_handler_inside_the_action_keeps_its_grants() {
        const BYSTANDER: &str = "bystander";
        const PROBE: &str = "bystander.read";
        let host = new_host();
        let handler = "maki.api.create_autocmd('User', { pattern = 'probe', callback = function()\n\
             if pcall(maki.fs.read, '/nonexistent') then\n\
             maki.api.register_action('read', function() return true end)\n\
             end\n\
             end })";
        host.load_source(BYSTANDER, handler).unwrap();
        let source = format!(
            "maki.api.register_action('{SHORT_NAME}', function()\n\
             maki.api.exec_autocmds('User', {{ pattern = 'probe' }})\n\
             return true\n\
             end, {{ capability = {{}} }})"
        );
        host.load_source(OWNER, &source).unwrap();

        call(&host, "true", None);
        call_named(&host, PROBE, "true", None);
    }

    /// A layer set inside the action steers only what the price covers, also
    /// once the action is over. The slot charges `fs_read`, which the owner
    /// holds.
    #[test_case("{ capability = {} }", "false" ; "a_price_without_it")]
    #[test_case("{ capability = { 'fs_read' } }", "true" ; "a_price_with_it")]
    fn a_layer_set_inside_the_action_steers_within_the_price(opts: &str, layered: &str) {
        const SLOT_OWNER: &str = "slotowner";
        const FIRE: &str = "slotowner.fire";
        let host = new_host();
        let slot = "local s = maki.api.declare_slot('slotowner.s', function() return 'plain' end, \
             { capability = { 'fs_read' } })\n\
             maki.api.register_action('fire', function() return s() == 'layered' end)";
        host.load_source(SLOT_OWNER, slot).unwrap();
        let source = format!(
            "maki.api.register_action('{SHORT_NAME}', function()\n\
             maki.api.set_slot('slotowner.s', function() return 'layered' end)\n\
             return true\n\
             end, {opts})"
        );
        host.load_source(OWNER, &source).unwrap();

        call(&host, "true", None);
        call_named(&host, FIRE, layered, None);
    }

    /// Once the callback returns, its owner holds every grant again.
    #[test]
    fn the_price_ends_with_the_callback() {
        let host = new_host();
        let source = format!(
            "{}maki.ui.action('{ACTION}')\nlocal ok = pcall(maki.fs.read, '/nonexistent')\nassert(ok)",
            register_with("{ capability = {} }")
        );
        host.load_source(OWNER, &source).unwrap();
    }

    /// A price in a grant the owner does not hold would be a toll on reach
    /// it has no claim to.
    #[test]
    fn register_action_refuses_to_name_a_permission_its_owner_lacks() {
        let err = new_host()
            .load_source_with_permissions(
                OWNER,
                &register_with("{ capability = { 'net' } }"),
                holding(&[Permission::FsRead]),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(REGISTER_ACTION) && err.contains(UNGRANTED_PRICE_ERR),
            "{err}"
        );
    }

    /// A package named `user.paste` registering `plain` would otherwise own
    /// `user.paste.plain`, an action the user's `init.lua` may call.
    #[test_case("plain.x" ; "a_dotted_name")]
    #[test_case("" ; "an_empty_name")]
    fn register_action_refuses_a_name_that_could_pose_as_another(name: &str) {
        let err = new_host()
            .load_source(
                OWNER,
                &format!("maki.api.register_action('{name}', function() end)"),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains(ACTION_NAME_ERR), "{err}");
    }

    /// A plugin cannot know the name it was installed under, so the host
    /// puts it in front and hands the full name back.
    #[test_case(OWNER, ACTION            ; "a_plugin_by_its_name")]
    #[test_case(GLOBAL_INIT, USER_ACTION ; "the_global_init_lua_under_user")]
    #[test_case(PROJECT_INIT, USER_ACTION ; "the_project_init_lua_under_user")]
    fn register_action_answers_the_prefixed_name(plugin: &str, full: &str) {
        let host = new_host();
        let source = format!(
            "assert(maki.api.register_action('{SHORT_NAME}', function() return true end) == '{full}')"
        );
        host.load_source(plugin, &source).unwrap();
        call_named(&host, full, "true", None);
    }

    /// A plugin named `user` would otherwise own `user.`, and could register
    /// or replace the actions the keys composed in `init.lua` call.
    #[test]
    fn a_plugin_named_user_cannot_register_actions() {
        let host = new_host();
        host.load_source(GLOBAL_INIT, &register("return true"))
            .unwrap();

        let err = host
            .load_source(USER_PLUGIN, &register("return false"))
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
        let source = register("return false") + &register("return true");
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
        host.load_source(OWNER, &register("")).unwrap();
        let err = host
            .load_source(CALLER, &format!("maki.ui.action('{name}')"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(UNKNOWN_ACTION_ERR), "{err}");
    }

    /// Both `init.lua` files register under `user.`. The project one wins
    /// whichever loaded last, and each can go without taking the other's.
    #[test]
    fn the_project_init_lua_wins_and_the_global_one_comes_back() {
        let host = new_host();
        host.load_source(PROJECT_INIT, &register("return true"))
            .unwrap();
        host.load_source(GLOBAL_INIT, &register("return false"))
            .unwrap();
        call_named(&host, USER_ACTION, "true", None);
        assert_eq!(
            listed(&host),
            [(USER_ACTION.to_owned(), PROJECT_INIT.to_owned())]
        );

        host.unload(PROJECT_INIT).unwrap();

        call_named(&host, USER_ACTION, "false", None);
        assert_eq!(
            listed(&host),
            [(USER_ACTION.to_owned(), GLOBAL_INIT.to_owned())]
        );
    }
}
