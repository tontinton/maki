use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

use crate::api::buf::{BufHandle, BufferStore};
use crate::api::create_maki_global;
use crate::api::ctx::{FinishPayload, LuaCtx};
use crate::api::fn_api::{JobEvent, JobStore};
use crate::api::fs::check_sandbox;
use crate::api::tool::{LuaTool, PendingTool, PendingTools, ToolCallReply, extract_tool_return};
use crate::error::PluginError;

const INTERRUPT_MSG: &str = "plugin interrupted: cancelled, deadline exceeded, or shutting down";
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NIL_WITHOUT_FINISH_MSG: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";

pub type LoadResult = Result<Vec<(Arc<str>, RawRenderHints)>, PluginError>;

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
        ctx: LuaCtx,
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
    Shutdown,
}

pub struct LiveCtx {
    pub event_tx: maki_agent::EventSender,
    pub tool_use_id: String,
}

struct CallState {
    cancel: CancelToken,
    deadline: Option<Instant>,
}

struct CallStateGuard<'a>(&'a Mutex<Option<CallState>>);

impl Drop for CallStateGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

struct JobCleanupGuard<'a>(&'a LuaRuntime);

impl Drop for JobCleanupGuard<'_> {
    fn drop(&mut self) {
        self.0.cleanup_jobs();
    }
}

struct ToolKeys {
    handler: RegistryKey,
    header: Option<RegistryKey>,
}

struct LuaRuntime {
    lua: Lua,
    pending: PendingTools,
    plugins: HashMap<Arc<str>, HashMap<Arc<str>, ToolKeys>>,
    registry: Arc<ToolRegistry>,
    tx: flume::Sender<Request>,
    cwd: PathBuf,
    call_state: Arc<Mutex<Option<CallState>>>,
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
        let cwd = std::env::current_dir().unwrap_or_default();
        let call_state: Arc<Mutex<Option<CallState>>> = Arc::new(Mutex::new(None));

        let interrupt_state = Arc::clone(&call_state);
        let interrupt_shutdown = Arc::clone(&shutdown);
        lua.set_interrupt(move |_| {
            if interrupt_shutdown.load(Ordering::Acquire) {
                return Err(mlua::Error::runtime(INTERRUPT_MSG));
            }
            let guard = interrupt_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(state) = guard.as_ref() {
                let cancelled = state.cancel.is_cancelled();
                let expired = state.deadline.is_some_and(|d| Instant::now() > d);
                if cancelled || expired {
                    return Err(mlua::Error::runtime(INTERRUPT_MSG));
                }
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

        lua.set_app_data(BufferStore::new());

        Ok(Self {
            lua,
            pending,
            plugins: HashMap::new(),
            registry,
            tx,
            cwd,
            call_state,
            shutdown,
            bundled_dirs,
        })
    }

    fn fs_roots(&self, plugin_dir: Option<&Path>) -> Arc<[PathBuf]> {
        let cwd_canon = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        let mut roots = vec![cwd_canon];
        if let Some(dir) = plugin_dir {
            let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
            if !roots.contains(&canon) {
                roots.push(canon);
            }
        }
        roots.into()
    }

    fn drop_plugin_keys(&mut self, name: &str) {
        if let Some(keys) = self.plugins.remove(name) {
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

    fn build_plugin_env(
        &self,
        fs_roots: Arc<[PathBuf]>,
        plugin: Arc<str>,
        require_root: Option<PathBuf>,
    ) -> Result<mlua::Table, mlua::Error> {
        let maki = create_maki_global(&self.lua, Arc::clone(&self.pending), fs_roots, plugin)?;
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
                let abs_str = abs_path.to_string_lossy();
                let resolved = check_sandbox(&abs_str, std::slice::from_ref(dir))
                    .map_err(|e| mlua::Error::runtime(e.to_string()))?;
                Ok(std::fs::read_to_string(&resolved).ok())
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

        let roots = self.fs_roots(plugin_dir.as_deref());
        let require_root = plugin_dir.as_ref().map(|d| d.join("lua"));

        let env = self
            .build_plugin_env(roots, Arc::clone(&name), require_root)
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
        self.plugins.insert(name, keys);

        Ok(hints)
    }

    fn clear_plugin(&mut self, plugin: &str) {
        self.registry.clear_plugin(plugin);
        self.drop_plugin_keys(plugin);
    }

    fn call_tool(
        &self,
        plugin: &str,
        tool: &str,
        input: Value,
        mut ctx: LuaCtx,
        deadline: Option<Instant>,
        live: Option<&LiveCtx>,
    ) -> ToolCallReply {
        let Some(keys) = self.plugins.get(plugin) else {
            return ToolCallReply::err(format!("plugin not loaded: {plugin}"));
        };
        let Some(tool_keys) = keys.get(tool) else {
            return ToolCallReply::err(format!("tool not found: {tool}"));
        };
        let handler: Function = match self.lua.registry_value(&tool_keys.handler) {
            Ok(f) => f,
            Err(e) => return ToolCallReply::err(e.to_string()),
        };
        if self.shutdown.load(Ordering::Acquire) {
            return ToolCallReply::err("plugin host shutting down");
        }

        {
            let mut guard = self.call_state.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(CallState {
                cancel: ctx.cancel.clone(),
                deadline,
            });
        }

        let _clear_on_drop = CallStateGuard(&self.call_state);

        let (finish_tx, finish_rx) = flume::bounded::<FinishPayload>(1);
        ctx.finish_tx = Some(finish_tx);

        self.lua.set_app_data(JobStore::new());
        let _cleanup = JobCleanupGuard(self);

        let input_lua = match self.lua.to_value(&input) {
            Ok(v) => v,
            Err(e) => return ToolCallReply::err(e.to_string()),
        };
        let ctx_ud = match self.lua.create_userdata(ctx) {
            Ok(u) => u,
            Err(e) => return ToolCallReply::err(e.to_string()),
        };

        let result = handler.call::<LuaValue>((input_lua, ctx_ud));

        match result {
            Ok(LuaValue::Nil) => {
                if let Some(live) = live {
                    if let Some(store) = self.lua.app_data_ref::<BufferStore>() {
                        if let Some(shared) = store.live_buf() {
                            let _ = live.event_tx.send(maki_agent::AgentEvent::LiveToolBuf {
                                id: live.tool_use_id.clone(),
                                body: Arc::clone(shared),
                            });
                        }
                    }
                }
                self.dispatch_loop(finish_rx)
            }
            Ok(val) => extract_tool_return(&self.lua, &val),
            Err(e) => ToolCallReply::err(e.to_string()),
        }
    }

    fn finish_reply(payload: FinishPayload) -> ToolCallReply {
        ToolCallReply {
            result: if payload.is_error {
                Err(payload.llm_output)
            } else {
                Ok(payload.llm_output)
            },
            snapshot: payload.body,
            header: None,
        }
    }

    fn dispatch_loop(&self, finish_rx: flume::Receiver<FinishPayload>) -> ToolCallReply {
        let no_jobs_started = self
            .lua
            .app_data_ref::<JobStore>()
            .is_some_and(|s| s.is_empty());

        if no_jobs_started {
            self.lua.gc_collect().ok();
            match finish_rx.recv_timeout(DISPATCH_POLL_INTERVAL) {
                Ok(payload) => return Self::finish_reply(payload),
                _ => return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
            }
        }

        let event_rx = self
            .lua
            .app_data_ref::<JobStore>()
            .map(|s| s.event_rx.clone());

        loop {
            {
                let guard = self.call_state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(state) = guard.as_ref() {
                    if state.cancel.is_cancelled() {
                        return ToolCallReply::err("cancelled");
                    }
                    if state.deadline.is_some_and(|d| Instant::now() > d) {
                        return ToolCallReply::err("timeout");
                    }
                }
            }

            match finish_rx.try_recv() {
                Ok(payload) => return Self::finish_reply(payload),
                Err(flume::TryRecvError::Disconnected) => {
                    return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
                }
                Err(flume::TryRecvError::Empty) => {}
            }

            let has_alive = self
                .lua
                .app_data_ref::<JobStore>()
                .is_some_and(|s| s.has_alive_jobs());

            if !has_alive {
                match finish_rx.recv_timeout(DISPATCH_POLL_INTERVAL) {
                    Ok(payload) => return Self::finish_reply(payload),
                    Err(flume::RecvTimeoutError::Disconnected) => {
                        return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
                    }
                    Err(flume::RecvTimeoutError::Timeout) => {
                        return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
                    }
                }
            }

            let tagged = event_rx
                .as_ref()
                .and_then(|rx| rx.recv_timeout(DISPATCH_POLL_INTERVAL).ok());

            let Some(tagged) = tagged else {
                continue;
            };

            let callback = self.lua.app_data_ref::<JobStore>().and_then(|s| {
                let key = s.callback_key(tagged.job_id, &tagged.event)?;
                self.lua.registry_value::<Function>(key).ok()
            });

            let is_exit = matches!(tagged.event, JobEvent::Exit(_));

            if let Some(func) = callback {
                let arg: LuaValue = match &tagged.event {
                    JobEvent::Stdout(line) | JobEvent::Stderr(line) => self
                        .lua
                        .create_string(line)
                        .map(LuaValue::String)
                        .unwrap_or(LuaValue::Nil),
                    JobEvent::Exit(code) => LuaValue::Integer(*code as i64),
                };
                if let Err(e) = func.call::<()>((tagged.job_id, arg)) {
                    return ToolCallReply::err(format!("job callback error: {e}"));
                }
            }

            if is_exit {
                if let Some(mut store) = self.lua.app_data_mut::<JobStore>() {
                    store.mark_dead(tagged.job_id);
                }
            }
        }
    }

    fn cleanup_jobs(&self) {
        if let Some(mut store) = self.lua.app_data_mut::<JobStore>() {
            store.kill_all();
            store.clear(&self.lua);
        }
        if let Some(mut buf_store) = self.lua.app_data_mut::<BufferStore>() {
            buf_store.clear();
        }
    }

    fn compute_header(&self, plugin: &str, tool: &str, input: Value) -> HeaderResult {
        let result = (|| {
            let tk = self.plugins.get(plugin)?.get(tool)?;
            let key = tk.header.as_ref()?;
            let func = self.lua.registry_value::<Function>(key).ok()?;
            let input_lua = self.lua.to_value(&input).ok()?;
            match func.call::<LuaValue>(input_lua).ok()? {
                LuaValue::String(s) => Some(HeaderResult::plain(s.to_str().ok()?.to_owned())),
                LuaValue::UserData(ud) => {
                    let h = ud.borrow::<BufHandle>().ok()?;
                    let snapshot = self.lua.app_data_mut::<BufferStore>()?.take(h.0)?;
                    Some(HeaderResult::Styled(snapshot))
                }
                _ => None,
            }
        })();
        if let Some(mut store) = self.lua.app_data_mut::<BufferStore>() {
            store.clear();
        }
        result.unwrap_or_else(|| HeaderResult::plain(tool.to_string()))
    }
}

pub(crate) struct LuaThread {
    pub tx: flume::Sender<Request>,
    pub join: Option<JoinHandle<()>>,
    pub shutdown: Arc<AtomicBool>,
}

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

            loop {
                let msg = match rx.recv() {
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
                        let res = rt.call_tool(&plugin, &tool, input, ctx, deadline, live.as_ref());
                        let _ = reply.send(res);
                    }
                    Request::ClearPlugin { plugin, reply } => {
                        rt.clear_plugin(&plugin);
                        let _ = reply.send(());
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
                }
            }
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
