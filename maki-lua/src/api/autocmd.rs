use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table, Value};

use crate::api::util::dispatch::{DepthGuard, Reentry};
use crate::runtime::{run_detached, strip_traceback};

static NEXT_AUTOCMD_ID: AtomicU64 = AtomicU64::new(1);

const WILDCARD_PATTERN: &str = "*";

pub(crate) struct AutocmdEntry {
    pub id: u64,
    pub callback: Function,
    pub plugin: Arc<str>,
    pub once: bool,
    pub patterns: Option<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct AutocmdStore {
    pub(crate) listeners: HashMap<String, Vec<AutocmdEntry>>,
}

impl AutocmdStore {
    pub fn register(&mut self, event: String, entry: AutocmdEntry) {
        self.listeners.entry(event).or_default().push(entry);
    }

    pub fn remove(&mut self, id: u64) {
        for entries in self.listeners.values_mut() {
            entries.retain(|e| e.id != id);
        }
        self.listeners.retain(|_, v| !v.is_empty());
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        for entries in self.listeners.values_mut() {
            entries.retain(|e| e.plugin.as_ref() != plugin);
        }
        self.listeners.retain(|_, v| !v.is_empty());
    }
}

fn pattern_matches(patterns: Option<&[String]>, fired: Option<&str>) -> bool {
    match patterns {
        None => true,
        Some(ps) => {
            ps.iter().any(|p| p == WILDCARD_PATTERN)
                || fired.is_some_and(|f| ps.iter().any(|p| p == f))
        }
    }
}

/// One dispatch path for host-fired and plugin-fired events. Never throws.
/// Each callback runs in its own coroutine under its own detached task
/// scope, so it may suspend (the `maki.fs.*` helpers park on
/// `smol::unblock`); an inline resume would die with "attempt to yield
/// across metamethod / C-call boundary". The per-callback scope also means
/// task-owned jobs a handler starts die with that handler instead of
/// outliving it to the end of the batch.
///
/// The snapshot below looks racy but is not: all Lua runs on the runtime
/// thread and plugin unloads arrive through the request channel, so nothing
/// can touch the store mid-dispatch.
///
/// `data` is shared across callbacks (nvim does the same), but each callback
/// gets its own `ev` table, so one plugin's mutation cannot leak into the
/// next.
pub(crate) async fn dispatch(lua: Lua, event: String, pattern: Option<String>, data: Value) {
    let Ok(_guard) = DepthGuard::enter(&lua, "autocmd", &event, Reentry::Vm) else {
        tracing::warn!(event, "autocmd dispatch exceeded max depth, skipping");
        return;
    };
    let snapshot: Vec<(u64, Arc<str>, Function)> = {
        let Some(mut store) = lua.app_data_mut::<AutocmdStore>() else {
            return;
        };
        let Some(entries) = store.listeners.get_mut(&event) else {
            return;
        };
        let mut snapshot = Vec::new();
        // Drop `once` entries now, at snapshot time: if a callback refires
        // the same event they are already gone, so they stay exactly-once.
        entries.retain(|e| {
            let fires = pattern_matches(e.patterns.as_deref(), pattern.as_deref());
            if fires {
                snapshot.push((e.id, Arc::clone(&e.plugin), e.callback.clone()));
            }
            !(fires && e.once)
        });
        snapshot
    };
    for (id, plugin, callback) in snapshot {
        let ev = match make_ev_table(&lua, id, &event, pattern.as_deref(), &data) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(event, error = %e, "failed to build autocmd ev table");
                return;
            }
        };
        if let Err(e) = run_detached(&lua, async {
            let thread = lua.create_thread(callback)?;
            thread.into_async::<()>(ev)?.await
        })
        .await
        {
            tracing::warn!(
                event,
                plugin = &*plugin,
                error = %strip_traceback(&e),
                "plugin callback failed"
            );
        }
    }
}

fn make_ev_table(
    lua: &Lua,
    id: u64,
    event: &str,
    pattern: Option<&str>,
    data: &Value,
) -> LuaResult<Table> {
    let ev = lua.create_table()?;
    ev.set("id", id)?;
    ev.set("event", event)?;
    ev.set("match", pattern)?;
    ev.set("data", data.clone())?;
    Ok(ev)
}

fn parse_string_or_seq(value: Value, what: &str) -> LuaResult<Vec<String>> {
    match value {
        Value::String(s) => Ok(vec![s.to_str()?.to_owned()]),
        Value::Table(t) => t.sequence_values::<String>().collect(),
        _ => Err(mlua::Error::runtime(format!(
            "{what} must be a string or string[]"
        ))),
    }
}

/// Listen for one or more events. Returns an id you can pass to
/// `del_autocmd` later to remove the listener.
///
/// Built-in events fired by the host: `"TurnStart"`, `"TurnEnd"`,
/// `"TurnError"`, `"ToolStart"`, `"ToolDone"`, `"ToolReviewed"`,
/// `"AutoCompacting"`, `"CompactionDone"`, `"PlanReady"`, `"SessionReset"`,
/// `"SessionEnd"`, `"SessionFocusChanged"`, `"SessionStatusChanged"`,
/// `"TaskStatusChanged"`, and `"ModelChanged"`. Plugins can also fire their
/// own events with `exec_autocmds`.
///
/// Every host event carries `data.session_id`. For `"SessionReset"` and
/// `"SessionEnd"` that is the session being left behind, the other events
/// name the session now running or focused. What each event adds:
///
/// - `"ToolStart"`, `"ToolDone"`: `data.tool_id` and `data.tool`.
/// - `"ToolReviewed"`: `data.tool`, `data.reviewer`, `data.model`,
///   `data.verdict`, `data.reason`, `data.resolution`, `data.cost`, and
///   `data.list_cost`.
/// - `"TurnEnd"`: `data.reason` (`"finished"`, `"max_tokens"`,
///   `"max_turns"`, or `"cancelled"`), `data.usage` (four token fields,
///   cache included), `data.cost`, `data.list_cost`, `data.context_size`,
///   `data.context_window`, and `data.num_turns` (model round-trips the
///   turn took). `cost` is the real bill; `list_cost` is the un-subsidised
///   reference price and is only set for subsidised models, so a budget
///   plugin against list price should fall back to `cost` when absent.
/// - `"AutoCompacting"`: `data.context_size` and `data.context_window` at
///   trigger time.
/// - `"CompactionDone"`: `data.context_size_before`,
///   `data.context_size_after`, and `data.context_window`.
/// - `"PlanReady"`: `data.path`, the absolute path of the plan file the
///   agent just wrote. Fires once per draft.
/// - `"SessionFocusChanged"`: `data.previous_session_id`, absent on the
///   first focus at startup.
/// - `"SessionStatusChanged"`: `data.status` (`"working"`, `"needs_input"`,
///   or `"idle"`), `data.title`, and `data.focused` (boolean).
/// - `"TaskStatusChanged"`: `data.id`, `data.name`, and `data.status`
///   (`"working"`, `"done"`, or `"error"`), when a subagent starts or
///   changes status. A task that comes back from disk already finished
///   stays quiet, so reloading a session does not replay old tasks.
/// - `"ModelChanged"`: `data.model` in the shape `maki.model.get` returns,
///   plus `data.previous_spec`. Picking the model already in use stays
///   quiet, and so does startup.
///
/// `"TurnEnd"` fires once per turn and only for the main session, so
/// subagent turns never show up. A manual `/compact` ends its run without
/// ending a turn, so it stays quiet too.
///
/// Drivers are not all caught up. `"TurnStart"` and `"PlanReady"` come from
/// `maki-ui` only. `maki-acp` runs the agent on its own loop and does not
/// call the dispatcher yet, so plugins loaded under ACP receive no turn
/// events. Everything else fires under `maki -p` and sdk mode as well.
///
/// `"SessionEnd"` is the teardown signal: it fires first so handlers can
/// still inspect or stop the session's jobs, then session-owned jobs are
/// reaped. `data.reason` names the path it came from: `"reset"` (TUI
/// `/new`), `"load"`, `"delete"` (tab closed), `"shutdown"`, `"reload"`
/// (`/reload` is rebuilding the plugin host, and the session carries on in
/// the new one), `"replaced"` (an ACP client took the session's place), or
/// `"completed"` (a headless run finished).
///
/// On `"shutdown"`, `"reload"`, `"replaced"`, and `"completed"` the host is
/// already tearing down, so the UI is detached (`maki.fn` roundtrips fail
/// right away) and every handler shares one grace period. `data.deadline_ms`
/// is how much of it is left at dispatch, so write state out with `maki.fs`
/// and do not park. On the other reasons nothing waits and `data.deadline_ms`
/// is nil.
///
/// `"SessionReset"` stays TUI-only (`/new`) and fires on the same path as
/// `"SessionEnd"` with `reason = "reset"`.
///
/// Jobs started inside a callback die with the dispatch unless you await
/// them there (`jobwait`) or hand them to a session
/// (`scope = { session = ... }`).
///
/// @param event string|string[] Event name or list of names.
/// @param opts table Options:
///   `callback` (function) called with an ev table `{ id, event, match, data }`.
///   `once` (boolean) remove the handler after it fires once (default false).
///   `pattern` (string|string[]) only fire when the pattern matches. `"*"` matches everything. Omit to match all.
/// @return (integer) Autocmd id.
/// @example
/// local id = maki.api.create_autocmd("TurnEnd", {
///   callback = function(ev)
///     print("turn ended: " .. ev.event)
///   end,
/// })
#[lua_fn]
fn create_autocmd(lua: &Lua, #[ctx] plugin: Arc<str>, event: Value, opts: Table) -> LuaResult<u64> {
    let events = parse_string_or_seq(event, "event")?;
    let callback: Function = opts.get("callback")?;
    let once: bool = opts.get("once").unwrap_or(false);
    let patterns = match opts.get::<Value>("pattern")? {
        Value::Nil => None,
        v => Some(parse_string_or_seq(v, "pattern")?),
    };
    let id = NEXT_AUTOCMD_ID.fetch_add(1, Ordering::Relaxed);
    let mut store = lua
        .app_data_mut::<AutocmdStore>()
        .ok_or_else(|| mlua::Error::runtime("autocmd store not initialized"))?;
    for event in events {
        store.register(
            event,
            AutocmdEntry {
                id,
                callback: callback.clone(),
                plugin: Arc::clone(&plugin),
                once,
                patterns: patterns.clone(),
            },
        );
    }
    Ok(id)
}

/// Remove a previously registered autocmd. Does nothing if the {id}
/// does not exist.
///
/// @param id integer Id returned by `create_autocmd`.
/// @return
/// @example
/// maki.api.del_autocmd(id)
#[lua_fn]
fn del_autocmd(lua: &Lua, id: u64) -> LuaResult<()> {
    if let Some(mut store) = lua.app_data_mut::<AutocmdStore>() {
        store.remove(id);
    }
    Ok(())
}

/// Fire one or more events manually. Every matching autocmd callback
/// runs to completion before this function returns.
///
/// A handler may suspend, so this call may too.
///
/// @param event string|string[] Event name or list of names to fire.
/// @param opts table? Options:
///   `pattern` (string) passed to callbacks as `ev.match`.
///   `data` (any) arbitrary value passed as `ev.data`.
/// @return
/// @example
/// maki.api.exec_autocmds("MyEvent", {
///   pattern = "init",
///   data = { msg = "hello" },
/// })
#[lua_fn]
async fn exec_autocmds(lua: Lua, event: Value, opts: Option<Table>) -> LuaResult<()> {
    let events = parse_string_or_seq(event, "event")?;
    let (pattern, data) = match opts {
        Some(opts) => {
            let pattern = match opts.get::<Value>("pattern")? {
                Value::Nil => None,
                Value::String(s) => Some(s.to_str()?.to_owned()),
                _ => return Err(mlua::Error::runtime("pattern must be a string")),
            };
            (pattern, opts.get::<Value>("data")?)
        }
        None => (None, Value::Nil),
    };
    for event in events {
        dispatch(lua.clone(), event, pattern.clone(), data.clone()).await;
    }
    Ok(())
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_autocmd_methods(plugin: Arc<str>), DOCS [
        create_autocmd(plugin), del_autocmd, exec_autocmds,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(None, None => true ; "no_patterns_no_fired")]
    #[test_case(None, Some("x") => true ; "no_patterns_with_fired")]
    #[test_case(Some(&["*"]), None => true ; "wildcard_no_fired")]
    #[test_case(Some(&["a", "*"]), Some("z") => true ; "wildcard_among_others")]
    #[test_case(Some(&["a", "b"]), Some("b") => true ; "fired_in_patterns")]
    #[test_case(Some(&["a", "b"]), Some("c") => false ; "fired_not_in_patterns")]
    #[test_case(Some(&["a"]), None => false ; "patterns_but_no_fired")]
    fn match_rule(patterns: Option<&[&str]>, fired: Option<&str>) -> bool {
        let owned = patterns.map(|ps| ps.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        pattern_matches(owned.as_deref(), fired)
    }
}
