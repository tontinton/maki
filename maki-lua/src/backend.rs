//! Routing of routed tool calls to an injected fs backend.
//!
//! The host installs the routed backend via `set_sandbox_backend`, which
//! replaces [`BackendRegistry::default`]; `run_tool_call` binds the handler
//! coroutine's thread pointer for the duration of a routed call so its
//! `maki.fs.*` accesses resolve to the sandbox.
//!
//! Unbound threads still see `default`, so what the default holds is the real
//! answer to "which filesystem is this tool on". It starts as the host
//! filesystem and becomes `SandboxFs` once the host enables the sandbox, which
//! is why a `host_access = true` tool still lands in the sandbox for file
//! access: it skips the routing, not the backend. The only reads that ignore
//! the default are the ones that ask for the host explicitly.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use maki_agent::fs_backend::HostFs;
use maki_fs::FsBackend;
use mlua::Lua;

/// Injected backend plus the handler threads currently routed to it.
pub(crate) struct BackendRegistry {
    default: Mutex<Arc<dyn FsBackend>>,
    active: Mutex<HashMap<usize, Arc<dyn FsBackend>>>,
}

impl BackendRegistry {
    /// Defaults to the host filesystem so a backend is always configured.
    pub(crate) fn new() -> Self {
        Self {
            default: Mutex::new(host_backend()),
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Install the backend that future routed calls run against.
    pub(crate) fn set_default(&self, backend: Arc<dyn FsBackend>) {
        if let Ok(mut default) = self.default.lock() {
            *default = backend;
        }
    }

    /// Bind this Lua thread pointer to the routed backend. No-op when the
    /// default mutex is poisoned; reports whether the binding landed.
    pub(crate) fn bind(&self, thread_ptr: usize) -> bool {
        let Ok(default) = self.default.lock() else {
            return false;
        };
        let backend = Arc::clone(&default);
        drop(default);
        if let Ok(mut active) = self.active.lock() {
            active.insert(thread_ptr, backend);
            return true;
        }
        false
    }

    pub(crate) fn unbind(&self, thread_ptr: usize) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&thread_ptr);
        }
    }

    fn resolve(&self, lua: &Lua) -> Arc<dyn FsBackend> {
        let thread_ptr = lua.current_thread().to_pointer() as usize;
        self.active
            .lock()
            .ok()
            .and_then(|active| active.get(&thread_ptr).cloned())
            .or_else(|| self.default.lock().ok().map(|default| default.clone()))
            .unwrap_or_else(host_backend)
    }

    /// The backend bound to this thread, if any — the default is not one.
    fn bound(&self, lua: &Lua) -> Option<Arc<dyn FsBackend>> {
        let thread_ptr = lua.current_thread().to_pointer() as usize;
        self.active
            .lock()
            .ok()
            .and_then(|active| active.get(&thread_ptr).cloned())
    }
}

/// The bare host filesystem, shared by everything that has no backend bound:
/// an unbound `maki.fs.*` call, and a job started outside a routed handler.
pub(crate) fn host_backend() -> Arc<dyn FsBackend> {
    static HOST: LazyLock<Arc<dyn FsBackend>> = LazyLock::new(|| Arc::new(HostFs));
    Arc::clone(&HOST)
}

/// The backend a `maki.fs.*` call resolves to: the per-thread binding when the
/// coroutine is routed, otherwise the injected default. Always configured.
pub(crate) fn resolve_backend(lua: &Lua) -> Arc<dyn FsBackend> {
    lua.app_data_ref::<Arc<BackendRegistry>>()
        .map(|registry| registry.resolve(lua))
        .unwrap_or_else(host_backend)
}

/// The backend bound to the coroutine running right now, if any. Used where a
/// sandbox-bound call has a host fallback that only makes sense when unbound.
pub(crate) fn bound_backend(lua: &Lua) -> Option<Arc<dyn FsBackend>> {
    lua.app_data_ref::<Arc<BackendRegistry>>()
        .and_then(|registry| registry.bound(lua))
}
