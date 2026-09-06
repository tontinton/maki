//! `maki.api.register_input_completer`: inline input completion sources.
//! Typing a registered trigger char in the prompt opens a popup the
//! handler fills; selecting an item inserts its text at the cursor.

use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table};

use crate::api::util::command::{
    CompleterEntry, CompleterMap, CompleterWriter, CompletionItem, publish_completer_snapshot,
};

pub(crate) fn items_from_lua(items: &Table) -> mlua::Result<Vec<CompletionItem>> {
    let mut out = Vec::new();
    for (i, item) in items.sequence_values::<Table>().enumerate() {
        let item = item.map_err(|e| {
            mlua::Error::runtime(format!(
                "items_from_lua: item at index {i} is not a table: {e}"
            ))
        })?;
        let label = item.get::<String>("label").map_err(|_| {
            mlua::Error::runtime(format!(
                "items_from_lua: item at index {i} missing required 'label' string"
            ))
        })?;
        if label.is_empty() {
            return Err(mlua::Error::runtime(format!(
                "items_from_lua: item at index {i} has empty 'label'"
            )));
        }
        let insert = match item.get::<mlua::Value>("insert")? {
            mlua::Value::Nil => label.clone(),
            mlua::Value::String(s) => s.to_str()?.to_owned(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "items_from_lua: item at index {i} field 'insert' must be a string, got {}",
                    other.type_name()
                )));
            }
        };
        let detail = match item.get::<mlua::Value>("detail")? {
            mlua::Value::Nil => None,
            mlua::Value::String(s) => Some(s.to_str()?.to_owned()),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "items_from_lua: item at index {i} field 'detail' must be a string, got {}",
                    other.type_name()
                )));
            }
        };
        out.push(CompletionItem {
            label,
            insert,
            detail,
        });
    }
    Ok(out)
}

/// Register an inline completion source for the prompt input. When the
/// user types {trigger} at a word boundary, a popup opens and `handler`
/// is called with the text typed after the trigger, re-queried on every
/// keystroke. Selecting an item replaces the trigger and query with the
/// item's `insert` text (default: its label); Esc keeps the literal text.
///
/// The bundled files completer registers `@` this way; a plugin can bind
/// any other character to any source (issue trackers, snippets, emoji).
///
/// Handlers run outside any tool-call task scope, so `maki.fn.jobstart`
/// there needs `owner = "plugin"`. An `insert` that starts with the
/// trigger reopens the popup on the inserted text — the bundled files
/// completer uses this to drill into directories.
///
/// Registration is live and same-name replaces; `/reload` clears a
/// plugin's completers.
///
/// @param spec table Completer specification:
///   trigger (string) Required. Single character, e.g. "@" or "#".
///   name    (string) Required. Unique per plugin; same name replaces.
///   handler (function) Required. `function(query) -> items` where items
///           is a list of `{ label (required), insert?, detail? }`.
///           Returning nil or an empty list shows "no matches".
/// @return
/// @example
/// maki.api.register_input_completer({
///   trigger = "#",
///   name = "github-prs",
///   handler = function(query)
///     local out = {}
///     for _, pr in ipairs(search_prs(query)) do
///       out[#out + 1] = { label = "#" .. pr.number .. " " .. pr.title, insert = pr.url }
///     end
///     return out
///   end,
/// })
#[lua_fn]
fn register_input_completer(lua: &Lua, #[ctx] plugin: Arc<str>, spec: Table) -> LuaResult<()> {
    let trigger: String = spec.get("trigger").map_err(|_| {
        mlua::Error::runtime("register_input_completer: 'trigger' must be a string")
    })?;
    let mut chars = trigger.chars();
    let (Some(trigger), None) = (chars.next(), chars.next()) else {
        return Err(mlua::Error::runtime(
            "register_input_completer: 'trigger' must be a single character",
        ));
    };
    if trigger.is_alphanumeric() || trigger.is_whitespace() || trigger == '_' {
        // `_` is treated as a word char by the paste helper's word-boundary
        // predicate, so `foo_bar` would fire the popup on `_bar` and never
        // agree with the surrounding input on what counts as one word.
        return Err(mlua::Error::runtime(
            "register_input_completer: 'trigger' must be punctuation, not alphanumeric, whitespace, or '_'",
        ));
    }
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_input_completer: 'name' must be a string"))?;
    if name.is_empty() {
        return Err(mlua::Error::runtime(
            "register_input_completer: 'name' must be non-empty",
        ));
    }
    let handler: Function = spec.get("handler").map_err(|_| {
        mlua::Error::runtime("register_input_completer: 'handler' must be a function")
    })?;
    let mut map = map_mut(lua)?;
    let clash = map
        .iter()
        .find(|(other, entries)| {
            other.as_ref() != plugin.as_ref() && entries.values().any(|e| e.trigger == trigger)
        })
        .map(|(other, _)| Arc::clone(other));
    if let Some(other) = clash {
        return Err(mlua::Error::runtime(format!(
            "register_input_completer: trigger '{trigger}' already registered by plugin '{other}'; unregister first"
        )));
    }
    let key = lua.create_registry_value(handler)?;
    if let Some(old) = map.entry(Arc::clone(&plugin)).or_default().insert(
        Arc::from(name),
        CompleterEntry {
            trigger,
            handler: key,
        },
    ) {
        let _ = lua.remove_registry_value(old.handler);
    }
    drop(map);
    publish(lua);
    Ok(())
}

/// Remove one of this plugin's input completers by name. Unknown names
/// are a no-op, so a toggle can call it unconditionally; `/reload`
/// drops all of a plugin's completers.
///
/// @param name string The `name` the completer was registered under.
/// @return
/// @example
/// maki.api.unregister_input_completer("github-prs")
#[lua_fn]
fn unregister_input_completer(lua: &Lua, #[ctx] plugin: Arc<str>, name: String) -> LuaResult<()> {
    let mut map = map_mut(lua)?;
    if let Some(entries) = map.get_mut(&plugin)
        && let Some(old) = entries.remove(name.as_str())
    {
        let _ = lua.remove_registry_value(old.handler);
        if entries.is_empty() {
            map.remove(&plugin);
        }
        drop(map);
        publish(lua);
    }
    Ok(())
}

fn map_mut(lua: &Lua) -> LuaResult<mlua::AppDataRefMut<'_, CompleterMap>> {
    lua.app_data_mut::<CompleterMap>()
        .ok_or_else(|| mlua::Error::runtime("register_input_completer: not initialized"))
}

fn publish(lua: &Lua) {
    if let (Some(map), Some(writer)) = (
        lua.app_data_ref::<CompleterMap>(),
        lua.app_data_ref::<CompleterWriter>(),
    ) {
        publish_completer_snapshot(&map, &writer);
    }
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_completer_methods(plugin: Arc<str>), DOCS [
        register_input_completer(plugin), unregister_input_completer(plugin),
    ]
}
