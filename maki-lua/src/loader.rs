use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use include_dir::{Dir, include_dir};
use maki_agent::tools::ToolRegistry;
use maki_config::LuaPluginsConfig;

use crate::error::PluginError;
use crate::runtime::{self, LuaThread, Request};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

static BUNDLED_INDEX_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../plugins/index");
static BUNDLED_DIRS: &[&Dir] = &[&BUNDLED_INDEX_DIR];

pub struct PluginHost {
    inner: Option<LuaThread>,
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
    pub fn new(
        config: &LuaPluginsConfig,
        registry: Arc<ToolRegistry>,
    ) -> Result<Self, PluginError> {
        if !config.enabled {
            return Ok(Self { inner: None });
        }

        let lua = runtime::spawn(registry, BUNDLED_DIRS)?;
        let host = Self { inner: Some(lua) };

        for builtin in &config.builtins {
            let dir = match builtin.as_str() {
                "index" => &BUNDLED_INDEX_DIR,
                other => {
                    tracing::warn!(builtin = other, "unknown builtin plugin, skipping");
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
            host.load_source_named(name, init.to_owned(), None)?;
        }

        if let Some(ref init_path) = config.init_file {
            let source = fs::read_to_string(init_path).map_err(|e| PluginError::Io {
                path: init_path.clone(),
                source: e,
            })?;
            let plugin_dir = init_path.parent().map(Path::to_path_buf);
            host.load_source_named(Arc::from("user"), source, plugin_dir)?;
        }

        Ok(host)
    }

    fn tx(&self) -> Result<&flume::Sender<Request>, PluginError> {
        self.inner
            .as_ref()
            .map(|r| &r.tx)
            .ok_or(PluginError::HostDead)
    }

    fn load_source_named(
        &self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<(), PluginError> {
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
        self.load_source_named(Arc::from(name), source.to_owned(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::tools::ToolRegistry;
    use maki_config::LuaPluginsConfig;

    #[test]
    fn new_with_disabled_config_is_noop() {
        let reg = Arc::new(ToolRegistry::new());
        let names_before = reg.names();
        let config = LuaPluginsConfig {
            enabled: false,
            builtins: vec![],
            init_file: None,
        };
        let _host = PluginHost::new(&config, reg.clone()).unwrap();
        assert_eq!(reg.names(), names_before);
    }
}
