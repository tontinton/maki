use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use event_listener::Event;

use include_dir::Dir;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::{
    HeaderResult, PermissionScopes, RegistryError, Tool, ToolRegistry, ToolSource,
};
use maki_agent::{BufferSnapshot, SharedBuf};
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Value as LuaValue, VmState};
use serde_json::Value;

use maki_config::RawConfig;

use crate::api::buf::{BufHandle, BufferStore};
use crate::api::command::{CommandHandlerMap, publish_command_snapshot};
use crate::api::command::{LuaCommandReader, LuaCommandWriter, UiAction};
use crate::api::create_maki_global;
use crate::api::ctx::LuaCtx;
use crate::api::fn_api::{JobEvent, JobStore};
use crate::api::setup::ConfigStore;
use crate::api::tool::{LuaTool, PendingTool, PendingTools, ToolCallReply};
use crate::error::PluginError;

const INTERRUPT_MSG: &str = "plugin interrupted: cancelled, deadline exceeded, or shutting down";
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NIL_WITHOUT_FINISH_MSG: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
const MAX_INFLIGHT_TOOLS: usize = 64;
const GC_STEP_INTERVAL: usize = 4;
const INTERRUPT_CANCEL_CHECK_INTERVAL: u32 = 128;
const ASYNC_RUN_DEFAULT_DEADLINE: Duration = Duration::from_secs(60);

pub type LoadResult = Result<(), PluginError>;
pub(crate) type PromptExtraCallbacks = BTreeMap<Arc<str>, RegistryKey>;

/// Load and clear requests drain in-flight tools first so we never
/// mutate a plugin environment while a tool call is still running.
pub enum Request {
    LoadSource {
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
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
    FireBufClick {
        tool_id: String,
        row: u32,
        reply: flume::Sender<Option<ClickReply>>,
    },
    RunCommand {
        plugin: Arc<str>,
        command: Arc<str>,
        args: String,
    },
    CollectPromptExtras {
        reply: flume::Sender<Vec<String>>,
    },
    Shutdown,
    RestoreTool {
        tool: Arc<str>,
        tool_use_id: String,
        output: String,
        input: Value,
        is_error: bool,
        tool_output_lines: maki_config::ToolOutputLines,
        reply: flume::Sender<Option<RestoreReply>>,
    },
}

pub struct RestoreReply {
    pub body: Option<BufferSnapshot>,
    pub header: Option<BufferSnapshot>,
}

pub struct ClickReply {
    pub snapshot: BufferSnapshot,
    pub live_buf: Arc<SharedBuf>,
}

#[derive(Clone)]
pub struct LiveCtx {
    pub event_tx: maki_agent::EventSender,
    pub tool_use_id: String,
}

struct TaskCtx {
    cancel: CancelToken,
    deadline: Option<Instant>,
    jobs: JobStore,
    bufs: BufferStore,
    live: Option<LiveCtx>,
}

impl TaskCtx {
    fn new(cancel: CancelToken, deadline: Option<Instant>, live: Option<LiveCtx>) -> Self {
        Self {
            cancel,
            deadline,
            jobs: JobStore::new(),
            bufs: BufferStore::new(),
            live,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ThreadKey(usize);

impl ThreadKey {
    fn current(lua: &Lua) -> Self {
        Self(lua.current_thread().to_pointer() as usize)
    }
}

/// Keyed by coroutine pointer. Single-threaded, so no locking needed.
type TaskMap = HashMap<ThreadKey, TaskCtx>;

type ClickHandlerMap = HashMap<String, (RegistryKey, Arc<SharedBuf>)>;

pub(crate) fn with_task_jobs<R>(lua: &Lua, f: impl FnOnce(&mut JobStore) -> R) -> Option<R> {
    let key = ThreadKey::current(lua);
    let mut tasks = lua.app_data_mut::<TaskMap>()?;
    let ctx = tasks.get_mut(&key)?;
    Some(f(&mut ctx.jobs))
}

pub(crate) fn with_task_bufs<R>(lua: &Lua, f: impl FnOnce(&mut BufferStore) -> R) -> Option<R> {
    let key = ThreadKey::current(lua);
    let mut tasks = lua.app_data_mut::<TaskMap>()?;
    let ctx = tasks.get_mut(&key)?;
    Some(f(&mut ctx.bufs))
}

pub(crate) fn with_click_handlers<R>(
    lua: &Lua,
    f: impl FnOnce(&mut ClickHandlerMap) -> R,
) -> Option<R> {
    lua.app_data_mut::<ClickHandlerMap>().map(|mut m| f(&mut m))
}

pub(crate) fn with_live_ctx<R>(lua: &Lua, f: impl FnOnce(&LiveCtx) -> R) -> Option<R> {
    let key = ThreadKey::current(lua);
    lua.app_data_ref::<TaskMap>()
        .and_then(|tasks| tasks.get(&key)?.live.as_ref().map(f))
}

pub(crate) fn enqueue_async_task(lua: &Lua, work_fn: RegistryKey) -> Result<(), mlua::Error> {
    let key = ThreadKey::current(lua);
    let (cancel, live_ctx, live_buf) = lua
        .app_data_ref::<TaskMap>()
        .and_then(|m| {
            let ctx = m.get(&key)?;
            Some((
                ctx.cancel.clone(),
                ctx.live.clone(),
                ctx.bufs.live_buf().cloned(),
            ))
        })
        .unwrap_or((CancelToken::none(), None, None));

    let task = PendingAsyncTask {
        work_fn,
        cancel,
        deadline: Some(Instant::now() + ASYNC_RUN_DEFAULT_DEADLINE),
        live_ctx,
        live_buf,
    };

    let queue = lua
        .app_data_ref::<SpawnQueue>()
        .ok_or_else(|| mlua::Error::runtime("spawn queue not initialized"))?;
    queue.borrow_mut().push(task);
    Ok(())
}

struct TaskCleanupGuard {
    lua: Lua,
    key: ThreadKey,
}

impl Drop for TaskCleanupGuard {
    fn drop(&mut self) {
        if let Some(mut task) = self
            .lua
            .app_data_mut::<TaskMap>()
            .and_then(|mut m| m.remove(&self.key))
        {
            task.jobs.kill_all();
            task.jobs.clear(&self.lua);
            task.bufs.clear();
        }
    }
}

fn register_task(lua: &Lua, thread_key: ThreadKey, ctx: TaskCtx) -> TaskCleanupGuard {
    if let Some(mut tasks) = lua.app_data_mut::<TaskMap>() {
        tasks.insert(thread_key, ctx);
    }
    TaskCleanupGuard {
        lua: lua.clone(),
        key: thread_key,
    }
}

/// Caps concurrent coroutines so they don't blow the Lua stack or starve
/// the executor. Also serves as a drain barrier for load/clear ops.
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

    async fn drain(&self) {
        self.wait_below(1).await;
    }
}

struct GateGuard<'a> {
    gate: &'a InflightGate,
}

impl<'a> GateGuard<'a> {
    fn new(gate: &'a InflightGate) -> Self {
        gate.increment();
        Self { gate }
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.decrement();
    }
}

pub(crate) struct PendingAsyncTask {
    pub work_fn: RegistryKey,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
    pub live_ctx: Option<LiveCtx>,
    pub live_buf: Option<Arc<SharedBuf>>,
}

pub(crate) type SpawnQueue = RefCell<Vec<PendingAsyncTask>>;

fn drain_spawn_queue(lua: &Lua, ex: &Rc<smol::LocalExecutor<'_>>, gate: &Rc<InflightGate>) {
    let tasks: Vec<PendingAsyncTask> = {
        let Some(queue) = lua.app_data_ref::<SpawnQueue>() else {
            return;
        };
        let mut q = queue.borrow_mut();
        if q.is_empty() {
            return;
        }
        q.drain(..).collect()
    };

    for task in tasks {
        if task.cancel.is_cancelled() {
            lua.remove_registry_value(task.work_fn).ok();
            continue;
        }

        let lua = lua.clone();
        let g = Rc::clone(gate);
        let ex2 = Rc::clone(ex);

        ex.spawn(async move {
            let _gate_guard = GateGuard::new(&g);

            let run = async {
                let work_fn: Function = lua.registry_value(&task.work_fn)?;
                let thread = lua.create_thread(work_fn)?;
                let thread_key = ThreadKey(thread.to_pointer() as usize);
                let _cleanup = register_task(
                    &lua,
                    thread_key,
                    TaskCtx::new(task.cancel.clone(), task.deadline, task.live_ctx.clone()),
                );
                let async_thread = thread.into_async::<LuaValue>(())?;
                match task.deadline {
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

            let result = run.await;
            if let Err(e) = &result {
                tracing::debug!(error = %e, "async.run: task failed");
            }

            if let Some(ref live) = task.live_ctx {
                if let Some(ref buf) = task.live_buf {
                    let _ = live.event_tx.send(maki_agent::AgentEvent::ToolSnapshot {
                        id: live.tool_use_id.clone(),
                        snapshot: buf.take(),
                    });
                }
            }

            lua.remove_registry_value(task.work_fn).ok();
            drain_spawn_queue(&lua, &ex2, &g);
        })
        .detach();
    }
}

struct ToolKeys {
    handler: RegistryKey,
    header: Option<RegistryKey>,
    restore: Option<RegistryKey>,
    permission_scopes: Option<RegistryKey>,
}

type PluginMap = Rc<RefCell<HashMap<Arc<str>, HashMap<Arc<str>, ToolKeys>>>>;

/// Plugins run sandboxed: `require`/`io`/`package` are removed, and
/// `os`/`debug` go through Luau's built-in restrictions.
struct LuaRuntime {
    lua: Lua,
    pending: PendingTools,
    plugins: PluginMap,
    registry: Arc<ToolRegistry>,
    tx: flume::Sender<Request>,
    shutdown: Arc<AtomicBool>,
    bundled_dirs: &'static [&'static Dir<'static>],
    ui_action_tx: Option<flume::Sender<UiAction>>,
}

impl LuaRuntime {
    fn new(
        registry: Arc<ToolRegistry>,
        tx: flume::Sender<Request>,
        shutdown: Arc<AtomicBool>,
        bundled_dirs: &'static [&'static Dir<'static>],
        ui_action_tx: Option<flume::Sender<UiAction>>,
        command_writer: LuaCommandWriter,
    ) -> Result<Self, PluginError> {
        let lua = Lua::new();
        let pending: PendingTools = Arc::new(Mutex::new(Vec::new()));

        let interrupt_shutdown = Arc::clone(&shutdown);
        let interrupt_lua = lua.clone();
        let interrupt_tick = Cell::new(0u32);
        lua.set_interrupt(move |_| {
            if interrupt_shutdown.load(Ordering::Acquire) {
                return Err(mlua::Error::runtime(INTERRUPT_MSG));
            }
            let tick = interrupt_tick.get().wrapping_add(1);
            interrupt_tick.set(tick);
            if tick % INTERRUPT_CANCEL_CHECK_INTERVAL != 0 {
                return Ok(VmState::Continue);
            }
            // current_thread() pushes onto the Lua stack which is unsafe in
            // interrupts (luau-lang/luau#446). Rate-limit this check to avoid
            // exhausting the auxiliary stack in long-running coroutines.
            let thread = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                interrupt_lua.current_thread()
            }));
            let Ok(thread) = thread else {
                return Ok(VmState::Continue);
            };
            let key = ThreadKey(thread.to_pointer() as usize);
            let cancelled = interrupt_lua
                .app_data_ref::<TaskMap>()
                .and_then(|m| {
                    let ctx = m.get(&key)?;
                    let cancel = ctx.cancel.is_cancelled();
                    let expired = ctx.deadline.is_some_and(|d| Instant::now() > d);
                    Some(cancel || expired)
                })
                .unwrap_or(false);
            if cancelled {
                return Err(mlua::Error::runtime(INTERRUPT_MSG));
            }
            Ok(VmState::Continue)
        });

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

        lua.set_app_data(TaskMap::new());
        lua.set_app_data(ClickHandlerMap::new());
        lua.set_app_data(CommandHandlerMap::new());
        lua.set_app_data(SpawnQueue::default());
        lua.set_app_data(command_writer);
        lua.set_app_data(PromptExtraCallbacks::default());

        Ok(Self {
            lua,
            pending,
            plugins: Rc::new(RefCell::new(HashMap::new())) as PluginMap,
            registry,
            tx,
            shutdown,
            bundled_dirs,
            ui_action_tx,
        })
    }

    fn drop_plugin_keys(&mut self, name: &str) {
        if let Some(keys) = self.plugins.borrow_mut().remove(name) {
            for (_, tk) in keys {
                if let Err(e) = self.lua.remove_registry_value(tk.handler) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua handler key");
                }
                if let Some(sk) = tk.header {
                    if let Err(e) = self.lua.remove_registry_value(sk) {
                        tracing::warn!(plugin = name, error = %e, "failed to drop lua header key");
                    }
                }
                if let Some(sk) = tk.permission_scopes {
                    if let Err(e) = self.lua.remove_registry_value(sk) {
                        tracing::warn!(plugin = name, error = %e, "failed to drop lua permission_scopes key");
                    }
                }
            }
        }
        if let Some(mut cmd_map) = self.lua.app_data_mut::<CommandHandlerMap>() {
            if let Some(cmds) = cmd_map.remove(name) {
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
        }
        if let Some(mut extras) = self.lua.app_data_mut::<PromptExtraCallbacks>() {
            if let Some(key) = extras.remove(name) {
                if let Err(e) = self.lua.remove_registry_value(key) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop prompt extra key");
                }
            }
        }
    }

    async fn collect_prompt_extras(&self) -> Vec<String> {
        let callbacks: Vec<(Arc<str>, Function)> = {
            let Some(map) = self.lua.app_data_ref::<PromptExtraCallbacks>() else {
                return Vec::new();
            };
            map.iter()
                .filter_map(|(plugin, key)| {
                    let func = self.lua.registry_value::<Function>(key).ok()?;
                    Some((Arc::clone(plugin), func))
                })
                .collect()
        };
        let mut extras = Vec::new();
        for (plugin, func) in &callbacks {
            let result: mlua::Result<LuaValue> = async {
                let thread = self.lua.create_thread(func.clone())?;
                thread.into_async::<LuaValue>(())?.await
            }
            .await;
            match result {
                Ok(LuaValue::String(s)) => extras.push(s.to_string_lossy()),
                Ok(LuaValue::Nil) => {}
                Ok(_) => {
                    tracing::warn!(plugin = %plugin, "prompt extra callback returned non-string")
                }
                Err(e) => {
                    tracing::warn!(plugin = %plugin, error = %e, "prompt extra callback failed")
                }
            }
        }
        extras
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
            if let Some(sk) = t.header_key {
                if let Err(e) = self.lua.remove_registry_value(sk) {
                    tracing::warn!(error = %e, "failed to drop lua header key on rollback");
                }
            }
            if let Some(sk) = t.permission_scopes_key {
                if let Err(e) = self.lua.remove_registry_value(sk) {
                    tracing::warn!(error = %e, "failed to drop lua permission_scopes key on rollback");
                }
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

            if let Ok(cached) = loaded.get::<LuaValue>(modname.as_str()) {
                if cached != LuaValue::Nil {
                    return Ok(cached);
                }
            }

            if loading.get::<bool>(modname.as_str()).unwrap_or(false) {
                return Ok(LuaValue::Boolean(true));
            }

            loading.set(modname.as_str(), true)?;

            let rel_path = modname.replace('.', "/") + ".lua";

            let source_str: Result<Option<String>, mlua::Error> = (|| {
                for dir in bundled_dirs {
                    if let Some(file) = dir.get_file(&rel_path) {
                        if let Some(contents) = file.contents_utf8() {
                            return Ok(Some(contents.to_owned()));
                        }
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

    async fn load_source(
        &mut self,
        name: Arc<str>,
        source: &str,
        plugin_dir: Option<PathBuf>,
    ) -> LoadResult {
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
        )
        .map_err(|e| PluginError::Lua {
            plugin: name.to_string(),
            source: e,
        })?;

        let env = self
            .build_env(maki, require_root)
            .map_err(|e| PluginError::Lua {
                plugin: name.to_string(),
                source: e,
            })?;

        self.drop_plugin_keys(&name);

        let exec_result = self
            .lua
            .load(source)
            .set_name(name.as_ref())
            .set_environment(env)
            .exec_async()
            .await;

        if let Err(e) = exec_result {
            let stale = self.drain_pending();
            self.discard_pending(stale);
            self.drop_plugin_keys(&name);
            return Err(PluginError::Lua {
                plugin: name.to_string(),
                source: e,
            });
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
                    tx: self.tx.clone(),
                    plugin: Arc::clone(&name),
                    has_header_fn: t.header_key.is_some(),
                    permission_scope_kind: t.permission_scope_kind.clone(),
                    timeout: t.timeout,
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
                        permission_scopes: t.permission_scopes_key,
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
    }

    /// Registers a TaskCtx so `maki.ui.buf()` works inside the handler.
    fn compute_header(&self, plugin: &str, tool: &str, input: Value) -> HeaderResult {
        let plugins = self.plugins.borrow();
        let Some(tk) = plugins.get(plugin).and_then(|p| p.get(tool)) else {
            return HeaderResult::plain(tool.to_string());
        };
        let Some(key) = tk.header.as_ref() else {
            return HeaderResult::plain(tool.to_string());
        };
        let func = match self.lua.registry_value::<Function>(key) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "header fn registry lookup failed");
                return HeaderResult::plain(tool.to_string());
            }
        };
        let input_lua = match self.lua.to_value(&input) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "header fn input serialization failed");
                return HeaderResult::plain(tool.to_string());
            }
        };

        let key = ThreadKey::current(&self.lua);
        let task_ctx = TaskCtx::new(CancelToken::none(), None, None);
        let Some(mut tasks) = self.lua.app_data_mut::<TaskMap>() else {
            return HeaderResult::plain(tool.to_string());
        };
        tasks.insert(key, task_ctx);
        drop(tasks);

        let _cleanup = TaskCleanupGuard {
            lua: self.lua.clone(),
            key,
        };

        match func.call::<LuaValue>(input_lua) {
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

    async fn restore_tool(
        &self,
        tool: &str,
        tool_use_id: &str,
        output: &str,
        input: Value,
        is_error: bool,
        tool_output_lines: maki_config::ToolOutputLines,
    ) -> Option<RestoreReply> {
        let func = {
            let plugins = self.plugins.borrow();
            let tk = plugins.values().find_map(|tools| tools.get(tool))?;
            let key = tk.restore.as_ref()?;
            self.lua.registry_value::<Function>(key).ok()?
        };
        let input_lua = self.lua.to_value(&input).ok()?;
        let thread = self.lua.create_thread(func).ok()?;
        let thread_key = ThreadKey(thread.to_pointer() as usize);

        let (dummy_tx, _) = flume::unbounded();
        let task_ctx = TaskCtx::new(
            CancelToken::none(),
            None,
            Some(LiveCtx {
                event_tx: maki_agent::EventSender::new(dummy_tx, 0),
                tool_use_id: tool_use_id.to_owned(),
            }),
        );
        let _cleanup = register_task(&self.lua, thread_key, task_ctx);

        let ctx_ud = self
            .lua
            .create_userdata(crate::api::ctx::RestoreCtx { tool_output_lines })
            .ok()?;
        let ret = thread
            .into_async::<LuaValue>((input_lua, output, is_error, ctx_ud))
            .ok()?
            .await
            .inspect_err(|e| tracing::warn!(tool, error = %e, "restore callback failed"))
            .ok()?;

        extract_restore_reply(&ret)
    }

    fn compute_permission_scopes(
        &self,
        plugin: &str,
        tool: &str,
        input: Value,
    ) -> Option<PermissionScopes> {
        let plugins = self.plugins.borrow();
        let tk = plugins.get(plugin)?.get(tool)?;
        let key = tk.permission_scopes.as_ref()?;
        let func = match self.lua.registry_value::<Function>(key) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "failed to resolve permission_scopes callback");
                return None;
            }
        };
        let lua_input = match self.lua.to_value(&input) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "failed to convert input for permission_scopes");
                return None;
            }
        };
        let result: LuaValue = match func.call(lua_input) {
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

    fn run_init_lua(
        &self,
        source: &str,
        source_name: &str,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let map_err = |e: mlua::Error| PluginError::Lua {
            plugin: source_name.to_owned(),
            source: e,
        };

        let config_store: ConfigStore = Arc::new(Mutex::new(None));
        let require_root = plugin_dir.as_ref().map(|d| d.join("lua"));

        let setup_fn = crate::api::setup::create_setup_fn(&self.lua, Arc::clone(&config_store))
            .map_err(&map_err)?;
        let maki = self.lua.create_table().map_err(&map_err)?;
        maki.set("setup", setup_fn).map_err(&map_err)?;
        maki.set(
            "fs",
            crate::api::fs::create_fs_table(&self.lua).map_err(&map_err)?,
        )
        .map_err(&map_err)?;
        maki.set(
            "json",
            crate::api::json::create_json_table(&self.lua).map_err(&map_err)?,
        )
        .map_err(&map_err)?;
        maki.set(
            "uv",
            crate::api::uv::create_uv_table(&self.lua).map_err(&map_err)?,
        )
        .map_err(&map_err)?;

        let env = self.build_env(maki, require_root).map_err(&map_err)?;

        self.lua
            .load(source)
            .set_name(source_name)
            .set_environment(env)
            .exec()
            .map_err(&map_err)?;

        let raw = config_store.lock().unwrap().take();
        Ok(raw)
    }
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

/// A handler returning nil means "I went async". This loop polls job
/// events until the plugin calls `ctx:finish()` or every job dies.
async fn dispatch_async(
    lua: &Lua,
    key: ThreadKey,
    finish_rx: flume::Receiver<ToolCallReply>,
) -> ToolCallReply {
    let task_state = lua.app_data_ref::<TaskMap>().and_then(|m| {
        let ctx = m.get(&key)?;
        Some((ctx.cancel.clone(), ctx.deadline, !ctx.jobs.is_empty()))
    });

    let Some((cancel, deadline, has_jobs)) = task_state else {
        return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
    };

    if !has_jobs {
        lua.gc_collect().ok();
        smol::Timer::after(DISPATCH_POLL_INTERVAL).await;
        return match finish_rx.try_recv() {
            Ok(reply) => reply,
            _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
        };
    }

    let is_cancelled = || cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() > d);
    let mut event_buf = Vec::new();

    loop {
        if is_cancelled() {
            return ToolCallReply::err("cancelled");
        }

        match finish_rx.try_recv() {
            Ok(reply) => return reply,
            Err(flume::TryRecvError::Disconnected) => {
                return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
            }
            Err(flume::TryRecvError::Empty) => {}
        }

        if let Some(m) = lua.app_data_ref::<TaskMap>() {
            if let Some(ctx) = m.get(&key) {
                ctx.jobs.drain_events(&mut event_buf);
            }
        }

        if event_buf.is_empty() {
            let has_alive = lua
                .app_data_ref::<TaskMap>()
                .and_then(|m| Some(m.get(&key)?.jobs.has_alive_jobs()))
                .unwrap_or(false);

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
            let is_exit = matches!(event, JobEvent::Exit(_));

            let callback = lua.app_data_ref::<TaskMap>().and_then(|m| {
                let ctx = m.get(&key)?;
                ctx.jobs
                    .callback_key(job_id, &event)
                    .and_then(|k| lua.registry_value::<Function>(k).ok())
            });

            if let Some(func) = callback {
                let arg: LuaValue = match &event {
                    JobEvent::Stdout(line) | JobEvent::Stderr(line) => lua
                        .create_string(line)
                        .map(LuaValue::String)
                        .unwrap_or(LuaValue::Nil),
                    JobEvent::Exit(code) => LuaValue::Integer(*code as i64),
                };
                if let Err(e) = func.call::<()>((job_id, arg)) {
                    return ToolCallReply::err(format!("job callback error: {e}"));
                }
            }

            if is_exit {
                if let Some(mut tasks) = lua.app_data_mut::<TaskMap>() {
                    if let Some(ctx) = tasks.get_mut(&key) {
                        ctx.jobs.mark_dead(job_id);
                    }
                }
            }
        }
    }
}

/// Coroutines interleave at yield points on a single `smol::LocalExecutor`.
/// Deadlines work in three layers: `set_interrupt` catches tight CPU loops,
/// `smol::Timer` races catch I/O waits, and the dispatch loop covers jobs.
#[allow(clippy::too_many_arguments)]
async fn run_tool_call(
    lua: Lua,
    plugin: Arc<str>,
    tool: Arc<str>,
    input: Value,
    mut ctx: Box<LuaCtx>,
    deadline: Option<Instant>,
    live: Option<LiveCtx>,
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
            Err(e) => return ToolCallReply::err(e.to_string()),
        }
    };
    if shutdown.load(Ordering::Acquire) {
        return ToolCallReply::err("plugin host shutting down");
    }

    let (finish_tx, finish_rx) = flume::bounded::<ToolCallReply>(1);
    ctx.finish_tx = Some(finish_tx);
    let cancel = ctx.cancel.clone();

    let input_lua = match lua.to_value(&input) {
        Ok(v) => v,
        Err(e) => return ToolCallReply::err(e.to_string()),
    };
    let ctx_ud = match lua.create_userdata(*ctx) {
        Ok(u) => u,
        Err(e) => return ToolCallReply::err(e.to_string()),
    };

    let thread = match lua.create_thread(handler) {
        Ok(t) => t,
        Err(e) => return ToolCallReply::err(e.to_string()),
    };
    let thread_key = ThreadKey(thread.to_pointer() as usize);

    let task_ctx = TaskCtx::new(cancel, deadline, live);
    let _cleanup = register_task(&lua, thread_key, task_ctx);

    let async_thread = match thread.into_async::<LuaValue>((input_lua, ctx_ud)) {
        Ok(at) => at,
        Err(e) => return ToolCallReply::err(e.to_string()),
    };

    let call_future = async {
        match async_thread.await {
            Ok(LuaValue::Nil) => {
                let live_shared = lua.app_data_ref::<TaskMap>().and_then(|m| {
                    let ctx = m.get(&thread_key)?;
                    let live = ctx.live.as_ref()?;
                    let shared = ctx.bufs.live_buf()?;
                    Some((
                        live.event_tx.clone(),
                        live.tool_use_id.clone(),
                        Arc::clone(shared),
                    ))
                });
                if let Some((event_tx, tool_use_id, shared)) = live_shared {
                    let _ = event_tx.send(maki_agent::AgentEvent::LiveToolBuf {
                        id: tool_use_id,
                        body: shared,
                    });
                }
                dispatch_async(&lua, thread_key, finish_rx).await
            }
            Ok(val) => ToolCallReply::from_lua_value(&val),
            Err(e) => ToolCallReply::err(e.to_string()),
        }
    };

    match deadline {
        Some(dl) => {
            futures_lite::future::race(call_future, async {
                smol::Timer::at(dl).await;
                ToolCallReply::err("timeout")
            })
            .await
        }
        None => call_future.await,
    }
}

pub(crate) struct LuaThread {
    pub tx: flume::Sender<Request>,
    pub join: Option<JoinHandle<()>>,
    pub shutdown: Arc<AtomicBool>,
    pub command_reader: LuaCommandReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
}

/// Lua gets its own OS thread so nothing needs a Mutex. `smol::block_on`
/// drives cooperative async, and load/clear requests wait for in-flight tools.
pub fn spawn(
    registry: Arc<ToolRegistry>,
    bundled_dirs: &'static [&'static Dir<'static>],
) -> Result<LuaThread, PluginError> {
    let (tx, rx) = flume::unbounded::<Request>();
    let tx_clone = tx.clone();
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let (init_tx, init_rx) = flume::bounded::<Result<(), PluginError>>(1);
    let (ui_action_tx, ui_action_rx) = flume::unbounded::<UiAction>();
    let (command_writer, command_reader) = LuaCommandWriter::new();

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

            smol::block_on(ex.run(async {
                loop {
                    let msg = match rx.recv_async().await {
                        Ok(m) => m,
                        Err(_) => break,
                    };
                    match msg {
                        Request::Shutdown => break,
                        Request::LoadSource {
                            name,
                            source,
                            plugin_dir,
                            reply,
                        } => {
                            gate.drain().await;
                            let res = rt.load_source(Arc::clone(&name), &source, plugin_dir).await;
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
                            gate.wait_below(MAX_INFLIGHT_TOOLS).await;
                            let lua = rt.lua.clone();
                            let plugins = Rc::clone(&rt.plugins);
                            let shutdown_ref = Arc::clone(&rt.shutdown);
                            let g = Rc::clone(&gate);
                            let ex_ref = Rc::clone(&ex);

                            ex.spawn(async move {
                                let _gate_guard = GateGuard::new(&g);
                                let res = run_tool_call(
                                    lua.clone(),
                                    plugin,
                                    tool,
                                    input,
                                    ctx,
                                    deadline,
                                    live,
                                    plugins,
                                    shutdown_ref,
                                )
                                .await;
                                drain_spawn_queue(&lua, &ex_ref, &g);
                                let _ = reply.send(res);
                            })
                            .detach();
                        }
                        Request::ClearPlugin { plugin, reply } => {
                            gate.drain().await;
                            rt.clear_plugin(&plugin);
                            let _ = reply.send(());
                        }
                        Request::FireBufClick { tool_id, row, reply } => {
                            let entry =
                                rt.lua.app_data_ref::<ClickHandlerMap>().and_then(|m| {
                                    let (key, buf) = m.get(&tool_id)?;
                                    let func = rt.lua.registry_value::<Function>(key).ok()?;
                                    Some((func, Arc::clone(buf)))
                                });
                            if let Some((func, buf)) = entry {
                                let lua = rt.lua.clone();
                                let ex_ref = Rc::clone(&ex);
                                let g = Rc::clone(&gate);
                                ex.spawn(async move {
                                    let Ok(data) = lua.create_table() else {
                                        let _ = reply.send(None);
                                        return;
                                    };
                                    let _ = data.set("row", row);
                                    if let Err(e) = func.call_async::<()>(data).await {
                                        tracing::warn!(tool_id, error = %e, "click handler failed");
                                    }
                                    drain_spawn_queue(&lua, &ex_ref, &g);
                                    let _ = reply.send(Some(ClickReply {
                                        snapshot: buf.take(),
                                        live_buf: buf,
                                    }));
                                })
                                .detach();
                            } else {
                                let _ = reply.send(None);
                            }
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
                                let ex_ref = Rc::clone(&ex);
                                let g = Rc::clone(&gate);
                                ex.spawn(async move {
                                    let run = async {
                                        let thread = lua.create_thread(func)?;
                                        let thread_key = ThreadKey(thread.to_pointer() as usize);
                                        let _cleanup = register_task(&lua, thread_key, TaskCtx::new(
                                            CancelToken::none(),
                                            None,
                                            None,
                                        ));
                                        thread.into_async::<()>(args)?.await
                                    };
                                    if let Err(e) = run.await {
                                        tracing::warn!(plugin = %plugin, command = %command, error = %e, "command handler failed");
                                    }
                                    drain_spawn_queue(&lua, &ex_ref, &g);
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
                            let res = rt.compute_header(&plugin, &tool, input);
                            let _ = reply.send(res);
                        }
                        Request::ComputePermissionScopes {
                            plugin,
                            tool,
                            input,
                            reply,
                        } => {
                            let res = rt.compute_permission_scopes(&plugin, &tool, input);
                            let _ = reply.send(res);
                        }
                        Request::RunInitLua {
                            source,
                            source_name,
                            plugin_dir,
                            reply,
                        } => {
                            gate.drain().await;
                            let res = rt.run_init_lua(&source, &source_name, plugin_dir);
                            let _ = reply.send(res);
                        }
                        Request::CollectPromptExtras { reply } => {
                            let extras = rt.collect_prompt_extras().await;
                            let _ = reply.send(extras);
                        }
                    Request::RestoreTool {
                        tool,
                        tool_use_id,
                        output,
                        input,
                        is_error,
                        tool_output_lines,
                        reply,
                    } => {
                        let res = rt.restore_tool(&tool, &tool_use_id, &output, input, is_error, tool_output_lines).await;
                        drain_spawn_queue(&rt.lua, &ex, &gate);
                        let _ = reply.send(res);
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
        join: Some(handle),
        shutdown,
        command_reader,
        ui_action_rx,
    })
}

#[cfg(test)]
pub(crate) fn install_live_ctx(lua: &Lua, tool_use_id: &str) {
    let key = ThreadKey::current(lua);
    if lua.app_data_ref::<TaskMap>().is_none() {
        lua.set_app_data(TaskMap::new());
    }
    let (tx, _rx) = flume::unbounded();
    let ctx = TaskCtx::new(
        CancelToken::none(),
        None,
        Some(LiveCtx {
            event_tx: maki_agent::EventSender::new(tx, 0),
            tool_use_id: tool_use_id.to_owned(),
        }),
    );
    lua.app_data_mut::<TaskMap>().unwrap().insert(key, ctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tool::ToolCallReply;
    use maki_agent::{SnapshotLine, SnapshotSpan, SpanStyle};

    fn make_buf_handle(text: &str) -> BufHandle {
        let buf = Arc::new(maki_agent::SharedBuf::new());
        buf.append(SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        });
        BufHandle { id: 0, buf }
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
        let reply = ToolCallReply::from_lua_value(&val);
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
        let reply = ToolCallReply::from_lua_value(&LuaValue::Table(t));
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
        let reply = ToolCallReply::from_lua_value(&LuaValue::Table(t));
        assert!(reply.result.is_err());
        assert!(reply.snapshot.is_some());
    }

    #[test]
    fn task_cleanup_guard_removes_entry() {
        let lua = Lua::new();
        lua.set_app_data(TaskMap::new());
        let key = ThreadKey::current(&lua);
        {
            lua.app_data_mut::<TaskMap>()
                .unwrap()
                .insert(key, task_ctx(None));
        }
        drop(TaskCleanupGuard {
            lua: lua.clone(),
            key,
        });
        let tasks = lua.app_data_ref::<TaskMap>().unwrap();
        assert!(!tasks.contains_key(&key));
    }

    fn task_ctx(live: Option<LiveCtx>) -> TaskCtx {
        TaskCtx::new(CancelToken::none(), None, live)
    }

    #[test]
    fn with_live_ctx_follows_task_live_field() {
        let lua = Lua::new();
        lua.set_app_data(TaskMap::new());
        let key = ThreadKey::current(&lua);

        lua.app_data_mut::<TaskMap>()
            .unwrap()
            .insert(key, task_ctx(None));
        assert!(with_live_ctx(&lua, |_| ()).is_none());

        let (tx, _rx) = flume::unbounded();
        lua.app_data_mut::<TaskMap>()
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .live = Some(LiveCtx {
            event_tx: maki_agent::EventSender::new(tx, 0),
            tool_use_id: "tool_abc".into(),
        });
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
        lua.set_app_data(TaskMap::new());
        lua.set_app_data(SpawnQueue::new(Vec::new()));
        lua
    }

    fn enqueue_dummy(lua: &Lua) -> RegistryKey {
        let func = lua.create_function(|_, _: ()| Ok(())).unwrap();
        lua.create_registry_value(func).unwrap()
    }

    fn enqueue_with_ctx(lua: &Lua, ctx: TaskCtx) {
        let key = ThreadKey::current(lua);
        lua.app_data_mut::<TaskMap>().unwrap().insert(key, ctx);
    }

    #[test]
    fn gate_guard_tracks_count_via_raii() {
        let g = gate();
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
    fn enqueue_async_task_works_without_task_ctx() {
        let lua = enqueue_test_lua();
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = &queue.borrow()[0];
        assert!(queued.live_ctx.is_none());
        assert!(queued.live_buf.is_none());
    }

    #[test]
    fn enqueue_async_task_inherits_cancel_token() {
        let lua = enqueue_test_lua();
        let (trigger, token) = CancelToken::new();
        enqueue_with_ctx(&lua, TaskCtx::new(token, None, None));
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = &queue.borrow()[0];
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
        enqueue_with_ctx(
            &lua,
            TaskCtx::new(CancelToken::none(), Some(parent_deadline), None),
        );

        let before = Instant::now();
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let task_deadline = queue.borrow()[0].deadline.unwrap();
        assert!(
            task_deadline > before,
            "async task should get a fresh deadline, not inherit expired parent"
        );
    }

    fn push_pending_task(lua: &Lua, cancel: CancelToken, deadline: Option<Instant>) {
        let work_fn = enqueue_dummy(lua);
        lua.app_data_ref::<SpawnQueue>()
            .unwrap()
            .borrow_mut()
            .push(PendingAsyncTask {
                work_fn,
                cancel,
                deadline,
                live_ctx: None,
                live_buf: None,
            });
    }

    #[test]
    fn drain_spawn_queue_skips_cancelled_tasks() {
        let ex = Rc::new(smol::LocalExecutor::new());
        smol::block_on(ex.run(async {
            let lua = enqueue_test_lua();
            let (trigger, token) = CancelToken::new();
            trigger.cancel();
            push_pending_task(&lua, token, None);

            let g = Rc::new(gate());
            drain_spawn_queue(&lua, &ex, &g);
            smol::future::yield_now().await;
            assert_eq!(g.count.get(), 0);
        }));
    }

    #[test]
    fn drain_spawn_queue_runs_and_decrements_gate() {
        let ex = Rc::new(smol::LocalExecutor::new());
        smol::block_on(ex.run(async {
            let lua = enqueue_test_lua();
            push_pending_task(
                &lua,
                CancelToken::none(),
                Some(Instant::now() + Duration::from_secs(5)),
            );

            let g = Rc::new(gate());
            drain_spawn_queue(&lua, &ex, &g);

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
