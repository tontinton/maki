use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use flume::Sender;
use serde::Deserialize;
use serde_json::Value;
use strum::IntoEnumIterator;
use tracing::{debug, warn};

use crate::model::{Model, ModelPricing, ModelTier, models_for_provider};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::anthropic::Anthropic;
use super::copilot::Copilot;
use super::deepseek::DeepSeek;
use super::google::Google;
use super::mistral::Mistral;
use super::ollama::Ollama;
use super::openai::OpenAi;
use super::synthetic::Synthetic;
use super::zai::{Zai, ZaiPlan};

const INFO_TIMEOUT: Duration = Duration::from_secs(5);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDERS_DIR: &str = "providers";

struct DynamicProviderMeta {
    slug: String,
    display_name: String,
    base: ProviderKind,
    system_prefix: Option<String>,
    has_auth: bool,
    script_path: PathBuf,
    models: Vec<ScriptModel>,
}

#[derive(Deserialize)]
struct ScriptInfo {
    display_name: String,
    base: String,
    #[serde(default)]
    system_prefix: Option<String>,
    has_auth: bool,
}

#[derive(Deserialize)]
struct ScriptModel {
    id: String,
    #[serde(default = "default_tier")]
    tier: ModelTier,
    #[serde(default)]
    supports_tool_examples: Option<bool>,
    #[serde(default = "default_max_output_tokens")]
    max_output_tokens: u32,
    #[serde(default = "default_context_window")]
    context_window: u32,
    #[serde(default)]
    pricing: Option<ModelPricing>,
}

fn default_tier() -> ModelTier {
    ModelTier::Medium
}

fn default_max_output_tokens() -> u32 {
    16384
}

fn default_context_window() -> u32 {
    128_000
}

#[derive(Deserialize)]
struct ScriptResolvedAuth {
    base_url: Option<String>,
    headers: HashMap<String, String>,
}

impl From<ScriptResolvedAuth> for ResolvedAuth {
    fn from(s: ScriptResolvedAuth) -> Self {
        Self {
            base_url: s.base_url,
            headers: s.headers.into_iter().collect(),
        }
    }
}

fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn builtin_slugs() -> Vec<String> {
    ProviderKind::iter().map(|k| k.to_string()).collect()
}

fn providers_dir() -> Option<PathBuf> {
    maki_storage::paths::config_dir()
        .ok()
        .map(|d| d.join(PROVIDERS_DIR))
}

fn run_script(path: &Path, subcommand: &str, timeout: Duration) -> Result<String, AgentError> {
    let mut child = Command::new(path)
        .arg(subcommand)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AgentError::Config {
            message: format!("failed to run {} {subcommand}: {e}", path.display()),
        })?;

    let output = match wait_timeout::ChildExt::wait_timeout(&mut child, timeout) {
        Ok(Some(_)) => child.wait_with_output().map_err(|e| AgentError::Config {
            message: format!(
                "failed to read output of {} {subcommand}: {e}",
                path.display()
            ),
        })?,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AgentError::Config {
                message: format!(
                    "{} {subcommand} timed out after {}s",
                    path.display(),
                    timeout.as_secs()
                ),
            });
        }
        Err(e) => {
            return Err(AgentError::Config {
                message: format!("failed to wait on {} {subcommand}: {e}", path.display()),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AgentError::Config {
            message: if stderr.is_empty() {
                format!(
                    "{} {subcommand} exited with {}",
                    path.display(),
                    output.status
                )
            } else {
                stderr
            },
        });
    }

    String::from_utf8(output.stdout).map_err(|_| AgentError::Config {
        message: format!("{} {subcommand}: stdout is not valid UTF-8", path.display()),
    })
}

fn run_script_interactive(path: &Path, subcommand: &str) -> Result<(), AgentError> {
    let status = Command::new(path)
        .arg(subcommand)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| AgentError::Config {
            message: format!("failed to run {} {subcommand}: {e}", path.display()),
        })?;

    if !status.success() {
        return Err(AgentError::Config {
            message: format!("{} {subcommand} exited with {status}", path.display()),
        });
    }
    Ok(())
}

fn resolve_auth(meta: &DynamicProviderMeta) -> Result<ResolvedAuth, AgentError> {
    let stdout = run_script(&meta.script_path, "resolve", SCRIPT_TIMEOUT)?;
    let parsed: ScriptResolvedAuth =
        serde_json::from_str(&stdout).map_err(|e| AgentError::Config {
            message: format!("{} resolve: invalid JSON: {e}", meta.script_path.display()),
        })?;
    Ok(parsed.into())
}

fn discover_in(dir: &Path) -> Vec<DynamicProviderMeta> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let builtins = builtin_slugs();
    let mut result = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = path.metadata()
                && meta.permissions().mode() & 0o111 == 0
            {
                debug!(path = %path.display(), "skipping non-executable file");
                continue;
            }
        }

        #[cfg(windows)]
        {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext = ext.to_ascii_lowercase();
                if !matches!(ext.as_str(), "exe" | "bat" | "cmd" | "ps1") {
                    debug!(path = %path.display(), "skipping non-executable file");
                    continue;
                }
            } else {
                debug!(path = %path.display(), "skipping file without extension");
                continue;
            }
        }

        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        if !is_valid_slug(&slug) {
            warn!(slug, "invalid provider slug, skipping");
            continue;
        }

        if builtins.iter().any(|b| b == &slug) {
            warn!(slug, "slug collides with built-in provider, skipping");
            continue;
        }

        let stdout = match run_script(&path, "info", INFO_TIMEOUT) {
            Ok(s) => s,
            Err(e) => {
                warn!(slug, error = %e, "failed to get provider info, skipping");
                continue;
            }
        };

        let info: ScriptInfo = match serde_json::from_str(&stdout) {
            Ok(i) => i,
            Err(e) => {
                warn!(slug, error = %e, "invalid info JSON, skipping");
                continue;
            }
        };

        let base = match ProviderKind::from_str(&info.base) {
            Ok(k) => k,
            Err(_) => {
                warn!(slug, base = info.base, "unknown base provider, skipping");
                continue;
            }
        };

        let models = match run_script(&path, "models", INFO_TIMEOUT) {
            Ok(s) => serde_json::from_str::<Vec<ScriptModel>>(&s).unwrap_or_else(|e| {
                warn!(slug, error = %e, "invalid models JSON, falling back to base models");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };

        result.push(DynamicProviderMeta {
            slug,
            display_name: info.display_name,
            base,
            system_prefix: info.system_prefix.filter(|s| !s.is_empty()),
            has_auth: info.has_auth,
            script_path: path,
            models,
        });
    }

    result
}

static DISCOVERED: OnceLock<Vec<DynamicProviderMeta>> = OnceLock::new();

fn discover() -> &'static [DynamicProviderMeta] {
    DISCOVERED.get_or_init(|| providers_dir().map(|d| discover_in(&d)).unwrap_or_default())
}

fn find_meta(slug: &str) -> Option<&'static DynamicProviderMeta> {
    discover().iter().find(|m| m.slug == slug)
}

pub fn login(slug: &str) -> Result<(), AgentError> {
    let meta = find_meta(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown provider '{slug}'"),
    })?;
    if !meta.has_auth {
        return Err(AgentError::Config {
            message: format!("provider '{}' does not support login (uses API key)", slug),
        });
    }
    run_script_interactive(&meta.script_path, "login")
}

pub fn logout(slug: &str) -> Result<(), AgentError> {
    let meta = find_meta(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown provider '{slug}'"),
    })?;
    if !meta.has_auth {
        return Err(AgentError::Config {
            message: format!("provider '{}' does not support logout (uses API key)", slug),
        });
    }
    run_script_interactive(&meta.script_path, "logout")
}

pub fn auth_providers() -> Vec<(&'static str, &'static str)> {
    discover()
        .iter()
        .filter(|m| m.has_auth)
        .map(|m| (m.slug.as_str(), m.display_name.as_str()))
        .collect()
}

pub fn create(slug: &str, timeouts: super::Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let meta = find_meta(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown dynamic provider '{slug}'"),
    })?;
    let resolved = resolve_auth(meta)?;
    let auth = Arc::new(Mutex::new(resolved));

    let inner: Box<dyn Provider> = match meta.base {
        ProviderKind::Anthropic => Box::new(
            Anthropic::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::OpenAi => Box::new(
            OpenAi::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Google => Box::new(Google::with_auth(auth.clone(), timeouts)),
        ProviderKind::Copilot => Box::new(
            Copilot::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Ollama => Box::new(
            Ollama::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Mistral => Box::new(
            Mistral::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Zai => Box::new(
            Zai::with_auth(ZaiPlan::Standard, auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::ZaiCodingPlan => Box::new(
            Zai::with_auth(ZaiPlan::Coding, auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Synthetic => Box::new(
            Synthetic::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::DeepSeek => Box::new(
            DeepSeek::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
    };

    Ok(Box::new(DynamicProvider {
        script_path: &meta.script_path,
        inner,
        auth,
    }))
}

pub fn display_name(slug: &str) -> Option<&'static str> {
    find_meta(slug).map(|m| m.display_name.as_str())
}

pub fn dynamic_model_specs() -> Vec<String> {
    let mut specs = Vec::new();
    for meta in discover() {
        if meta.models.is_empty() {
            for entry in models_for_provider(meta.base) {
                for prefix in entry.prefixes {
                    specs.push(format!("{}/{prefix}", meta.slug));
                }
            }
        } else {
            for m in &meta.models {
                specs.push(format!("{}/{}", meta.slug, m.id));
            }
        }
    }
    specs
}

pub fn base_for_slug(slug: &str) -> Option<ProviderKind> {
    find_meta(slug).map(|m| m.base)
}

pub fn lookup_model(slug: &str, model_id: &str) -> Option<Model> {
    let meta = find_meta(slug)?;
    let script_model = meta.models.iter().find(|m| model_id.starts_with(&m.id))?;
    Some(Model {
        id: model_id.to_string(),
        provider: meta.base,
        dynamic_slug: Some(slug.to_string()),
        tier: script_model.tier,
        family: meta.base.family(),
        supports_tool_examples_override: script_model.supports_tool_examples,
        pricing: script_model.pricing.clone().unwrap_or_default(),
        max_output_tokens: script_model.max_output_tokens,
        context_window: script_model.context_window,
    })
}

pub fn find_model_for_tier(slug: &str, tier: ModelTier) -> Option<Model> {
    let meta = find_meta(slug)?;
    let script_model = meta.models.iter().find(|m| m.tier == tier)?;
    Some(Model {
        id: script_model.id.clone(),
        provider: meta.base,
        dynamic_slug: Some(slug.to_string()),
        tier,
        family: meta.base.family(),
        supports_tool_examples_override: script_model.supports_tool_examples,
        pricing: script_model.pricing.clone().unwrap_or_default(),
        max_output_tokens: script_model.max_output_tokens,
        context_window: script_model.context_window,
    })
}

struct DynamicProvider {
    script_path: &'static Path,
    inner: Box<dyn Provider>,
    auth: Arc<Mutex<ResolvedAuth>>,
}

impl DynamicProvider {
    fn run_auth_script(&self, subcommand: &'static str) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let script_path = self.script_path;
            let auth = self.auth.clone();
            smol::unblock(move || {
                let stdout = run_script(script_path, subcommand, SCRIPT_TIMEOUT)?;
                let parsed: ScriptResolvedAuth =
                    serde_json::from_str(&stdout).map_err(|e| AgentError::Config {
                        message: format!(
                            "{} {subcommand}: invalid JSON: {e}",
                            script_path.display()
                        ),
                    })?;
                *auth.lock().unwrap() = parsed.into();
                Ok(())
            })
            .await
        })
    }
}

impl Provider for DynamicProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        self.inner.stream_message(
            model, messages, system, tools, event_tx, thinking, session_id,
        )
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        self.inner.list_models()
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.run_auth_script("refresh")
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.run_auth_script("resolve")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use tempfile::TempDir;
    use test_case::test_case;

    #[test_case("myslug", true ; "valid_simple")]
    #[test_case("my-slug", true ; "valid_hyphen")]
    #[test_case("my_slug", true ; "valid_underscore")]
    #[test_case("A1", true ; "valid_upper")]
    #[test_case("", false ; "empty")]
    #[test_case("-bad", false ; "leading_hyphen")]
    #[test_case("has.dot", false ; "has_dot")]
    #[test_case("has/slash", false ; "has_slash")]
    #[test_case("has space", false ; "has_space")]
    fn slug_validation(input: &str, expected: bool) {
        assert_eq!(is_valid_slug(input), expected);
    }

    #[test]
    fn script_resolved_auth_deserialization() {
        let with_base =
            r#"{"base_url": "https://example.com", "headers": {"authorization": "Bearer tok"}}"#;
        let resolved: ResolvedAuth = serde_json::from_str::<ScriptResolvedAuth>(with_base)
            .unwrap()
            .into();
        assert_eq!(resolved.base_url.as_deref(), Some("https://example.com"));
        assert_eq!(resolved.headers[0].1, "Bearer tok");

        let without_base = r#"{"headers": {"authorization": "Bearer x"}}"#;
        let resolved: ResolvedAuth = serde_json::from_str::<ScriptResolvedAuth>(without_base)
            .unwrap()
            .into();
        assert!(resolved.base_url.is_none());
    }

    #[test]
    fn script_info_deserialization() {
        let minimal = r#"{"display_name": "Test", "base": "anthropic", "has_auth": true}"#;
        let info: ScriptInfo = serde_json::from_str(minimal).unwrap();
        assert_eq!(info.display_name, "Test");
        assert_eq!(info.base, "anthropic");
        assert!(info.has_auth);
        assert!(info.system_prefix.is_none());

        let with_prefix = r#"{"display_name": "T", "base": "openai", "has_auth": false, "system_prefix": "You are X."}"#;
        let info: ScriptInfo = serde_json::from_str(with_prefix).unwrap();
        assert_eq!(info.system_prefix.as_deref(), Some("You are X."));
    }

    #[test]
    fn script_model_deserialization() {
        let full = r#"{"id": "my-model", "tier": "strong", "supports_tool_examples": true, "max_output_tokens": 32000, "context_window": 200000, "pricing": {"input": 3.0, "output": 15.0, "cache_write": 3.75, "cache_read": 0.30}}"#;
        let model: ScriptModel = serde_json::from_str(full).unwrap();
        assert_eq!(model.id, "my-model");
        assert_eq!(model.tier, ModelTier::Strong);
        assert_eq!(model.supports_tool_examples, Some(true));
        assert!(model.pricing.is_some());

        let minimal: ScriptModel = serde_json::from_str(r#"{"id": "custom-v1"}"#).unwrap();
        assert_eq!(minimal.tier, ModelTier::Medium);
        assert_eq!(minimal.supports_tool_examples, None);
        assert_eq!(minimal.max_output_tokens, 16384);
        assert_eq!(minimal.context_window, 128_000);
        assert!(minimal.pricing.is_none());
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, info_json: &str) -> PathBuf {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  info) echo '{info_json}' ;;\n  resolve) echo '{{\"headers\": {{\"authorization\": \"Bearer test\"}}}}' ;;\n  refresh) echo '{{\"headers\": {{\"authorization\": \"Bearer refreshed\"}}}}' ;;\n  *) exit 1 ;;\nesac\n"
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn discover_finds_valid_script() {
        let tmp = TempDir::new().unwrap();
        write_script(
            tmp.path(),
            "test-provider",
            r#"{"display_name": "Test", "base": "anthropic", "has_auth": true}"#,
        );
        let providers = discover_in(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].slug, "test-provider");
        assert_eq!(providers[0].display_name, "Test");
        assert_eq!(providers[0].base, ProviderKind::Anthropic);
        assert!(providers[0].has_auth);
        assert!(providers[0].models.is_empty());
    }

    #[cfg(unix)]
    #[test_case("anthropic", r#"{"display_name": "Fake", "base": "anthropic", "has_auth": false}"# ; "builtin_collision")]
    #[test_case("has.dot", r#"{"display_name": "Bad", "base": "anthropic", "has_auth": false}"# ; "invalid_slug")]
    #[test_case("weird", r#"{"display_name": "Weird", "base": "unknown-provider", "has_auth": false}"# ; "unknown_base")]
    fn discover_skips_invalid(name: &str, info_json: &str) {
        let tmp = TempDir::new().unwrap();
        write_script(tmp.path(), name, info_json);
        assert!(discover_in(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn discover_parses_models_subcommand() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom-llm");
        let script = r#"#!/bin/sh
case "$1" in
  info) echo '{"display_name": "Custom", "base": "openai", "has_auth": false}' ;;
  models) echo '[{"id": "custom-v1", "tier": "strong", "max_output_tokens": 32000, "context_window": 200000}]' ;;
  resolve) echo '{"headers": {"authorization": "Bearer test"}}' ;;
  *) exit 1 ;;
esac
"#;
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let providers = discover_in(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].models.len(), 1);
        assert_eq!(providers[0].models[0].id, "custom-v1");
        assert_eq!(providers[0].models[0].tier, ModelTier::Strong);
    }

    #[cfg(unix)]
    #[test]
    fn run_script_error_on_bad_subcommand() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(
            tmp.path(),
            "test-err",
            r#"{"display_name": "T", "base": "anthropic", "has_auth": false}"#,
        );
        assert!(matches!(
            run_script(&path, "nonexistent", SCRIPT_TIMEOUT).unwrap_err(),
            AgentError::Config { .. }
        ));
    }

    #[cfg(unix)]
    #[test_case("ollama", ProviderKind::Ollama ; "base_ollama")]
    #[test_case("mistral", ProviderKind::Mistral ; "base_mistral")]
    #[test_case("zai", ProviderKind::Zai ; "base_zai")]
    #[test_case("zai-coding-plan", ProviderKind::ZaiCodingPlan ; "base_zai_coding_plan")]
    #[test_case("synthetic", ProviderKind::Synthetic ; "base_synthetic")]
    #[test_case("deepseek", ProviderKind::DeepSeek ; "base_deepseek")]
    fn discover_accepts_all_bases(base: &str, expected: ProviderKind) {
        let tmp = TempDir::new().unwrap();
        let info = format!(r#"{{"display_name": "Test", "base": "{base}", "has_auth": false}}"#);
        write_script(tmp.path(), "custom-test", &info);
        let providers = discover_in(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].base, expected);
    }
}
