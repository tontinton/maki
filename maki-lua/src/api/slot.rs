use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, MultiValue, Result as LuaResult, Table, Value};

use crate::api::util::dispatch::{DepthGuard, call_isolated};

#[derive(Clone)]
pub(crate) struct SlotLayer {
    pub plugin: Arc<str>,
    pub func: Function,
}

/// `owner: None` means orphan fillers: `set_slot` ran before the owner's
/// `declare_slot`. They wait here and attach once the owner declares.
#[derive(Default)]
pub(crate) struct SlotEntry {
    pub owner: Option<Arc<str>>,
    pub default: Option<Function>,
    pub layers: Vec<SlotLayer>,
}

#[derive(Default)]
pub(crate) struct SlotStore {
    pub slots: HashMap<String, SlotEntry>,
}

impl SlotStore {
    pub fn clear_plugin(&mut self, plugin: &str) {
        for entry in self.slots.values_mut() {
            entry.layers.retain(|l| l.plugin.as_ref() != plugin);
            if entry.owner.as_deref() == Some(plugin) {
                entry.owner = None;
                entry.default = None;
            }
        }
        self.slots
            .retain(|_, e| e.owner.is_some() || !e.layers.is_empty());
    }
}

/// Each layer gets a fresh single-shot `prev`. Calling it twice, or after
/// the layer already returned, throws instead of running the rest of the
/// chain again: the states make double execution impossible by shape.
enum PrevState {
    Armed,
    Running,
    Done(LuaResult<MultiValue>),
    Expired,
}

type PrevCell = Arc<Mutex<PrevState>>;

fn take_state(cell: &PrevCell, next: PrevState) -> PrevState {
    mem::replace(&mut cell.lock().expect("prev state poisoned"), next)
}

fn set_state(cell: &PrevCell, state: PrevState) {
    *cell.lock().expect("prev state poisoned") = state;
}

fn slot_store_mut(lua: &Lua) -> LuaResult<mlua::AppDataRefMut<'_, SlotStore>> {
    lua.app_data_mut::<SlotStore>()
        .ok_or_else(|| mlua::Error::runtime("slot store not initialized"))
}

fn make_prev(
    lua: &Lua,
    name: &str,
    default: &Function,
    layers: &Arc<[SlotLayer]>,
    rest: usize,
    state: &PrevCell,
) -> LuaResult<Function> {
    let name = name.to_owned();
    let default = default.clone();
    let layers = Arc::clone(layers);
    let state = Arc::clone(state);
    lua.create_function(
        move |lua, args: MultiValue| match take_state(&state, PrevState::Running) {
            PrevState::Armed => {
                let r = invoke_chain(lua, &name, &default, &layers, rest, args);
                set_state(&state, PrevState::Done(r.clone()));
                r
            }
            prior => {
                let what = match prior {
                    PrevState::Expired => "expired",
                    _ => "already consumed",
                };
                set_state(&state, prior);
                Err(mlua::Error::runtime(format!(
                    "prev for slot '{name}' {what}"
                )))
            }
        },
    )
}

/// Runs the chain so everything below a layer executes exactly once.
///
/// `idx` is the number of layers left; layer `idx - 1` runs with a fresh
/// single-shot `prev` that continues the chain. The `(default, layers)`
/// snapshot cannot race an unload: all Lua runs on the runtime thread and
/// unloads arrive through the request channel.
///
/// When a layer errors, its `prev` state tells us how far it got:
/// - never called `prev`: skip the broken layer, run the rest with the
///   layer's own input
/// - called `prev`: the rest already ran, so return the stored outcome
///   rather than re-running it
///
/// Errors from the default propagate unwrapped: the default is the owner's
/// own function, same as any local call.
fn invoke_chain(
    lua: &Lua,
    name: &str,
    default: &Function,
    layers: &Arc<[SlotLayer]>,
    idx: usize,
    args: MultiValue,
) -> LuaResult<MultiValue> {
    let Some(layer) = idx.checked_sub(1).map(|i| &layers[i]) else {
        return default.call(args);
    };
    let state: PrevCell = Arc::new(Mutex::new(PrevState::Armed));
    let prev = make_prev(lua, name, default, layers, idx - 1, &state)?;
    let mut layer_args = args.clone();
    layer_args.push_front(Value::Function(prev));
    let result = call_isolated::<MultiValue>(lua, &layer.func, layer_args, name, &layer.plugin);
    match (result, take_state(&state, PrevState::Expired)) {
        (Some(r), _) => Ok(r),
        (None, PrevState::Done(r)) => r,
        (None, PrevState::Armed) => invoke_chain(lua, name, default, layers, idx - 1, args),
        (None, PrevState::Running | PrevState::Expired) => Err(mlua::Error::runtime(format!(
            "prev for slot '{name}' left in inconsistent state"
        ))),
    }
}

/// The callable closes over `name` only and reads the store on every call,
/// so a handle given out before a reload keeps working after it.
fn make_callable(lua: &Lua, name: String) -> LuaResult<Function> {
    lua.create_function(move |lua, args: MultiValue| {
        let _guard = DepthGuard::enter(lua, "slot", &name).map_err(|_| {
            mlua::Error::runtime(format!(
                "slot '{name}' exceeded max depth (recursive filler? call prev instead)"
            ))
        })?;
        let (default, layers): (Function, Arc<[SlotLayer]>) = {
            let store = lua
                .app_data_ref::<SlotStore>()
                .ok_or_else(|| mlua::Error::runtime("slot store not initialized"))?;
            store
                .slots
                .get(&name)
                .and_then(|e| Some((e.default.clone()?, e.layers.as_slice().into())))
                .ok_or_else(|| mlua::Error::runtime(format!("slot '{name}' is not declared")))?
        };
        invoke_chain(lua, &name, &default, &layers, layers.len(), args)
    })
}

/// Create a named extension point owned by your plugin. You provide a
/// {default} function, and other plugins can wrap it with layers using
/// `set_slot`. The returned callable runs the full chain: outermost
/// layer first, then inward, ending at {default}.
///
/// Throws if another plugin already owns a slot with the same {name}.
///
/// @param name string Unique slot name, e.g. `"myplugin.render"`.
/// @param default function Default implementation, called when no layers wrap it.
/// @return (function) Callable that dispatches through all layers.
/// @example
/// local render = maki.api.declare_slot("myplugin.render", function(text)
///   return text:upper()
/// end)
/// print(render("hello")) -- HELLO
#[lua_fn]
fn declare_slot(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    name: String,
    default: Function,
) -> LuaResult<Function> {
    {
        let mut store = slot_store_mut(lua)?;
        let entry = store.slots.entry(name.clone()).or_default();
        if let Some(owner) = &entry.owner {
            return Err(mlua::Error::runtime(format!(
                "slot '{name}' already declared by '{owner}'"
            )));
        }
        entry.owner = Some(Arc::clone(&plugin));
        entry.default = Some(default);
    }
    make_callable(lua, name)
}

/// Add a layer around an existing (or future) slot. Layers wrap the
/// default from the outside in. Each layer receives `prev` as its
/// first argument. Call `prev(...)` to continue down the chain.
/// Calling `prev` more than once throws.
///
/// You can call this before the owner runs `declare_slot`. The layer
/// is queued and attached when the slot is declared.
///
/// @param name string Slot name to wrap.
/// @param wrapper function Layer: `function(prev, ...)`. Call `prev(...)` to continue.
/// @return
/// @example
/// maki.api.set_slot("myplugin.render", function(prev, text)
///   return prev("[" .. text .. "]")
/// end)
#[lua_fn]
fn set_slot(lua: &Lua, #[ctx] plugin: Arc<str>, name: String, wrapper: Function) -> LuaResult<()> {
    slot_store_mut(lua)?
        .slots
        .entry(name)
        .or_default()
        .layers
        .push(SlotLayer {
            plugin: Arc::clone(&plugin),
            func: wrapper,
        });
    Ok(())
}

/// List all known slots and their current state. Useful for debugging
/// which plugins own or wrap each slot.
///
/// @return (table) Map of slot name to `{ owner, declared, fillers }`.
/// @example
/// for name, info in pairs(maki.api.get_slots()) do
///   print(name, info.owner, info.declared)
/// end
#[lua_fn]
fn get_slots(lua: &Lua) -> LuaResult<Table> {
    let out = lua.create_table()?;
    let Some(store) = lua.app_data_ref::<SlotStore>() else {
        return Ok(out);
    };
    for (name, entry) in &store.slots {
        let info = lua.create_table()?;
        info.set("owner", entry.owner.as_deref())?;
        info.set("declared", entry.default.is_some())?;
        let fillers = lua.create_table()?;
        for layer in &entry.layers {
            fillers.push(layer.plugin.as_ref())?;
        }
        info.set("fillers", fillers)?;
        out.set(name.as_str(), info)?;
    }
    Ok(out)
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_slot_methods(plugin: Arc<str>), DOCS [
        declare_slot(plugin), set_slot(plugin), get_slots,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(lua: &Lua) -> Function {
        lua.create_function(|_, ()| Ok(())).unwrap()
    }

    fn entry(lua: &Lua, owner: &str, filler_plugins: &[&str]) -> SlotEntry {
        SlotEntry {
            owner: Some(Arc::from(owner)),
            default: Some(noop(lua)),
            layers: filler_plugins
                .iter()
                .map(|p| SlotLayer {
                    plugin: Arc::from(*p),
                    func: noop(lua),
                })
                .collect(),
        }
    }

    #[test]
    fn clear_plugin_semantics() {
        let lua = Lua::new();
        let mut store = SlotStore::default();
        store
            .slots
            .insert("s".into(), entry(&lua, "owner", &["a", "b"]));
        store
            .slots
            .insert("solo".into(), entry(&lua, "solo", &["solo"]));

        store.clear_plugin("a");
        let e = &store.slots["s"];
        assert_eq!(e.layers.len(), 1, "only the cleared plugin's layer goes");
        assert_eq!(e.layers[0].plugin.as_ref(), "b");
        assert!(e.owner.is_some());

        store.clear_plugin("owner");
        let e = &store.slots["s"];
        assert!(e.owner.is_none() && e.default.is_none());
        assert_eq!(e.layers.len(), 1, "foreign layer survives owner unload");

        store.clear_plugin("solo");
        assert!(
            !store.slots.contains_key("solo"),
            "fully-cleared entry is dropped"
        );
    }
}
