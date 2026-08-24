use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use maki_agent::tools::HookStage;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, MultiValue, Result as LuaResult, Table, Value};

use crate::api::util::dispatch::{DepthGuard, Reentry, call_swallowing};
use crate::plugin_permissions::PluginPermissions;

/// Slot names the host fires itself. A plugin declaring one would shadow a
/// point whose firing order dispatch guarantees, so the namespace is closed.
pub(crate) const HOST_PREFIX: &str = "tool.";

const SEAM: &str = "slot";

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

pub(crate) struct SlotStore {
    pub slots: HashMap<String, SlotEntry>,
    /// The published view of `slots`, for the one reader that cannot take the
    /// Lua state: a tool call on the agent's thread.
    layered: Arc<LayeredTools>,
}

impl SlotStore {
    pub fn new(layered: Arc<LayeredTools>) -> Self {
        Self {
            slots: HashMap::new(),
            layered,
        }
    }

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
        self.publish();
    }

    /// Rebuilds the published index from `slots`, which stays the only thing
    /// anyone writes. Every mutation ends here, so a tool call never reads a
    /// layer a reload took away, or misses one it just added.
    fn publish(&self) {
        let mut stages = StageSets::default();
        for (name, entry) in &self.slots {
            if entry.layers.is_empty() {
                continue;
            }
            if let Some((tool, stage)) = host_slot_target(name) {
                stages[stage as usize].insert(Arc::from(tool));
            }
        }
        self.layered.0.store(Arc::new(stages));
    }
}

type StageSets = [HashSet<Arc<str>>; HookStage::ALL.len()];

/// Which tools have a layer on which stage, keyed by tool rather than by slot
/// name so the check every tool call makes costs one atomic load and one
/// lookup, with nothing formatted and nothing allocated.
///
/// Owned by the runtime that created the [`SlotStore`], so two plugin hosts in
/// one process each answer for their own layers.
#[derive(Default)]
pub struct LayeredTools(ArcSwap<StageSets>);

impl LayeredTools {
    pub fn wraps(&self, tool: &str, stage: HookStage) -> bool {
        self.0.load()[stage as usize].contains(tool)
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

/// The innermost call of a host-fired chain: no plugin owns those slots, so
/// the arguments fall through unchanged when every layer defers.
fn identity_default(lua: &Lua) -> LuaResult<Function> {
    lua.create_function(|_, args: MultiValue| Ok(args))
}

/// Everything a chain needs except its position in it. Bundled because
/// `create_async_function` wants an owned copy per call, and the alternative is
/// cloning four captures by hand at every hop.
#[derive(Clone)]
struct Chain {
    lua: Lua,
    name: Arc<str>,
    default: Function,
    layers: Arc<[SlotLayer]>,
}

fn make_prev(chain: &Chain, rest: usize, state: &PrevCell) -> LuaResult<Function> {
    let owned = chain.clone();
    let state = Arc::clone(state);
    chain.lua.create_async_function(move |_, args: MultiValue| {
        let chain = owned.clone();
        let state = Arc::clone(&state);
        async move {
            match take_state(&state, PrevState::Running) {
                PrevState::Armed => {
                    let r = invoke_chain(chain, rest, args).await;
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
                        "prev for slot '{}' {what}",
                        chain.name
                    )))
                }
            }
        }
    })
}

/// Runs the chain so everything below a layer executes exactly once.
///
/// `idx` is the number of layers left; layer `idx - 1` runs with a fresh
/// single-shot `prev` that continues the chain. The `(default, layers)`
/// snapshot cannot race an unload: all Lua runs on the runtime thread and
/// unloads arrive through the request channel.
///
/// Layers may park, which is what lets one shell out or read a file before it
/// decides. They run in the caller's task ([`call_swallowing`]), so the
/// caller's cancellation and deadline reach the layers producing its answer.
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
    chain: Chain,
    idx: usize,
    args: MultiValue,
) -> Pin<Box<dyn Future<Output = LuaResult<MultiValue>> + Send>> {
    Box::pin(async move {
        let Some(layer) = idx.checked_sub(1).map(|i| chain.layers[i].clone()) else {
            return chain.default.call_async(args).await;
        };
        let state: PrevCell = Arc::new(Mutex::new(PrevState::Armed));
        let prev = make_prev(&chain, idx - 1, &state)?;
        let mut layer_args = args.clone();
        layer_args.push_front(Value::Function(prev));
        let result =
            call_swallowing::<MultiValue>(&layer.func, layer_args, &chain.name, &layer.plugin)
                .await;
        match (result, take_state(&state, PrevState::Expired)) {
            (Some(r), _) => Ok(r),
            (None, PrevState::Done(r)) => r,
            (None, PrevState::Armed) => invoke_chain(chain, idx - 1, args).await,
            (None, PrevState::Running | PrevState::Expired) => Err(mlua::Error::runtime(format!(
                "prev for slot '{}' left in inconsistent state",
                chain.name
            ))),
        }
    })
}

fn snapshot(lua: &Lua, name: &str) -> Option<(Option<Function>, Arc<[SlotLayer]>)> {
    let store = lua.app_data_ref::<SlotStore>()?;
    let entry = store.slots.get(name)?;
    Some((entry.default.clone(), entry.layers.as_slice().into()))
}

/// The one way into [`invoke_chain`], so no caller can start a chain without
/// the depth bound that stops a layer from re-entering its own seam forever.
async fn run_chain(
    lua: &Lua,
    name: Arc<str>,
    default: Function,
    layers: Arc<[SlotLayer]>,
    args: MultiValue,
) -> LuaResult<MultiValue> {
    let _guard = DepthGuard::enter(lua, SEAM, &name, Reentry::Task).map_err(|_| {
        mlua::Error::runtime(format!(
            "slot '{name}' exceeded max depth (recursive filler? call prev instead)"
        ))
    })?;
    let depth = layers.len();
    let chain = Chain {
        lua: lua.clone(),
        name,
        default,
        layers,
    };
    invoke_chain(chain, depth, args).await
}

/// The slot a stage of a tool call fires: `("bash", Input)` -> `tool.bash.input`.
pub(crate) fn host_slot_name(tool: &str, stage: HookStage) -> String {
    format!("{HOST_PREFIX}{tool}.{}", stage.as_str())
}

/// The inverse of [`host_slot_name`]. `None` for any other name, including a
/// `tool.` name whose suffix names no stage.
pub(crate) fn host_slot_target(slot: &str) -> Option<(&str, HookStage)> {
    let (tool, suffix) = slot.strip_prefix(HOST_PREFIX)?.rsplit_once('.')?;
    let stage = HookStage::ALL.into_iter().find(|s| s.as_str() == suffix)?;
    Some((tool, stage))
}

/// Fires a host-owned slot: same layer contract as a declared one, with an
/// identity default nobody can replace. `allow_layer` says which plugins'
/// layers may see it, and living in the caller keeps slots ignorant of tools
/// and permissions.
///
/// `None` means nothing ran, which the identity default handing back `args`
/// would not say: the caller has to leave the value alone rather than report a
/// rewrite.
pub(crate) async fn run_host_chain(
    lua: &Lua,
    name: &str,
    args: MultiValue,
    allow_layer: &dyn Fn(&str) -> bool,
) -> LuaResult<Option<MultiValue>> {
    let Some((_, layers)) = snapshot(lua, name) else {
        return Ok(None);
    };
    let layers: Arc<[SlotLayer]> = layers
        .iter()
        .filter(|layer| allow_layer(&layer.plugin))
        .cloned()
        .collect();
    if layers.is_empty() {
        return Ok(None);
    }
    run_chain(lua, Arc::from(name), identity_default(lua)?, layers, args)
        .await
        .map(Some)
}

/// The callable closes over `name` only and reads the store on every call,
/// so a handle given out before a reload keeps working after it.
fn make_callable(lua: &Lua, name: String) -> LuaResult<Function> {
    let name: Arc<str> = Arc::from(name.as_str());
    lua.create_async_function(move |lua, args: MultiValue| {
        let name = Arc::clone(&name);
        async move {
            let (default, layers) = snapshot(&lua, &name)
                .and_then(|(default, layers)| Some((default?, layers)))
                .ok_or_else(|| mlua::Error::runtime(format!("slot '{name}' is not declared")))?;
            run_chain(&lua, name, default, layers, args).await
        }
    })
}

/// Create a named extension point owned by your plugin. You provide a
/// {default} function, and other plugins can wrap it with layers using
/// `set_slot`. The returned callable runs the full chain: outermost
/// layer first, then inward, ending at {default}.
///
/// Throws if another plugin already owns a slot with the same {name}, or
/// if {name} starts with `"tool."`, which the host fires itself.
///
/// The chain is async: the default and every layer may park (`maki.fs.*`,
/// `maki.fn.jobwait`, `maki.agent.call_tool`, ...), and so does the
/// returned callable. Call it from a tool handler, a command, or an
/// autocmd, rather than from a `header` or `restore` function, which
/// cannot wait. The chain runs in your task, so cancelling the caller
/// cancels the layers it is waiting on.
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
    if name.starts_with(HOST_PREFIX) {
        return Err(mlua::Error::runtime(format!(
            "slot '{name}' is host owned: '{HOST_PREFIX}' names are fired by maki itself, \
             use set_slot to wrap one"
        )));
    }
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
/// A layer may park, and one that throws is skipped: the chain continues
/// as if it had returned `prev(...)` untouched, so a broken layer never
/// takes the seam down with it.
///
/// Layers wrap in registration order, so the last one registered runs
/// first and sees the value before the others do.
///
/// Maki fires two slots per tool itself: `tool.<name>.input` before
/// permissions look at the call, and `tool.<name>.output` on the text it
/// produced. Both take `function(prev, value, ctx)` and answer with a
/// table to replace the value, nothing to leave it alone, or
/// `nil, reason` to stop the call. Wrapping one costs the capability the
/// tool declares, and a tool declaring none costs every permission. See
/// [Hooks](/docs/hooks/).
///
/// Wrapping a plugin-declared slot you do not own steers a chain someone
/// else's plugin trusts, so it costs full trust (every permission granted).
/// Layering your own slot is free.
///
/// @param name string Slot name to wrap.
/// @param wrapper function Layer: `function(prev, ...)`. Call `prev(...)` to continue.
/// @return
/// @example
/// maki.api.set_slot("myplugin.render", function(prev, text)
///   return prev("[" .. text .. "]")
/// end)
#[lua_fn]
fn set_slot(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] permissions: PluginPermissions,
    name: String,
    wrapper: Function,
) -> LuaResult<()> {
    let mut store = slot_store_mut(lua)?;
    let entry = store.slots.entry(name.clone()).or_default();
    // Host `tool.*` slots gate at chain-fire time via `layer_delegation` so a
    // reload that narrows permissions takes effect on the next call.
    let host_slot = name.starts_with(HOST_PREFIX);
    let self_owned = entry.owner.as_deref() == Some(plugin.as_ref());
    if !host_slot && !self_owned && !permissions.holds_all() {
        return Err(mlua::Error::runtime(format!(
            "set_slot: wrapping '{name}' steers another plugin's chain and requires full trust; \
             grant every permission in plugin.toml or wrap only slots your plugin declares"
        )));
    }
    entry.layers.push(SlotLayer {
        plugin: Arc::clone(&plugin),
        func: wrapper,
    });
    store.publish();
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
    extend "maki.api" => pub(crate) fn add_slot_methods(plugin: Arc<str>, permissions: PluginPermissions), DOCS [
        declare_slot(plugin), set_slot(plugin, permissions), get_slots,
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

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

    #[test_case("tool.bash.input", Some(("bash", HookStage::Input)); "input")]
    #[test_case("tool.bash.output", Some(("bash", HookStage::Output)); "output")]
    #[test_case("tool.mcp__srv__do.input", Some(("mcp__srv__do", HookStage::Input)); "underscored_name")]
    #[test_case("tool.srv.do.input", Some(("srv.do", HookStage::Input)); "dotted_name")]
    #[test_case("tool.bash.header", None; "unknown_suffix")]
    #[test_case("tool.bash", None; "no_suffix")]
    #[test_case("myplugin.render", None; "not_host_owned")]
    fn host_slot_target_reads_the_wrapped_tool(slot: &str, target: Option<(&str, HookStage)>) {
        assert_eq!(host_slot_target(slot), target);
    }

    /// The two directions have to stay each other's inverse, since dispatch
    /// builds the name it fires and `publish` parses the names it was given.
    #[test_case("bash", HookStage::Input ; "input")]
    #[test_case("srv.do", HookStage::Output ; "dotted_output")]
    fn host_slot_names_round_trip(tool: &str, stage: HookStage) {
        let name = host_slot_name(tool, stage);
        assert_eq!(host_slot_target(&name), Some((tool, stage)));
    }

    #[test]
    fn clear_plugin_semantics() {
        let lua = Lua::new();
        let mut store = SlotStore::new(Arc::default());
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

    /// The published index is derived, never written to directly, so an unload
    /// can only narrow it.
    #[test]
    fn publishing_follows_the_layers() {
        let lua = Lua::new();
        let layered: Arc<LayeredTools> = Arc::default();
        let mut store = SlotStore::new(Arc::clone(&layered));
        store.slots.insert(
            host_slot_name("bash", HookStage::Input),
            entry(&lua, "owner", &["a"]),
        );
        store
            .slots
            .insert("myplugin.render".into(), entry(&lua, "owner", &["a"]));
        store.publish();
        assert!(layered.wraps("bash", HookStage::Input));
        assert!(!layered.wraps("bash", HookStage::Output));
        assert!(!layered.wraps("myplugin.render", HookStage::Input));

        store.clear_plugin("a");
        assert!(
            !layered.wraps("bash", HookStage::Input),
            "the index drops with the plugin that registered the layer"
        );
    }
}
