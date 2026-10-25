//! What `maki.api.list_plugins` reports beyond what the live registries hold:
//! how each plugin's load went, and the last runtime errors it raised.
//!
//! Shared between the host thread, which sees loads that fail before any Lua
//! runs, and the Lua thread, which sees everything after.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use maki_agent::tools::{ToolRegistry, ToolSource};
use mlua::{Function, Lua};
use serde::Serialize;
use strum::{IntoStaticStr, VariantNames};

use crate::api::autocmd::AutocmdStore;
use crate::api::util::command::CommandHandlerMap;
use crate::runtime::plugin_of;

/// Enough to see what keeps failing without letting a plugin that errors on
/// every keystroke grow without bound.
pub(crate) const ERROR_RING: usize = 20;
const CALLBACK_FAILED_MSG: &str = "plugin callback failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PluginKind {
    Bundled,
    Package,
    Init,
}

/// Where a runtime error surfaced, as `maki.api.list_plugins` reports it in
/// `seam`. A layer failing on a slot reports the slot's name instead
/// ([`record_slot_error`]). The snake_case names are public API: plugins
/// filter on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ErrorSeam {
    Autocmd,
    Command,
    Keybind,
    DeferFn,
    Async,
    Job,
    CancelHook,
    Hint,
    Header,
    Describe,
    PermissionScopes,
    Start,
    Restore,
    Click,
    BufChange,
    PlanRow,
    Notify,
    Watchdog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PluginErrorEntry {
    pub t: u64,
    pub seam: String,
    pub msg: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PluginStatus {
    pub source: PluginKind,
    pub path: Option<PathBuf>,
    pub enabled: bool,
    pub loaded: bool,
    pub load_error: Option<String>,
    pub errors: VecDeque<PluginErrorEntry>,
}

impl PluginStatus {
    fn new(source: PluginKind) -> Self {
        Self {
            source,
            path: None,
            enabled: true,
            loaded: false,
            load_error: None,
            errors: VecDeque::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PluginStatusStore(Arc<Mutex<BTreeMap<String, PluginStatus>>>);

impl PluginStatusStore {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, PluginStatus>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entry<'a>(
        map: &'a mut BTreeMap<String, PluginStatus>,
        name: &str,
        source: PluginKind,
    ) -> &'a mut PluginStatus {
        map.entry(name.to_owned())
            .or_insert_with(|| PluginStatus::new(source))
    }

    /// Kept errors survive a reload, so a plugin that failed and was fixed
    /// still shows what it did before.
    pub fn record_load(
        &self,
        name: &str,
        source: PluginKind,
        path: Option<PathBuf>,
        result: Result<(), String>,
    ) {
        let mut map = self.lock();
        let status = Self::entry(&mut map, name, source);
        status.source = source;
        if path.is_some() {
            status.path = path;
        }
        status.enabled = true;
        match result {
            Ok(()) => {
                status.loaded = true;
                status.load_error = None;
            }
            Err(msg) => {
                status.loaded = false;
                status.load_error = Some(msg);
            }
        }
    }

    /// An installed plugin that is not loading, either because the config
    /// turned it off or because nothing activated an `opt/` package.
    pub fn record_idle(
        &self,
        name: &str,
        source: PluginKind,
        path: Option<PathBuf>,
        enabled: bool,
    ) {
        let mut map = self.lock();
        let status = Self::entry(&mut map, name, source);
        if status.loaded || status.load_error.is_some() {
            return;
        }
        status.source = source;
        if path.is_some() {
            status.path = path;
        }
        status.enabled = enabled;
    }

    pub fn record_unload(&self, name: &str) {
        if let Some(status) = self.lock().get_mut(name) {
            status.loaded = false;
        }
    }

    pub fn record_error(&self, name: &str, seam: &str, msg: String) {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut map = self.lock();
        let source = if crate::loader::is_bundled(name) {
            PluginKind::Bundled
        } else {
            PluginKind::Package
        };
        let errors = &mut Self::entry(&mut map, name, source).errors;
        if errors.len() == ERROR_RING {
            errors.pop_front();
        }
        errors.push_back(PluginErrorEntry {
            t,
            seam: seam.to_owned(),
            msg,
        });
    }

    pub fn snapshot(&self) -> BTreeMap<String, PluginStatus> {
        self.lock().clone()
    }
}

/// Logs a runtime error and records it against `plugin`. The one place every
/// callback failure goes through, so a new hook cannot log without also
/// showing up in `list_plugins`. A runtime without the store (some unit
/// tests) only logs.
pub(crate) fn record_error(lua: &Lua, plugin: &str, seam: ErrorSeam, err: impl Display) {
    record_at(lua, plugin, seam.into(), err);
}

/// [`record_error`] for a slot layer, whose seam is the slot it sits on.
pub(crate) fn record_slot_error(lua: &Lua, plugin: &str, slot: &str, err: impl Display) {
    record_at(lua, plugin, slot, err);
}

/// [`record_error`] for a callback nobody wrote an owner down for: the
/// plugin is the one whose env `func` closes over. A function from no
/// plugin env (host code) is only logged.
pub(crate) fn record_callback_error(
    lua: &Lua,
    func: &Function,
    seam: ErrorSeam,
    err: impl Display,
) {
    match plugin_of(lua, func) {
        Some(plugin) => record_at(lua, &plugin, seam.into(), err),
        None => tracing::warn!(seam = <&str>::from(seam), error = %err, CALLBACK_FAILED_MSG),
    }
}

fn record_at(lua: &Lua, plugin: &str, seam: &str, err: impl Display) {
    tracing::warn!(plugin, seam, error = %err, CALLBACK_FAILED_MSG);
    if let Some(store) = lua.app_data_ref::<PluginStatusStore>() {
        store.record_error(plugin, seam, err.to_string());
    }
}

/// One `maki.api.list_plugins` entry.
#[derive(Debug, Serialize)]
pub(crate) struct PluginListing {
    pub name: String,
    pub source: PluginKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub enabled: bool,
    pub loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
    pub errors: Vec<PluginErrorEntry>,
    pub commands: Vec<String>,
    pub tools: Vec<String>,
    pub autocmds: usize,
}

/// Every plugin the store or a live registry knows about, by name. A plugin
/// that registered something but has no recorded load (a runtime started
/// without the store) is reported as loaded, since its registrations are.
pub(crate) fn plugin_listing(lua: &Lua) -> Vec<PluginListing> {
    let mut commands: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(map) = lua.app_data_ref::<CommandHandlerMap>() {
        for (plugin, cmds) in map.iter() {
            let names = commands.entry(plugin.to_string()).or_default();
            names.extend(cmds.keys().map(ToString::to_string));
            names.sort();
        }
    }
    let mut tools: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(registry) = lua.app_data_ref::<Arc<ToolRegistry>>() {
        for entry in registry.iter().iter() {
            if let ToolSource::Lua { plugin } = &entry.source {
                tools
                    .entry(plugin.to_string())
                    .or_default()
                    .push(entry.name().to_owned());
            }
        }
    }
    let mut autocmds: BTreeMap<String, usize> = BTreeMap::new();
    if let Some(store) = lua.app_data_ref::<AutocmdStore>() {
        for entry in store.listeners.values().flatten() {
            *autocmds.entry(entry.plugin.to_string()).or_default() += 1;
        }
    }
    let mut statuses = lua
        .app_data_ref::<PluginStatusStore>()
        .map(|store| store.snapshot())
        .unwrap_or_default();
    for name in commands.keys().chain(tools.keys()).chain(autocmds.keys()) {
        statuses.entry(name.clone()).or_insert_with(|| {
            let source = if crate::loader::is_bundled(name) {
                PluginKind::Bundled
            } else {
                PluginKind::Package
            };
            PluginStatus {
                loaded: true,
                ..PluginStatus::new(source)
            }
        });
    }
    statuses
        .into_iter()
        .map(|(name, status)| PluginListing {
            commands: commands.remove(&name).unwrap_or_default(),
            tools: tools.remove(&name).unwrap_or_default(),
            autocmds: autocmds.remove(&name).unwrap_or_default(),
            source: status.source,
            path: status.path.map(|p| p.display().to_string()),
            enabled: status.enabled,
            loaded: status.loaded,
            load_error: status.load_error,
            errors: status.errors.into(),
            name,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_errors() {
        let store = PluginStatusStore::default();
        for i in 0..ERROR_RING + 5 {
            store.record_error("p", ErrorSeam::Autocmd.into(), format!("e{i}"));
        }
        let errors = &store.snapshot()["p"].errors;
        assert_eq!(errors.len(), ERROR_RING);
        assert_eq!(errors.front().unwrap().msg, "e5");
        assert_eq!(errors.back().unwrap().msg, format!("e{}", ERROR_RING + 4));
    }

    #[test]
    fn a_later_success_clears_the_load_error_but_keeps_runtime_errors() {
        let store = PluginStatusStore::default();
        store.record_error("p", ErrorSeam::Command.into(), "boom".into());
        store.record_load("p", PluginKind::Package, None, Err("bad".into()));
        store.record_load("p", PluginKind::Package, None, Ok(()));
        let status = &store.snapshot()["p"];
        assert!(status.loaded);
        assert_eq!(status.load_error, None);
        assert_eq!(status.errors.len(), 1);
    }

    #[test]
    fn idle_does_not_override_a_load() {
        let store = PluginStatusStore::default();
        store.record_load("p", PluginKind::Package, None, Ok(()));
        store.record_idle("p", PluginKind::Package, None, false);
        assert!(store.snapshot()["p"].enabled);
    }
}
