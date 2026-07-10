use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use flume::Sender;
use maki_config::providers::ProvidersConfig;
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use maki_storage::sessions::{BodyOverride, EffortDialectId, ThinkingFieldConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::IntoEnumIterator;
use tracing::{debug, warn};

use crate::model::{Model, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

use super::ResolvedAuth;
use super::anthropic::Anthropic;
use super::copilot::Copilot;
use super::deepseek::DeepSeek;
use super::google::Google;
use super::local::{LLAMACPP, LocalEndpoint, OLLAMA};
use super::mistral::Mistral;
use super::openai::OpenAi;
use super::opencode::Opencode;
use super::openrouter::OpenRouter;
use super::synthetic::Synthetic;
use super::tensorx::TensorX;
use super::zai::Zai;

const INFO_TIMEOUT: Duration = Duration::from_secs(5);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDERS_DIR: &str = "providers";
const SCRIPT_CACHE_FILE: &str = "provider-scripts.json";

struct DynamicProviderMeta {
    slug: String,
    display_name: String,
    base: ProviderKind,
    system_prefix: Option<String>,
    has_auth: bool,
    script_path: PathBuf,
    models: Vec<ScriptModel>,
    model_filters: Vec<ScriptModelFilter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptInfo {
    display_name: String,
    base: String,
    #[serde(default)]
    system_prefix: Option<String>,
    has_auth: bool,
    /// Provider-wide, glob-matched body shaping rules. Each entry contributes
    /// its `body_override` to every model id whose id matches its `match` glob.
    /// Per-model entries in [`ScriptModel`] take precedence when both apply.
    #[serde(default)]
    model_filters: Vec<ScriptModelFilter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptModelFilter {
    #[serde(rename = "match")]
    match_pattern: String,
    #[serde(default)]
    body_override: Option<BodyOverride>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptModel {
    id: String,
    #[serde(default = "default_tier")]
    tier: ModelTier,
    #[serde(default)]
    supports_tool_examples: Option<bool>,
    #[serde(default)]
    supports_thinking: Option<bool>,
    #[serde(default)]
    supports_vision: Option<bool>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default = "default_context_window")]
    context_window: u32,
    #[serde(default)]
    pricing: Option<ModelPricing>,
    #[serde(default)]
    thinking_dialect: Option<EffortDialectId>,
    #[serde(default)]
    thinking_fields: Option<ThinkingFieldConfig>,
    #[serde(default)]
    body_override: Option<BodyOverride>,
}

impl ScriptModel {
    fn to_model(
        &self,
        slug: &str,
        base: ProviderKind,
        id_matcher: &str,
        id: String,
        tier: ModelTier,
        model_filters: &[ScriptModelFilter],
    ) -> Model {
        let body_override = resolve_overrides(id_matcher, model_filters, &self.body_override);
        Model {
            id,
            provider: Arc::from(slug),
            tier,
            family: base.family(),
            supports_tool_examples_override: self.supports_tool_examples,
            supports_thinking_override: self.supports_thinking,
            supports_vision_override: self.supports_vision,
            pricing: self.pricing.clone().unwrap_or_default(),
            max_output_tokens: self.max_output_tokens,
            context_window: self.context_window,
            thinking_dialect: self.thinking_dialect,
            thinking_fields: self.thinking_fields.clone(),
            body_override,
        }
    }
}

fn default_tier() -> ModelTier {
    ModelTier::Medium
}

fn default_context_window() -> u32 {
    128_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Full shell-style glob match: `*` matches any run (including empty), `?` matches
/// a single char, both over the whole byte sequence (slashes included). Model ids
/// in maki are short, so we don't need to anchor the pattern globally — the
/// caller decides whether to match exactly or as a substring.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_inner(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;
    while ti < text.len() {
        if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Resolve the effective `body_override` for one model id. Per-model
/// value applies first; matching `model_filters` entries then accumulate
/// in declaration order. `defaults` and `replace` deep-merge later-wins;
/// `filter` unions with first-seen preserved.
fn resolve_overrides(
    model_id: &str,
    filters: &[ScriptModelFilter],
    per_model: &Option<BodyOverride>,
) -> Option<BodyOverride> {
    let mut result = per_model.clone();
    for entry in filters {
        if !glob_match(&entry.match_pattern, model_id) {
            continue;
        }
        if let Some(entry_ov) = &entry.body_override {
            let merged = result.get_or_insert_with(BodyOverride::default);
            if let Some(entry_defaults) = &entry_ov.defaults {
                let target = merged
                    .defaults
                    .get_or_insert_with(|| Value::Object(Default::default()));
                merge_value_late_wins(target, entry_defaults);
            }
            if let Some(entry_replace) = &entry_ov.replace {
                let target = merged
                    .replace
                    .get_or_insert_with(|| Value::Object(Default::default()));
                merge_value_late_wins(target, entry_replace);
            }
            for k in &entry_ov.filter {
                if !merged.filter.contains(k) {
                    merged.filter.push(k.clone());
                }
            }
        }
    }
    result
}

/// Two-object deep merge, later-wins. Used to fold multiple `BodyOverride`
/// contributions (per-model, then each matching glob in order) into one shape.
fn merge_value_late_wins(target: &mut Value, src: &Value) {
    let (Some(t), Some(s)) = (target.as_object_mut(), src.as_object()) else {
        return;
    };
    for (k, v) in s {
        match t.get_mut(k) {
            Some(existing) if existing.is_object() && v.is_object() => {
                merge_value_late_wins(existing, v);
            }
            _ => {
                t.insert(k.clone(), v.clone());
            }
        }
    }
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

/// `info` and `models` describe the script, not the world, so their output only
/// changes when the script does. Caching them keeps two process spawns per
/// provider off every startup.
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone)]
struct ScriptDescription {
    modified_ns: u128,
    size: u64,
    info: String,
    /// `None` when the script has no `models` subcommand, or its run failed.
    models: Option<String>,
}

type ScriptCache = HashMap<String, ScriptDescription>;

fn cache_path() -> Option<PathBuf> {
    Some(StateDir::resolve().ok()?.path().join(SCRIPT_CACHE_FILE))
}

fn read_cache() -> ScriptCache {
    cache_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn script_stamp(path: &Path) -> Option<(u128, u64)> {
    let meta = path.metadata().ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((modified.as_nanos(), meta.len()))
}

/// Reuses `cached` for as long as the file stays untouched. A failing `info`
/// is fatal for the provider, while `models` is optional and a failure there
/// is cached as `None`. A script we cannot stat gets a zero stamp, which never
/// matches, so it is described again on every run.
fn describe_script(
    slug: &str,
    path: &Path,
    cached: Option<&ScriptDescription>,
) -> Option<ScriptDescription> {
    let stamp = script_stamp(path);
    if let Some(hit) = cached
        && stamp == Some((hit.modified_ns, hit.size))
    {
        return Some(hit.clone());
    }

    let info = match run_script(path, "info", INFO_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            warn!(slug, error = %e, "failed to get provider info, skipping");
            return None;
        }
    };
    let (modified_ns, size) = stamp.unwrap_or_default();
    Some(ScriptDescription {
        modified_ns,
        size,
        info,
        models: run_script(path, "models", INFO_TIMEOUT).ok(),
    })
}

/// Bad `models` output is survivable, the base provider's model list takes
/// over. Anything else the script gets wrong drops it, with a line in the log.
fn build_meta(
    slug: String,
    script_path: PathBuf,
    described: &ScriptDescription,
) -> Option<DynamicProviderMeta> {
    let info: ScriptInfo = match serde_json::from_str(&described.info) {
        Ok(i) => i,
        Err(e) => {
            warn!(
                slug,
                error = %e,
                "invalid info JSON, dynamic provider skipped; fix the script's `info` output"
            );
            return None;
        }
    };

    let base = match ProviderKind::from_str(&info.base) {
        Ok(k) => k,
        Err(_) => {
            warn!(slug, base = info.base, "unknown base provider, skipping");
            return None;
        }
    };

    let models = match &described.models {
        Some(json) => match serde_json::from_str::<Vec<ScriptModel>>(json) {
            Ok(m) => m,
            Err(e) => {
                let snippet: String = json.chars().take(120).collect();
                warn!(
                    slug,
                    error = %e,
                    snippet = %snippet,
                    "invalid models JSON: dynamic provider has no models; fix the script's `models` output"
                );
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    Some(DynamicProviderMeta {
        slug,
        display_name: info.display_name,
        base,
        system_prefix: info.system_prefix.filter(|s| !s.is_empty()),
        has_auth: info.has_auth,
        script_path,
        models,
        model_filters: info.model_filters,
    })
}

fn write_cache(cache: &ScriptCache) {
    let Some(path) = cache_path() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    if let Err(e) = maki_storage::atomic_write(&path, &bytes) {
        debug!(error = %e, "failed to write provider script cache");
    }
}

fn discover_in(dir: &Path) -> Vec<DynamicProviderMeta> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let builtins = builtin_slugs();
    let cache = read_cache();
    let mut next = ScriptCache::new();
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

        let name_part = if cfg!(windows) {
            path.file_stem()
        } else {
            path.file_name()
        };
        let slug = match name_part.and_then(|n| n.to_str()) {
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

        let Some(described) = describe_script(&slug, &path, cache.get(&slug)) else {
            continue;
        };
        result.extend(build_meta(slug.clone(), path, &described));
        // Cached even when the description was rejected, so a script we refuse
        // is not re-run on the next startup either.
        next.insert(slug, described);
    }

    if next != cache {
        write_cache(&next);
    }
    result
}

static DISCOVERED: OnceLock<Vec<DynamicProviderMeta>> = OnceLock::new();

fn discover() -> &'static [DynamicProviderMeta] {
    DISCOVERED.get_or_init(|| {
        // Load config first: it hard-exits on malformed providers.toml, so fail
        // before spawning every provider script.
        let custom = ProvidersConfig::load();
        let mut metas = providers_dir().map(|d| discover_in(&d)).unwrap_or_default();
        // A script and a providers.toml entry must not share a slug. The script
        // loses, the same way it already loses to a builtin, and we say so
        // instead of silently picking a winner.
        metas.retain(|m| {
            if custom.get(&m.slug).is_some() {
                warn!(
                    slug = %m.slug,
                    "provider slug also defined in providers.toml, skipping script"
                );
                false
            } else {
                true
            }
        });
        metas
    })
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
            LocalEndpoint::with_auth(&OLLAMA, auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::LlamaCpp => Box::new(
            LocalEndpoint::with_auth(&LLAMACPP, auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Mistral => Box::new(
            Mistral::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Zai => Box::new(
            Zai::with_auth(auth.clone(), timeouts).with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Synthetic => Box::new(
            Synthetic::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::DeepSeek => Box::new(
            DeepSeek::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::OpenRouter => Box::new(
            OpenRouter::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::TensorX => Box::new(
            TensorX::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Opencode => Box::new(
            Opencode::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
    };

    Ok(Box::new(DynamicProvider {
        script_path: &meta.script_path,
        inner,
        auth,
        models: &meta.models,
    }))
}

pub fn display_name(slug: &str) -> Option<&'static str> {
    find_meta(slug).map(|m| m.display_name.as_str())
}

pub fn dynamic_model_specs_for(slug: &str) -> Vec<String> {
    let Some(meta) = find_meta(slug) else {
        return Vec::new();
    };
    meta.models
        .iter()
        .map(|m| format!("{slug}/{}", m.id))
        .collect()
}

pub fn discovered_slugs() -> Vec<&'static str> {
    discover().iter().map(|m| m.slug.as_str()).collect()
}

pub fn base_for_slug(slug: &str) -> Option<ProviderKind> {
    find_meta(slug).map(|m| m.base)
}

pub fn lookup_model(slug: &str, model_id: &str) -> Option<Model> {
    let meta = find_meta(slug)?;
    let script_model = meta
        .models
        .iter()
        .filter(|m| model_id.starts_with(&m.id))
        .max_by_key(|m| m.id.len())?;
    Some(script_model.to_model(
        slug,
        meta.base,
        model_id,
        model_id.to_string(),
        script_model.tier,
        &meta.model_filters,
    ))
}

pub fn find_model_for_tier(slug: &str, tier: ModelTier) -> Option<Model> {
    let meta = find_meta(slug)?;
    let script_model = meta.models.iter().find(|m| m.tier == tier)?;
    Some(script_model.to_model(
        slug,
        meta.base,
        &script_model.id,
        script_model.id.clone(),
        tier,
        &meta.model_filters,
    ))
}

struct DynamicProvider {
    script_path: &'static Path,
    inner: Box<dyn Provider>,
    auth: Arc<Mutex<ResolvedAuth>>,
    models: &'static [ScriptModel],
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
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        self.inner
            .stream_message(model, messages, system, tools, event_tx, opts, session_id)
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async {
            Ok(self
                .models
                .iter()
                .map(|m| crate::model::ModelInfo {
                    id: m.id.clone(),
                    context_window: Some(m.context_window),
                    max_output_tokens: m.max_output_tokens,
                    pricing: m.pricing.clone(),
                    supports_thinking: None,
                    supports_vision: m.supports_vision,
                    tier: None,
                    provider_info: None,
                })
                .collect())
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.run_auth_script("refresh")
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.run_auth_script("reload")
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        self.inner.fetch_usage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs::{self, File};
    #[cfg(unix)]
    use std::io::Write;
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
        let full = r#"{"id": "my-model", "tier": "strong", "supports_tool_examples": true, "max_output_tokens": 32000, "context_window": 200000, "pricing": {"input": 3.0, "output": 15.0, "cache_write": 3.75, "cache_read": 0.30}, "thinking_dialect": "glm", "body_override": {"defaults": {"generationConfig": {"temperature": 0.1}}, "filter": ["poison"]}}"#;
        let model: ScriptModel = serde_json::from_str(full).unwrap();
        assert_eq!(model.id, "my-model");
        assert_eq!(model.tier, ModelTier::Strong);
        assert_eq!(model.supports_tool_examples, Some(true));
        assert!(model.pricing.is_some());
        assert_eq!(model.thinking_dialect, Some(EffortDialectId::Glm));
        let ov = model.body_override.as_ref().expect("body_override parsed");
        let defaults = ov.defaults.as_ref().expect("defaults parsed");
        assert_eq!(defaults["generationConfig"]["temperature"], 0.1);
        assert_eq!(ov.filter, vec!["poison".to_string()]);

        let minimal: ScriptModel = serde_json::from_str(r#"{"id": "custom-v1"}"#).unwrap();
        assert_eq!(minimal.tier, ModelTier::Medium);
        assert_eq!(minimal.supports_tool_examples, None);
        assert!(minimal.max_output_tokens.is_none());
        assert_eq!(minimal.context_window, 128_000);
        assert!(minimal.pricing.is_none());
        assert!(minimal.thinking_dialect.is_none());
        assert!(minimal.thinking_fields.is_none());
        assert!(minimal.body_override.is_none());
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, info_json: &str) -> PathBuf {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  info) echo '{info_json}' ;;\n  resolve) echo '{{\"headers\": {{\"authorization\": \"Bearer test\"}}}}' ;;\n  refresh) echo '{{\"headers\": {{\"authorization\": \"Bearer refreshed\"}}}}' ;;\n  *) exit 1 ;;\nesac\n"
        );
        let mut file = File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
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
        let mut file = File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let providers = discover_in(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].models.len(), 1);
        assert_eq!(providers[0].models[0].id, "custom-v1");
        assert_eq!(providers[0].models[0].tier, ModelTier::Strong);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_overrides_applies_filters_in_declaration_order() {
        let filters = vec![
            ScriptModelFilter {
                match_pattern: "hint*".into(),
                body_override: Some(BodyOverride {
                    defaults: Some(
                        serde_json::json!({"chat_template_kwargs": {"enable_thinking": true}}),
                    ),
                    replace: None,
                    filter: vec!["min_tokens".into(), "red".into()],
                }),
            },
            ScriptModelFilter {
                match_pattern: "unrelated-*".into(),
                body_override: Some(BodyOverride {
                    defaults: Some(serde_json::json!({"poison": true})),
                    replace: None,
                    filter: vec!["poison_field".into()],
                }),
            },
        ];
        let per_model = Some(BodyOverride {
            defaults: Some(serde_json::json!({"temperature": 0.1, "trace_id": "abc"})),
            replace: None,
            filter: vec!["always_strip".into()],
        });

        let resolved = resolve_overrides("hint-mod-3", &filters, &per_model).unwrap();

        let defaults = resolved.defaults.as_ref().unwrap();
        assert_eq!(defaults["temperature"], 0.1);
        assert_eq!(defaults["trace_id"], "abc");
        assert_eq!(defaults["chat_template_kwargs"]["enable_thinking"], true);
        assert!(defaults.get("poison").is_none());

        assert_eq!(resolved.filter, vec!["always_strip", "min_tokens", "red"]);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_overrides_no_filters_no_overrides_returns_neutrals() {
        let result = resolve_overrides("model-x", &[], &None);
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn discover_resolves_overrides_for_matching_model() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ovr-llm");
        let script = r#"#!/bin/sh
case "$1" in
  info) echo '{
    "display_name": "OvR",
    "base": "openai",
    "has_auth": false,
    "model_filters": [
      {"match": "hint*", "body_override": {"defaults": {"chat_template_kwargs": {"enable_thinking": true}}, "filter": ["min_tokens"]}}
    ]
  }' ;;
  models) echo '[
    {"id": "hint-mod", "tier": "strong", "max_output_tokens": 32000, "context_window": 200000, "body_override": {"defaults": {"temperature": 0.1}, "filter": ["always_strip"]}},
    {"id": "plain-mod", "tier": "medium", "max_output_tokens": 16000, "context_window": 128000}
  ]' ;;
  resolve) echo '{"headers": {"authorization": "Bearer test"}}' ;;
  *) exit 1 ;;
esac
"#;
        let mut file = File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        let metas = discover_in(tmp.path());
        assert_eq!(metas.len(), 1);
        let meta = &metas[0];
        assert_eq!(meta.model_filters.len(), 1);
        assert_eq!(meta.model_filters[0].match_pattern, "hint*");
        assert_eq!(meta.models.len(), 2);

        let m = meta.models.iter().find(|m| m.id == "hint-mod").unwrap();
        let resolved =
            resolve_overrides("hint-mod-large", &meta.model_filters, &m.body_override).unwrap();
        let defaults = resolved.defaults.as_ref().unwrap();
        assert_eq!(defaults["temperature"], 0.1);
        assert_eq!(defaults["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(
            resolved.filter,
            vec!["always_strip".to_string(), "min_tokens".to_string()]
        );

        let plain = meta.models.iter().find(|m| m.id == "plain-mod").unwrap();
        assert!(plain.body_override.is_none());
        let result = resolve_overrides("plain-mod", &meta.model_filters, &plain.body_override);
        assert!(result.is_none());
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
    #[test_case("llama-cpp", ProviderKind::LlamaCpp ; "base_llama_cpp")]
    #[test_case("mistral", ProviderKind::Mistral ; "base_mistral")]
    #[test_case("zai", ProviderKind::Zai ; "base_zai")]
    #[test_case("synthetic", ProviderKind::Synthetic ; "base_synthetic")]
    #[test_case("deepseek", ProviderKind::DeepSeek ; "base_deepseek")]
    #[test_case("opencode", ProviderKind::Opencode ; "base_opencode")]
    fn discover_accepts_all_bases(base: &str, expected: ProviderKind) {
        let tmp = TempDir::new().unwrap();
        let info = format!(r#"{{"display_name": "Test", "base": "{base}", "has_auth": false}}"#);
        write_script(tmp.path(), "custom-test", &info);
        let providers = discover_in(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].base, expected);
    }
}
