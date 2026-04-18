use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use maki_agent::tools::ToolRegistry;
use maki_config::LuaPluginsConfig;

use crate::error::PluginError;
use crate::runtime::{self, LuaThread, Request};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

const BUNDLED_INDEX: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../plugins/index/init.lua"
));

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

        let lua = runtime::spawn(registry)?;
        let host = Self { inner: Some(lua) };

        let mut seen: HashSet<Arc<str>> = HashSet::new();

        for builtin in &config.builtins {
            let source = match builtin.as_str() {
                "index" => BUNDLED_INDEX,
                other => {
                    tracing::warn!(builtin = other, "unknown builtin plugin, skipping");
                    continue;
                }
            };
            let name: Arc<str> = Arc::from(builtin.as_str());
            seen.insert(Arc::clone(&name));
            host.load_source_named(name, source.to_owned(), None)?;
        }

        for dir in &config.user_dirs {
            host.load_dir(dir, &mut seen)?;
        }

        Ok(host)
    }

    fn tx(&self) -> Result<&flume::Sender<Request>, PluginError> {
        self.inner
            .as_ref()
            .map(|r| &r.tx)
            .ok_or(PluginError::HostDead)
    }

    fn load_dir(&self, dir: &Path, seen: &mut HashSet<Arc<str>>) -> Result<(), PluginError> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                tracing::debug!(path = %dir.display(), "plugin dir not found, skipping");
                return Ok(());
            }
            Err(e) => {
                return Err(PluginError::Io {
                    path: dir.to_owned(),
                    source: e,
                });
            }
        };

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
            .collect();
        paths.sort();

        for path in paths {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => Arc::from(s),
                None => {
                    tracing::warn!(path = %path.display(), "plugin file has non-UTF8 name, skipping");
                    continue;
                }
            };
            if seen.contains(&stem) {
                return Err(PluginError::DuplicatePlugin {
                    plugin: stem.to_string(),
                });
            }
            seen.insert(Arc::clone(&stem));
            self.load_file(&path, stem)?;
        }
        Ok(())
    }

    fn load_file(&self, path: &Path, name: Arc<str>) -> Result<(), PluginError> {
        let plugin_dir = path.parent().map(Path::to_path_buf);
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        self.load_source_named(name, source, plugin_dir)
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
    use maki_agent::tools::ToolRegistry;
    use maki_config::LuaPluginsConfig;

    use super::*;

    fn enabled_config() -> LuaPluginsConfig {
        LuaPluginsConfig {
            enabled: true,
            builtins: vec![],
            user_dirs: vec![],
        }
    }

    fn disabled_config() -> LuaPluginsConfig {
        LuaPluginsConfig {
            enabled: false,
            builtins: vec![],
            user_dirs: vec![],
        }
    }

    fn fresh_registry() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }

    #[test]
    fn dropping_host_stops_thread() {
        let reg = fresh_registry();
        let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
        host.load_source("noop", "").unwrap();
        let start = std::time::Instant::now();
        drop(host);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "drop took too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn dropping_disabled_host_is_noop() {
        let reg = fresh_registry();
        let host = PluginHost::new(&disabled_config(), Arc::clone(&reg)).unwrap();
        drop(host);
    }
}
