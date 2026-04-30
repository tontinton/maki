use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use include_dir::{Dir, include_dir};
use maki_agent::RawRenderHints;
use maki_agent::tools::ToolRegistry;
use maki_config::{PluginsConfig, RawConfig};

use crate::error::PluginError;
use crate::runtime::{self, LuaThread, Request};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

struct BundledPlugin {
    name: &'static str,
    dir: Dir<'static>,
}

/// `lib` is not a default builtin; it only exists so plugins can
/// `require()` shared modules across plugin boundaries.
static BUNDLED_PLUGINS: &[BundledPlugin] = &[
    BundledPlugin {
        name: "index",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/index"),
    },
    BundledPlugin {
        name: "webfetch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/webfetch"),
    },
    BundledPlugin {
        name: "websearch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/websearch"),
    },
    BundledPlugin {
        name: "bash",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/bash"),
    },
    BundledPlugin {
        name: "lib",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
    },
];

static BUNDLED_DIRS: LazyLock<&'static [&'static Dir<'static>]> = LazyLock::new(|| {
    let dirs: Vec<&'static Dir<'static>> = BUNDLED_PLUGINS.iter().map(|p| &p.dir).collect();
    Vec::leak(dirs)
});

pub struct PluginHost {
    inner: Option<LuaThread>,
    render_hints: Vec<(Arc<str>, RawRenderHints)>,
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(ref mut inner) = self.inner else {
            return;
        };
        let Some(handle) = inner.join.take() else {
            return;
        };
        inner.shutdown.store(true, Ordering::Release);
        let _ = inner.tx.send(Request::Shutdown);
        let (done_tx, done_rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_err());
        });
        match done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => tracing::warn!("lua thread panicked on shutdown"),
            Err(_) => tracing::warn!("lua thread did not stop within timeout, detaching"),
            Ok(false) => {}
        }
    }
}

impl PluginHost {
    pub fn new(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let lua = runtime::spawn(registry, *BUNDLED_DIRS)?;
        Ok(Self {
            inner: Some(lua),
            render_hints: Vec::new(),
        })
    }

    pub fn disabled() -> Self {
        Self {
            inner: None,
            render_hints: Vec::new(),
        }
    }

    pub fn load_init_files(&self, cwd: &Path) -> Result<Option<RawConfig>, PluginError> {
        let mut merged: Option<RawConfig> = None;

        for global_dir in maki_config::global_config_dirs() {
            self.run_init_file(&global_dir.join("init.lua"), "global/init.lua", &mut merged)?;
            if merged.is_some() {
                break;
            }
        }
        self.run_init_file(&cwd.join(".maki/init.lua"), "project/init.lua", &mut merged)?;

        Ok(merged)
    }

    fn run_init_file(
        &self,
        path: &Path,
        label: &str,
        merged: &mut Option<RawConfig>,
    ) -> Result<(), PluginError> {
        if !path.is_file() {
            return Ok(());
        }
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        if let Some(raw) = self.send_run_init_lua(source, label.to_owned(), plugin_dir)? {
            match merged {
                Some(existing) => existing.merge(raw),
                None => *merged = Some(raw),
            }
        }
        Ok(())
    }

    pub fn load_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        for builtin in &config.tools {
            let dir = match BUNDLED_PLUGINS.iter().find(|p| p.name == builtin.as_str()) {
                Some(p) => &p.dir,
                None => {
                    tracing::warn!(
                        builtin = builtin.as_str(),
                        "unknown builtin plugin, skipping"
                    );
                    continue;
                }
            };
            let init = dir
                .get_file("init.lua")
                .and_then(|f| f.contents_utf8())
                .ok_or_else(|| PluginError::Lua {
                    plugin: builtin.clone(),
                    source: mlua::Error::runtime("bundled plugin missing init.lua"),
                })?;
            let name: Arc<str> = Arc::from(builtin.as_str());
            self.load_source_named(name, init.to_owned(), None)?;
        }
        Ok(())
    }

    fn tx(&self) -> Result<&flume::Sender<Request>, PluginError> {
        self.inner
            .as_ref()
            .map(|r| &r.tx)
            .ok_or(PluginError::HostDead)
    }

    fn send_load(
        &self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Vec<(Arc<str>, RawRenderHints)>, PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::LoadSource {
            name,
            source,
            plugin_dir,
            reply: reply_tx,
        })
        .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    pub fn send_run_init_lua(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::RunInitLua {
            source,
            source_name,
            plugin_dir,
            reply: reply_tx,
        })
        .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    fn load_source_named(
        &mut self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<(), PluginError> {
        let hints = self.send_load(name, source, plugin_dir)?;
        self.render_hints.extend(hints);
        Ok(())
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::ClearPlugin {
            plugin: Arc::from(plugin),
            reply: reply_tx,
        })
        .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?;
        Ok(())
    }

    pub fn load_source(&self, name: &str, source: &str) -> Result<(), PluginError> {
        self.send_load(Arc::from(name), source.to_owned(), None)?;
        Ok(())
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        self.send_load(Arc::from("user"), source, plugin_dir)?;
        Ok(())
    }

    pub fn drain_render_hints(&mut self) -> Vec<(Arc<str>, RawRenderHints)> {
        std::mem::take(&mut self.render_hints)
    }

    pub fn event_handle(&self) -> Option<EventHandle> {
        self.inner
            .as_ref()
            .map(|t| EventHandle { tx: t.tx.clone() })
    }
}

#[derive(Clone)]
pub struct EventHandle {
    tx: flume::Sender<Request>,
}

impl EventHandle {
    pub fn fire_click(&self, tool_id: &str, row: u32) {
        let _ = self.tx.try_send(Request::FireBufClick {
            tool_id: tool_id.to_owned(),
            row,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::tools::ToolRegistry;

    #[test]
    fn disabled_is_noop() {
        let reg = Arc::new(ToolRegistry::new());
        let names_before = reg.names();
        let _host = PluginHost::disabled();
        assert_eq!(reg.names(), names_before);
    }

    #[test]
    fn fire_click_sends_request_through_channel() {
        let (tx, rx) = flume::bounded(8);
        let handle = EventHandle { tx };
        handle.fire_click("tool_42", 7);
        let req = rx.try_recv().unwrap();
        match req {
            Request::FireBufClick { tool_id, row } => {
                assert_eq!(tool_id, "tool_42");
                assert_eq!(row, 7);
            }
            _ => panic!("expected FireBufClick"),
        }
    }
}
