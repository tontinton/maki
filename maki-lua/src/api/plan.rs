//! `maki.plan`: the plan-mode surface. Plugins read the current plan state,
//! layer `ui.plan_form.actions` to shape the form's menu, and layer
//! `ui.plan_form` to take the form over.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::util::command::{
    PlanFormRow, PlanRequest, PlanRowAction, UiAction, ui_json_roundtrip,
};
use crate::api::util::convert::TableExt;
use crate::api::util::pair::Pair;

const ID_KEY: &str = "id";
const LABEL_KEY: &str = "label";
const DESC_KEY: &str = "desc";
const ACTION_KEY: &str = "action";
const HANDLER_KEY: &str = "handler";

/// Rows past this are dropped, so a runaway layer costs its own overflow and
/// not the built-in rows the user knows.
const MAX_ROWS: usize = 32;
/// The form draws one line per row, so a runaway label cannot push the rows
/// below it, and the hint bar, off the bottom of the box.
const MAX_LABEL_CHARS: usize = 64;
const MAX_DESC_CHARS: usize = 96;

/// Which plugin's handler a row carries, collected hop by hop as the
/// `ui.plan_form.actions` chain answers.
///
/// Keyed by the handler, not by row id, so a layer stays free to relabel and
/// reorder rows. A handler is attributed the first time the chain sees it,
/// and after that only the plugin that introduced it may file it under
/// further ids, or an unload would reap a key under the wrong plugin.
#[derive(Default)]
pub(crate) struct RowOwners(HashMap<usize, RowOwner>);

struct RowOwner {
    plugin: Arc<str>,
    /// The ids this handler has been filed under, by the plugin that owns it.
    ids: HashSet<String>,
    /// Held so the collector cannot recycle the address the map keys by while
    /// the chain is still running.
    _handler: Function,
}

impl RowOwners {
    /// One hop of the chain: {plugin} answered with {table}, so every handler
    /// in it the chain has not seen before is {plugin}'s from here on.
    pub fn observe(&mut self, plugin: &Arc<str>, table: &Table) {
        for entry in table.sequence_values::<Table>().flatten() {
            let (Ok(id), Ok(Some(handler))) = (
                entry.get::<String>(ID_KEY),
                entry.opt::<Function>(HANDLER_KEY),
            ) else {
                continue;
            };
            match self.0.entry(handler.to_pointer() as usize) {
                Entry::Occupied(mut seen) => {
                    let owner = seen.get_mut();
                    if owner.plugin == *plugin {
                        owner.ids.insert(id);
                    } else if !owner.ids.contains(&id) {
                        tracing::warn!(
                            plugin = %plugin,
                            owner = %owner.plugin,
                            row = %id,
                            "plan form layer filed another plugin's row handler under a new id"
                        );
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(RowOwner {
                        plugin: Arc::clone(plugin),
                        ids: HashSet::from([id]),
                        _handler: handler,
                    });
                }
            }
        }
    }

    fn owner(&self, id: &str, handler: &Function) -> Option<&Arc<str>> {
        let owner = self.0.get(&(handler.to_pointer() as usize))?;
        owner.ids.contains(id).then_some(&owner.plugin)
    }

    /// The plugin a handler belongs to whatever id it ended up under, for the
    /// log line about a row the host is dropping.
    fn introduced_by(&self, handler: &Function) -> Option<&Arc<str>> {
        Some(&self.0.get(&(handler.to_pointer() as usize))?.plugin)
    }
}

/// A row's handler and the plugin it answers for, kept together so an unload
/// can reap the key.
pub(crate) struct RowHandler {
    pub plugin: Arc<str>,
    pub key: RegistryKey,
}

/// The menu one session is drawing, and the generation the form echoes back
/// on a pick.
struct SessionMenu {
    generation: u64,
    rows: Vec<Option<RowHandler>>,
}

/// The handlers behind the plugin rows of the plan form each session has
/// open. A session draws one form at a time, so a new menu replaces the last
/// one's keys.
#[derive(Default)]
pub(crate) struct PlanRowHandlers {
    menus: HashMap<String, SessionMenu>,
    /// Never reused, so a stale pick can only ever miss.
    next_generation: u64,
}

impl PlanRowHandlers {
    /// The handler for a pick, or why there is none. A {generation} mismatch
    /// means the form the user picked from is not the menu these handlers
    /// belong to.
    pub fn handler(
        &self,
        session: &str,
        generation: u64,
        row: usize,
    ) -> Result<&RowHandler, &'static str> {
        let menu = self.menus.get(session).ok_or(NO_MENU_ERR)?;
        if menu.generation != generation {
            return Err(STALE_MENU_ERR);
        }
        menu.rows
            .get(row)
            .and_then(Option::as_ref)
            .ok_or(NO_HANDLER_ERR)
    }
}

pub(crate) const NO_MENU_ERR: &str = "no plan form menu for this session";
pub(crate) const STALE_MENU_ERR: &str = "plan form menu is no longer the one on screen";
pub(crate) const NO_HANDLER_ERR: &str = "plan form row has no handler";

fn handlers_mut(lua: &Lua) -> LuaResult<mlua::AppDataRefMut<'_, PlanRowHandlers>> {
    lua.app_data_mut::<PlanRowHandlers>()
        .ok_or_else(|| mlua::Error::runtime("plan row handlers not initialized"))
}

/// Publish the menu a session is about to draw, dropping the handlers of the
/// menu it replaces so a long-lived session does not accumulate registry
/// keys. The generation it answers with is what a pick has to echo back.
pub(crate) fn install_row_handlers(
    lua: &Lua,
    session: String,
    rows: Vec<Option<RowHandler>>,
) -> LuaResult<u64> {
    let mut store = handlers_mut(lua)?;
    store.next_generation += 1;
    let generation = store.next_generation;
    let replaced = store
        .menus
        .insert(session, SessionMenu { generation, rows });
    drop(store);
    drop_keys(lua, replaced.into_iter().flat_map(|m| m.rows));
    Ok(generation)
}

/// Reap the handlers a plugin owns. The rows stay on screen and go on naming
/// outcomes the host can still run, and only the revoked code goes away.
pub(crate) fn clear_plugin_rows(lua: &Lua, plugin: &str) {
    let Some(mut store) = lua.app_data_mut::<PlanRowHandlers>() else {
        return;
    };
    let mut dropped = Vec::new();
    for menu in store.menus.values_mut() {
        for slot in &mut menu.rows {
            if slot.as_ref().is_some_and(|h| h.plugin.as_ref() == plugin) {
                dropped.extend(slot.take());
            }
        }
    }
    drop(store);
    drop_keys(lua, dropped.into_iter().map(Some));
}

/// Reap a menu that was published for nobody, leaving a newer one for the
/// same session alone: a draft that landed while this one was still being
/// answered has already replaced it, and that one is on screen.
///
/// A {generation} of zero is a menu no handler was stashed under.
pub(crate) fn clear_menu_generation(lua: &Lua, session: &str, generation: u64) {
    let Some(mut store) = lua.app_data_mut::<PlanRowHandlers>() else {
        return;
    };
    let stale = store
        .menus
        .get(session)
        .is_some_and(|menu| menu.generation == generation);
    let removed = stale.then(|| store.menus.remove(session)).flatten();
    drop(store);
    drop_keys(lua, removed.into_iter().flat_map(|m| m.rows));
}

/// Reap a session's menu, or its handlers outlive every tab that could run
/// them.
pub(crate) fn clear_session_rows(lua: &Lua, session: &str) {
    let Some(mut store) = lua.app_data_mut::<PlanRowHandlers>() else {
        return;
    };
    let removed = store.menus.remove(session);
    drop(store);
    drop_keys(lua, removed.into_iter().flat_map(|m| m.rows));
}

fn drop_keys(lua: &Lua, handlers: impl Iterator<Item = Option<RowHandler>>) {
    for handler in handlers.flatten() {
        if let Err(e) = lua.remove_registry_value(handler.key) {
            tracing::warn!(plugin = %handler.plugin, error = %e, "failed to drop plan row handler key");
        }
    }
}

/// The rows the host proposes, as the table the bottom of the
/// `ui.plan_form.actions` chain answers with.
pub(crate) fn rows_to_table(lua: &Lua, rows: &[PlanFormRow]) -> LuaResult<Table> {
    let out = lua.create_table()?;
    for row in rows {
        let t = lua.create_table()?;
        t.set(ID_KEY, row.id.as_str())?;
        t.set(LABEL_KEY, row.label.as_str())?;
        t.set(DESC_KEY, row.desc.as_str())?;
        t.set(ACTION_KEY, row.action.map(PlanRowAction::tag))?;
        out.push(t)?;
    }
    Ok(out)
}

/// Why the host will not draw a row, one rule per constant, so the log line
/// about a dropped row can name the rule.
const NO_ID_ERR: &str = "row needs a non-empty 'id'";
const DUPLICATE_ID_ERR: &str = "row id is used twice";
const NO_LABEL_ERR: &str = "row needs a non-empty 'label'";
const UNKNOWN_ACTION_ERR: &str = "row names an action the host does not have";
const UNATTRIBUTED_HANDLER_ERR: &str =
    "row carries a handler the host cannot attribute to the layer that filed it";
const NOTHING_TO_RUN_ERR: &str = "row needs a 'handler', an 'action', or both";

/// One line of plugin text, cut to {max} chars with control characters folded
/// to spaces, so a newline cannot add a line the form never measured.
fn one_line(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect();
    if text.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

/// A row's handler as the chain answered with it, before a registry key is
/// taken: the plugin the host attributed it to, and the function itself.
type OwnedHandler = (Arc<str>, Function);

/// One row of the answered menu, or the rule it broke.
fn row_from_table(
    entry: &Table,
    owners: &RowOwners,
    seen: &mut HashSet<String>,
) -> Result<(PlanFormRow, Option<OwnedHandler>), &'static str> {
    let id = entry
        .get::<String>(ID_KEY)
        .ok()
        .filter(|id| !id.is_empty())
        .ok_or(NO_ID_ERR)?;
    if !seen.insert(id.clone()) {
        return Err(DUPLICATE_ID_ERR);
    }
    let label = entry
        .get::<String>(LABEL_KEY)
        .ok()
        .filter(|label| !label.is_empty())
        .ok_or(NO_LABEL_ERR)?;
    let action = match entry.opt::<String>(ACTION_KEY).unwrap_or_default() {
        Some(tag) => Some(PlanRowAction::from_tag(&tag).ok_or(UNKNOWN_ACTION_ERR)?),
        None => None,
    };
    let handler = match entry.opt::<Function>(HANDLER_KEY).unwrap_or_default() {
        Some(handler) => {
            let plugin = owners
                .owner(&id, &handler)
                .cloned()
                .ok_or(UNATTRIBUTED_HANDLER_ERR)?;
            Some((plugin, handler))
        }
        None => None,
    };
    if action.is_none() && handler.is_none() {
        return Err(NOTHING_TO_RUN_ERR);
    }
    Ok((
        PlanFormRow {
            id,
            label: one_line(&label, MAX_LABEL_CHARS),
            desc: one_line(
                &entry.get::<String>(DESC_KEY).unwrap_or_default(),
                MAX_DESC_CHARS,
            ),
            action,
            plugin: handler.as_ref().map(|(plugin, _)| Arc::clone(plugin)),
        },
        handler,
    ))
}

/// The menu the chain answered with. A row's `handler` runs first and its
/// `action` after, so copying a built-in row and adding a handler extends the
/// built-in outcome. A handler returning `false` drops the action, and a row
/// with neither is one nobody can run.
///
/// A row the host cannot run costs that row and nothing else. The menu is the
/// work of every layer in the chain, so dropping the whole menu over one bad
/// row would hand any plugin a way to suppress every other plugin's.
pub(crate) fn rows_from_table(
    lua: &Lua,
    table: Table,
    owners: &RowOwners,
) -> LuaResult<(Vec<PlanFormRow>, Vec<Option<RowHandler>>)> {
    let mut rows = Vec::new();
    let mut funcs = Vec::new();
    let mut seen = HashSet::new();
    for entry in table.sequence_values::<Table>() {
        if rows.len() == MAX_ROWS {
            tracing::warn!(max = MAX_ROWS, "plan form menu truncated to the row cap");
            break;
        }
        let Ok(entry) = entry else {
            tracing::warn!("plan form menu holds something that is not a row, dropping it");
            continue;
        };
        match row_from_table(&entry, owners, &mut seen) {
            Ok((row, func)) => {
                rows.push(row);
                funcs.push(func);
            }
            Err(reason) => {
                let blamed = entry
                    .opt::<Function>(HANDLER_KEY)
                    .ok()
                    .flatten()
                    .and_then(|h| owners.introduced_by(&h).cloned());
                tracing::warn!(
                    row = entry.get::<String>(ID_KEY).unwrap_or_default(),
                    plugin = blamed.as_deref().unwrap_or("<host>"),
                    reason,
                    "dropping a plan form row the host cannot run"
                );
            }
        }
    }
    let mut handlers = Vec::with_capacity(funcs.len());
    for func in funcs {
        handlers.push(match func {
            Some((plugin, handler)) => Some(RowHandler {
                plugin,
                key: lua.create_registry_value(handler)?,
            }),
            None => None,
        });
    }
    Ok((rows, handlers))
}

/// The table a plugin row's handler is called with when the user picks it.
pub(crate) fn row_handler_opts(
    lua: &Lua,
    session: &str,
    path: &str,
    parallel: bool,
) -> LuaResult<Table> {
    let opts = lua.create_table()?;
    opts.set("session", session)?;
    opts.set("path", path)?;
    opts.set("parallel", parallel)?;
    Ok(opts)
}

/// Read the current plan state. Returns `{ mode, path, content, ready }`:
/// - `mode` is `"plan"` or `"build"`.
/// - `path` is the absolute plan path once the session has one, else `nil`.
/// - `ready` is `true` once the agent has written the plan file.
/// - `content` is the file contents, `nil` when the plan is not ready or the
///   read failed.
///
/// @param opts table? `session` (string?) Session id, defaults to focused.
/// @return (table|nil, string|nil) Plan snapshot table, or nil and an error.
/// @example
/// local plan, err = maki.plan.read({ session = id })
/// if plan and plan.ready then
///   print(plan.path, plan.content)
/// end
#[lua_fn]
async fn read(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let session = match opts {
        Some(opts) => opts.opt("session")?,
        None => None,
    };
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::Read { session },
        reply_tx,
    })
    .await
}

lua_table! {
    /// Plan-mode surface for plugins.
    ///
    /// Read the plan, and shape the plan form by layering the two slots maki
    /// fires around it. Plan state is per session, so every call takes an
    /// optional `session` and defaults to the focused tab.
    ///
    /// `ui.plan_form.actions` is the menu. The default answers with the
    /// built-in rows, each `{ id, label, desc, action }`, and a layer
    /// appends, reorders or drops them before returning the list. Every row
    /// needs an `id` no other row uses, since that is how a later layer finds
    /// it. Anything past the 32nd row is dropped.
    ///
    /// A row carrying a `handler` has that function called on the Lua thread
    /// with `{ session, path, parallel }` when the user picks it. The row's
    /// `action` runs after the handler returns, unless the handler returned
    /// `false` or failed, so copying a built-in row and adding a handler
    /// keeps the built-in outcome. Drop the `action` to replace it.
    ///
    /// `ui.plan_form` is the form itself. A layer that answers `false` keeps
    /// it closed and renders the plan however it likes.
    ///
    /// Layering either slot costs every permission, the price of steering a
    /// call whose reach nobody declared: a row decides what pressing Enter
    /// does, up to a build-mode turn with every tool behind it.
    ///
    /// Unloading your plugin hands the form back and reaps its row handlers.
    ///
    /// ```lua
    /// -- A row of your own, next to the built-in ones:
    /// maki.api.set_slot("ui.plan_form.actions", function(prev, ev)
    ///   local rows = prev(ev)
    ///   table.insert(rows, {
    ///     id = "commit_and_implement",
    ///     label = "Commit and implement",
    ///     desc = "Commit the plan file first, then implement it",
    ///     handler = function(opts)
    ///       maki.fn.system({ "git", "commit", "-am", "plan" })
    ///       maki.session.set_mode("build", { session = opts.session })
    ///       maki.session.prompt("Implement " .. opts.path, { session = opts.session })
    ///     end,
    ///   })
    ///   return rows
    /// end)
    ///
    /// -- Render the plan yourself for as long as this plugin is loaded:
    /// maki.api.set_slot("ui.plan_form", function(prev, ev)
    ///   local plan = maki.plan.read({ session = ev.session })
    ///   return false
    /// end)
    /// ```
    "maki.plan" => pub(crate) fn create_plan_table(tx: Option<flume::Sender<UiAction>>), DOCS [
        read(tx),
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::api::util::command::NO_UI_ERR;

    const BUILTIN_ID: &str = "implement";
    const BUILTIN_LABEL: &str = "Implement plan";
    const PLUGIN_ID: &str = "commit_and_implement";
    const PLUGIN_LABEL: &str = "Commit and implement";
    const PLUGIN: &str = "planner";
    const OTHER_PLUGIN: &str = "other";
    const SESSION: &str = "s1";
    /// A row nothing is wrong with, next to the one the case breaks.
    const KEEPER_ID: &str = "keeper";

    fn builtin_rows() -> Vec<PlanFormRow> {
        vec![PlanFormRow {
            id: BUILTIN_ID.to_owned(),
            label: BUILTIN_LABEL.to_owned(),
            desc: "Keep current context".to_owned(),
            action: Some(PlanRowAction::Implement),
            plugin: None,
        }]
    }

    /// Stands in for the chain: the host proposes, {script} answers, and the
    /// answer is attributed the way a layer's hop would be.
    fn roundtrip(lua: &Lua, script: &str) -> (Vec<PlanFormRow>, Vec<Option<RowHandler>>) {
        let proposed = rows_to_table(lua, &builtin_rows()).unwrap();
        lua.globals().set("rows", proposed).unwrap();
        let answered: Table = lua.load(script).eval().unwrap();
        let mut owners = RowOwners::default();
        owners.observe(&Arc::from(PLUGIN), &answered);
        rows_from_table(lua, answered, &owners).unwrap()
    }

    fn plugin_row(id: &str) -> String {
        format!(r#"{{ id = "{id}", label = "{PLUGIN_LABEL}", handler = function() end }}"#)
    }

    #[test]
    fn the_proposed_rows_survive_a_layer_that_just_hands_them_back() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(&lua, "return rows");
        assert_eq!(rows, builtin_rows());
        assert!(handlers.iter().all(Option::is_none));
    }

    #[test]
    fn a_row_with_a_handler_belongs_to_the_layer_that_added_it() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(
            &lua,
            &format!("table.insert(rows, {}) return rows", plugin_row(PLUGIN_ID)),
        );
        assert_eq!(rows[1].label, PLUGIN_LABEL);
        assert_eq!(rows[1].plugin.as_deref(), Some(PLUGIN));
        assert_eq!(rows[1].action, None);
        assert!(handlers[0].is_none());
        assert_eq!(handlers[1].as_ref().unwrap().plugin.as_ref(), PLUGIN);
    }

    /// Handler-then-action: a copied built-in row keeps its outcome.
    #[test]
    fn a_builtin_row_that_gains_a_handler_keeps_its_action() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(&lua, "rows[1].handler = function() end return rows");
        assert_eq!(rows[0].action, Some(PlanRowAction::Implement));
        assert_eq!(rows[0].plugin.as_deref(), Some(PLUGIN));
        assert!(handlers[0].is_some());
    }

    /// The last layer to set a row's handler owns it, or an unload would reap
    /// a key another loaded plugin is still behind.
    #[test]
    fn the_layer_that_replaces_a_handler_takes_it_over() {
        let lua = Lua::new();
        let proposed = rows_to_table(&lua, &builtin_rows()).unwrap();
        lua.globals().set("rows", proposed).unwrap();
        let mut owners = RowOwners::default();

        let first: Table = lua
            .load(format!(
                "table.insert(rows, {}) return rows",
                plugin_row(PLUGIN_ID)
            ))
            .eval()
            .unwrap();
        owners.observe(&Arc::from(PLUGIN), &first);
        let second: Table = lua
            .load("rows[2].handler = function() end return rows")
            .eval()
            .unwrap();
        owners.observe(&Arc::from(OTHER_PLUGIN), &second);

        let (rows, _) = rows_from_table(&lua, second, &owners).unwrap();
        assert_eq!(rows[1].plugin.as_deref(), Some(OTHER_PLUGIN));
    }

    /// A handler nobody in the chain answered for would run with permissions
    /// the host cannot name.
    #[test]
    fn an_unattributed_handler_is_rejected() {
        let lua = Lua::new();
        let table: Table = lua
            .load(format!("return {{ {} }}", plugin_row(PLUGIN_ID)))
            .eval()
            .unwrap();
        let (rows, _) = rows_from_table(&lua, table, &RowOwners::default()).unwrap();
        assert!(rows.is_empty());
    }

    /// Why attribution is keyed by handler: a layer that takes another
    /// plugin's handler out of `prev(ev)` and files it under a fresh id would
    /// otherwise own it, and the unload meant to reap it would walk past.
    #[test]
    fn a_handler_refiled_under_a_new_id_by_another_layer_is_refused() {
        let lua = Lua::new();
        let proposed = rows_to_table(&lua, &builtin_rows()).unwrap();
        lua.globals().set("rows", proposed).unwrap();
        let mut owners = RowOwners::default();

        let first: Table = lua
            .load(format!(
                "table.insert(rows, {}) return rows",
                plugin_row(PLUGIN_ID)
            ))
            .eval()
            .unwrap();
        owners.observe(&Arc::from(PLUGIN), &first);
        let second: Table = lua
            .load(r#"rows[2].id = "relabelled" return rows"#)
            .eval()
            .unwrap();
        owners.observe(&Arc::from(OTHER_PLUGIN), &second);

        let (rows, handlers) = rows_from_table(&lua, second, &owners).unwrap();
        assert_eq!(rows.len(), 1, "only the row the host proposed survives");
        assert_eq!(rows[0].id, BUILTIN_ID);
        assert!(handlers.iter().all(Option::is_none));
    }

    /// A plugin may file its own handler under a second id, since the unload
    /// that reaps one reaps both.
    #[test]
    fn a_layer_can_reuse_its_own_handler_on_a_second_row() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(
            &lua,
            r#"local h = function() end
               table.insert(rows, { id = "a", label = "a", handler = h })
               table.insert(rows, { id = "b", label = "b", handler = h })
               return rows"#,
        );
        assert_eq!(rows.len(), 3);
        assert!(handlers[1].is_some() && handlers[2].is_some());
    }

    #[test]
    fn a_layer_can_drop_a_builtin_row() {
        let lua = Lua::new();
        let (rows, _) = roundtrip(&lua, "table.remove(rows, 1) return rows");
        assert!(rows.is_empty());
    }

    /// A runaway layer keeps the rows that fit and loses the rest.
    #[test]
    fn a_menu_past_the_cap_is_truncated() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(
            &lua,
            &format!(
                r#"for i = 1, {} do
                       table.insert(rows, {{ id = "r" .. i, label = "row", handler = function() end }})
                   end
                   return rows"#,
                MAX_ROWS * 4
            ),
        );
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(handlers.len(), MAX_ROWS);
    }

    /// A row the host cannot run costs that row and nothing else, or one bad
    /// layer would be a way to suppress every other plugin's rows.
    #[test_case(r#"{ id = "x", label = "x", action = "nope" }"#, 1 ; "unknown_action")]
    #[test_case(r#"{ id = "x", action = "implement" }"#, 1 ; "missing_label")]
    #[test_case(r#"{ id = "x", label = "", action = "implement" }"#, 1 ; "empty_label")]
    #[test_case(r#"{ label = "x", action = "implement" }"#, 1 ; "missing_id")]
    #[test_case(r#"{ id = "", label = "x", action = "implement" }"#, 1 ; "empty_id")]
    #[test_case(r#"{ id = "x", label = "x" }"#, 1 ; "no_handler_and_no_action")]
    #[test_case(r#"42"#, 1 ; "not_even_a_row")]
    #[test_case(r#"{ id = "x", label = "a", action = "implement" }, { id = "x", label = "b", action = "refine" }"#, 2 ; "duplicate_ids")]
    fn a_row_the_host_cannot_run_is_dropped_on_its_own(row: &str, kept: usize) {
        let lua = Lua::new();
        let table: Table = lua
            .load(format!(
                r#"return {{ {row}, {{ id = "{KEEPER_ID}", label = "keep", action = "implement" }} }}"#
            ))
            .eval()
            .unwrap();
        let (rows, handlers) = rows_from_table(&lua, table, &RowOwners::default()).unwrap();
        assert_eq!(rows.len(), kept);
        assert_eq!(handlers.len(), kept);
        assert_eq!(
            rows.last().map(|r| r.id.as_str()),
            Some(KEEPER_ID),
            "a bad row must not cost the rows around it"
        );
    }

    /// The form draws one line per row, so a runaway string cannot push the
    /// rows under it and the hint bar out of the box.
    #[test]
    fn a_runaway_label_is_cut_to_one_line() {
        let lua = Lua::new();
        let (rows, _) = roundtrip(
            &lua,
            r#"rows[1].label = string.rep("l", 4000)
               rows[1].desc = "one\ntwo"
               return rows"#,
        );
        assert!(rows[0].label.chars().count() <= MAX_LABEL_CHARS + 1);
        assert!(!rows[0].desc.contains('\n'));
    }

    fn install(lua: &Lua, plugin: &str) -> u64 {
        let func: Function = lua.load("return function() end").eval().unwrap();
        let handler = lua.create_registry_value(func).unwrap();
        install_row_handlers(
            lua,
            SESSION.to_owned(),
            vec![Some(RowHandler {
                plugin: Arc::from(plugin),
                key: handler,
            })],
        )
        .unwrap()
    }

    /// The revoked plugin's code stops being reachable the moment it is
    /// unloaded.
    #[test]
    fn unloading_a_plugin_reaps_its_row_handlers() {
        let lua = Lua::new();
        lua.set_app_data(PlanRowHandlers::default());
        let generation = install(&lua, PLUGIN);
        let store = lua.app_data_ref::<PlanRowHandlers>().unwrap();
        assert!(store.handler(SESSION, generation, 0).is_ok());
        drop(store);

        clear_plugin_rows(&lua, OTHER_PLUGIN);
        assert!(
            lua.app_data_ref::<PlanRowHandlers>()
                .unwrap()
                .handler(SESSION, generation, 0)
                .is_ok(),
            "another plugin's unload must not touch this handler"
        );

        clear_plugin_rows(&lua, PLUGIN);
        assert_eq!(
            lua.app_data_ref::<PlanRowHandlers>()
                .unwrap()
                .handler(SESSION, generation, 0)
                .err(),
            Some(NO_HANDLER_ERR)
        );
    }

    /// A closed tab's handlers go with it, and are not held for the life of
    /// the process.
    #[test]
    fn closing_a_session_reaps_its_menu() {
        let lua = Lua::new();
        lua.set_app_data(PlanRowHandlers::default());
        let generation = install(&lua, PLUGIN);

        clear_session_rows(&lua, SESSION);
        assert_eq!(
            lua.app_data_ref::<PlanRowHandlers>()
                .unwrap()
                .handler(SESSION, generation, 0)
                .err(),
            Some(NO_MENU_ERR)
        );
    }

    /// A chain that answered after the form stopped waiting published a menu
    /// nobody can pick from. It is reaped rather than left for the life of
    /// the session.
    #[test]
    fn a_menu_nobody_is_drawing_is_reaped() {
        let lua = Lua::new();
        lua.set_app_data(PlanRowHandlers::default());
        let orphan = install(&lua, PLUGIN);

        clear_menu_generation(&lua, SESSION, orphan);
        assert_eq!(
            lua.app_data_ref::<PlanRowHandlers>()
                .unwrap()
                .handler(SESSION, orphan, 0)
                .err(),
            Some(NO_MENU_ERR)
        );
    }

    /// Reaping an orphan must not take the menu that replaced it: a draft
    /// that landed while the first was still being answered is the one on
    /// screen.
    #[test]
    fn reaping_an_orphan_spares_the_menu_that_replaced_it() {
        let lua = Lua::new();
        lua.set_app_data(PlanRowHandlers::default());
        let orphan = install(&lua, PLUGIN);
        let on_screen = install(&lua, PLUGIN);

        clear_menu_generation(&lua, SESSION, orphan);
        assert!(
            lua.app_data_ref::<PlanRowHandlers>()
                .unwrap()
                .handler(SESSION, on_screen, 0)
                .is_ok()
        );
    }

    /// A chain that answers late installs its handlers over a menu already on
    /// screen, and the generation keeps a pick from reaching them.
    #[test]
    fn a_pick_from_a_replaced_menu_is_rejected() {
        let lua = Lua::new();
        lua.set_app_data(PlanRowHandlers::default());
        let drawn = install(&lua, PLUGIN);
        let installed_later = install(&lua, PLUGIN);
        assert_ne!(drawn, installed_later);

        let store = lua.app_data_ref::<PlanRowHandlers>().unwrap();
        assert_eq!(store.handler(SESSION, drawn, 0).err(), Some(STALE_MENU_ERR));
        assert!(store.handler(SESSION, installed_later, 0).is_ok());
    }

    #[test]
    fn plan_read_without_ui_returns_error_pair() {
        let lua = Lua::new();
        let table = create_plan_table(&lua, None).unwrap();
        lua.globals().set("plan", table).unwrap();
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load("return plan.read()").eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// Plan state is per session, so the call has to be able to name one.
    #[test_case(r#"plan.read({ session = "s1" })"#, Some("s1") ; "explicit_session")]
    #[test_case("plan.read()", None ; "focused_session")]
    fn the_session_option_travels_to_the_ui(call: &str, expected: Option<&str>) {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let served = std::thread::spawn(move || {
            let Ok(UiAction::Plan { req, reply_tx }) = rx.recv() else {
                panic!("expected a Plan UiAction");
            };
            reply_tx.send(Ok(serde_json::json!(true))).unwrap();
            req
        });
        smol::block_on(lua.load(format!("return {call}")).eval_async::<Value>()).unwrap();
        assert_eq!(served.join().unwrap().session(), expected);
    }
}
