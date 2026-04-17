use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use maki_agent::cancel::CancelToken;
use maki_agent::tools::{RegistryError, Tool, ToolRegistry, ToolSource};
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Value as LuaValue, VmState};
use serde_json::Value;

use crate::api::create_maki_global;
use crate::api::ctx::LuaCtx;
use crate::api::tool::{LuaTool, PendingTool, PendingTools, ToolCallResult, coerce_tool_result};
use crate::error::PluginError;

const INTERRUPT_MSG: &str = "plugin interrupted: cancelled, deadline exceeded, or shutting down";

pub enum Request {
    LoadSource {
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
        reply: flume::Sender<Result<(), PluginError>>,
    },
    CallTool {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        ctx: LuaCtx,
        deadline: Option<Instant>,
        reply: flume::Sender<ToolCallResult>,
    },
    ClearPlugin {
        plugin: Arc<str>,
        reply: flume::Sender<()>,
    },
    Shutdown,
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

struct LuaRuntime {
    lua: Lua,
    pending: PendingTools,
    plugins: HashMap<Arc<str>, HashMap<Arc<str>, RegistryKey>>,
    registry: Arc<ToolRegistry>,
    tx: flume::Sender<Request>,
    cwd: PathBuf,
    call_state: Arc<Mutex<Option<CallState>>>,
    shutdown: Arc<AtomicBool>,
}

impl LuaRuntime {
    fn new(
        registry: Arc<ToolRegistry>,
        tx: flume::Sender<Request>,
        shutdown: Arc<AtomicBool>,
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
        for dangerous in &["os", "io", "debug", "package", "require"] {
            globals
                .set(*dangerous, LuaValue::Nil)
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

        Ok(Self {
            lua,
            pending,
            plugins: HashMap::new(),
            registry,
            tx,
            cwd,
            call_state,
            shutdown,
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

    /// Log and continue on individual failures since partial removal still helps.
    fn drop_plugin_keys(&mut self, name: &str) {
        if let Some(keys) = self.plugins.remove(name) {
            for (_, key) in keys {
                if let Err(e) = self.lua.remove_registry_value(key) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua handler key");
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
        }
    }

    fn build_plugin_env(
        &self,
        fs_roots: Arc<[PathBuf]>,
        plugin: Arc<str>,
    ) -> Result<mlua::Table, mlua::Error> {
        let maki = create_maki_global(&self.lua, Arc::clone(&self.pending), fs_roots, plugin)?;
        let env = self.lua.create_table()?;
        env.set("maki", maki)?;
        let meta = self.lua.create_table()?;
        meta.set("__index", self.lua.globals())?;
        env.set_metatable(Some(meta))?;
        Ok(env)
    }

    fn load_source(
        &mut self,
        name: Arc<str>,
        source: &str,
        plugin_dir: Option<PathBuf>,
    ) -> Result<(), PluginError> {
        let stale = self.drain_pending();
        debug_assert!(
            stale.is_empty(),
            "leftover pending tools from previous load"
        );
        self.discard_pending(stale);

        let roots = self.fs_roots(plugin_dir.as_deref());

        let env = self
            .build_plugin_env(roots, Arc::clone(&name))
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

        let keys: HashMap<Arc<str>, RegistryKey> = pending
            .into_iter()
            .map(|t| (t.name, t.handler_key))
            .collect();
        self.plugins.insert(name, keys);

        Ok(())
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
        ctx: LuaCtx,
        deadline: Option<Instant>,
    ) -> ToolCallResult {
        let keys = self
            .plugins
            .get(plugin)
            .ok_or_else(|| format!("plugin not loaded: {plugin}"))?;

        let handler_key = keys
            .get(tool)
            .ok_or_else(|| format!("tool not found: {tool}"))?;

        let handler: Function = self
            .lua
            .registry_value(handler_key)
            .map_err(|e| e.to_string())?;

        if self.shutdown.load(Ordering::Acquire) {
            return Err("plugin host shutting down".into());
        }

        {
            let mut guard = self.call_state.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(CallState {
                cancel: ctx.cancel.clone(),
                deadline,
            });
        }

        let _clear_on_drop = CallStateGuard(&self.call_state);

        let input_lua = self.lua.to_value(&input).map_err(|e| e.to_string())?;
        let ctx_ud = self.lua.create_userdata(ctx).map_err(|e| e.to_string())?;

        let result = handler.call::<LuaValue>((input_lua, ctx_ud));

        match result {
            Ok(val) => coerce_tool_result(&val),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub(crate) struct LuaThread {
    pub tx: flume::Sender<Request>,
    pub join: Option<JoinHandle<()>>,
    pub shutdown: Arc<AtomicBool>,
}

pub fn spawn(registry: Arc<ToolRegistry>) -> Result<LuaThread, PluginError> {
    let (tx, rx) = flume::unbounded::<Request>();
    let tx_clone = tx.clone();
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let (init_tx, init_rx) = flume::bounded::<Result<(), PluginError>>(1);

    let handle = thread::Builder::new()
        .name("maki-lua".to_owned())
        .spawn(move || {
            let mut rt = match LuaRuntime::new(registry, tx_clone, shutdown_thread) {
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
                    } => {
                        let res = rt.call_tool(&plugin, &tool, input, ctx, deadline);
                        let _ = reply.send(res);
                    }
                    Request::ClearPlugin { plugin, reply } => {
                        rt.clear_plugin(&plugin);
                        let _ = reply.send(());
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
