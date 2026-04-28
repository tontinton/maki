use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::PluginError;

const PLUGINS_DIR: &str = "plugins";
const STATE_FILE: &str = "plugins.toml";
const CACHE_DIR: &str = "cache";
const MARKETPLACES_DIR: &str = "marketplaces";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PluginScope {
    User,
    Project,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub marketplace: String,
    #[serde(default)]
    pub version: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_scope")]
    pub scope: PluginScope,
}

fn default_enabled() -> bool {
    true
}

fn default_scope() -> PluginScope {
    PluginScope::User
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginState {
    #[serde(default)]
    pub marketplace: Vec<MarketplaceEntry>,
    #[serde(default)]
    pub plugin: Vec<PluginEntry>,
}

impl PluginState {
    pub fn find_marketplace(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.marketplace.iter().find(|m| m.name == name)
    }

    pub fn find_plugin(&self, name: &str) -> Option<&PluginEntry> {
        self.plugin.iter().find(|p| p.name == name)
    }

    pub fn find_plugin_mut(&mut self, name: &str) -> Option<&mut PluginEntry> {
        self.plugin.iter_mut().find(|p| p.name == name)
    }
}

pub fn plugins_dir(home: &Path) -> PathBuf {
    home.join(PLUGINS_DIR)
}

pub fn state_file(home: &Path) -> PathBuf {
    plugins_dir(home).join(STATE_FILE)
}

pub fn cache_dir(home: &Path) -> PathBuf {
    plugins_dir(home).join(CACHE_DIR)
}

pub fn marketplaces_dir(home: &Path) -> PathBuf {
    plugins_dir(home).join(MARKETPLACES_DIR)
}

pub fn plugin_cache_path(home: &Path, plugin_name: &str, marketplace_name: &str) -> PathBuf {
    cache_dir(home).join(format!("{plugin_name}--{marketplace_name}"))
}

pub fn load_state(home: &Path) -> Result<PluginState, PluginError> {
    let path = state_file(home);
    if !path.exists() {
        debug!("no plugins.toml found, returning default state");
        return Ok(PluginState::default());
    }
    let content = fs::read_to_string(&path)?;
    let state: PluginState = toml::from_str(&content)?;
    Ok(state)
}

pub fn save_state(home: &Path, state: &PluginState) -> Result<(), PluginError> {
    let dir = plugins_dir(home);
    fs::create_dir_all(&dir)?;
    let content = toml::to_string_pretty(state)?;
    maki_storage::atomic_write(&state_file(home), content.as_bytes())?;
    debug!("saved plugin state");
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn load_missing_state_returns_default() {
        let dir = TempDir::new().unwrap();
        let state = load_state(dir.path()).unwrap();
        assert!(state.marketplace.is_empty());
        assert!(state.plugin.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let state = PluginState {
            marketplace: vec![MarketplaceEntry {
                name: "my-plugins".into(),
                source: "owner/repo".into(),
            }],
            plugin: vec![PluginEntry {
                name: "test-plugin".into(),
                marketplace: "my-plugins".into(),
                version: "1.0.0".into(),
                enabled: true,
                scope: PluginScope::User,
            }],
        };
        save_state(dir.path(), &state).unwrap();
        let loaded = load_state(dir.path()).unwrap();
        assert_eq!(loaded.marketplace.len(), 1);
        assert_eq!(loaded.plugin.len(), 1);
        assert_eq!(loaded.plugin[0].name, "test-plugin");
    }

    #[test]
    fn find_marketplace_by_name() {
        let state = PluginState {
            marketplace: vec![MarketplaceEntry {
                name: "my-plugins".into(),
                source: "owner/repo".into(),
            }],
            plugin: vec![],
        };
        assert!(state.find_marketplace("my-plugins").is_some());
        assert!(state.find_marketplace("nonexistent").is_none());
    }
}
