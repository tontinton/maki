use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use include_dir::Dir;
use maki_agent::RawRenderHints;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::{HeaderResult, RegistryError, Tool, ToolRegistry, ToolSource};
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Value as LuaValue, VmState};
use serde_json::Value;

use maki_config::RawConfig;

use crate::api::buf::{BufHandle, BufferStore};
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

pub type LoadResult = Result<Vec<(Arc<str>, RawRenderHints)>, PluginError>;

/// `LoadSource` and `ClearPlugin` drain all in-flight tool calls before
/// proceeding, so the plugin environment is never mutated mid-call.
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
    },
    Shutdown,
}

/// Bundles `event_tx` + `tool_use_id` so `CallTool` does not need
/// two separate Option fields that must stay in sync.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ThreadKey(usize);

impl ThreadKey {
    fn current(lua: &Lua) -> Self {
        Self(lua.current_thread().to_pointer() as usize)
    }
}

/// Keyed by coroutine pointer. All access is on the single Lua OS thread.
type TaskMap = HashMap<ThreadKey, TaskCtx>;

type ClickHandlerMap = HashMap<String, RegistryKey>;

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

/// RAII guard that cleans up a task (kills jobs, clears bufs) even on
/// panics or early returns.
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

struct ToolKeys {
    handler: RegistryKey,
    header: Option<RegistryKey>,
}

type PluginMap = Rc<RefCell<HashMap<Arc<str>, HashMap<Arc<str>, ToolKeys>>>>;

/// Sandbox-first: `require`, `io`, `package` are stripped, `os` and `debug`
/// are limited by Luau's built-in sandbox.
struct LuaRuntime {
    lua: Lua,
    pending: PendingTools,
    plugins: PluginMap,
    registry: Arc<ToolRegistry>,
    tx: flume::Sender<Request>,
    shutdown: Arc<AtomicBool>,
    bundled_dirs: &'static [&'static Dir<'static>],
}

impl LuaRuntime {
    fn new(
        registry: Arc<ToolRegistry>,
        tx: flume::Sender<Request>,
        shutdown: Arc<AtomicBool>,
        bundled_dirs: &'static [&'static Dir<'static>],
    ) -> Result<Self, PluginError> {
        let lua = Lua::new();
        let pending: PendingTools = Arc::new(Mutex::new(Vec::new()));

        // Each coroutine gets its own cancel token and deadline, so the
        // interrupt handler does an O(1) HashMap lookup to check just that task.
        let interrupt_shutdown = Arc::clone(&shutdown);
        let interrupt_lua = lua.clone();
        lua.set_interrupt(move |_| {
            if interrupt_shutdown.load(Ordering::Acquire) {
                return Err(mlua::Error::runtime(INTERRUPT_MSG));
            }
            let key = ThreadKey(interrupt_lua.current_thread().to_pointer() as usize);
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

        Ok(Self {
            lua,
            pending,
            plugins: Rc::new(RefCell::new(HashMap::new())) as PluginMap,
            registry,
            tx,
            shutdown,
            bundled_dirs,
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
            }
        }
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

    /// Bundled dirs are checked before the filesystem, which lets plugins
    /// `require()` shared modules like `truncate.lua` from the `lib` bundle.
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

    fn load_source(
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
        let maki = create_maki_global(&self.lua, Arc::clone(&self.pending), Arc::clone(&name))
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

        let exec_result = self
            .lua
            .load(source)
            .set_name(name.as_ref())
            .set_environment(env)
            .exec();

        if let Err(e) = exec_result {
            let stale = self.drain_pending();
            self.discard_pending(stale);
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
                    permission_scope_field: t.permission_scope_field.clone(),
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

        self.drop_plugin_keys(&name);

        let hints: Vec<(Arc<str>, RawRenderHints)> = pending
            .iter()
            .filter_map(|t| t.render_hints.clone().map(|h| (Arc::clone(&t.name), h)))
            .collect();

        let keys: HashMap<Arc<str>, ToolKeys> = pending
            .into_iter()
            .map(|t| {
                (
                    t.name,
                    ToolKeys {
                        handler: t.handler_key,
                        header: t.header_key,
                    },
                )
            })
            .collect();
        self.plugins.borrow_mut().insert(name, keys);

        Ok(hints)
    }

    fn clear_plugin(&mut self, plugin: &str) {
        self.registry.clear_plugin(plugin);
        self.drop_plugin_keys(plugin);
    }

    /// Temporarily inserts a TaskCtx so `maki.ui.buf()` works during header
    /// computation. `TaskCleanupGuard` handles teardown.
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
        let task_ctx = TaskCtx {
            cancel: CancelToken::none(),
            deadline: None,
            jobs: JobStore::new(),
            bufs: BufferStore::new(),
            live: None,
        };
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

/// After the handler returns nil (async mode), this loop polls job events
/// and waits for either `ctx:finish()` or all jobs to die.
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

/// Tool calls run concurrently on a `smol::LocalExecutor`. Coroutines
/// interleave at yield points (async I/O). Deadlines are enforced three
/// ways: CPU-bound loops via `set_interrupt`, I/O waits via `smol::Timer`
/// race, and spawned jobs via the dispatch loop.
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

    let task_ctx = TaskCtx {
        cancel,
        deadline,
        jobs: JobStore::new(),
        bufs: BufferStore::new(),
        live,
    };
    if let Some(mut tasks) = lua.app_data_mut::<TaskMap>() {
        tasks.insert(thread_key, task_ctx);
    }

    let _cleanup = TaskCleanupGuard {
        lua: lua.clone(),
        key: thread_key,
    };

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
}

/// Runs on a dedicated OS thread so the Lua instance never crosses thread
/// boundaries (no Mutex needed). `smol::block_on` drives cooperative async.
/// `LoadSource`/`ClearPlugin` wait for in-flight calls to finish first.
pub fn spawn(
    registry: Arc<ToolRegistry>,
    bundled_dirs: &'static [&'static Dir<'static>],
) -> Result<LuaThread, PluginError> {
    let (tx, rx) = flume::unbounded::<Request>();
    let tx_clone = tx.clone();
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let (init_tx, init_rx) = flume::bounded::<Result<(), PluginError>>(1);

    let handle = thread::Builder::new()
        .name("maki-lua".to_owned())
        .spawn(move || {
            let mut rt = match LuaRuntime::new(registry, tx_clone, shutdown_thread, bundled_dirs) {
                Ok(r) => {
                    let _ = init_tx.send(Ok(()));
                    r
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let ex = smol::LocalExecutor::new();
            let inflight = Rc::new(Cell::new(0usize));

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
                            while inflight.get() > 0 {
                                smol::future::yield_now().await;
                            }
                            let res = rt.load_source(Arc::clone(&name), &source, plugin_dir);
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
                            let shutdown_ref = Arc::clone(&rt.shutdown);
                            let counter = Rc::clone(&inflight);
                            counter.set(counter.get() + 1);

                            ex.spawn(async move {
                                let res = run_tool_call(
                                    lua,
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
                                let _ = reply.send(res);
                                counter.set(counter.get() - 1);
                            })
                            .detach();
                        }
                        Request::ClearPlugin { plugin, reply } => {
                            while inflight.get() > 0 {
                                smol::future::yield_now().await;
                            }
                            rt.clear_plugin(&plugin);
                            let _ = reply.send(());
                        }
                        Request::FireBufClick { tool_id, row } => {
                            let handler_fn =
                                rt.lua.app_data_ref::<ClickHandlerMap>().and_then(|m| {
                                    let key = m.get(&tool_id)?;
                                    rt.lua.registry_value::<Function>(key).ok()
                                });
                            if let Some(func) = handler_fn {
                                let lua = rt.lua.clone();
                                ex.spawn(async move {
                                    let Ok(data) = lua.create_table() else {
                                        return;
                                    };
                                    let _ = data.set("row", row);
                                    if let Err(e) = func.call_async::<()>(data).await {
                                        tracing::warn!(tool_id, error = %e, "click handler failed");
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
                            let res = rt.compute_header(&plugin, &tool, input);
                            let _ = reply.send(res);
                        }
                        Request::RunInitLua {
                            source,
                            source_name,
                            plugin_dir,
                            reply,
                        } => {
                            while inflight.get() > 0 {
                                smol::future::yield_now().await;
                            }
                            let res = rt.run_init_lua(&source, &source_name, plugin_dir);
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
    })
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
    fn from_lua_value_nil_is_error() {
        let reply = ToolCallReply::from_lua_value(&LuaValue::Nil);
        assert!(reply.result.is_err());
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

    #[test]
    fn with_task_accessors_return_none_when_missing() {
        let lua = Lua::new();
        assert!(with_task_jobs(&lua, |_| 42).is_none());
        assert!(with_task_bufs(&lua, |_| 42).is_none());

        lua.set_app_data(TaskMap::new());
        assert!(with_task_jobs(&lua, |_| 42).is_none());
        assert!(with_task_bufs(&lua, |_| 42).is_none());
    }

    fn task_ctx(live: Option<LiveCtx>) -> TaskCtx {
        TaskCtx {
            cancel: CancelToken::none(),
            deadline: None,
            jobs: JobStore::new(),
            bufs: BufferStore::new(),
            live,
        }
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
}
