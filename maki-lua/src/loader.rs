use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use include_dir::{Dir, include_dir};
use maki_agent::RawRenderHints;
use maki_agent::tools::ToolRegistry;
use maki_config::PluginsConfig;

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
    pub fn new(config: &PluginsConfig, registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        if !config.enabled {
            return Ok(Self {
                inner: None,
                render_hints: Vec::new(),
            });
        }

        let lua = runtime::spawn(registry, *BUNDLED_DIRS)?;
        let mut host = Self {
            inner: Some(lua),
            render_hints: Vec::new(),
        };

        for builtin in &config.builtins {
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

    pub fn drain_render_hints(&mut self) -> Vec<(Arc<str>, RawRenderHints)> {
        std::mem::take(&mut self.render_hints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::tools::ToolRegistry;
    use maki_config::PluginsConfig;

    #[test]
    fn new_with_disabled_config_is_noop() {
        let reg = Arc::new(ToolRegistry::new());
        let names_before = reg.names();
        let config = PluginsConfig {
            enabled: false,
            builtins: vec![],
            init_file: None,
        };
        let _host = PluginHost::new(&config, reg.clone()).unwrap();
        assert_eq!(reg.names(), names_before);
    }
}
