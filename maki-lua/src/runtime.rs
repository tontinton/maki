use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::c_int;
use std::panic::catch_unwind;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use event_listener::Event;

use include_dir::Dir;
use maki_agent::cancel::CancelToken;
use maki_agent::prompt::{PromptId, ResolvedSlots, Slot, SlotEntry};
use maki_agent::tools::{
    HeaderResult, PermissionScopes, RegistryError, Tool, ToolLive, ToolRegistry, ToolSource,
};
use maki_agent::{BufferSnapshot, SharedBuf, SnapshotLine, SnapshotSpan, SpanStyle};
use mlua::{Compiler, Function, Lua, RegistryKey, Value as LuaValue, ffi};
use serde_json::Value;

use maki_config::RawConfig;

use crate::api::autocmd::AutocmdStore;
use crate::api::create_maki_global;
use crate::api::r#fn::{JobStore, deliver_job_event};
use crate::api::keymap::KeymapReader;
use crate::api::keymap::{KeymapStore, KeymapWriter};
use crate::api::options::{PluginOptionSpecs, PluginOpts, collect_plugin_options};
use crate::api::slot::SlotStore;
use crate::api::tool::{LuaTool, PendingTool, PendingTools, PermissionScopeSpec, ToolCallReply};
use crate::api::ui::HintStore;
use crate::api::ui::buf::{BufHandle, BufferStore};
use crate::api::util::command::{CommandHandlerMap, HintWriter, publish_command_snapshot};
use crate::api::util::command::{LuaCommandReader, LuaCommandWriter, UiAction};
use crate::api::util::convert::json_to_lua;
use crate::api::util::ctx::LuaCtx;
use crate::api::util::setup::ConfigStore;
use crate::error::PluginError;
use crate::plugin_permissions::{PluginPermissions, load_plugin_permissions};

const INTERRUPT_SHUTDOWN_MSG: &str = "plugin interrupted: host shutting down";
const INTERRUPT_CANCELLED_MSG: &str = "plugin interrupted: task cancelled";
const INTERRUPT_DEADLINE_MSG: &str = "plugin interrupted: deadline exceeded";
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NIL_WITHOUT_FINISH_MSG: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
pub(crate) const CANCELLED_MSG: &str = "cancelled";
const MAX_INFLIGHT_TOOLS: usize = 64;
/// Finished tools kept clickable without a restore round-trip. Purely a
/// cache: a click that misses it falls back to the restore item carried
/// by the request, so eviction only costs latency, never correctness.
/// The UI reuses this cap for how many finished bufs it keeps watching.
pub const WARM_TOOL_CAP: usize = 32;
const GC_STEP_INTERVAL: usize = 4;
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);
const OPT_LEVEL_JIT: u8 = 2;
const OPT_LEVEL_DEBUGGABLE: u8 = 1;
const DEBUG_INFO_FULL: u8 = 2;
const ASYNC_RUN_DEFAULT_DEADLINE: Duration = Duration::from_secs(60);
/// Async tasks spawned during restore may spawn further tasks; cap the rounds.
const RESTORE_SPAWN_ROUNDS: usize = 8;
/// Keeps a buggy plugin's restore task from freezing the lua loop.
const RESTORE_ASYNC_DEADLINE: Duration = Duration::from_secs(10);
/// Hard cap on one whole restore item. The watchdog interrupt only lands
/// while Lua runs, so a restore parked on a never-resolving await would otherwise
/// hold its gate slot forever and deadlock `gate.drain()` in the dispatcher.
/// Generous on purpose: legit restores of heavy items take double-digit
/// seconds on a loaded debug build, and a wrongly killed restore loses the
/// tool's rendered output.
const RESTORE_ITEM_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_END_EVENT: &str = "TurnEnd";
/// Without a cap, a runaway plugin OOM-kills the whole process.
/// With one, it hits a catchable Lua error instead.
const LUA_MEMORY_LIMIT: usize = 512 * 1024 * 1024;

pub type LoadResult = Result<(), PluginError>;
pub(crate) enum HintContent {
    Static(String),
    Callback(RegistryKey),
}

pub(crate) struct PromptHintRegistration {
    pub(crate) prompts: Option<Vec<PromptId>>,
    pub(crate) slot: Slot,
    pub(crate) content: HintContent,
}

pub(crate) type PromptHintCallbacks = BTreeMap<Arc<str>, Vec<PromptHintRegistration>>;

/// Load/clear drain in-flight tools first so we never mutate a
/// plugin environment while a tool call is still running.
pub enum Request {
    LoadSource {
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
        permissions: PluginPermissions,
        opts: PluginOpts,
        reply: flume::Sender<LoadResult>,
    },
    CallTool {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        ctx: Box<LuaCtx>,
        deadline: Option<Instant>,
        reply: flume::Sender<ToolCallReply>,
        live: Option<LiveCtx>,
    },
    ComputeHeader {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        reply: flume::Sender<HeaderResult>,
    },
    ComputePermissionScopes {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        reply: flume::Sender<Option<PermissionScopes>>,
    },
    ClearPlugin {
        plugin: Arc<str>,
        reply: flume::Sender<()>,
    },
    RunInitLua {
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
        reply: flume::Sender<Result<Option<RawConfig>, PluginError>>,
    },
    RunCommand {
        plugin: Arc<str>,
        command: Arc<str>,
        args: String,
    },
    CollectPromptSlots {
        reply: flume::Sender<ResolvedSlots>,
    },
    CollectPluginOptions {
        reply: flume::Sender<PluginOptionSpecs>,
    },
    Shutdown,
    RestoreToolAsync {
        item: RestoreItem,
        event_tx: maki_agent::EventSender,
    },
    RestoreComplete {
        flag: Arc<AtomicBool>,
    },
    FireAutocmd {
        event: String,
        data: Value,
    },
    ClickTool {
        tool_use_id: String,
        /// 1-based line in the tool's live buffer; 0 means the click landed
        /// outside the buffer (e.g. on the header line).
        row: usize,
        /// Cold path for finished tools: when no live or warm handle
        /// exists, restore from this item (its `clicks` already include
        /// `row`) instead of dropping the click.
        fallback: Option<Box<ClickFallback>>,
    },
    RunKeybindCallback {
        id: u64,
    },
    Describe {
        plugin: Arc<str>,
        tool: Arc<str>,
        dctx: Value,
        reply: flume::Sender<Option<String>>,
    },
    /// Runs the tool's `start` fn so it can publish a live buf before the
    /// permission prompt paints. Best-effort: Lua errors are logged, never
    /// propagated.
    StartTool {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        live: LiveCtx,
        ctx: Box<LuaCtx>,
        reply: flume::Sender<()>,
    },
}

pub struct RestoreItem {
    pub tool: Arc<str>,
    pub tool_use_id: String,
    pub output: String,
    pub input: Value,
    pub is_error: bool,
    pub tool_output_lines: maki_config::ToolOutputLines,
    /// Lets the UI discard snapshots from a stale theme.
    pub theme_gen: Option<u64>,
    /// Buf rows the user clicked since the tool completed, replayed in
    /// order after restore so the tool's own toggle logic reproduces the
    /// expansion state (each row was measured against the layout the
    /// previous replays produce).
    pub clicks: Vec<usize>,
    /// Structured state the tool persisted alongside its output.
    pub state: Option<Value>,
}

pub(crate) struct ClickFallback {
    pub item: RestoreItem,
    pub event_tx: maki_agent::EventSender,
}

pub(crate) struct RestoreReply {
    pub body: Option<BufferSnapshot>,
    pub header: Option<BufferSnapshot>,
}

impl RestoreReply {
    pub(crate) fn emit(
        self,
        tool_use_id: &str,
        theme_gen: Option<u64>,
        event_tx: &maki_agent::EventSender,
    ) {
        if let Some(snapshot) = self.body {
            let _ = event_tx.send(maki_agent::AgentEvent::ToolSnapshot {
                id: tool_use_id.to_owned(),
                snapshot,
                theme_gen,
            });
        }
        if let Some(snapshot) = self.header {
            let _ = event_tx.send(maki_agent::AgentEvent::ToolHeaderSnapshot {
                id: tool_use_id.to_owned(),
                snapshot,
                theme_gen,
            });
        }
    }
}

#[derive(Clone)]
pub struct LiveCtx {
    pub event_tx: maki_agent::EventSender,
    pub tool_use_id: String,
}

/// Lua is single-threaded so this Mutex never contends, but
/// `Lua::app_data` requires `Send + Sync` with the `send` feature.
pub(crate) struct TaskCell {
    pub(crate) cancel: CancelToken,
    pub(crate) deadline: Cell<Option<Instant>>,
    pub(crate) deadline_secs: Cell<Option<u64>>,
    pub(crate) jobs: JobStore,
    pub(crate) bufs: BufferStore,
    pub(crate) live: Option<LiveCtx>,
    /// The buf that owns click routing for this task: the last one passed
    /// to `ctx:live_buf` or returned as a reply/restore `body`. Fallback is
    /// the first buf the task created (`bufs.live_buf()`).
    pub(crate) root_buf: Option<Arc<SharedBuf>>,
    /// Forwards live bufs and annotations to a parent
    /// `maki.agent.call_tool(on_live_buf/on_annotation)`.
    pub(crate) live_sink: Option<flume::Sender<ToolLive>>,
    /// When `Some`, `maki.async.run` tasks queue here instead of the global
    /// `SpawnQueue` so restore can run them inline before snapshotting.
    pub(crate) inline_spawn: Option<Vec<PendingAsyncTask>>,
    /// Set by [`TaskScope::new`]; `enqueue_async_task` upgrades it so queued
    /// tasks share ownership of `bufs`. See [`BufsClaim`].
    bufs_claim: Weak<BufsClaim>,
}

impl TaskCell {
    pub(crate) fn new(
        cancel: CancelToken,
        deadline: Option<Instant>,
        live: Option<LiveCtx>,
    ) -> Self {
        Self {
            cancel,
            deadline: Cell::new(deadline),
            deadline_secs: Cell::new(None),
            jobs: JobStore::new(),
            bufs: BufferStore::new(),
            live,
            root_buf: None,
            live_sink: None,
            inline_spawn: None,
            bufs_claim: Weak::new(),
        }
    }
}

pub(crate) type TaskHandle = Arc<Mutex<TaskCell>>;

type LiveTasks = Rc<RefCell<HashMap<String, TaskHandle>>>;
type WarmTools = Rc<RefCell<VecDeque<WarmTool>>>;

/// A finished tool that still answers clicks. `handle` is a fresh cell
/// holding only the root buf; `_claim` keeps the buf handler slots alive
/// (they normally clear at scope drop) until this entry is evicted.
struct WarmTool {
    id: String,
    handle: TaskHandle,
    _claim: Arc<BufsClaim>,
}

pub(crate) fn lock_cell(handle: &TaskHandle) -> std::sync::MutexGuard<'_, TaskCell> {
    handle.lock().unwrap_or_else(|e| e.into_inner())
}

/// The buf whose click handler owns this task's clicks: the explicit root
/// (live_buf / reply body / restore body), else the first created buf.
fn resolve_root_buf(handle: &TaskHandle) -> Option<Arc<SharedBuf>> {
    let cell = lock_cell(handle);
    cell.root_buf
        .clone()
        .or_else(|| cell.bufs.live_buf().cloned())
}

/// Sole place the `--no-jit` flag touches VM state. Called once at VM
/// creation, before any chunk (init.lua included) is compiled. Jit off
/// drops to the O1 interpreter with full debug info: that combination
/// keeps the most usable backtraces.
fn apply_jit(lua: &Lua, enabled: bool) {
    lua.enable_jit(enabled);
    let compiler = if enabled {
        Compiler::new().set_optimization_level(OPT_LEVEL_JIT)
    } else {
        Compiler::new()
            .set_optimization_level(OPT_LEVEL_DEBUGGABLE)
            .set_debug_level(DEBUG_INFO_FULL)
    };
    lua.set_compiler(compiler);
}

type InterruptFn = unsafe extern "C-unwind" fn(*mut ffi::lua_State, c_int);

/// The poker thread and the VM thread race on this field, so the write
/// must be atomic to stay defined behavior on the Rust side.
fn store_interrupt(state: *mut ffi::lua_State, cb: Option<InterruptFn>) {
    let raw = cb.map_or(ptr::null_mut(), |f| f as *mut ());
    unsafe {
        let slot = &raw mut (*ffi::lua_callbacks(state)).interrupt;
        AtomicPtr::from_ptr(slot.cast::<*mut ()>()).store(raw, Ordering::Release);
    }
}

/// Shutdown flag mirrored into app data so the watchdog interrupt can
/// re-check it on the Lua thread.
struct ShutdownFlag(Arc<AtomicBool>);

/// Cancellation watchdog. A resident mlua interrupt fires at every
/// safepoint and costs ~100ns a pop, which ate most of the codegen win
/// (see `benches/luau_perf.rs`). So the VM runs with no interrupt at
/// all, and this thread arms a one-shot native one every poll tick.
/// Luau documents `lua_callbacks(L)->interrupt` as safe to assign from
/// another thread, and the VM only pays a null check per safepoint.
/// The callback re-checks shutdown/cancel/deadline on the Lua thread
/// before raising, so a stale poke never kills the wrong task.
struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn spawn(lua: &Lua, shutdown: Arc<AtomicBool>) -> Self {
        lua.set_app_data(ShutdownFlag(shutdown));
        let main_state =
            lua.exec_raw_lua(|raw| unsafe { ffi::lua_mainthread(raw.state()) }) as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            // Keeps the VM alive while this thread can still write to it,
            // even if a refactor reorders drops.
            let keep_alive = lua.clone();
            move || {
                let _keep_alive = keep_alive;
                loop {
                    thread::park_timeout(WATCHDOG_POLL_INTERVAL);
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    store_interrupt(main_state as *mut ffi::lua_State, Some(watchdog_interrupt));
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// One-shot interrupt armed by [`Watchdog`]: disarms itself, re-checks the
/// kill conditions, and raises a plain string error that unwinds like any
/// Lua error. Must not raise during GC (`gc >= 0`), same rule mlua follows.
unsafe extern "C-unwind" fn watchdog_interrupt(state: *mut ffi::lua_State, gc: c_int) {
    if gc >= 0 {
        return;
    }
    store_interrupt(state, None);
    // A Rust panic must not unwind into the VM; treat it as "no kill".
    let msg = catch_unwind(|| interrupt_reason(state)).unwrap_or(None);
    if let Some(msg) = msg {
        unsafe {
            // A safepoint frame may have zero free slots; grow before pushing
            // (raw pushes assert a free slot). On failure the next poke retries.
            if ffi::lua_checkstack(state, 1) == 0 {
                return;
            }
            ffi::lua_pushlstring(state, msg.as_ptr().cast(), msg.len());
            ffi::lua_error(state);
        }
    }
}

fn interrupt_reason(state: *mut ffi::lua_State) -> Option<&'static str> {
    let lua = unsafe { Lua::get_or_init_from_ptr(state) };
    if lua
        .app_data_ref::<ShutdownFlag>()
        .is_some_and(|f| f.0.load(Ordering::Relaxed))
    {
        return Some(INTERRUPT_SHUTDOWN_MSG);
    }
    let handle = lua.app_data_ref::<TaskHandle>()?;
    let cell = lock_cell(&handle);
    if cell.cancel.is_cancelled() {
        Some(INTERRUPT_CANCELLED_MSG)
    } else if cell.deadline.get().is_some_and(|d| Instant::now() > d) {
        Some(INTERRUPT_DEADLINE_MSG)
    } else {
        None
    }
}

/// Scopes a `TaskCell` into `Lua::app_data` for one task, restoring
/// the previous on drop. Async work must use `scope_future` because
/// concurrent tasks on the same executor overwrite app_data between yields.
pub(crate) struct TaskScope {
    lua: Lua,
    handle: TaskHandle,
    prev: Option<TaskHandle>,
    /// Dropped after `Drop::drop` runs, so jobs die before bufs can clear.
    /// Warm entries clone it to keep buf handlers alive past completion.
    bufs_claim: Arc<BufsClaim>,
}

impl TaskScope {
    pub(crate) fn new(lua: &Lua, cell: TaskCell) -> Self {
        let handle: TaskHandle = Arc::new(Mutex::new(cell));
        let claim = Arc::new(BufsClaim(Arc::clone(&handle)));
        lock_cell(&handle).bufs_claim = Arc::downgrade(&claim);
        let prev = lua.set_app_data::<TaskHandle>(Arc::clone(&handle));
        Self {
            lua: lua.clone(),
            handle,
            prev,
            bufs_claim: claim,
        }
    }

    /// The shared Lua keeps the last task's handle around, so system
    /// callbacks need a fresh scope or the watchdog interrupt kills them
    /// (stale handle looks cancelled). Prefer [`run_detached`] over raw
    /// scopes.
    pub(crate) fn detached(lua: &Lua) -> Self {
        Self::new(lua, TaskCell::new(CancelToken::none(), None, None))
    }

    pub(crate) fn handle(&self) -> &TaskHandle {
        &self.handle
    }

    pub(crate) fn bufs_claim(&self) -> Arc<BufsClaim> {
        Arc::clone(&self.bufs_claim)
    }

    pub(crate) fn scope_future<F>(&self, inner: F) -> ScopedFuture<F> {
        ScopedFuture {
            lua: self.lua.clone(),
            handle: Arc::clone(&self.handle),
            inner,
        }
    }
}

/// Runs an async system callback under a [detached] scope so callers
/// can't forget to set one up.
///
/// Job callbacks (`on_stdout` etc.) are pumped whenever {fut} is
/// suspended, so a handler parked in e.g. `win:recv()` still streams
/// job output, like Neovim firing callbacks from its idle event loop.
///
/// [detached]: TaskScope::detached
pub(crate) async fn run_detached<F: std::future::Future>(lua: &Lua, fut: F) -> F::Output {
    let scope = TaskScope::detached(lua);
    let handle = Arc::clone(scope.handle());
    let pump = async {
        let mut event_buf = Vec::new();
        loop {
            lock_cell(&handle).jobs.drain_events(&mut event_buf);
            for (job_id, event) in event_buf.drain(..) {
                if let Err(e) = deliver_job_event(lua, job_id, &event) {
                    tracing::warn!(error = %strip_traceback(&e), "detached job callback failed");
                }
            }
            smol::Timer::after(DISPATCH_POLL_INTERVAL).await;
        }
    };
    let out = scope.scope_future(smol::future::or(fut, pump)).await;
    drop(scope);
    out
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        {
            let mut cell = lock_cell(&self.handle);
            cell.jobs.kill_all();
            cell.jobs.clear(&self.lua);
        }
        match self.prev.take() {
            Some(p) => {
                self.lua.set_app_data(p);
            }
            None => {
                self.lua.remove_app_data::<TaskHandle>();
            }
        }
    }
}

/// Re-publishes the task handle on every `poll` so concurrent tasks
/// on the shared Lua each see their own `TaskCell`.
pub(crate) struct ScopedFuture<F> {
    lua: Lua,
    handle: TaskHandle,
    inner: F,
}

impl<F: std::future::Future> std::future::Future for ScopedFuture<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: `inner` is structurally pinned; `lua`/`handle` are
        // never moved out.
        let this = unsafe { self.get_unchecked_mut() };
        let prev = this
            .lua
            .set_app_data::<TaskHandle>(Arc::clone(&this.handle));
        let result = unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(cx);
        match prev {
            Some(p) => {
                this.lua.set_app_data(p);
            }
            None => {
                this.lua.remove_app_data::<TaskHandle>();
            }
        }
        result
    }
}

pub(crate) fn active_task(lua: &Lua) -> TaskHandle {
    lua.app_data_ref::<TaskHandle>()
        .map(|r| Arc::clone(&*r))
        .expect("task accessor called outside a task scope")
}

pub(crate) fn with_task_jobs<R>(lua: &Lua, f: impl FnOnce(&mut JobStore) -> R) -> R {
    f(&mut lock_cell(&active_task(lua)).jobs)
}

pub(crate) fn with_task_bufs<R>(lua: &Lua, f: impl FnOnce(&mut BufferStore) -> R) -> R {
    f(&mut lock_cell(&active_task(lua)).bufs)
}

#[cfg(test)]
pub(crate) fn with_live_ctx<R>(lua: &Lua, f: impl FnOnce(&LiveCtx) -> R) -> Option<R> {
    let handle = lua.app_data_ref::<TaskHandle>()?;
    lock_cell(&handle).live.as_ref().map(f)
}

pub(crate) fn enqueue_async_task(lua: &Lua, work_fn: RegistryKey) -> Result<(), mlua::Error> {
    let handle = lua.app_data_ref::<TaskHandle>();
    let (cancel, live_ctx) = match &handle {
        Some(h) => {
            let cell = lock_cell(h);
            (cell.cancel.clone(), cell.live.clone())
        }
        None => (CancelToken::none(), None),
    };

    let mut task = PendingAsyncTask {
        work_fn,
        cancel,
        deadline: Some(Instant::now() + ASYNC_RUN_DEFAULT_DEADLINE),
        live_ctx,
        owner: None,
    };

    if let Some(h) = &handle {
        let mut cell = lock_cell(h);
        // Inline tasks live inside the cell, so a claim there would be a
        // strong Arc cycle; they run before the scope drops anyway.
        if let Some(inline) = cell.inline_spawn.as_mut() {
            inline.push(task);
            return Ok(());
        }
        task.owner = cell.bufs_claim.upgrade();
    }

    let queue = lua
        .app_data_ref::<SpawnQueue>()
        .ok_or_else(|| mlua::Error::runtime("spawn queue not initialized"))?;
    queue.tx.send(task).ok();
    Ok(())
}

/// Caps concurrent coroutines to avoid blowing the Lua stack.
/// Also serves as a drain barrier for load/clear ops.
struct InflightGate {
    lua: Lua,
    count: Cell<usize>,
    ops_since_gc: Cell<usize>,
    event: Event,
}

impl InflightGate {
    fn new(lua: Lua) -> Self {
        Self {
            lua,
            count: Cell::new(0),
            ops_since_gc: Cell::new(0),
            event: Event::new(),
        }
    }

    fn increment(&self) {
        self.count.set(self.count.get() + 1);
    }

    fn decrement(&self) {
        self.count.set(self.count.get().saturating_sub(1));
        self.event.notify(usize::MAX);
        let ops = self.ops_since_gc.get() + 1;
        if ops >= GC_STEP_INTERVAL {
            self.ops_since_gc.set(0);
            self.lua.gc_step().ok();
        } else {
            self.ops_since_gc.set(ops);
        }
    }

    async fn wait_below(&self, limit: usize) {
        loop {
            if self.count.get() < limit {
                return;
            }
            let listener = self.event.listen();
            if self.count.get() < limit {
                return;
            }
            listener.await;
        }
    }

    /// Guards are taken on a task's first poll (`acquire`), so one yield
    /// lets just-spawned tasks register before the barrier reads the count;
    /// a `drain` queued right behind a spawn cannot slip past it.
    async fn drain(&self) {
        smol::future::yield_now().await;
        self.wait_below(1).await;
    }

    /// Admission and accounting in one step, on the task's own poll: the
    /// dispatcher can spawn a whole backlog in one go without ever parking,
    /// and the cap still holds because no coroutine is created before its
    /// guard exists.
    async fn acquire(self: &Rc<Self>) -> GateGuard {
        self.wait_below(MAX_INFLIGHT_TOOLS).await;
        GateGuard::new(self)
    }
}

struct GateGuard(Rc<InflightGate>);

impl GateGuard {
    fn new(gate: &Rc<InflightGate>) -> Self {
        gate.increment();
        Self(Rc::clone(gate))
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.decrement();
    }
}

/// Restore items run as spawned tasks, so queue order no longer says when a
/// batch is done: the App sends its `restoring` flag after the items, and the
/// flag may only clear once every in-flight item has finished (it drives the
/// restore spinner).
#[derive(Default)]
struct RestoreTracker {
    inflight: Cell<usize>,
    flags: RefCell<Vec<Arc<AtomicBool>>>,
}

impl RestoreTracker {
    /// Flags are global across sessions on purpose: any batch reaching idle
    /// releases every registered spinner flag.
    fn release_if_idle(&self) {
        if self.inflight.get() == 0 {
            for flag in self.flags.borrow_mut().drain(..) {
                flag.store(false, Ordering::Relaxed);
            }
        }
    }

    fn finish(&self) {
        self.inflight.set(self.inflight.get().saturating_sub(1));
        self.release_if_idle();
    }

    fn complete(&self, flag: Arc<AtomicBool>) {
        self.flags.borrow_mut().push(flag);
        self.release_if_idle();
    }

    /// Counts one in-flight item until the guard drops, so an early return
    /// (or future refactor) inside a restore task can't strand the spinner.
    fn track(self: &Rc<Self>) -> RestoreGuard {
        self.inflight.set(self.inflight.get() + 1);
        RestoreGuard(Rc::clone(self))
    }
}

struct RestoreGuard(Rc<RestoreTracker>);

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

pub(crate) struct PendingAsyncTask {
    pub work_fn: RegistryKey,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
    pub live_ctx: Option<LiveCtx>,
    pub owner: Option<Arc<BufsClaim>>,
}

/// Shared ownership of a task's `bufs`: the scope holds one clone, each
/// queued `maki.async.run` task holds one, so the `Arc` strong count is the
/// single source of truth for liveness. Dropping the last clone clears the
/// store, breaking Lua GC watcher/click cycles. Root buf is resolved lazily
/// because it may not exist at enqueue time.
pub(crate) struct BufsClaim(TaskHandle);

impl BufsClaim {
    fn root_buf(&self) -> Option<Arc<SharedBuf>> {
        resolve_root_buf(&self.0)
    }
}

impl Drop for BufsClaim {
    fn drop(&mut self) {
        lock_cell(&self.0).bufs.clear();
    }
}

/// Channel of `maki.async.run` tasks. The dispatcher recvs the `rx` side as
/// one arm of its biased select, so a send wakes the loop even while the
/// enqueuing coroutine stays parked.
pub(crate) struct SpawnQueue {
    tx: flume::Sender<PendingAsyncTask>,
    rx: flume::Receiver<PendingAsyncTask>,
}

impl SpawnQueue {
    fn new() -> Self {
        let (tx, rx) = flume::unbounded();
        Self { tx, rx }
    }
}

async fn run_work_fn(
    lua: &Lua,
    work_fn: &RegistryKey,
    deadline: Option<Instant>,
) -> Result<LuaValue, mlua::Error> {
    let func: Function = lua.registry_value(work_fn)?;
    let fut = lua.create_thread(func)?.into_async::<LuaValue>(())?;
    match deadline {
        Some(dl) => {
            futures_lite::future::race(fut, async {
                smol::Timer::at(dl).await;
                Err(mlua::Error::runtime("timeout"))
            })
            .await
        }
        None => fut.await,
    }
}

fn spawn_async_task(
    lua: &Lua,
    ex: &Rc<smol::LocalExecutor<'_>>,
    gate: &Rc<InflightGate>,
    task: PendingAsyncTask,
) {
    if task.cancel.is_cancelled() {
        lua.remove_registry_value(task.work_fn).ok();
        return;
    }

    let lua = lua.clone();
    let g = Rc::clone(gate);

    ex.spawn(async move {
        let _gate_guard = g.acquire().await;

        let scope = TaskScope::new(
            &lua,
            TaskCell::new(task.cancel.clone(), task.deadline, task.live_ctx.clone()),
        );
        let result = scope
            .scope_future(run_work_fn(&lua, &task.work_fn, task.deadline))
            .await;
        if let Err(e) = &result {
            tracing::debug!(error = %e, "async.run: task failed");
        }

        if let Some(ref live) = task.live_ctx
            && let Some(buf) = task.owner.as_ref().and_then(|c| c.root_buf())
        {
            // Always `read`, not `read_if_dirty`: the dirty flag is
            // consume-once and the UI polls each frame, so the flag
            // races. Re-emitting identical content is harmless.
            let _ = live.event_tx.send(maki_agent::AgentEvent::ToolSnapshot {
                id: live.tool_use_id.clone(),
                snapshot: maki_agent::BufferSnapshot::from_arc(buf.read()),
                theme_gen: None,
            });
        }

        drop(scope);
        lua.remove_registry_value(task.work_fn).ok();
    })
    .detach();
}

/// Barrier for load/clear ops: drains queued `maki.async.run` tasks and
/// waits for every in-flight task, looping until both are quiescent. A bare
/// `gate.drain()` is not enough: a click handler that runs during the drain
/// can enqueue an async job into the spawn queue, which only the dispatcher
/// loop would spawn - after the barrier already passed.
async fn drain_barrier(
    lua: &Lua,
    ex: &Rc<smol::LocalExecutor<'_>>,
    gate: &Rc<InflightGate>,
    spawn_rx: &flume::Receiver<PendingAsyncTask>,
) {
    loop {
        while let Ok(task) = spawn_rx.try_recv() {
            spawn_async_task(lua, ex, gate, task);
        }
        gate.drain().await;
        if spawn_rx.is_empty() {
            return;
        }
    }
}

struct ToolKeys {
    handler: RegistryKey,
    header: Option<RegistryKey>,
    restore: Option<RegistryKey>,
    start: Option<RegistryKey>,
    permission_scopes: Option<RegistryKey>,
    describe: Option<RegistryKey>,
}

type PluginMap = Rc<RefCell<HashMap<Arc<str>, HashMap<Arc<str>, ToolKeys>>>>;

struct LuaRuntime {
    /// Held for its Drop (joins the poker thread). Field order doesn't
    /// matter: the thread keeps its own `Lua` clone alive.
    _watchdog: Watchdog,
    lua: Lua,
    pending: PendingTools,
    plugins: PluginMap,
    live_tasks: LiveTasks,
    warm_tools: WarmTools,
    registry: Arc<ToolRegistry>,
    tx: flume::Sender<Request>,
    shutdown: Arc<AtomicBool>,
    bundled_dirs: &'static [&'static Dir<'static>],
    ui_action_tx: Option<flume::Sender<UiAction>>,
}

impl LuaRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: Arc<ToolRegistry>,
        tx: flume::Sender<Request>,
        shutdown: Arc<AtomicBool>,
        bundled_dirs: &'static [&'static Dir<'static>],
        ui_action_tx: Option<flume::Sender<UiAction>>,
        command_writer: LuaCommandWriter,
        keymap_writer: KeymapWriter,
        hint_writer: HintWriter,
        jit: bool,
    ) -> Result<Self, PluginError> {
        let lua = Lua::new();
        apply_jit(&lua, jit);
        lua.set_memory_limit(LUA_MEMORY_LIMIT)
            .map_err(|e| PluginError::Lua {
                plugin: "<init>".to_owned(),
                source: e,
            })?;
        let pending: PendingTools = Arc::new(Mutex::new(Vec::new()));

        let watchdog = Watchdog::spawn(&lua, Arc::clone(&shutdown));

        let globals = lua.globals();
        for name in &["require", "io", "package"] {
            globals
                .set(*name, LuaValue::Nil)
                .map_err(|e| PluginError::Lua {
                    plugin: "<init>".to_owned(),
                    source: e,
                })?;
        }
        drop(globals);
        lua.sandbox(true).map_err(|e| PluginError::Lua {
            plugin: "<init>".to_owned(),
            source: e,
        })?;

        lua.set_app_data(CommandHandlerMap::new());
        lua.set_app_data(SpawnQueue::new());
        lua.set_app_data(command_writer);
        lua.set_app_data(PromptHintCallbacks::default());
        lua.set_app_data(PluginOptionSpecs::default());
        lua.set_app_data(AutocmdStore::default());
        lua.set_app_data(SlotStore::default());
        lua.set_app_data(KeymapStore::new());
        lua.set_app_data(keymap_writer);
        lua.set_app_data(HintStore::new());
        lua.set_app_data(hint_writer);
        lua.set_app_data(Arc::clone(&registry));

        let plugins: PluginMap = Rc::new(RefCell::new(HashMap::new()));
        {
            let lua = lua.clone();
            let plugins = Rc::clone(&plugins);
            crate::api::tool::set_local_describe(move |plugin, tool, dctx| {
                run_describe(&lua, &plugins, plugin, tool, dctx)
            });
        }
        {
            let lua = lua.clone();
            let plugins = Rc::clone(&plugins);
            crate::api::tool::set_local_tool_handles(move |tool| {
                let plugins = plugins.borrow();
                let tk = plugins.values().find_map(|tools| tools.get(tool))?;
                let to_fn = |key: Option<&RegistryKey>| {
                    key.and_then(|k| lua.registry_value::<Function>(k).ok())
                };
                Some((to_fn(tk.header.as_ref()), to_fn(tk.restore.as_ref())))
            });
        }

        Ok(Self {
            _watchdog: watchdog,
            lua,
            pending,
            plugins,
            live_tasks: Rc::new(RefCell::new(HashMap::new())),
            warm_tools: Rc::new(RefCell::new(VecDeque::new())),
            registry,
            tx,
            shutdown,
            bundled_dirs,
            ui_action_tx,
        })
    }

    fn drop_plugin_keys(&mut self, name: &str) {
        self.warm_tools.borrow_mut().clear();
        if let Some(mut store) = self.lua.app_data_mut::<PluginOptionSpecs>() {
            store.remove(name);
        }
        if let Some(mut store) = self.lua.app_data_mut::<AutocmdStore>() {
            store.clear_plugin(name);
        }
        if let Some(mut store) = self.lua.app_data_mut::<SlotStore>() {
            store.clear_plugin(name);
        }
        if let Some(keys) = self.plugins.borrow_mut().remove(name) {
            for (_, tk) in keys {
                if let Err(e) = self.lua.remove_registry_value(tk.handler) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua handler key");
                }
                if let Some(sk) = tk.header
                    && let Err(e) = self.lua.remove_registry_value(sk)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua header key");
                }
                if let Some(sk) = tk.permission_scopes
                    && let Err(e) = self.lua.remove_registry_value(sk)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua permission_scopes key");
                }
                if let Some(sk) = tk.start
                    && let Err(e) = self.lua.remove_registry_value(sk)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua start key");
                }
                if let Some(sk) = tk.describe
                    && let Err(e) = self.lua.remove_registry_value(sk)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua describe key");
                }
            }
        }
        if let Some(mut cmd_map) = self.lua.app_data_mut::<CommandHandlerMap>()
            && let Some(cmds) = cmd_map.remove(name)
        {
            for (_, entry) in cmds {
                if let Err(e) = self.lua.remove_registry_value(entry.handler) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop command handler key");
                }
            }
            drop(cmd_map);
            if let (Some(map), Some(writer)) = (
                self.lua.app_data_ref::<CommandHandlerMap>(),
                self.lua.app_data_ref::<LuaCommandWriter>(),
            ) {
                publish_command_snapshot(&map, &writer);
            }
        }
        if let Some(mut hints) = self.lua.app_data_mut::<PromptHintCallbacks>()
            && let Some(regs) = hints.remove(name)
        {
            for reg in regs {
                if let HintContent::Callback(key) = reg.content
                    && let Err(e) = self.lua.remove_registry_value(key)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop prompt hint key");
                }
            }
        }
    }

    async fn run_hint_callback(&self, plugin: &str, func: Function) -> Option<String> {
        let result: mlua::Result<LuaValue> = run_detached(&self.lua, async {
            let thread = self.lua.create_thread(func)?;
            thread.into_async::<LuaValue>(())?.await
        })
        .await;
        match result {
            Ok(LuaValue::String(s)) => Some(s.to_string_lossy()),
            Ok(LuaValue::Nil) => None,
            Ok(_) => {
                tracing::warn!(plugin, "prompt hint callback returned non-string");
                None
            }
            Err(e) => {
                tracing::warn!(plugin, error = %e, "prompt hint callback failed");
                None
            }
        }
    }

    async fn collect_prompt_slots(&self) -> ResolvedSlots {
        struct Pending {
            plugin: Arc<str>,
            prompts: Option<Vec<PromptId>>,
            slot: Slot,
            content: PendingContent,
        }
        enum PendingContent {
            Static(String),
            Callback(Function),
        }

        let pending: Vec<Pending> = {
            let Some(map) = self.lua.app_data_ref::<PromptHintCallbacks>() else {
                return ResolvedSlots::default();
            };
            map.iter()
                .flat_map(|(plugin, regs)| {
                    regs.iter().filter_map(move |r| {
                        let content = match &r.content {
                            HintContent::Static(s) => PendingContent::Static(s.clone()),
                            HintContent::Callback(key) => match self.lua.registry_value(key) {
                                Ok(func) => PendingContent::Callback(func),
                                Err(e) => {
                                    tracing::warn!(plugin = %plugin, error = %e, "failed to read prompt hint callback");
                                    return None;
                                }
                            },
                        };
                        Some(Pending {
                            plugin: Arc::clone(plugin),
                            prompts: r.prompts.clone(),
                            slot: r.slot,
                            content,
                        })
                    })
                })
                .collect()
        };

        let mut slots = ResolvedSlots::default();
        for item in pending {
            let content = match item.content {
                PendingContent::Static(s) => Some(s),
                PendingContent::Callback(func) => self.run_hint_callback(&item.plugin, func).await,
            };
            let Some(content) = content else { continue };
            let explicit = item.prompts.is_some();
            for &pid in item.prompts.as_deref().unwrap_or(PromptId::ALL) {
                if !pid.has_slot(item.slot) {
                    if explicit {
                        tracing::warn!(
                            plugin = %item.plugin,
                            slot = ?item.slot,
                            prompt = ?pid,
                            "prompt hint targets a prompt that has no such slot; ignoring"
                        );
                    }
                    continue;
                }
                slots.insert(
                    pid,
                    item.slot,
                    SlotEntry {
                        plugin: Arc::clone(&item.plugin),
                        content: content.clone(),
                    },
                );
            }
        }
        slots
    }

    fn drain_pending(&self) -> Vec<PendingTool> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    fn discard_pending(&mut self, tools: Vec<PendingTool>) {
        for t in tools {
            if let Err(e) = self.lua.remove_registry_value(t.handler_key) {
                tracing::warn!(error = %e, "failed to drop lua handler key on rollback");
            }
            if let Some(sk) = t.header_key
                && let Err(e) = self.lua.remove_registry_value(sk)
            {
                tracing::warn!(error = %e, "failed to drop lua header key on rollback");
            }
            if let Some(PermissionScopeSpec::Callback(sk)) = t.permission_scopes
                && let Err(e) = self.lua.remove_registry_value(sk)
            {
                tracing::warn!(error = %e, "failed to drop lua permission_scopes key on rollback");
            }
            if let Some(sk) = t.describe_key
                && let Err(e) = self.lua.remove_registry_value(sk)
            {
                tracing::warn!(error = %e, "failed to drop lua describe key on rollback");
            }
        }
    }

    fn build_env(
        &self,
        maki: mlua::Table,
        require_root: Option<PathBuf>,
    ) -> Result<mlua::Table, mlua::Error> {
        let env = self.lua.create_table()?;
        env.set("maki", maki)?;

        if require_root.is_some() || !self.bundled_dirs.is_empty() {
            let require_fn = self.create_require_fn(&env, require_root)?;
            env.set("require", require_fn)?;
        }

        let meta = self.lua.create_table()?;
        meta.set("__index", self.lua.globals())?;
        env.set_metatable(Some(meta))?;
        Ok(env)
    }

    /// Bundled dirs go first so plugins can `require()` shared modules
    /// (like `maki.truncate`) without touching the filesystem.
    fn create_require_fn(
        &self,
        env: &mlua::Table,
        require_root: Option<PathBuf>,
    ) -> Result<Function, mlua::Error> {
        let lua_dir = require_root.map(|r| r.canonicalize().unwrap_or(r));
        let loaded = self.lua.create_table()?;
        let loading = self.lua.create_table()?;
        let env_clone = env.clone();
        let bundled_dirs = self.bundled_dirs;

        self.lua.create_function(move |lua, modname: String| {
            if modname.is_empty() {
                return Err(mlua::Error::runtime(
                    "require: module name must be non-empty",
                ));
            }

            if let Ok(cached) = loaded.get::<LuaValue>(modname.as_str())
                && cached != LuaValue::Nil
            {
                return Ok(cached);
            }

            if loading.get::<bool>(modname.as_str()).unwrap_or(false) {
                return Ok(LuaValue::Boolean(true));
            }

            loading.set(modname.as_str(), true)?;

            let rel_path = modname.replace('.', "/") + ".lua";

            let source_str: Result<Option<String>, mlua::Error> = (|| {
                for dir in bundled_dirs {
                    if let Some(file) = dir.get_file(&rel_path)
                        && let Some(contents) = file.contents_utf8()
                    {
                        return Ok(Some(contents.to_owned()));
                    }
                }
                let Some(dir) = lua_dir.as_ref() else {
                    return Ok(None);
                };
                let abs_path = dir.join(&rel_path);
                let normalized = abs_path.components().fold(PathBuf::new(), |mut acc, c| {
                    match c {
                        std::path::Component::ParentDir => {
                            acc.pop();
                        }
                        std::path::Component::CurDir => {}
                        _ => acc.push(c),
                    }
                    acc
                });
                if !normalized.starts_with(dir) {
                    return Err(mlua::Error::runtime(format!(
                        "require: '{modname}' outside sandbox"
                    )));
                }
                Ok(std::fs::read_to_string(&normalized).ok())
            })();

            let source_str = source_str?;

            let Some(source) = source_str else {
                let _ = loading.set(modname.as_str(), LuaValue::Nil);
                return Err(mlua::Error::runtime(format!(
                    "require '{modname}': module not found"
                )));
            };

            let result: LuaValue = match lua
                .load(&source)
                .set_name(&modname)
                .set_environment(env_clone.clone())
                .eval()
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = loading.set(modname.as_str(), LuaValue::Nil);
                    return Err(e);
                }
            };

            loading.set(modname.as_str(), LuaValue::Nil)?;
            let stored = if result == LuaValue::Nil {
                LuaValue::Boolean(true)
            } else {
                result.clone()
            };
            loaded.set(modname.as_str(), stored)?;

            Ok(result)
        })
    }

    /// `plugins.<name>` options only reach a plugin through
    /// `maki.api.register_options`; if the plugin never declared any, every
    /// key the user set is a typo or unsupported, so fail the load loudly.
    fn check_opts_consumed(&self, name: &str, opts: &PluginOpts) -> Result<(), mlua::Error> {
        if opts.is_empty()
            || self
                .lua
                .app_data_ref::<PluginOptionSpecs>()
                .is_some_and(|store| store.contains_key(name))
        {
            return Ok(());
        }
        let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
        Err(mlua::Error::runtime(format!(
            "unknown options in plugins.{name}: {} (this plugin declares no options via maki.api.register_options)",
            keys.join(", ")
        )))
    }

    async fn load_source(
        &mut self,
        name: Arc<str>,
        source: &str,
        plugin_dir: Option<PathBuf>,
        permissions: &PluginPermissions,
        opts: PluginOpts,
        config_store: Option<&ConfigStore>,
    ) -> LoadResult {
        let map_err = |e: mlua::Error| PluginError::Lua {
            plugin: name.to_string(),
            source: e,
        };

        let stale = self.drain_pending();
        debug_assert!(
            stale.is_empty(),
            "leftover pending tools from previous load"
        );
        self.discard_pending(stale);

        let require_root = plugin_dir.as_ref().map(|d| d.join("lua"));
        let maki = create_maki_global(
            &self.lua,
            Arc::clone(&self.pending),
            Arc::clone(&name),
            self.ui_action_tx.clone(),
            permissions,
            Arc::clone(&opts),
        )
        .map_err(&map_err)?;

        if let Some(cs) = config_store {
            let setup_fn = crate::api::util::setup::create_setup_fn(&self.lua, Arc::clone(cs))
                .map_err(&map_err)?;
            maki.set("setup", setup_fn).map_err(&map_err)?;
        }

        let env = self.build_env(maki, require_root).map_err(&map_err)?;

        self.drop_plugin_keys(&name);

        let exec_result = self
            .lua
            .load(source)
            .set_name(name.as_ref())
            .set_environment(env)
            .exec_async()
            .await;

        let exec_result = exec_result.and_then(|()| self.check_opts_consumed(&name, &opts));
        if let Err(e) = exec_result {
            let stale = self.drain_pending();
            self.discard_pending(stale);
            self.drop_plugin_keys(&name);
            return Err(map_err(e));
        }

        let pending = self.drain_pending();

        let registry_entries: Vec<(Arc<dyn Tool>, ToolSource)> = pending
            .iter()
            .map(|t| {
                let tool: Arc<dyn Tool> = Arc::new(LuaTool {
                    name: Arc::clone(&t.name),
                    description: t.description.clone(),
                    schema: t.schema,
                    audience: t.audience,
                    kind: t.kind.clone(),
                    tx: self.tx.clone(),
                    plugin: Arc::clone(&name),
                    has_header_fn: t.header_key.is_some(),
                    has_start_fn: t.start_key.is_some(),
                    permission_scope_kind: t
                        .permission_scopes
                        .as_ref()
                        .map(PermissionScopeSpec::kind),
                    mutable_path_field: t.mutable_path_field.clone(),
                    timeout: t.timeout,
                    start_annotation: t.start_annotation.clone(),
                    examples: t.examples.clone(),
                    has_describe_fn: t.describe_key.is_some(),
                });
                (
                    tool,
                    ToolSource::Lua {
                        plugin: Arc::clone(&name),
                    },
                )
            })
            .collect();

        if let Err(e) = self.registry.replace_plugin(&name, registry_entries) {
            self.discard_pending(pending);
            return Err(match e {
                RegistryError::NameConflict { name: n, .. } => PluginError::NameConflict {
                    plugin: name.to_string(),
                    tool: n,
                },
            });
        }

        let keys: HashMap<Arc<str>, ToolKeys> = pending
            .into_iter()
            .map(|t| {
                (
                    t.name,
                    ToolKeys {
                        handler: t.handler_key,
                        header: t.header_key,
                        restore: t.restore_key,
                        start: t.start_key,
                        permission_scopes: match t.permission_scopes {
                            Some(PermissionScopeSpec::Callback(k)) => Some(k),
                            _ => None,
                        },
                        describe: t.describe_key,
                    },
                )
            })
            .collect();
        self.plugins.borrow_mut().insert(name, keys);

        Ok(())
    }

    fn clear_plugin(&mut self, plugin: &str) {
        self.registry.clear_plugin(plugin);
        self.drop_plugin_keys(plugin);
        if let Some(mut store) = self.lua.app_data_mut::<KeymapStore>() {
            let keys = store.clear_plugin(plugin);
            let entries = store.snapshot_entries();
            drop(store);
            for key in keys {
                let _ = self.lua.remove_registry_value(key);
            }
            if let Some(writer) = self.lua.app_data_ref::<KeymapWriter>() {
                writer.publish(entries);
            }
        }
        if let Some(mut store) = self.lua.app_data_mut::<HintStore>() {
            store.clear_plugin(plugin);
            let entries = store.snapshot_entries();
            drop(store);
            if let Some(writer) = self.lua.app_data_ref::<HintWriter>() {
                writer.publish(entries);
            }
        }
    }

    fn evict_warm(&self, tool_use_id: &str) {
        self.warm_tools.borrow_mut().retain(|w| w.id != tool_use_id);
    }

    async fn compute_permission_scopes(
        &self,
        plugin: &str,
        tool: &str,
        input: Value,
    ) -> Option<PermissionScopes> {
        let (func, lua_input) = plugin_fn(
            &self.lua,
            &self.plugins,
            plugin,
            tool,
            "permission_scopes",
            |tk| tk.permission_scopes.as_ref(),
            &input,
        )?;
        let result: LuaValue = match run_detached(&self.lua, func.call_async(lua_input)).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "permission_scopes callback failed");
                return None;
            }
        };
        let table = match result {
            LuaValue::Table(t) => t,
            _ => return None,
        };
        let scopes_table: mlua::Table = table.get("scopes").ok()?;
        let mut scopes = Vec::new();
        for (_, s) in scopes_table.pairs::<usize, String>().flatten() {
            scopes.push(s);
        }
        if scopes.is_empty() {
            return None;
        }
        let force_prompt: bool = table.get("force_prompt").unwrap_or(false);
        Some(PermissionScopes {
            scopes,
            force_prompt,
        })
    }

    async fn run_init_lua(
        &mut self,
        source: &str,
        source_name: &str,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let config_store: ConfigStore = Arc::new(Mutex::new(None));
        let perms = load_plugin_permissions(plugin_dir.as_deref());
        self.load_source(
            Arc::from(source_name),
            source,
            plugin_dir,
            &perms,
            PluginOpts::default(),
            Some(&config_store),
        )
        .await?;
        Ok(config_store.lock().unwrap().take())
    }
}

/// Resolves a plugin callback and converts its json input, warning on
/// failure. `None` when the tool has no such callback registered.
fn plugin_fn(
    lua: &Lua,
    plugins: &PluginMap,
    plugin: &str,
    tool: &str,
    callback: &'static str,
    key: impl FnOnce(&ToolKeys) -> Option<&RegistryKey>,
    input: &Value,
) -> Option<(Function, LuaValue)> {
    let func = {
        let plugins = plugins.borrow();
        let key = key(plugins.get(plugin)?.get(tool)?)?;
        match lua.registry_value::<Function>(key) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(plugin, tool, callback, error = %e, "callback registry lookup failed");
                return None;
            }
        }
    };
    match json_to_lua(lua, input) {
        Ok(v) => Some((func, v)),
        Err(e) => {
            tracing::warn!(plugin, tool, callback, error = %e, "callback input conversion failed");
            None
        }
    }
}

/// Async so header fns can yield (highlight, markdown). A sync call
/// would hit the C-call boundary and silently fall back to the plain name.
async fn compute_header(
    lua: &Lua,
    plugins: &PluginMap,
    plugin: &str,
    tool: &str,
    input: Value,
) -> HeaderResult {
    let Some((func, input_lua)) = plugin_fn(
        lua,
        plugins,
        plugin,
        tool,
        "header",
        |tk| tk.header.as_ref(),
        &input,
    ) else {
        return HeaderResult::plain(tool.to_string());
    };

    let result = run_detached(lua, func.call_async::<LuaValue>(input_lua)).await;

    match result {
        Ok(LuaValue::String(s)) => match s.to_str() {
            Ok(s) => HeaderResult::plain(s.to_owned()),
            Err(_) => HeaderResult::plain(tool.to_string()),
        },
        Ok(LuaValue::UserData(ud)) => match ud.borrow::<BufHandle>() {
            Ok(h) => HeaderResult::Styled(h.buf.take()),
            Err(_) => HeaderResult::plain(tool.to_string()),
        },
        Ok(_) => HeaderResult::plain(tool.to_string()),
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "header fn call failed");
            HeaderResult::plain(tool.to_string())
        }
    }
}

async fn restore_item(lua: &Lua, plugins: &PluginMap, item: RestoreItem) -> Option<RestoreReply> {
    let (func, plugin_name) = {
        let plugins = plugins.borrow();
        let (pname, tk) = plugins
            .iter()
            .find_map(|(pname, tools)| tools.get(&*item.tool).map(|tk| (pname.clone(), tk)))?;
        let key = tk.restore.as_ref()?;
        (lua.registry_value::<Function>(key).ok()?, pname)
    };
    let input_lua = json_to_lua(lua, &item.input).ok()?;
    let thread = lua.create_thread(func).ok()?;

    let (dummy_tx, _) = flume::unbounded();
    let cell = TaskCell::new(
        CancelToken::none(),
        Some(Instant::now() + RESTORE_ITEM_TIMEOUT),
        Some(LiveCtx {
            event_tx: maki_agent::EventSender::new(dummy_tx, 0),
            tool_use_id: item.tool_use_id.clone(),
        }),
    );

    let ctx = lua
        .create_userdata(LuaCtx::restore(item.tool_output_lines, item.state))
        .ok()?;
    let inner = thread
        .into_async::<LuaValue>((input_lua, &*item.output, item.is_error, ctx))
        .ok()?;
    let scope = TaskScope::new(lua, cell);
    lock_cell(scope.handle()).inline_spawn = Some(Vec::new());
    let ret = scope
        .scope_future(inner)
        .await
        .inspect_err(|e| tracing::warn!(tool = &*item.tool, error = %e, "restore callback failed"))
        .ok()?;
    run_inline_tasks(lua, &scope).await;

    if let Some(buf) = crate::api::ui::buf::buf_from_reply(&ret) {
        lock_cell(scope.handle()).root_buf = Some(buf);
    }

    if !item.clicks.is_empty()
        && let Some(root) = resolve_root_buf(scope.handle())
        && let Some(func) = crate::api::ui::buf::click_fn(&root)
    {
        for &row in &item.clicks {
            let Ok(data) = lua.create_table() else {
                break;
            };
            let _ = data.set("row", row);
            if let Err(e) = scope.scope_future(func.call_async::<()>(data)).await {
                tracing::warn!(tool = &*item.tool, error = %e, "click replay failed");
                break;
            }
            run_inline_tasks(lua, &scope).await;
        }
    }

    drop(scope);

    let mut reply = extract_restore_reply(&ret)?;
    if reply.header.is_none() {
        reply.header = Some(
            compute_header(lua, plugins, &plugin_name, &item.tool, item.input)
                .await
                .into_snapshot(),
        );
    }
    Some(reply)
}

/// Runs `maki.async.run` tasks queued during restore inline, so their
/// buf mutations land before the snapshot is extracted. Tasks may queue
/// more tasks, hence the rounds.
async fn run_inline_tasks(lua: &Lua, scope: &TaskScope) {
    for _ in 0..RESTORE_SPAWN_ROUNDS {
        let tasks = {
            let mut cell = lock_cell(scope.handle());
            match cell.inline_spawn.as_mut() {
                Some(queue) if !queue.is_empty() => std::mem::take(queue),
                _ => return,
            }
        };
        for task in tasks {
            if !task.cancel.is_cancelled() {
                let deadline = Some(Instant::now() + RESTORE_ASYNC_DEADLINE);
                if let Err(e) = scope
                    .scope_future(run_work_fn(lua, &task.work_fn, deadline))
                    .await
                {
                    tracing::debug!(error = %e, "restore inline async task failed");
                }
            }
            lua.remove_registry_value(task.work_fn).ok();
        }
    }
}

/// Spawns one restore item as a gated task. The restore supersedes any
/// warm click handle, so evict it first: a later click must not resurface
/// the stale view.
fn spawn_restore(
    ex: &Rc<smol::LocalExecutor<'_>>,
    gate: &Rc<InflightGate>,
    restores: &Rc<RestoreTracker>,
    rt: &LuaRuntime,
    item: RestoreItem,
    event_tx: maki_agent::EventSender,
) {
    rt.evict_warm(&item.tool_use_id);
    let tracker = restores.track();
    let lua = rt.lua.clone();
    let plugins = Rc::clone(&rt.plugins);
    let g = Rc::clone(gate);
    ex.spawn(async move {
        let _tracker = tracker;
        // Acquired before the timeout race starts, so the per-item deadline
        // measures the item's own run, not time queued behind the whole batch.
        let _gate_guard = g.acquire().await;
        let id = item.tool_use_id.clone();
        let theme_gen = item.theme_gen;
        let tool = Arc::clone(&item.tool);
        let res = futures_lite::future::race(restore_item(&lua, &plugins, item), async {
            smol::Timer::after(RESTORE_ITEM_TIMEOUT).await;
            tracing::warn!(tool = &*tool, "restore item timed out");
            None
        })
        .await;
        if let Some(reply) = res {
            reply.emit(&id, theme_gen, &event_tx);
        }
    })
    .detach();
}

fn extract_restore_reply(ret: &LuaValue) -> Option<RestoreReply> {
    let (body, header) = match ret {
        LuaValue::UserData(ud) => {
            let h = ud.borrow::<BufHandle>().ok()?;
            (Some(h.buf.take()), None)
        }
        LuaValue::Table(t) => {
            let body = t.get::<LuaValue>("body").ok().and_then(|v| {
                let ud = v.as_userdata()?;
                let h = ud.borrow::<BufHandle>().ok()?;
                Some(h.buf.take())
            });
            let header = t.get::<LuaValue>("header").ok().and_then(|v| {
                let ud = v.as_userdata()?;
                let h = ud.borrow::<BufHandle>().ok()?;
                Some(h.buf.take())
            });
            (body, header)
        }
        _ => return None,
    };
    Some(RestoreReply { body, header })
}

/// Handler returned nil, meaning it went async. Polls job events
/// until `ctx:finish()`, all jobs die, or the deadline expires.
async fn dispatch_async(
    lua: &Lua,
    handle: TaskHandle,
    plugin: &str,
    tool: &str,
    finish_rx: flume::Receiver<ToolCallReply>,
) -> ToolCallReply {
    let (cancel, has_jobs) = {
        let cell = lock_cell(&handle);
        (cell.cancel.clone(), !cell.jobs.is_empty())
    };

    if !has_jobs {
        lua.gc_collect().ok();
        smol::Timer::after(DISPATCH_POLL_INTERVAL).await;
        return match finish_rx.try_recv() {
            Ok(reply) => reply,
            _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
        };
    }

    let timed_out = || {
        lock_cell(&handle)
            .deadline
            .get()
            .is_some_and(|d| Instant::now() > d)
    };
    let mut event_buf = Vec::new();

    loop {
        if cancel.is_cancelled() {
            return ToolCallReply::err(CANCELLED_MSG);
        }
        if timed_out() {
            return timeout_reply(&handle, plugin, tool);
        }

        match finish_rx.try_recv() {
            Ok(reply) => return reply,
            Err(flume::TryRecvError::Disconnected) => {
                return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
            }
            Err(flume::TryRecvError::Empty) => {}
        }

        lock_cell(&handle).jobs.drain_events(&mut event_buf);

        if event_buf.is_empty() {
            let has_alive = lock_cell(&handle).jobs.has_alive_jobs();
            if !has_alive {
                smol::Timer::after(DISPATCH_POLL_INTERVAL).await;
                return match finish_rx.try_recv() {
                    Ok(reply) => reply,
                    _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
                };
            }
            smol::Timer::after(DISPATCH_POLL_INTERVAL).await;
            continue;
        }

        for (job_id, event) in event_buf.drain(..) {
            if let Err(e) = deliver_job_event(lua, job_id, &event) {
                return ToolCallReply::err(format!("job callback error: {}", strip_traceback(&e)));
            }
        }
    }
}

fn strip_traceback(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { cause, .. } => {
            let mut inner = cause;
            while let mlua::Error::CallbackError { cause, .. } = inner.as_ref() {
                inner = cause;
            }
            inner.to_string()
        }
        other => other.to_string(),
    }
}

/// The error message format is load-bearing: the bash plugin's `restore`
/// parses it to re-render the timeout sentinel on session reload.
fn timeout_reply(handle: &TaskHandle, plugin: &str, tool: &str) -> ToolCallReply {
    let secs = lock_cell(handle).deadline_secs.get().unwrap_or(0);
    let live_buf = resolve_root_buf(handle);
    let qualified = if plugin == tool || plugin.is_empty() {
        tool.to_owned()
    } else {
        format!("{plugin}.{tool}")
    };

    if let Some(ref buf) = live_buf {
        buf.append(SnapshotLine {
            spans: vec![SnapshotSpan {
                text: format!("Timed out after {secs}s"),
                style: SpanStyle::Named("dim".into()),
            }],
        });
    }

    let mut reply = ToolCallReply::err(format!("tool {qualified} timed out after {secs}s"));
    reply.live_buf = live_buf;
    reply
}

fn run_describe(
    lua: &Lua,
    plugins: &PluginMap,
    plugin: &str,
    tool: &str,
    dctx: &Value,
) -> Option<String> {
    let func: Function = {
        let plugins_ref = plugins.borrow();
        let key = plugins_ref.get(plugin)?.get(tool)?.describe.as_ref()?;
        lua.registry_value(key).ok()?
    };
    let arg = match json_to_lua(lua, dctx) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "describe dctx conversion failed");
            return None;
        }
    };
    // Runs inline on the dispatcher: without its own scope it executes under
    // whatever handle a parked coroutine left installed, and that task's
    // cancel/deadline would kill the callback (see TaskScope::detached).
    let _scope = TaskScope::detached(lua);
    match func.call::<String>(arg) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "describe callback failed");
            None
        }
    }
}

/// Sends no `ToolSnapshot` on completion: the preview buf must stay live so
/// the UI keeps polling it until the handler's own `LiveToolBuf` takes over.
async fn run_tool_start(
    lua: &Lua,
    func: Function,
    tool: &str,
    input: Value,
    live: LiveCtx,
    ctx: Box<LuaCtx>,
) {
    let scope = TaskScope::new(lua, TaskCell::new(ctx.cancel.clone(), None, Some(live)));
    let run = async {
        let input_lua = json_to_lua(lua, &input)?;
        let ctx_ud = lua.create_userdata(*ctx)?;
        let thread = lua.create_thread(func)?;
        thread.into_async::<LuaValue>((input_lua, ctx_ud))?.await
    };
    if let Err(e) = scope.scope_future(run).await {
        tracing::warn!(tool, error = %e, "start callback failed");
    }
}

/// Two layers of deadline enforcement: the watchdog interrupt catches
/// tight CPU loops, the dispatch loop catches I/O waits.
#[allow(clippy::too_many_arguments)]
async fn run_tool_call(
    lua: Lua,
    plugin: Arc<str>,
    tool: Arc<str>,
    input: Value,
    mut ctx: Box<LuaCtx>,
    deadline: Option<Instant>,
    live: Option<LiveCtx>,
    live_tasks: LiveTasks,
    warm_tools: WarmTools,
    plugins: PluginMap,
    shutdown: Arc<AtomicBool>,
) -> ToolCallReply {
    let handler: Function = {
        let plugins_ref = plugins.borrow();
        let Some(keys) = plugins_ref.get(&*plugin) else {
            return ToolCallReply::err(format!("plugin not loaded: {plugin}"));
        };
        let Some(tool_keys) = keys.get(&*tool) else {
            return ToolCallReply::err(format!("tool not found: {tool}"));
        };
        match lua.registry_value(&tool_keys.handler) {
            Ok(f) => f,
            Err(e) => return ToolCallReply::err(strip_traceback(&e)),
        }
    };
    if shutdown.load(Ordering::Acquire) {
        return ToolCallReply::err("plugin host shutting down");
    }

    let (finish_tx, finish_rx) = flume::bounded::<ToolCallReply>(1);
    ctx.finish_tx = Some(finish_tx);
    let cancel = ctx.cancel.clone();

    let input_lua = match json_to_lua(&lua, &input) {
        Ok(v) => v,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };
    let live_sink = ctx.agent().and_then(|a| a.live_sink.clone());
    let ctx_ud = match lua.create_userdata(*ctx) {
        Ok(u) => u,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };

    let thread = match lua.create_thread(handler) {
        Ok(t) => t,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };
    let live_id = live.as_ref().map(|l| l.tool_use_id.clone());
    let mut cell = TaskCell::new(cancel, deadline, live);
    cell.live_sink = live_sink;
    let scope = TaskScope::new(&lua, cell);
    let handle = Arc::clone(scope.handle());

    let async_thread = match thread.into_async::<LuaValue>((input_lua, ctx_ud)) {
        Ok(at) => at,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };
    if let Some(id) = &live_id {
        live_tasks
            .borrow_mut()
            .insert(id.clone(), Arc::clone(&handle));
    }

    let call_future = scope.scope_future(async {
        let handler_result = {
            let deadline = lock_cell(&handle).deadline.get();
            match deadline {
                Some(dl) => {
                    futures_lite::future::race(async_thread, async {
                        smol::Timer::at(dl).await;
                        Err(mlua::Error::runtime("timeout"))
                    })
                    .await
                }
                None => async_thread.await,
            }
        };
        match handler_result {
            Ok(LuaValue::Nil) => {
                let (live, sink) = {
                    let cell = lock_cell(&handle);
                    (cell.live.clone(), cell.live_sink.clone())
                };
                if let Some(buf) = resolve_root_buf(&handle) {
                    if let Some(live) = live {
                        let _ = live.event_tx.send(maki_agent::AgentEvent::LiveToolBuf {
                            id: live.tool_use_id.clone(),
                            body: Arc::clone(&buf),
                        });
                    }
                    if let Some(sink) = sink {
                        let _ = sink.send(ToolLive::Buf(buf));
                    }
                }
                dispatch_async(&lua, Arc::clone(&handle), &plugin, &tool, finish_rx).await
            }
            Ok(val) => {
                if let Some(buf) = crate::api::ui::buf::buf_from_reply(&val) {
                    lock_cell(&handle).root_buf = Some(buf);
                }
                ToolCallReply::from_lua_value(&lua, &val)
            }
            Err(e) => ToolCallReply::err(strip_traceback(&e)),
        }
    });

    // `tool.rs` timeout is the absolute backstop; the dispatch loop
    // and watchdog interrupt enforce the per-plugin deadline from TaskCell.
    let reply = call_future.await;
    if let Some(id) = &live_id {
        live_tasks.borrow_mut().remove(id);
        // Best-effort cache: any tool with a root buf can serve clicks.
        // Warming a tool the UI never watches is harmless because its
        // clicks arrive as restore requests, which evict the entry.
        if let Some(root) = resolve_root_buf(&handle) {
            // A fresh cell, because the original's cancel token and
            // deadline are stale: the watchdog interrupt would use them to
            // kill warm clicks.
            let mut cell = TaskCell::new(CancelToken::none(), None, None);
            cell.root_buf = Some(root);
            let mut warm = warm_tools.borrow_mut();
            warm.push_back(WarmTool {
                id: id.clone(),
                handle: Arc::new(Mutex::new(cell)),
                _claim: scope.bufs_claim(),
            });
            if warm.len() > WARM_TOOL_CAP {
                warm.pop_front();
            }
        }
    }
    drop(scope);
    reply
}

pub(crate) struct LuaThread {
    pub tx: flume::Sender<Request>,
    pub prio_tx: flume::Sender<Request>,
    pub join: Option<JoinHandle<()>>,
    pub shutdown: Arc<AtomicBool>,
    pub command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: crate::api::util::command::HintReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
}

/// Lua lives on its own OS thread (no Send needed). `smol::block_on`
/// drives async, load/clear requests wait for in-flight tools.
pub fn spawn(
    registry: Arc<ToolRegistry>,
    bundled_dirs: &'static [&'static Dir<'static>],
    jit: bool,
) -> Result<LuaThread, PluginError> {
    let (tx, rx) = flume::unbounded::<Request>();
    let (prio_tx, prio_rx) = flume::unbounded::<Request>();
    let tx_clone = tx.clone();
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let (init_tx, init_rx) = flume::bounded::<Result<(), PluginError>>(1);
    let (ui_action_tx, ui_action_rx) = flume::unbounded::<UiAction>();
    let (command_writer, command_reader) = LuaCommandWriter::new();
    let (keymap_writer, keymap_reader) = KeymapWriter::new();
    let (hint_writer, hint_reader) = HintWriter::new();

    let handle = thread::Builder::new()
        .name("maki-lua".to_owned())
        .spawn(move || {
            let mut rt = match LuaRuntime::new(
                registry,
                tx_clone,
                shutdown_thread,
                bundled_dirs,
                Some(ui_action_tx),
                command_writer,
                keymap_writer,
                hint_writer,
                jit,
            ) {
                Ok(r) => {
                    let _ = init_tx.send(Ok(()));
                    r
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let ex = Rc::new(smol::LocalExecutor::new());
            let gate = Rc::new(InflightGate::new(rt.lua.clone()));
            let restores = Rc::new(RestoreTracker::default());
            let spawn_rx = rt
                .lua
                .app_data_ref::<SpawnQueue>()
                .expect("spawn queue installed at init")
                .rx
                .clone();

            smol::block_on(ex.run(async {
                loop {
                    while let Ok(task) = spawn_rx.try_recv() {
                        spawn_async_task(&rt.lua, &ex, &gate, task);
                    }
                    // Biased: user-initiated requests (commands, keybinds) jump
                    // ahead of bulk work like session restores so the UI stays
                    // snappy, and queued `maki.async.run` tasks jump ahead of
                    // plain requests.
                    let next = smol::future::or(
                        async { prio_rx.recv_async().await.map(Some) },
                        smol::future::or(
                            async {
                                let task = spawn_rx.recv_async().await?;
                                spawn_async_task(&rt.lua, &ex, &gate, task);
                                Ok(None)
                            },
                            async { rx.recv_async().await.map(Some) },
                        ),
                    )
                    .await;
                    let msg = match next {
                        Ok(Some(m)) => m,
                        Ok(None) => {
                            smol::future::yield_now().await;
                            continue;
                        }
                        Err(_) => break,
                    };
                    match msg {
                        Request::Shutdown => break,
                        Request::LoadSource {
                            name,
                            source,
                            plugin_dir,
                            permissions,
                            opts,
                            reply,
                        } => {
                            drain_barrier(&rt.lua, &ex, &gate, &spawn_rx).await;
                            let res = rt.load_source(Arc::clone(&name), &source, plugin_dir, &permissions, opts, None).await;
                            let _ = reply.send(res);
                        }
                        Request::CallTool {
                            plugin,
                            tool,
                            input,
                            ctx,
                            deadline,
                            reply,
                            live,
                        } => {
                            let lua = rt.lua.clone();
                            let plugins = Rc::clone(&rt.plugins);
                            let live_tasks = Rc::clone(&rt.live_tasks);
                            let warm_tools = Rc::clone(&rt.warm_tools);
                            let shutdown_ref = Arc::clone(&rt.shutdown);
                            let g = Rc::clone(&gate);
                            ex.spawn(async move {
                                let _gate_guard = g.acquire().await;
                                let res = run_tool_call(
                                    lua.clone(),
                                    plugin,
                                    tool,
                                    input,
                                    ctx,
                                    deadline,
                                    live,
                                    live_tasks,
                                    warm_tools,
                                    plugins,
                                    shutdown_ref,
                                )
                                .await;
                                let _ = reply.send(res);
                            })
                            .detach();
                        }
                        Request::ClearPlugin { plugin, reply } => {
                            drain_barrier(&rt.lua, &ex, &gate, &spawn_rx).await;
                            rt.clear_plugin(&plugin);
                            let _ = reply.send(());
                        }
                        Request::RunCommand {
                            plugin,
                            command,
                            args,
                        } => {
                            let handler_fn =
                                rt.lua.app_data_ref::<CommandHandlerMap>().and_then(|m| {
                                    let entry = m.get(&plugin)?.get(&command)?;
                                    rt.lua.registry_value::<Function>(&entry.handler).ok()
                                });
                            if let Some(func) = handler_fn {
                                let lua = rt.lua.clone();
                                ex.spawn(async move {
                                    let run = async {
                                        let thread = lua.create_thread(func)?;
                                        thread.into_async::<()>(args)?.await
                                    };
                                    if let Err(e) = run_detached(&lua, run).await {
                                        tracing::warn!(plugin = %plugin, command = %command, error = %e, "command handler failed");
                                    }
                                })
                                .detach();
                            }
                        }
                        Request::ComputeHeader {
                            plugin,
                            tool,
                            input,
                            reply,
                        } => {
                            let res =
                                compute_header(&rt.lua, &rt.plugins, &plugin, &tool, input).await;
                            let _ = reply.send(res);
                        }
                        Request::ComputePermissionScopes {
                            plugin,
                            tool,
                            input,
                            reply,
                        } => {
                            let res = rt.compute_permission_scopes(&plugin, &tool, input).await;
                            let _ = reply.send(res);
                        }
                        Request::RunInitLua {
                            source,
                            source_name,
                            plugin_dir,
                            reply,
                        } => {
                            drain_barrier(&rt.lua, &ex, &gate, &spawn_rx).await;
                            let res = rt.run_init_lua(&source, &source_name, plugin_dir).await;
                            let _ = reply.send(res);
                        }
                        Request::CollectPromptSlots { reply } => {
                            let slots = rt.collect_prompt_slots().await;
                            let _ = reply.send(slots);
                        }
                        Request::CollectPluginOptions { reply } => {
                            let _ = reply.send(collect_plugin_options(&rt.lua));
                        }
                        Request::RestoreToolAsync { item, event_tx } => {
                            spawn_restore(&ex, &gate, &restores, &rt, item, event_tx);
                        }
                        Request::RestoreComplete { flag } => {
                            restores.complete(flag);
                        }
                        Request::ClickTool {
                            tool_use_id,
                            row,
                            fallback,
                        } => {
                            let handle = rt
                                .live_tasks
                                .borrow()
                                .get(&tool_use_id)
                                .map(Arc::clone)
                                .or_else(|| {
                                    rt.warm_tools
                                        .borrow()
                                        .iter()
                                        .find(|w| w.id == tool_use_id)
                                        .map(|w| Arc::clone(&w.handle))
                                });
                            let func = handle
                                .as_ref()
                                .and_then(resolve_root_buf)
                                .and_then(|root| crate::api::ui::buf::click_fn(&root));
                            let (Some(handle), Some(func)) = (handle, func) else {
                                // No handle, or a buf without a click handler
                                // (some plugins wire clicks only in restore):
                                // either way the fallback restore serves it.
                                if let Some(fb) = fallback {
                                    spawn_restore(
                                        &ex, &gate, &restores, &rt, fb.item, fb.event_tx,
                                    );
                                } else {
                                    tracing::debug!(tool_use_id, "unhandled click ignored");
                                }
                                continue;
                            };
                            let lua = rt.lua.clone();
                            let g = Rc::clone(&gate);
                            let arg = match rt.lua.create_table() {
                                Ok(t) => {
                                    let _ = t.set("row", row);
                                    LuaValue::Table(t)
                                }
                                Err(_) => LuaValue::Nil,
                            };
                            ex.spawn(async move {
                                let _gate_guard = g.acquire().await;
                                let call = ScopedFuture {
                                    lua: lua.clone(),
                                    handle,
                                    inner: func.call_async::<()>(arg),
                                };
                                if let Err(e) = call.await {
                                    tracing::warn!(tool_use_id, error = %e, "live click failed");
                                }
                            })
                            .detach();
                        }
                        Request::FireAutocmd { event, data } => {
                            let data = json_to_lua(&rt.lua, &data).unwrap_or(LuaValue::Nil);
                            crate::api::autocmd::dispatch(&rt.lua, &event, None, data);
                            if event == TURN_END_EVENT {
                                rt.lua.gc_collect().ok();
                            }
                        }
                        Request::Describe {
                            plugin,
                            tool,
                            dctx,
                            reply,
                        } => {
                            let _ = reply
                                .send(run_describe(&rt.lua, &rt.plugins, &plugin, &tool, &dctx));
                        }
                        Request::StartTool {
                            plugin,
                            tool,
                            input,
                            live,
                            ctx,
                            reply,
                        } => {
                            let func = {
                                let plugins = rt.plugins.borrow();
                                plugins
                                    .get(&*plugin)
                                    .and_then(|p| p.get(&*tool))
                                    .and_then(|tk| tk.start.as_ref())
                                    .and_then(|key| rt.lua.registry_value::<Function>(key).ok())
                            };
                            let Some(func) = func else {
                                let _ = reply.send(());
                                continue;
                            };
                            let lua = rt.lua.clone();
                            let g = Rc::clone(&gate);
                            ex.spawn(async move {
                                let _gate_guard = g.acquire().await;
                                run_tool_start(&lua, func, &tool, input, live, ctx).await;
                                let _ = reply.send(());
                            })
                            .detach();
                        }
                        Request::RunKeybindCallback { id } => {
                            let func = rt.lua.app_data_ref::<KeymapStore>().and_then(|store| {
                                let key = store.callback_for_id(id)?;
                                rt.lua.registry_value::<Function>(key).ok()
                            });
                            if let Some(func) = func {
                                let lua = rt.lua.clone();
                                ex.spawn(async move {
                                    if let Err(e) = run_detached(&lua, func.call_async::<()>(())).await {
                                        tracing::warn!(keybind_id = id, error = %e, "keybind callback failed");
                                    }
                                }).detach();
                            }
                        }
                    }
                }
            }));
        })
        .map_err(|e| PluginError::Io {
            path: PathBuf::from("lua-thread"),
            source: e,
        })?;

    init_rx.recv().map_err(|_| PluginError::Lua {
        plugin: "<init>".to_owned(),
        source: mlua::Error::runtime("lua thread exited before init completed"),
    })??;

    Ok(LuaThread {
        tx,
        prio_tx,
        join: Some(handle),
        shutdown,
        command_reader,
        keymap_reader,
        hint_reader,
        ui_action_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tool::ToolCallReply;

    fn make_buf_handle(text: &str) -> BufHandle {
        let buf = Arc::new(maki_agent::SharedBuf::new());
        buf.append(SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        });
        BufHandle::foreign(buf)
    }

    fn test_lua() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(BufferStore::new());
        lua
    }

    #[test]
    fn from_lua_value_plain_string() {
        let lua = test_lua();
        let val = LuaValue::String(lua.create_string("ok").unwrap());
        let reply = ToolCallReply::from_lua_value(&lua, &val);
        assert_eq!(reply.result, Ok("ok".to_string()));
        assert!(reply.snapshot.is_none());
        assert!(reply.header.is_none());
    }

    #[test]
    fn from_lua_value_table_with_body_and_header() {
        let lua = test_lua();
        let body_handle = lua.create_userdata(make_buf_handle("body line")).unwrap();
        let hdr_handle = lua.create_userdata(make_buf_handle("hdr line")).unwrap();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "text").unwrap();
        t.set("body", body_handle).unwrap();
        t.set("header", hdr_handle).unwrap();
        let reply = ToolCallReply::from_lua_value(&lua, &LuaValue::Table(t));
        assert_eq!(reply.result, Ok("text".to_string()));
        assert_eq!(reply.snapshot.unwrap().first_line_text(), "body line");
        assert_eq!(reply.header.unwrap().first_line_text(), "hdr line");
    }

    #[test]
    fn from_lua_value_missing_llm_output_still_extracts_body() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        t.set("body", lua.create_userdata(make_buf_handle("x")).unwrap())
            .unwrap();
        let reply = ToolCallReply::from_lua_value(&lua, &LuaValue::Table(t));
        assert!(reply.result.is_err());
        assert!(reply.snapshot.is_some());
    }

    #[test]
    fn task_scope_clears_jobs_and_bufs_on_drop() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, task_cell(None));
        let handle = Arc::clone(scope.handle());
        lock_cell(&handle).bufs.create_live();
        assert!(lock_cell(&handle).bufs.live_buf().is_some());
        drop(scope);
        assert!(lock_cell(&handle).bufs.live_buf().is_none());
    }

    #[test]
    fn task_scope_drop_clears_buf_handler_slots() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, task_cell(None));
        let handle = with_task_bufs(&lua, |store| store.create());
        let shared = Arc::clone(&handle.buf);
        lua.globals()
            .set("buf", lua.create_userdata(handle.clone()).unwrap())
            .unwrap();
        lua.load(r#"buf:on("click", function() end); buf:on("change", function() hit = true end)"#)
            .exec()
            .unwrap();
        shared.append(SnapshotLine { spans: vec![] });
        assert!(lua.globals().get::<bool>("hit").unwrap());
        assert!(handle.click_fn().is_some());
        drop(scope);
        lua.globals().set("hit", false).unwrap();
        shared.append(SnapshotLine { spans: vec![] });
        assert!(!lua.globals().get::<bool>("hit").unwrap());
        assert!(handle.click_fn().is_none());
    }

    fn task_cell(live: Option<LiveCtx>) -> TaskCell {
        TaskCell::new(CancelToken::none(), None, live)
    }

    #[test]
    fn with_live_ctx_follows_task_live_field() {
        let lua = Lua::new();

        let (tx, _rx) = flume::unbounded();
        let with_live = task_cell(Some(LiveCtx {
            event_tx: maki_agent::EventSender::new(tx, 0),
            tool_use_id: "tool_abc".into(),
        }));

        let scope = TaskScope::new(&lua, task_cell(None));
        assert!(with_live_ctx(&lua, |_| ()).is_none());
        drop(scope);

        let _scope = TaskScope::new(&lua, with_live);
        assert_eq!(
            with_live_ctx(&lua, |ctx| ctx.tool_use_id.clone()).unwrap(),
            "tool_abc"
        );
    }

    fn gate() -> InflightGate {
        InflightGate::new(Lua::new())
    }

    #[test]
    fn inflight_gate_drain_requires_all_decrements() {
        let ex = smol::LocalExecutor::new();
        smol::block_on(ex.run(async {
            let g = Rc::new(gate());
            g.increment();
            g.increment();
            let g2 = Rc::clone(&g);
            let waiter = ex.spawn(async move { g2.drain().await });
            smol::future::yield_now().await;
            assert!(!waiter.is_finished());
            g.decrement();
            smol::future::yield_now().await;
            assert!(!waiter.is_finished());
            g.decrement();
            waiter.await;
        }));
    }

    #[test]
    fn inflight_gate_blocks_at_max_capacity() {
        let ex = smol::LocalExecutor::new();
        smol::block_on(ex.run(async {
            let g = Rc::new(gate());
            for _ in 0..MAX_INFLIGHT_TOOLS {
                g.increment();
            }
            let g2 = Rc::clone(&g);
            let waiter = ex.spawn(async move { g2.wait_below(MAX_INFLIGHT_TOOLS).await });
            smol::future::yield_now().await;
            assert!(!waiter.is_finished());
            g.decrement();
            waiter.await;
        }));
    }

    #[test]
    fn acquire_caps_concurrent_holders_even_when_spawned_in_bulk() {
        let ex = smol::LocalExecutor::new();
        smol::block_on(ex.run(async {
            let g = Rc::new(gate());
            let (release_tx, release_rx) = flume::unbounded::<()>();
            let tasks: Vec<_> = (0..MAX_INFLIGHT_TOOLS + 1)
                .map(|_| {
                    let g = Rc::clone(&g);
                    let release_rx = release_rx.clone();
                    ex.spawn(async move {
                        let _guard = g.acquire().await;
                        release_rx.recv_async().await.ok();
                    })
                })
                .collect();
            for _ in 0..MAX_INFLIGHT_TOOLS + 2 {
                smol::future::yield_now().await;
            }
            assert_eq!(g.count.get(), MAX_INFLIGHT_TOOLS);
            drop(release_tx);
            for t in tasks {
                t.await;
            }
            assert_eq!(g.count.get(), 0);
        }));
    }

    #[test]
    fn extract_restore_reply_userdata_returns_body_only() {
        let lua = test_lua();
        let handle = make_buf_handle("restored line");
        let ud = lua.create_userdata(handle).unwrap();
        let val = LuaValue::UserData(ud);
        let reply = extract_restore_reply(&val).expect("should extract from userdata");
        assert_eq!(reply.body.unwrap().first_line_text(), "restored line");
        assert!(reply.header.is_none());
    }

    #[test]
    fn extract_restore_reply_table_with_body_and_header() {
        let lua = test_lua();
        let body = lua.create_userdata(make_buf_handle("body")).unwrap();
        let header = lua.create_userdata(make_buf_handle("header")).unwrap();
        let t = lua.create_table().unwrap();
        t.set("body", body).unwrap();
        t.set("header", header).unwrap();
        let val = LuaValue::Table(t);
        let reply = extract_restore_reply(&val).unwrap();
        assert_eq!(reply.body.unwrap().first_line_text(), "body");
        assert_eq!(reply.header.unwrap().first_line_text(), "header");
    }

    const SPAWN_QUEUE_NOT_INIT: &str = "spawn queue not initialized";

    fn enqueue_test_lua() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(SpawnQueue::new());
        lua
    }

    fn enqueue_dummy(lua: &Lua) -> RegistryKey {
        let func = lua.create_function(|_, _: ()| Ok(())).unwrap();
        lua.create_registry_value(func).unwrap()
    }

    fn set_active(lua: &Lua, cell: TaskCell) -> TaskScope {
        TaskScope::new(lua, cell)
    }

    #[test]
    fn gate_guard_tracks_count_via_raii() {
        let g = Rc::new(gate());
        let g1 = GateGuard::new(&g);
        let g2 = GateGuard::new(&g);
        assert_eq!(g.count.get(), 2);
        drop(g1);
        assert_eq!(g.count.get(), 1);
        drop(g2);
        assert_eq!(g.count.get(), 0);
    }

    #[test]
    fn enqueue_async_task_missing_spawn_queue_errors() {
        let lua = Lua::new();
        let key = lua
            .create_registry_value(lua.create_function(|_, _: ()| Ok(())).unwrap())
            .unwrap();
        let err = enqueue_async_task(&lua, key).unwrap_err();
        assert!(err.to_string().contains(SPAWN_QUEUE_NOT_INIT));
    }

    #[test]
    fn enqueue_async_task_routes_to_inline_spawn_when_set() {
        let lua = enqueue_test_lua();
        let scope = set_active(&lua, TaskCell::new(CancelToken::none(), None, None));
        lock_cell(scope.handle()).inline_spawn = Some(Vec::new());

        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        assert!(
            lua.app_data_ref::<SpawnQueue>().unwrap().rx.is_empty(),
            "task must not reach the global queue"
        );
        let cell = lock_cell(scope.handle());
        assert_eq!(cell.inline_spawn.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn enqueue_async_task_works_without_task_ctx() {
        let lua = enqueue_test_lua();
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = queue.rx.try_recv().unwrap();
        assert!(queued.live_ctx.is_none());
        assert!(queued.owner.is_none());
    }

    #[test]
    fn enqueue_async_task_inherits_cancel_token() {
        let lua = enqueue_test_lua();
        let (trigger, token) = CancelToken::new();
        let _h = set_active(&lua, TaskCell::new(token, None, None));
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = queue.rx.try_recv().unwrap();
        assert!(!queued.cancel.is_cancelled());
        trigger.cancel();
        assert!(
            queued.cancel.is_cancelled(),
            "async task should inherit parent cancel"
        );
    }

    #[test]
    fn enqueue_async_task_uses_fresh_deadline_regardless_of_parent() {
        let lua = enqueue_test_lua();
        let parent_deadline = Instant::now() - Duration::from_secs(10);
        let _h = set_active(
            &lua,
            TaskCell::new(CancelToken::none(), Some(parent_deadline), None),
        );

        let before = Instant::now();
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let task_deadline = queue.rx.try_recv().unwrap().deadline.unwrap();
        assert!(
            task_deadline > before,
            "async task should get a fresh deadline, not inherit expired parent"
        );
    }

    #[test]
    fn scope_drop_defers_watcher_clear_until_owned_tasks_release() {
        use crate::api::ui::buf::HandlerSlot;

        let lua = enqueue_test_lua();
        let scope = set_active(&lua, TaskCell::new(CancelToken::none(), None, None));
        let handle = Arc::clone(scope.handle());

        let buf = Arc::new(SharedBuf::new());
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        buf.set_on_change(move || f.store(true, Ordering::Release));
        lock_cell(&handle)
            .bufs
            .track(HandlerSlot::Change(Arc::clone(&buf)));

        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();
        drop(scope);

        buf.set_lines(Vec::new());
        assert!(
            fired.load(Ordering::Acquire),
            "watcher must survive scope drop while an owned async task is pending"
        );

        let task = lua
            .app_data_ref::<SpawnQueue>()
            .unwrap()
            .rx
            .try_recv()
            .unwrap();
        drop(task);
        fired.store(false, Ordering::Release);
        buf.set_lines(Vec::new());
        assert!(
            !fired.load(Ordering::Acquire),
            "dropping the last owned task must clear the deferred watcher"
        );
    }

    fn pending_task(lua: &Lua, cancel: CancelToken, deadline: Option<Instant>) -> PendingAsyncTask {
        PendingAsyncTask {
            work_fn: enqueue_dummy(lua),
            cancel,
            deadline,
            live_ctx: None,
            owner: None,
        }
    }

    #[test]
    fn spawn_async_task_skips_cancelled_tasks() {
        let ex = Rc::new(smol::LocalExecutor::new());
        smol::block_on(ex.run(async {
            let lua = enqueue_test_lua();
            let (trigger, token) = CancelToken::new();
            trigger.cancel();

            let g = Rc::new(gate());
            spawn_async_task(&lua, &ex, &g, pending_task(&lua, token, None));
            smol::future::yield_now().await;
            assert_eq!(g.count.get(), 0);
        }));
    }

    fn watchdog_lua(shutdown: bool) -> (Lua, Watchdog) {
        let lua = Lua::new();
        let watchdog = Watchdog::spawn(&lua, Arc::new(AtomicBool::new(shutdown)));
        (lua, watchdog)
    }

    /// Generous vs the ~10ms expected kill; only a broken watchdog gets here.
    const WATCHDOG_TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// `while true do end` only stops if the watchdog kills it, so run it
    /// on a helper thread: a broken watchdog fails the test fast under any
    /// harness (not just nextest's terminate-after) instead of hanging it.
    /// The leaked thread then spins until the test process exits.
    fn hot_loop_expecting_kill(lua: &Lua) -> mlua::Error {
        let f = lua.load("while true do end").into_function().unwrap();
        let (tx, rx) = flume::bounded(1);
        thread::spawn(move || drop(tx.send(f.call::<bool>(()))));
        rx.recv_timeout(WATCHDOG_TEST_TIMEOUT)
            .expect("watchdog never killed the hot loop")
            .unwrap_err()
    }

    /// Runs long enough (50ms) to guarantee several watchdog pokes.
    fn timed_loop(lua: &Lua) -> Function {
        lua.load("local t = os.clock() while os.clock() - t < 0.05 do end return true")
            .into_function()
            .unwrap()
    }

    fn cancelled_handle() -> TaskHandle {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        Arc::new(Mutex::new(TaskCell::new(token, None, None)))
    }

    #[test]
    fn stale_cancelled_handle_aborts_callback_without_fresh_scope() {
        let (lua, _watchdog) = watchdog_lua(false);
        lua.set_app_data::<TaskHandle>(cancelled_handle());
        let err = hot_loop_expecting_kill(&lua);
        assert!(err.to_string().contains(INTERRUPT_CANCELLED_MSG));
    }

    #[test]
    fn fresh_task_scope_shields_callback_from_stale_cancelled_handle() {
        let (lua, _watchdog) = watchdog_lua(false);
        lua.set_app_data::<TaskHandle>(cancelled_handle());

        let scope = TaskScope::detached(&lua);
        let result = timed_loop(&lua).call::<bool>(());
        drop(scope);

        assert!(result.unwrap());
    }

    #[test]
    fn shutdown_flag_aborts_callback_even_with_fresh_scope() {
        let (lua, _watchdog) = watchdog_lua(true);

        let scope = TaskScope::detached(&lua);
        let err = hot_loop_expecting_kill(&lua);
        drop(scope);

        assert!(err.to_string().contains(INTERRUPT_SHUTDOWN_MSG));
    }

    #[test]
    fn jit_busy_loop_killed_at_deadline() {
        let (lua, _watchdog) = watchdog_lua(false);
        apply_jit(&lua, true);

        let deadline = Instant::now() + Duration::from_millis(20);
        let cell = TaskCell::new(CancelToken::none(), Some(deadline), None);
        lua.set_app_data::<TaskHandle>(Arc::new(Mutex::new(cell)));

        let err = hot_loop_expecting_kill(&lua);
        assert!(err.to_string().contains(INTERRUPT_DEADLINE_MSG));
    }

    #[test]
    fn spawn_async_task_runs_and_decrements_gate() {
        let ex = Rc::new(smol::LocalExecutor::new());
        smol::block_on(ex.run(async {
            let lua = enqueue_test_lua();
            let task = pending_task(
                &lua,
                CancelToken::none(),
                Some(Instant::now() + Duration::from_secs(5)),
            );

            let g = Rc::new(gate());
            spawn_async_task(&lua, &ex, &g, task);

            for _ in 0..10 {
                smol::future::yield_now().await;
                if g.count.get() == 0 {
                    return;
                }
            }
            panic!("gate count never reached 0 after draining");
        }));
    }
}
