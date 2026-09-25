use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use maki_agent::tools::HookStage;
use maki_agent::tools::hook::Authority;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, MultiValue, Result as LuaResult, Table, Value};

use crate::api::tool::permission_keys;
use crate::api::util::dispatch::{DepthGuard, Reentry, call_swallowing};
use crate::plugin_permissions::{MANIFEST_FILE, Permission, PluginPermissions};

/// Slot names the host fires itself. A plugin declaring one would shadow a
/// point whose firing order dispatch guarantees, so the namespace is closed.
pub(crate) const HOST_PREFIX: &str = "tool.";
/// The other closed namespace: built-in surfaces a plugin layers to take over.
pub(crate) const UI_PREFIX: &str = "ui.";
const PERMISSION_PREFIX: &str = "permission.";
const HOST_PREFIXES: [&str; 3] = [HOST_PREFIX, UI_PREFIX, PERMISSION_PREFIX];

/// Fired where the permission prompt would show. The default is the prompt
/// itself, so a layer either answers for the user or passes the call on.
pub(crate) const PERMISSION_PROMPT_SLOT: &str = "permission.prompt";

/// Fired when the agent finishes writing a plan. The default opens the
/// built-in plan form, so a layer that answers `false` owns the surface for
/// that draft.
pub(crate) const PLAN_FORM_SLOT: &str = "ui.plan_form";
/// Fired just before the plan form opens. The default answers with the
/// built-in rows.
pub(crate) const PLAN_FORM_ACTIONS_SLOT: &str = "ui.plan_form.actions";

const SEAM: &str = "slot";

#[derive(Clone)]
pub(crate) struct SlotLayer {
    pub plugin: Arc<str>,
    pub func: Function,
}

/// What a foreign layer pays to steer a declared slot, set by the owner at
/// `declare_slot` because the owner is the only one who knows whether its
/// default does anything with the arguments it is handed.
///
/// `None` is a slot that named no price. Like a tool that declares no
/// capability, that is not a promise the default exercises none, only that
/// nobody asked, so a layer pays [`Authority::Unbounded`]: every permission.
/// An empty list is the owner saying the arguments are inert, and layering is
/// free.
pub(crate) type SlotPrice = Option<Arc<[Permission]>>;

/// `owner: None` means orphan fillers: `set_slot` ran before the owner's
/// `declare_slot`. They wait here and attach once the owner declares, and what
/// each of them is entitled to is weighed when the chain fires, so the order
/// the two plugins loaded in never decides anything.
///
/// `owner` and `capability` outlive the owner's unload; only `default` clears.
/// Were the name freed with them, unloading a plugin would let anyone
/// re-declare its slot at a price of their own and inherit the layers, and the
/// callers, that trusted the old one.
#[derive(Default)]
pub(crate) struct SlotEntry {
    pub owner: Option<Arc<str>>,
    pub capability: SlotPrice,
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
        let mut surfaces = HashSet::new();
        for (name, entry) in &self.slots {
            if entry.layers.is_empty() {
                continue;
            }
            if let Some((tool, stage)) = host_slot_target(name) {
                stages[stage as usize].insert(Arc::from(tool));
            }
            if name.starts_with(UI_PREFIX) || name.starts_with(PERMISSION_PREFIX) {
                surfaces.insert(Arc::from(name.as_str()));
            }
        }
        self.layered.stages.store(Arc::new(stages));
        self.layered.surfaces.store(Arc::new(surfaces));
    }
}

type StageSets = [HashSet<Arc<str>>; HookStage::ALL.len()];

/// Which tools have a layer on which stage, keyed by tool rather than by slot
/// name so the check every tool call makes costs one atomic load and one
/// lookup, with nothing formatted and nothing allocated, plus which host
/// surfaces have one at all.
///
/// Owned by the runtime that created the [`SlotStore`], so two plugin hosts in
/// one process each answer for their own layers.
#[derive(Default)]
pub struct LayeredTools {
    stages: ArcSwap<StageSets>,
    /// The `ui.` and `permission.` slots with at least one layer. The host
    /// reads this before it asks a chain anything, so a stock install draws its
    /// built-in surface in the same frame instead of waiting on a roundtrip.
    surfaces: ArcSwap<HashSet<Arc<str>>>,
}

impl LayeredTools {
    pub fn wraps(&self, tool: &str, stage: HookStage) -> bool {
        self.stages.load()[stage as usize].contains(tool)
    }

    /// Whether any plugin is layering the host surface {slot}.
    pub fn layers_surface(&self, slot: &str) -> bool {
        self.surfaces.load().contains(slot)
    }

    /// Stands in for a plugin host with {slots} layered, for tests that drive
    /// the UI without one.
    #[doc(hidden)]
    pub fn with_surfaces(slots: &[&str]) -> Self {
        let this = Self::default();
        this.surfaces
            .store(Arc::new(slots.iter().map(|s| Arc::from(*s)).collect()));
        this
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

type DelegationFn = Box<dyn Fn(&str, Authority, &str) -> bool>;

thread_local! {
    /// Lives on the Lua runtime thread, installed by the runtime because only
    /// it knows what each loaded plugin currently holds. A thread without one
    /// has no grants to weigh a layer against, so nothing foreign gets through.
    static LAYER_DELEGATION: RefCell<Option<DelegationFn>> = const { RefCell::new(None) };
}

pub(crate) fn set_layer_delegation(f: impl Fn(&str, Authority, &str) -> bool + 'static) {
    LAYER_DELEGATION.with(|c| *c.borrow_mut() = Some(Box::new(f)));
}

fn delegated(plugin: &str, authority: Authority, slot: &str) -> bool {
    LAYER_DELEGATION.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|gate| gate(plugin, authority, slot))
    })
}

/// Which layers may steer a plugin-declared chain, answered every time it
/// fires rather than when a layer registered, so a reload that narrows a grant
/// lands on the very next call and a layer registered before its slot existed
/// is weighed once the slot does.
///
/// The owner steers its own chain for free. Anyone else pays what the owner
/// priced the slot at: the capabilities it named, all of them at once, or
/// every permission if it named none, the toll a layer on a tool that declares
/// no capability pays. One denied is dropped from the chain and logged, never
/// an error, because a layer is an opinion about a call and never a
/// precondition for making it.
fn entitled_layers(
    name: &str,
    owner: &str,
    price: &SlotPrice,
    layers: Arc<[SlotLayer]>,
) -> Arc<[SlotLayer]> {
    let entitled = |layer: &SlotLayer| {
        if layer.plugin.as_ref() == owner {
            return true;
        }
        match price {
            None => delegated(&layer.plugin, Authority::Unbounded, name),
            Some(needed) => needed
                .iter()
                .all(|p| delegated(&layer.plugin, Authority::Capability(*p), name)),
        }
    };
    if layers.iter().all(&entitled) {
        return layers;
    }
    layers.iter().filter(|l| entitled(l)).cloned().collect()
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

/// Told which plugin answered with which values, at every hop of a chain, so
/// the host can attribute parts of a produced value to the layer that put
/// them there.
pub(crate) type ChainObserver = Arc<dyn Fn(&Arc<str>, &MultiValue) + Send + Sync>;

/// Everything a chain needs except its position in it. Bundled because
/// `create_async_function` wants an owned copy per call, and the alternative is
/// cloning four captures by hand at every hop.
#[derive(Clone)]
struct Chain {
    lua: Lua,
    name: Arc<str>,
    default: Function,
    layers: Arc<[SlotLayer]>,
    observe: Option<ChainObserver>,
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
            (Some(r), _) => {
                if let Some(observe) = &chain.observe {
                    observe(&layer.plugin, &r);
                }
                Ok(r)
            }
            (None, PrevState::Done(r)) => r,
            (None, PrevState::Armed) => invoke_chain(chain, idx - 1, args).await,
            (None, PrevState::Running | PrevState::Expired) => Err(mlua::Error::runtime(format!(
                "prev for slot '{}' left in inconsistent state",
                chain.name
            ))),
        }
    })
}

type Snapshot = (
    Option<Arc<str>>,
    Option<Function>,
    SlotPrice,
    Arc<[SlotLayer]>,
);

fn snapshot(lua: &Lua, name: &str) -> Option<Snapshot> {
    let store = lua.app_data_ref::<SlotStore>()?;
    let entry = store.slots.get(name)?;
    Some((
        entry.owner.clone(),
        entry.default.clone(),
        entry.capability.clone(),
        entry.layers.as_slice().into(),
    ))
}

/// The one way into [`invoke_chain`], so no caller can start a chain without
/// the depth bound that stops a layer from re-entering its own seam forever.
async fn run_chain(
    lua: &Lua,
    name: Arc<str>,
    default: Function,
    layers: Arc<[SlotLayer]>,
    args: MultiValue,
    observe: Option<ChainObserver>,
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
        observe,
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
    run_host_chain_with(lua, name, identity_default(lua)?, args, allow_layer, None).await
}

/// [`run_host_chain`] with a default of the host's choosing, for a slot whose
/// contract is "produce a value" instead of "rewrite the one it was passed",
/// and an optional {observe} called with each layer's answer as the chain
/// unwinds, innermost first. That is the only place a value can still be told
/// apart from the layer that produced it.
pub(crate) async fn run_host_chain_with(
    lua: &Lua,
    name: &str,
    default: Function,
    args: MultiValue,
    allow_layer: &dyn Fn(&str) -> bool,
    observe: Option<ChainObserver>,
) -> LuaResult<Option<MultiValue>> {
    let Some((_, _, _, layers)) = snapshot(lua, name) else {
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
    run_chain(lua, Arc::from(name), default, layers, args, observe)
        .await
        .map(Some)
}

/// The plugins layering {name}, for a log line about a chain that failed as a
/// whole and cannot name the layer that did it.
pub(crate) fn layer_plugins(lua: &Lua, name: &str) -> String {
    let Some((_, _, _, layers)) = snapshot(lua, name) else {
        return String::new();
    };
    layers
        .iter()
        .map(|l| l.plugin.as_ref())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The callable closes over `name` only and reads the store on every call,
/// so a handle given out before a reload keeps working after it.
fn make_callable(lua: &Lua, name: String) -> LuaResult<Function> {
    let name: Arc<str> = Arc::from(name.as_str());
    lua.create_async_function(move |lua, args: MultiValue| {
        let name = Arc::clone(&name);
        async move {
            let (owner, default, price, layers) = snapshot(&lua, &name)
                .and_then(|(owner, default, price, layers)| Some((owner?, default?, price, layers)))
                .ok_or_else(|| mlua::Error::runtime(format!("slot '{name}' is not declared")))?;
            let layers = entitled_layers(&name, &owner, &price, layers);
            run_chain(&lua, name, default, layers, args, None).await
        }
    })
}

/// Create a named extension point owned by your plugin. You provide a
/// {default} function, and other plugins can wrap it with layers using
/// `set_slot`. The returned callable runs the full chain: outermost layer
/// first, then inward, ending at {default}.
///
/// {opts} prices what a layer from another plugin pays to steer your chain.
/// You set it, because you are the only one who knows what your default does
/// with the arguments it is handed. Pass `{ capability = { "net" } }` to
/// charge the permissions you name, all of them at once; `{ capability = {} }`
/// to let anyone layer for free, which is the honest price for a slot whose
/// arguments are inert; or leave {opts} out to charge every permission, what a
/// tool declaring no capability charges. You can only name permissions your
/// own plugin holds.
///
/// Throws if another plugin already owns a slot with the same {name}, or
/// if {name} starts with `"tool."`, `"ui."` or `"permission."`, which the host
/// fires itself.
/// The name stays yours across an unload: nobody else can take it over, or
/// re-declare it cheaper, while maki runs.
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
/// @param opts table|nil `{ capability = { "net", ... } }`: what a layer from another plugin pays.
/// @return (function) Callable that dispatches through all layers.
/// @example
/// -- anyone may layer this one: it only uppercases the text it is given
/// local render = maki.api.declare_slot("myplugin.render", function(text)
///   return text:upper()
/// end, { capability = {} })
/// print(render("hello")) -- HELLO
#[lua_fn]
fn declare_slot(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] permissions: PluginPermissions,
    name: String,
    default: Function,
    opts: Option<Table>,
) -> LuaResult<Function> {
    if let Some(prefix) = HOST_PREFIXES.iter().find(|p| name.starts_with(**p)) {
        return Err(mlua::Error::runtime(format!(
            "slot '{name}' is host owned: the '{prefix}' prefix is reserved for slots maki \
             fires itself. Layer one with maki.api.set_slot('{name}', ...), or declare yours \
             under a name of your own, e.g. '{plugin}.{}'",
            name.trim_start_matches(prefix)
        )));
    }
    let capability = parse_slot_capability(opts.as_ref(), &name, &permissions)?;
    {
        let mut store = slot_store_mut(lua)?;
        let entry = store.slots.entry(name.clone()).or_default();
        // A dormant entry (owner set, no default) is the owner's own name
        // waiting out an unload, so only its owner reclaims it, and a second
        // live declaration is still the mistake it always was.
        if let Some(prior) = &entry.owner
            && (entry.default.is_some() || prior.as_ref() != plugin.as_ref())
        {
            return Err(mlua::Error::runtime(format!(
                "slot '{name}' already declared by '{prior}'"
            )));
        }
        entry.owner = Some(Arc::clone(&plugin));
        entry.capability = capability;
        entry.default = Some(default);
    }
    make_callable(lua, name)
}

/// The price the owner set, weighed against what the owner itself holds: a
/// plugin cannot price its slot in a capability it was never granted, the same
/// rule `register_tool` applies to the capability a tool exposes. Otherwise a
/// denied plugin could mint a toll booth on reach it has no claim to.
fn parse_slot_capability(
    opts: Option<&Table>,
    name: &str,
    permissions: &PluginPermissions,
) -> LuaResult<SlotPrice> {
    let Some(opts) = opts else {
        return Ok(None);
    };
    let declared = match opts.get::<Value>("capability")? {
        Value::Nil => return Ok(None),
        Value::Table(declared) => declared,
        _ => {
            return Err(mlua::Error::runtime(format!(
                "declare_slot: '{name}' must give 'capability' as a list of permission names, \
                 e.g. {{ capability = {{ \"net\" }} }} (valid: {})",
                permission_keys()
            )));
        }
    };
    let mut priced: Vec<Permission> = Vec::new();
    for key in declared.sequence_values::<String>() {
        let key = key?;
        let permission = Permission::from_key(&key).ok_or_else(|| {
            mlua::Error::runtime(format!(
                "declare_slot: '{name}' prices layers at unknown permission '{key}' (valid: {})",
                permission_keys()
            ))
        })?;
        if !permissions.is_allowed(permission) {
            return Err(mlua::Error::runtime(format!(
                "declare_slot: '{name}' prices layers at '{permission}', which this plugin was not granted. \
                 Add `{permission} = true` under `[permissions]` in the {MANIFEST_FILE} next to the plugin file"
            )));
        }
        if !priced.contains(&permission) {
            priced.push(permission);
        }
    }
    Ok(Some(priced.into()))
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
/// Maki fires two slots around the plan form, both with
/// `ev = { path, session }`. `ui.plan_form.actions` asks for the form's
/// menu, and `ui.plan_form` asks whether the form opens at all. Both are
/// documented under [maki.plan](/docs/lua-api/#maki-plan).
///
/// Maki fires two slots per tool itself: `tool.<name>.input` before
/// permissions look at the call, and `tool.<name>.output` on the text it
/// produced. Both take `function(prev, value, ctx)` and answer with a
/// table to replace the value, nothing to leave it alone, or
/// `nil, reason` to stop the call. Wrapping one costs the capability the
/// tool declares, and a tool declaring none costs every permission. See
/// [Hooks](/docs/hooks/).
///
/// `permission.prompt` fires where the permission prompt would show, with
/// the prompt as its default. A layer takes `function(prev, req, ctx)` and
/// answers with one of the prompt's options, like
/// `{ decision = "allow_session" }`, or passes the call on with
/// `prev(req, ctx)`. See [Hooks](/docs/hooks/#permission-prompt).
///
/// Wrapping a slot another plugin declared steers a chain that plugin's
/// callers trust, so it costs whatever the owner priced it at in
/// `declare_slot`: the capabilities it named, every permission if it named
/// none, or nothing at all if it declared its arguments inert. Layering a slot
/// you declared yourself is free. Like the `tool.*` slots, this is decided
/// when the chain fires: the call skips a layer that is not entitled and
/// carries on, and a reload that changes what you hold takes effect on the
/// next call.
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
    let mut store = slot_store_mut(lua)?;
    let entry = store.slots.entry(name.clone()).or_default();
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
/// `capability` is the list of permissions a layer from another plugin pays,
/// and is absent on a slot whose owner named no price, which costs every
/// permission.
///
/// @return (table) Map of slot name to `{ owner, declared, fillers, capability }`.
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
        if let Some(priced) = &entry.capability {
            let capability = lua.create_table()?;
            for permission in priced.iter() {
                capability.push(permission.manifest_key())?;
            }
            info.set("capability", capability)?;
        }
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
        declare_slot(plugin, permissions), set_slot(plugin), get_slots,
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn noop(lua: &Lua) -> Function {
        lua.create_function(|_, ()| Ok(())).unwrap()
    }

    fn layer_set<B: FromIterator<SlotLayer>>(lua: &Lua, plugins: &[&str]) -> B {
        plugins
            .iter()
            .map(|p| SlotLayer {
                plugin: Arc::from(*p),
                func: noop(lua),
            })
            .collect()
    }

    fn entry(lua: &Lua, owner: &str, filler_plugins: &[&str]) -> SlotEntry {
        SlotEntry {
            owner: Some(Arc::from(owner)),
            capability: None,
            default: Some(noop(lua)),
            layers: layer_set(lua, filler_plugins),
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
        store.slots.insert(
            "orphan".into(),
            SlotEntry {
                layers: layer_set(&lua, &["a"]),
                ..Default::default()
            },
        );

        store.clear_plugin("a");
        let e = &store.slots["s"];
        assert_eq!(e.layers.len(), 1, "only the cleared plugin's layer goes");
        assert_eq!(e.layers[0].plugin.as_ref(), "b");
        assert!(e.owner.is_some());
        assert!(
            !store.slots.contains_key("orphan"),
            "an entry nobody owns goes with its last layer"
        );

        store.clear_plugin("owner");
        let e = &store.slots["s"];
        assert_eq!(
            e.owner.as_deref(),
            Some("owner"),
            "the name stays claimed, so an unload cannot be waited out and the slot re-declared cheaper"
        );
        assert!(e.default.is_none(), "but the chain has nothing left to run");
        assert_eq!(e.layers.len(), 1, "foreign layer survives owner unload");
    }

    /// Stands in for the gate the runtime installs: `full` holds everything,
    /// `netonly` holds `net`, and nobody else holds anything.
    fn grant_gate() {
        set_layer_delegation(|plugin, authority, _| match plugin {
            "full" => true,
            "netonly" => authority == Authority::Capability(Permission::Net),
            _ => false,
        });
    }

    fn steering(lua: &Lua, price: SlotPrice, plugins: &[&str]) -> Vec<String> {
        entitled_layers("s", "owner", &price, layer_set(lua, plugins))
            .iter()
            .map(|l| l.plugin.to_string())
            .collect()
    }

    /// The owner prices what everyone else pays to steer its chain, and pays
    /// nothing itself whatever it named.
    #[test]
    fn the_declared_price_decides_who_steers() {
        let lua = Lua::new();
        grant_gate();
        let all = &["owner", "full", "netonly", "denied"];

        assert_eq!(
            steering(&lua, None, all),
            ["owner", "full"],
            "naming no price charges every permission"
        );
        assert_eq!(
            steering(&lua, Some(Arc::from([Permission::Net])), all),
            ["owner", "full", "netonly"],
            "a named capability is the whole toll"
        );
        assert_eq!(
            steering(
                &lua,
                Some(Arc::from([Permission::Net, Permission::Run])),
                all
            ),
            ["owner", "full"],
            "naming two charges both at once"
        );
        assert_eq!(
            steering(&lua, Some(Arc::from([])), all),
            ["owner", "full", "netonly", "denied"],
            "an empty price is free for everyone"
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
