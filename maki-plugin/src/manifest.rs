use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::PluginError;

const MANIFEST_DIR: &str = ".claude-plugin";
const MANIFEST_FILE: &str = "plugin.json";

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
}

pub fn manifest_path(plugin_root: &Path) -> std::path::PathBuf {
    plugin_root.join(MANIFEST_DIR).join(MANIFEST_FILE)
}

pub fn load_manifest(plugin_root: &Path) -> Result<PluginManifest, PluginError> {
    let path = manifest_path(plugin_root);
    let content = fs::read_to_string(&path).map_err(|e| {
        PluginError::InvalidManifest {
            path: path.clone(),
            reason: e.to_string(),
        }
    })?;
    let manifest: PluginManifest =
        serde_json::from_str(&content).map_err(|e| PluginError::InvalidManifest {
            path,
            reason: e.to_string(),
        })?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const VALID_MANIFEST: &str = r#"{
        "name": "test-plugin",
        "version": "1.0.0",
        "description": "A test plugin",
        "skills": ["skills/"],
        "commands": ["commands/"]
    }"#;

    #[test]
    fn load_valid_manifest() {
        let dir = TempDir::new().unwrap();
        let manifest_dir = dir.path().join(".claude-plugin");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(manifest_dir.join("plugin.json"), VALID_MANIFEST).unwrap();

        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.name, "test-plugin");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.skills, vec!["skills/"]);
    }

    #[test]
    fn load_missing_manifest_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = load_manifest(dir.path());
        assert!(matches!(result, Err(PluginError::InvalidManifest { .. })));
    }
}
