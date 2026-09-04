//! `maki.plan`: the plan-mode surface. Plugins add rows to the plan form,
//! suppress the built-in form to own the UI themselves, and read the
//! current plan state without reaching into session internals.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::util::command::{PlanRequest, UiAction, ui_json_roundtrip, ui_send};
use crate::api::util::pair::{Pair, try_pair};

pub(crate) const PLAN_ACTION_DEFAULT_ORDER: i64 = 500;

/// Per-plugin plan actions the UI merges into the form menu.
pub(crate) type PlanActionHandlerMap = HashMap<Arc<str>, HashMap<Arc<str>, PlanActionEntry>>;

pub(crate) struct PlanActionEntry {
    pub handler: RegistryKey,
    pub label: Arc<str>,
    pub desc: Arc<str>,
    pub order: i64,
}

#[derive(Clone)]
pub struct PlanActionInfo {
    pub plugin: Arc<str>,
    pub name: Arc<str>,
    pub label: Arc<str>,
    pub desc: Arc<str>,
    pub order: i64,
}

#[derive(Clone, Default)]
pub struct PlanActionSnapshot {
    pub actions: Vec<PlanActionInfo>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct PlanActionReader(Arc<ArcSwap<PlanActionSnapshot>>);

impl PlanActionReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(
            PlanActionSnapshot::default(),
        )))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<PlanActionSnapshot>> {
        self.0.load()
    }
}

pub(crate) struct PlanActionWriter {
    store: Arc<ArcSwap<PlanActionSnapshot>>,
    generation: AtomicU64,
}

impl PlanActionWriter {
    pub fn new() -> (Self, PlanActionReader) {
        let inner = Arc::new(ArcSwap::from_pointee(PlanActionSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            PlanActionReader(inner),
        )
    }

    pub fn publish(&self, actions: Vec<PlanActionInfo>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(PlanActionSnapshot {
            actions,
            generation,
        }));
    }
}

pub(crate) fn publish_plan_action_snapshot(map: &PlanActionHandlerMap, writer: &PlanActionWriter) {
    let actions = map
        .iter()
        .flat_map(|(plugin, actions)| {
            actions.iter().map(move |(name, entry)| PlanActionInfo {
                plugin: Arc::clone(plugin),
                name: Arc::clone(name),
                label: Arc::clone(&entry.label),
                desc: Arc::clone(&entry.desc),
                order: entry.order,
            })
        })
        .collect();
    writer.publish(actions);
}

fn publish_snapshot(lua: &Lua) -> LuaResult<()> {
    let map = lua
        .app_data_ref::<PlanActionHandlerMap>()
        .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
    let writer = lua
        .app_data_ref::<PlanActionWriter>()
        .ok_or_else(|| mlua::Error::runtime("plan action writer not initialized"))?;
    publish_plan_action_snapshot(&map, &writer);
    Ok(())
}

/// Add a row to the plan-mode form menu. The form appears when the agent
/// finishes writing a plan; plugin rows sit alongside the built-in
/// "Refine plan", "Clear context and implement", and "Implement plan"
/// entries, sorted by `order`.
///
/// Same `name` registered twice by the same plugin replaces in place, so
/// a reload never stacks duplicates. Two different plugins can each
/// register the same name because rows are keyed by `(plugin, name)`.
///
/// The handler runs on the Lua thread when the user picks the row. Fire
/// the built-in outcomes with `maki.plan.implement` or
/// `maki.plan.open_editor`; returning without calling one just hides the
/// form.
///
/// @param spec table Action specification:
///   name    (string)   Required. Unique per plugin.
///   label   (string)   Required. Menu row title.
///   desc    (string)   Optional. Second row shown under the title.
///   order   (integer)  Optional. Position among rows (default 500). Built-ins are 0, 1000, 2000.
///   handler (function) Required. Called with `{ path, parallel, selected }` when the row is picked.
/// @return
/// @example
/// maki.api.register_plan_action({
///   name = "commit-and-implement",
///   label = "Commit and implement",
///   desc  = "Commit the plan file first, then implement",
///   handler = function(opts)
///     -- write the plan file to git etc.
///     maki.plan.implement({ clear_context = false })
///   end,
/// })
#[lua_fn]
fn register_plan_action(lua: &Lua, #[ctx] plugin: Arc<str>, spec: Table) -> LuaResult<()> {
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_plan_action: missing 'name'"))?;
    if name.is_empty() {
        return Err(mlua::Error::runtime(
            "register_plan_action: 'name' must be non-empty",
        ));
    }
    let label: String = spec
        .get("label")
        .map_err(|_| mlua::Error::runtime("register_plan_action: missing 'label'"))?;
    if label.is_empty() {
        return Err(mlua::Error::runtime(
            "register_plan_action: 'label' must be non-empty",
        ));
    }
    let desc: String = spec.get("desc").unwrap_or_default();
    let order: i64 = spec
        .get::<Option<i64>>("order")
        .map_err(|_| mlua::Error::runtime("register_plan_action: 'order' must be an integer"))?
        .unwrap_or(PLAN_ACTION_DEFAULT_ORDER);
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_plan_action: missing 'handler'"))?;

    let handler_key = lua.create_registry_value(handler)?;
    let name: Arc<str> = Arc::from(name.as_str());
    let label: Arc<str> = Arc::from(label.as_str());
    let desc: Arc<str> = Arc::from(desc.as_str());

    {
        let mut map = lua
            .app_data_mut::<PlanActionHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
        let by_plugin = map.entry(Arc::clone(&plugin)).or_default();
        // Silent replace within (plugin, name), matching register_reviewer /
        // register_command; reloads must not stack duplicates.
        if let Some(prev) = by_plugin.insert(
            Arc::clone(&name),
            PlanActionEntry {
                handler: handler_key,
                label,
                desc,
                order,
            },
        ) {
            let _ = lua.remove_registry_value(prev.handler);
        }
    }
    publish_snapshot(lua)
}

/// Remove one of this plugin's plan actions by name. Unknown names are a
/// no-op so a toggle can call it unconditionally.
///
/// @param name string The name the action was registered under.
/// @return
/// @example
/// maki.api.unregister_plan_action("commit-and-implement")
#[lua_fn]
fn unregister_plan_action(lua: &Lua, #[ctx] plugin: Arc<str>, name: String) -> LuaResult<()> {
    let removed = {
        let mut map = lua
            .app_data_mut::<PlanActionHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
        let Some(by_plugin) = map.get_mut(&plugin) else {
            return Ok(());
        };
        let removed = by_plugin.remove(name.as_str());
        if by_plugin.is_empty() {
            map.remove(&plugin);
        }
        removed
    };
    if let Some(entry) = removed {
        let _ = lua.remove_registry_value(entry.handler);
        publish_snapshot(lua)?;
    }
    Ok(())
}

/// Drop every plan action this plugin registered. Companion to
/// `unregister_plan_action` for disable toggles that don't want to name
/// each action.
///
/// @return
/// @example
/// maki.api.clear_plan_actions()
#[lua_fn]
fn clear_plan_actions(lua: &Lua, #[ctx] plugin: Arc<str>) -> LuaResult<()> {
    let removed = {
        let mut map = lua
            .app_data_mut::<PlanActionHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
        map.remove(&plugin).unwrap_or_default()
    };
    if removed.is_empty() {
        return Ok(());
    }
    for (_, entry) in removed {
        let _ = lua.remove_registry_value(entry.handler);
    }
    publish_snapshot(lua)
}

/// Read the current plan state without reaching into session internals.
/// Returns `{ mode, path, content, ready }`:
/// - `mode` is `"plan"` or `"build"`.
/// - `path` is the absolute plan path when in plan mode, else `nil`.
/// - `content` is the file contents when `ready` is true, else `nil`
///   (`nil` distinguishes "not ready" and "read failed" from an empty
///   plan).
/// - `ready` is `true` once the agent has written the plan file.
///
/// @return (table|nil, string|nil) Plan snapshot table, or nil and an error.
/// @example
/// local plan, err = maki.plan.read()
/// if plan and plan.ready then
///   print("plan at " .. plan.path)
///   print(plan.content)
/// end
#[lua_fn]
async fn read(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::Read,
        reply_tx: Some(reply_tx),
    })
    .await
}

/// Read or set whether the built-in plan form stays hidden when the
/// agent finishes writing a plan. A plan-viewer plugin sets this to
/// `true` so it can present its own UI on the `PlanReady` autocmd
/// without the built-in form flashing open.
///
/// Session-scoped, not persisted: a plugin re-asserts on `SessionReset`
/// if it wants durable suppression. The user's manual `Ctrl+T`
/// (`PlanToggle`) still opens the form, so the built-in stays reachable
/// as an escape hatch when the plugin's UI errors.
///
/// Pass no argument to read the current value.
///
/// @param hidden boolean? `true` to suppress, `false` to restore default, or nil to read.
/// @return (boolean|nil, string|nil) Previous value (or current when reading), or nil and an error.
/// @example
/// maki.plan.suppress_form(true)
/// local was = maki.plan.suppress_form(false)
/// local now = maki.plan.suppress_form()
#[lua_fn]
async fn suppress_form(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    hidden: Option<bool>,
) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::SuppressForm(hidden),
        reply_tx: Some(reply_tx),
    })
    .await
}

/// Fire the same "implement the plan" code path a built-in row would.
/// Call from a plan-action handler when it decides the plan is ready to
/// execute. `clear_context = true` starts a fresh session first
/// (equivalent to picking "Clear context and implement" from the
/// built-in menu).
///
/// @param opts table? Options:
///   clear_context (boolean) Default false. Start a fresh session before implementing.
/// @return (boolean|nil, string|nil) true once dispatched, or nil and an error.
/// @example
/// maki.plan.implement({ clear_context = true })
#[lua_fn]
fn implement(
    _lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<bool>> {
    let clear_context = opts
        .as_ref()
        .and_then(|t| t.get::<Option<bool>>("clear_context").ok().flatten())
        .unwrap_or(false);
    try_pair!(ui_send(
        tx.as_ref(),
        UiAction::Plan {
            req: PlanRequest::Implement { clear_context },
            reply_tx: None,
        },
    ));
    Ok((Some(true), None))
}

/// Open the current plan file in `$EDITOR`, same as the "edit plan"
/// keybinding on the built-in form.
///
/// @return (boolean|nil, string|nil) true once dispatched, or nil and an error.
/// @example
/// maki.plan.open_editor()
#[lua_fn]
fn open_editor(_lua: &Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<bool>> {
    try_pair!(ui_send(
        tx.as_ref(),
        UiAction::Plan {
            req: PlanRequest::OpenEditor,
            reply_tx: None,
        },
    ));
    Ok((Some(true), None))
}

lua_table! {
    /// Plan-mode surface: register menu actions on the plan form, own the
    /// UI by suppressing the built-in form, read the plan without touching
    /// session state, and fire the built-in implement/edit outcomes.
    ///
    /// @example
    /// -- Own the plan UI:
    /// maki.plan.suppress_form(true)
    /// maki.api.create_autocmd("PlanReady", {
    ///   callback = function(ev)
    ///     local plan = maki.plan.read()
    ///     -- render plan.content in your own window
    ///   end,
    /// })
    "maki.plan" => pub(crate) fn create_plan_table(tx: Option<flume::Sender<UiAction>>), DOCS [
        read(tx), suppress_form(tx), implement(tx), open_editor(tx),
    ]
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_plan_action_methods(plugin: Arc<str>), PLAN_ACTION_DOCS [
        register_plan_action(plugin), unregister_plan_action(plugin), clear_plan_actions(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::NO_UI_ERR;

    fn plugin() -> Arc<str> {
        Arc::from("test-plugin")
    }

    fn install_registry(lua: &Lua) -> PlanActionReader {
        lua.set_app_data(PlanActionHandlerMap::new());
        let (writer, reader) = PlanActionWriter::new();
        lua.set_app_data(writer);
        reader
    }

    fn load_actions(reader: &PlanActionReader) -> Vec<PlanActionInfo> {
        reader.load().actions.clone()
    }

    fn setup(lua: &Lua) -> PlanActionReader {
        let reader = install_registry(lua);
        let api = lua.create_table().unwrap();
        add_plan_action_methods(&api, lua, plugin()).unwrap();
        lua.globals().set("api", api).unwrap();
        reader
    }

    #[test]
    fn register_and_unregister_roundtrip() {
        let lua = Lua::new();
        let reader = setup(&lua);
        lua.load(
            r#"api.register_plan_action({
                name = "act", label = "Act", handler = function() end,
            })"#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name.as_ref(), "act");
        assert_eq!(actions[0].label.as_ref(), "Act");
        assert_eq!(actions[0].order, PLAN_ACTION_DEFAULT_ORDER);

        lua.load(r#"api.unregister_plan_action("act")"#)
            .exec()
            .unwrap();
        assert!(load_actions(&reader).is_empty());
    }

    #[test]
    fn register_replaces_same_name_within_plugin() {
        let lua = Lua::new();
        let reader = setup(&lua);
        lua.load(
            r#"
            api.register_plan_action({ name = "act", label = "first", handler = function() end })
            api.register_plan_action({ name = "act", label = "second", desc = "d", order = 42, handler = function() end })
        "#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 1, "duplicate name must replace, not stack");
        assert_eq!(actions[0].label.as_ref(), "second");
        assert_eq!(actions[0].desc.as_ref(), "d");
        assert_eq!(actions[0].order, 42);
    }

    #[test]
    fn different_plugins_can_share_a_name() {
        let lua = Lua::new();
        let reader = install_registry(&lua);
        let api_a = lua.create_table().unwrap();
        add_plan_action_methods(&api_a, &lua, Arc::from("plug-a")).unwrap();
        let api_b = lua.create_table().unwrap();
        add_plan_action_methods(&api_b, &lua, Arc::from("plug-b")).unwrap();
        lua.globals().set("a", api_a).unwrap();
        lua.globals().set("b", api_b).unwrap();

        lua.load(
            r#"
            a.register_plan_action({ name = "act", label = "A", handler = function() end })
            b.register_plan_action({ name = "act", label = "B", handler = function() end })
        "#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 2);
        let plugins: Vec<_> = actions.iter().map(|a| a.plugin.as_ref()).collect();
        assert!(plugins.contains(&"plug-a"));
        assert!(plugins.contains(&"plug-b"));
    }

    #[test]
    fn clear_plan_actions_drops_all_for_plugin_only() {
        let lua = Lua::new();
        let reader = install_registry(&lua);
        let api_a = lua.create_table().unwrap();
        add_plan_action_methods(&api_a, &lua, Arc::from("plug-a")).unwrap();
        let api_b = lua.create_table().unwrap();
        add_plan_action_methods(&api_b, &lua, Arc::from("plug-b")).unwrap();
        lua.globals().set("a", api_a).unwrap();
        lua.globals().set("b", api_b).unwrap();

        lua.load(
            r#"
            a.register_plan_action({ name = "x", label = "X", handler = function() end })
            a.register_plan_action({ name = "y", label = "Y", handler = function() end })
            b.register_plan_action({ name = "z", label = "Z", handler = function() end })
            a.clear_plan_actions()
        "#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].plugin.as_ref(), "plug-b");
        assert_eq!(actions[0].name.as_ref(), "z");
    }

    #[test]
    fn register_rejects_missing_name_label_handler() {
        let lua = Lua::new();
        setup(&lua);
        assert!(
            lua.load(r#"api.register_plan_action({ label = "L", handler = function() end })"#)
                .exec()
                .is_err()
        );
        assert!(
            lua.load(r#"api.register_plan_action({ name = "n", handler = function() end })"#)
                .exec()
                .is_err()
        );
        assert!(
            lua.load(r#"api.register_plan_action({ name = "n", label = "L" })"#)
                .exec()
                .is_err()
        );
        assert!(
            lua.load(
                r#"api.register_plan_action({ name = "", label = "L", handler = function() end })"#
            )
            .exec()
            .is_err()
        );
        assert!(
            lua.load(
                r#"api.register_plan_action({ name = "n", label = "", handler = function() end })"#
            )
            .exec()
            .is_err()
        );
    }

    #[test]
    fn plan_table_read_without_ui_returns_error_pair() {
        let lua = Lua::new();
        let table = create_plan_table(&lua, None).unwrap();
        lua.globals().set("plan", table).unwrap();
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load("return plan.read()").eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test]
    fn implement_forwards_clear_context_flag() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let (val, err): (bool, Option<String>) = lua
            .load("return plan.implement({ clear_context = true })")
            .eval()
            .unwrap();
        assert!(val);
        assert_eq!(err, None);
        match rx.try_recv().expect("implement request dispatched") {
            UiAction::Plan {
                req: PlanRequest::Implement { clear_context },
                reply_tx: None,
            } => {
                assert!(clear_context);
            }
            _ => panic!("expected Plan/Implement UiAction"),
        }
    }

    #[test]
    fn open_editor_dispatches_without_reply_channel() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let (val, err): (bool, Option<String>) =
            lua.load("return plan.open_editor()").eval().unwrap();
        assert!(val);
        assert_eq!(err, None);
        match rx.try_recv().expect("open_editor request dispatched") {
            UiAction::Plan {
                req: PlanRequest::OpenEditor,
                reply_tx: None,
            } => {}
            _ => panic!("expected Plan/OpenEditor UiAction"),
        }
    }

    #[test]
    fn suppress_form_roundtrips_through_ui() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        std::thread::spawn(move || {
            let Ok(UiAction::Plan {
                req: PlanRequest::SuppressForm(hidden),
                reply_tx: Some(reply_tx),
            }) = rx.recv()
            else {
                panic!("expected suppress_form Plan action");
            };
            assert_eq!(hidden, Some(true));
            reply_tx.send(Ok(serde_json::json!(false))).unwrap();
        });
        let (val, err): (bool, Option<String>) =
            smol::block_on(lua.load("return plan.suppress_form(true)").eval_async()).unwrap();
        assert_eq!(err, None);
        assert!(!val, "suppress_form must return previous value (false)");
    }
}
