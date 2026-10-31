use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table, Value};

use crate::agent_autocmd::host_event_fields;
use crate::api::util::convert::TableExt;
use crate::api::util::dispatch::{DepthGuard, Reentry};
use crate::plugin_permissions::{Permission, grant_hint};
use crate::runtime::{quoted_list, run_detached, strip_traceback, warn_plugin_once};

static NEXT_AUTOCMD_ID: AtomicU64 = AtomicU64::new(1);

const WILDCARD_PATTERN: &str = "*";

pub(crate) struct AutocmdEntry {
    pub id: u64,
    pub callback: Function,
    pub plugin: Arc<str>,
    pub once: bool,
    pub patterns: Option<Vec<String>>,
    /// Whether the plugin holds `session_read`, so it gets a host event
    /// whole rather than only its metadata.
    pub reads_content: bool,
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
/// next. A callback whose plugin lacks `session_read` shares a second `data`
/// instead, built once before any callback runs, holding only the keys
/// [`HostEventKind::fields`](crate::agent_autocmd::HostEventKind::fields)
/// calls metadata. An event the host never fires is a plugin's own, and
/// arrives as fired.
pub(crate) async fn dispatch(lua: Lua, event: String, pattern: Option<String>, data: Value) {
    let Ok(_guard) = DepthGuard::enter(&lua, "autocmd", &event, Reentry::Vm) else {
        tracing::warn!(event, "autocmd dispatch exceeded max depth, skipping");
        return;
    };
    let snapshot: Vec<(u64, Arc<str>, Function, bool)> = {
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
                snapshot.push((
                    e.id,
                    Arc::clone(&e.plugin),
                    e.callback.clone(),
                    e.reads_content,
                ));
            }
            !(fires && e.once)
        });
        snapshot
    };
    let redacted = if snapshot.iter().all(|(.., reads_content)| *reads_content) {
        None
    } else {
        match metadata_only(&lua, &event, &data) {
            Ok(redacted) => redacted,
            Err(e) => {
                tracing::warn!(event, error = %e, "failed to build redacted autocmd data");
                return;
            }
        }
    };
    for (id, plugin, callback, reads_content) in snapshot {
        let data = match &redacted {
            Some((stripped, removed)) if !reads_content => {
                warn_plugin_once(&lua, &plugin, &event, || {
                    content_omitted_message(&plugin, &event, removed)
                });
                stripped
            }
            _ => &data,
        };
        let ev = match make_ev_table(&lua, id, &event, pattern.as_deref(), data) {
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

/// {data} cut down to the keys {event} classifies as metadata, and the keys
/// cut, sorted. `None` when nothing was cut, so the caller shares the
/// original and warns nobody, and for an event the host never fires.
///
/// An allowlist on purpose: a key the host sends but nobody classified is
/// cut too, so a field that slips past the pin test fails closed.
///
/// Most events carry only metadata, so the keys are checked before anything
/// is built. A key that is not a string is cut by its type name and never
/// formatted, because a plugin can fire a host event with a key whose
/// `__tostring` throws, and that must not drop the event for everyone.
fn metadata_only(lua: &Lua, event: &str, data: &Value) -> LuaResult<Option<(Value, Vec<String>)>> {
    let (Some(fields), Value::Table(table)) = (host_event_fields(event), data) else {
        return Ok(None);
    };
    let is_metadata = |key: &Value| matches!(key, Value::String(k) if k.to_str().is_ok_and(|k| fields.is_metadata(&k)));
    if table
        .pairs::<Value, Value>()
        .all(|pair| pair.is_ok_and(|(key, _)| is_metadata(&key)))
    {
        return Ok(None);
    }
    let stripped = lua.create_table()?;
    let mut removed = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        if is_metadata(&key) {
            stripped.raw_set(key, value)?;
        } else if let Value::String(k) = &key {
            removed.push(k.to_string_lossy());
        } else {
            removed.push(format!("[{}]", key.type_name()));
        }
    }
    removed.sort_unstable();
    removed.dedup();
    stripped.set_metatable(table.metatable())?;
    Ok(Some((Value::Table(stripped), removed)))
}

pub fn content_omitted_message(plugin: &str, event: &str, removed: &[impl Display]) -> String {
    format!(
        "plugin {plugin}: '{event}' arrives without {}, which needs '{}'. {}",
        quoted_list(removed),
        Permission::SessionRead,
        grant_hint(&[Permission::SessionRead])
    )
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
/// `"TurnError"`, `"ToolStart"`, `"ToolDone"`, `"AutoCompacting"`,
/// `"CompactionDone"`, `"PlanReady"`, `"SessionReset"`, `"SessionEnd"`,
/// `"SessionFocusChanged"`, `"SessionStatusChanged"`, `"SessionTitleChanged"`,
/// `"SessionCwdChanged"`, `"TaskStatusChanged"`, `"TaskStopped"`, `"TaskFocusChanged"`, `"ModelChanged"`, `"InputChanged"`,
/// `"ModeChanged"`, `"FileIndexReady"`, `"JobStart"` and `"JobExit"`. Plugins can also fire their own
/// events with `exec_autocmds`.
///
/// Every host event carries `data.session_id` except `"FileIndexReady"`,
/// which is about a directory rather than a session, and `"ModeChanged"`,
/// since the mode is the same in every session. For `"SessionReset"` and
/// `"SessionEnd"` that is the session being left behind, the other events
/// name the session now running or focused. What each event adds:
///
/// - `"ToolStart"`, `"ToolDone"`: `data.tool_id` and `data.tool`.
/// - `"ToolDone"` adds `data.is_error` and `data.bytes`, the size of the
///   text the model reads. A call that ran also carries `data.duration_ms`
///   and `data.input`, the input after every `tool.*.input` layer. A call
///   that never ran, like a cancelled one, has neither.
/// - `"TurnStart"`: `data.text`, the message that started the turn.
/// - `"TurnEnd"`: `data.reason` (`"finished"`, `"max_tokens"`,
///   `"max_turns"`, `"cancelled"`, or `"dropped"` when an
///   `agent.user_message` layer refused the message), `data.usage` (four
///   token fields, cache included), `data.cost`, `data.list_cost`, `data.context_size`,
///   `data.context_window`, and `data.num_turns` (model round-trips the
///   turn took). `list_cost` is the un-subsidised list price and `cost` is
///   the real bill, so a budget plugin charges against whichever one it
///   wants.
/// - `"AutoCompacting"`: `data.context_size` and `data.context_window` at
///   trigger time.
/// - `"CompactionDone"`: `data.context_size_before`,
///   `data.context_size_after`, `data.context_window`, and `data.summary`,
///   the text that replaced the history.
/// - `"PlanReady"`: `data.path`, the absolute path of the plan file the
///   agent just wrote. Fires once per draft. Plan state is per session, so
///   pass `data.session_id` to `maki.plan.read`.
/// - `"SessionFocusChanged"`: `data.previous_session_id`, absent on the
///   first focus at startup.
/// - `"SessionStatusChanged"`: `data.status` (`"working"`, `"needs_input"`,
///   or `"idle"`), `data.title`, and `data.focused` (boolean).
/// - `"SessionTitleChanged"`: `data.title` and `data.focused` (boolean),
///   when the title changes (rename or auto-generation).
/// - `"SessionCwdChanged"`: `data.cwd`, the session's new working
///   directory, when `/cd` changes it.
/// - `"TaskStatusChanged"`: `data.id`, `data.name`, and `data.status`
///   (`"working"`, `"idle"`, `"done"`, `"error"`, or `"cancelled"`), when a subagent starts
///   or changes status. A task a plugin opened with `maki.agent.session`
///   carries `data.plugin`, and `"idle"` is such a task between prompts. A
///   task that comes back from disk already finished stays quiet, so
///   reloading a session does not replay old tasks.
/// - `"TaskStopped"`: `data.id`, when the user stops a task with Esc in its
///   tab or through `maki.task.stop`. A plugin task carries `data.plugin`,
///   and by then it is already gone from `maki.agent.list()`.
///   `"TaskStatusChanged"` compares tasks once per frame, so it can miss a
///   task that opened and stopped in the same frame. This event never does,
///   so an owner can rely on it to drop work it still has queued.
/// - `"TaskFocusChanged"`: `data.id`, the task now on screen (`"main"` or a
///   subagent's id, what `ctx:task_id()` reports inside a tool). Fires for
///   the chat cycling keys, `maki.task.focus`, and a session switch that
///   lands on another task.
/// - `"ModelChanged"`: `data.model` in the shape `maki.model.get` returns,
///   plus `data.previous_spec`. Picking the model already in use stays
///   quiet, and so does startup.
/// - `"JobStart"`, `"JobExit"`: session-owned jobs (`scope = { session =
///   ... }`) only. Both carry `data.id` and `data.plugin`. `"JobStart"`
///   adds `data.name` and `data.command`; `"JobExit"` adds
///   `data.exit_code`. A killed job exits with `-1`, and an unnamed job
///   has no `data.name`.
/// - `"InputChanged"`: `data.text`, `data.cursor`, `data.anchor`,
///   `data.version` and `data.session_id`, as
///   [`maki.ui.input`](#maki-ui-input) reports them. `data.cursor_only` is
///   true when only the cursor or the selection moved. `data.source` names
///   the plugin whose `maki.ui.input_edit` made the only change since the
///   last `"InputChanged"`, and is nil otherwise, so a handler can skip its
///   own edits. A package goes by its
///   [package name](/docs/packages/#install-from-git), and `init.lua` code
///   by `"global/init.lua"` or `"project/init.lua"`. Fires at most once per
///   screen redraw, and again for the input of a tab that takes focus.
///   [Marks](#input-marks) alone do not fire it, so a handler can call
///   `maki.ui.input_decorate` without looping.
/// - `"ModeChanged"`: `data.old_mode` and `data.new_mode`, each `"n"` or
///   `"i"`. Fires whenever `maki.fn.mode()` changes: on `maki.ui.set_mode`,
///   and when the chat input gains or loses the keyboard, as when a picker
///   opens. Use it to draw a mode indicator. Like `"InputChanged"`, it fires
///   only with a UI.
/// - `"FileIndexReady"`: `data.root`, the absolute directory that was
///   walked, `data.files`, how many paths the walk left, and `data.crashed`
///   and `data.truncated`, the two ways that list is not the whole tree.
///   Fires once per walk that ends, whatever `maki.fs.fuzzy_files` or the
///   `Ctrl+S` picker started it, so a plugin ranking files asks again
///   instead of polling. A walk cancelled before it ended stays quiet.
///   `data.root` is absent for a directory with no UTF-8 spelling, and so is
///   the `root` of the call that asked for the walk, so a plugin matching
///   the two takes an event without a root as a reason to ask again. Asking
///   again only ever re-reads the root the plugin passed.
///
/// Conversation content needs the `session_read` permission. Without it a
/// host event still arrives, carrying only its metadata fields:
/// `"TurnStart"` without `data.text`, `"ToolDone"` without `data.input`,
/// `"CompactionDone"` without `data.summary`, and `"InputChanged"` without
/// `data.text`. Maki warns once per plugin and event, naming what was left
/// out. Events a plugin fires under its own names arrive as fired.
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
fn create_autocmd(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] reads_content: bool,
    event: Value,
    opts: Table,
) -> LuaResult<u64> {
    let events = parse_string_or_seq(event, "event")?;
    let callback: Function = opts.get("callback")?;
    let once: bool = opts.opt_bool("once").unwrap_or(false);
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
                reads_content,
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
    extend "maki.api" => pub(crate) fn add_autocmd_methods(plugin: Arc<str>, reads_content: bool), DOCS [
        create_autocmd(plugin, reads_content), del_autocmd, exec_autocmds,
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
