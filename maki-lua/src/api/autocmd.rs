use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table, Value};

use crate::api::util::dispatch::{DepthGuard, call_isolated};

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
///
/// The snapshot below looks racy but is not: all Lua runs on the runtime
/// thread and plugin unloads arrive through the request channel, so nothing
/// can touch the store mid-dispatch.
///
/// `data` is shared across callbacks (nvim does the same), but each callback
/// gets its own `ev` table, so one plugin's mutation cannot leak into the
/// next.
pub(crate) fn dispatch(lua: &Lua, event: &str, pattern: Option<&str>, data: Value) {
    let Ok(_guard) = DepthGuard::enter(lua, "autocmd", event) else {
        tracing::warn!(event, "autocmd dispatch exceeded max depth, skipping");
        return;
    };
    let snapshot: Vec<(u64, Arc<str>, Function)> = {
        let Some(mut store) = lua.app_data_mut::<AutocmdStore>() else {
            return;
        };
        let Some(entries) = store.listeners.get_mut(event) else {
            return;
        };
        let mut snapshot = Vec::new();
        // Drop `once` entries now, at snapshot time: if a callback refires
        // the same event they are already gone, so they stay exactly-once.
        entries.retain(|e| {
            let fires = pattern_matches(e.patterns.as_deref(), pattern);
            if fires {
                snapshot.push((e.id, Arc::clone(&e.plugin), e.callback.clone()));
            }
            !(fires && e.once)
        });
        snapshot
    };
    for (id, plugin, callback) in snapshot {
        let ev = match make_ev_table(lua, id, event, pattern, &data) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(event, error = %e, "failed to build autocmd ev table");
                return;
            }
        };
        call_isolated::<()>(lua, &callback, ev, event, &plugin);
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
/// `"TurnError"`, `"SessionReset"`. Plugins can also fire their own
/// events with `exec_autocmds`.
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
/// runs synchronously before this function returns.
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
fn exec_autocmds(lua: &Lua, event: Value, opts: Option<Table>) -> LuaResult<()> {
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
        dispatch(lua, &event, pattern.as_deref(), data.clone());
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
