use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use flume::Sender;
use maki_config::providers::ProvidersConfig;
use maki_storage::StateDir;
use maki_storage::auth::lock_exclusive;
use maki_storage::id::SessionRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::IntoEnumIterator;
use tracing::{debug, warn};

use crate::manifest::ManifestRegistry;
use crate::model::{Model, ModelPricing, ModelTier, ThinkingSupport};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::types::ThinkingFields;
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

use super::ResolvedAuth;
use super::anthropic::Anthropic;
use super::aperture::Aperture;
use super::copilot::Copilot;
use super::deepseek::DeepSeek;
use super::google::Google;
use super::local::{LLAMACPP, LocalEndpoint, OLLAMA};
use super::mistral::Mistral;
use super::openai::OpenAi;
use super::opencode::Opencode;
use super::openrouter::OpenRouter;
use super::regolo::Regolo;
use super::synthetic::Synthetic;
use super::tensorx::TensorX;
use super::xai::Xai;
use super::zai::Zai;

const INFO_TIMEOUT: Duration = Duration::from_secs(5);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDERS_DIR: &str = "providers";
const SCRIPT_CACHE_FILE: &str = "provider-scripts.json";
const THINKING_FIELDS_KEY: &str = "thinking_fields";
const RELOAD_SUBCOMMAND: &str = "reload";

struct DynamicProviderMeta {
    slug: String,
    display_name: String,
    base: ProviderKind,
    system_prefix: Option<String>,
    has_auth: bool,
    script_path: PathBuf,
    models: Vec<ScriptModel>,
    refresh_gate: RefreshGate,
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
    #[serde(default)]
    supports_thinking: Option<bool>,
    #[serde(default)]
    requires_thinking: bool,
    #[serde(default)]
    supports_vision: Option<bool>,
    #[serde(default = "default_max_output_tokens")]
    max_output_tokens: u32,
    #[serde(default = "default_context_window")]
    context_window: u32,
    #[serde(default)]
    pricing: Option<ModelPricing>,
    #[serde(default)]
    thinking_fields: Option<ThinkingFields>,
}

impl ScriptModel {
    fn to_model(&self, slug: &str, base: ProviderKind, id: String, tier: ModelTier) -> Model {
        Model {
            id,
            provider: Arc::from(slug),
            tier,
            family: base.family(),
            supports_tool_examples_override: self.supports_tool_examples,
            thinking_override: ThinkingSupport::from_flags(
                self.supports_thinking,
                self.requires_thinking,
            ),
            supports_vision_override: self.supports_vision,
            pricing: self.pricing.clone().unwrap_or_default(),
            discovered_free: false,
            max_output_tokens: Some(self.max_output_tokens),
            context_window: self.context_window,
            thinking_fields: self.thinking_fields.clone().map(Box::new),
        }
    }
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

impl ScriptResolvedAuth {
    fn into_resolved(self, slug: &str) -> Result<ResolvedAuth, AgentError> {
        Ok(ResolvedAuth::new(slug, self.headers.into_iter().collect())?
            .with_base_url(self.base_url))
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
    maki_storage::paths::find_config_path(PROVIDERS_DIR)
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

/// Login writes a brand new token family, so it has to wait for an in flight
/// refresh instead of racing it: the loser of that race would put the old
/// family back on top of the new one, and the user would see the same auth
/// error right after logging in.
fn run_script_interactive(path: &Path, subcommand: &str) -> Result<(), AgentError> {
    let _lock = lock_exclusive(path);
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
    // A script may refresh under `resolve` when the token is near expiry, and
    // this runs on every `create`, so it takes the lock too.
    let _lock = lock_exclusive(&meta.script_path);
    let stdout = run_script(&meta.script_path, "resolve", SCRIPT_TIMEOUT)?;
    let parsed: ScriptResolvedAuth =
        serde_json::from_str(&stdout).map_err(|e| AgentError::Config {
            message: format!("{} resolve: invalid JSON: {e}", meta.script_path.display()),
        })?;
    parsed.into_resolved(&meta.slug)
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
            warn!(slug, error = %e, "invalid info JSON, skipping");
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

    // Entry by entry, so one bad model never costs the whole list.
    let models: Vec<ScriptModel> = match &described.models {
        Some(json) => serde_json::from_str::<Vec<Value>>(json)
            .unwrap_or_else(|e| {
                warn!(slug, error = %e, "invalid models JSON, falling back to base models");
                Vec::new()
            })
            .into_iter()
            .filter_map(|mut entry| {
                let error = match serde_json::from_value::<ScriptModel>(entry.clone()) {
                    Ok(model) => return Some(model),
                    Err(e) => e,
                };
                // Malformed thinking fields cost the field, not the model.
                let without_fields = entry
                    .as_object_mut()
                    .is_some_and(|o| o.remove(THINKING_FIELDS_KEY).is_some())
                    .then(|| serde_json::from_value::<ScriptModel>(entry).ok())
                    .flatten();
                match without_fields {
                    Some(model) => {
                        warn!(slug, error = %error, model = model.id, "invalid thinking_fields, dropping them");
                        Some(model)
                    }
                    None => {
                        warn!(slug, error = %error, "invalid model entry, skipping");
                        None
                    }
                }
            })
            .collect(),
        None => Vec::new(),
    };

    // Only the local and openai-compat paths merge thinking_fields into the
    // body, so anywhere else they would vanish without a trace.
    if !matches!(
        base,
        ProviderKind::LlamaCpp | ProviderKind::Ollama | ProviderKind::OpenAi
    ) && let Some(model) = models.iter().find(|m| m.thinking_fields.is_some())
    {
        warn!(
            slug,
            base = %info.base,
            model = model.id,
            "thinking_fields only applies to llama-cpp, ollama, and openai providers, ignoring"
        );
    }

    Some(DynamicProviderMeta {
        slug,
        display_name: info.display_name,
        base,
        system_prefix: info.system_prefix.filter(|s| !s.is_empty()),
        has_auth: info.has_auth,
        script_path,
        models,
        refresh_gate: RefreshGate::default(),
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
        ProviderKind::Regolo => Box::new(
            Regolo::with_auth(auth.clone(), timeouts)
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
        ProviderKind::Xai => Box::new(
            Xai::with_auth(auth.clone(), timeouts).with_system_prefix(meta.system_prefix.clone()),
        ),
        ProviderKind::Aperture => Box::new(
            Aperture::with_auth(auth.clone(), timeouts)
                .with_system_prefix(meta.system_prefix.clone()),
        ),
    };

    Ok(Box::new(DynamicProvider {
        slug: &meta.slug,
        script_path: &meta.script_path,
        inner,
        auth,
        models: &meta.models,
        refresh_gate: &meta.refresh_gate,
    }))
}

pub fn display_name(slug: &str) -> Option<&'static str> {
    find_meta(slug).map(|m| m.display_name.as_str())
}

pub fn dynamic_model_specs_for(slug: &str) -> Vec<String> {
    let Some(meta) = find_meta(slug) else {
        return Vec::new();
    };
    if meta.models.is_empty() {
        let base_slug = meta.base.to_string();
        ManifestRegistry::get(&base_slug)
            .map(|m| m.models)
            .unwrap_or(&[])
            .iter()
            .flat_map(|entry| entry.prefixes.iter())
            .map(|prefix| format!("{slug}/{prefix}"))
            .collect()
    } else {
        meta.models
            .iter()
            .map(|m| format!("{slug}/{}", m.id))
            .collect()
    }
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
    Some(script_model.to_model(slug, meta.base, model_id.to_string(), script_model.tier))
}

pub fn find_model_for_tier(slug: &str, tier: ModelTier) -> Option<Model> {
    let meta = find_meta(slug)?;
    let script_model = meta.models.iter().find(|m| m.tier == tier)?;
    Some(script_model.to_model(slug, meta.base, script_model.id.clone(), tier))
}

struct DynamicProvider {
    slug: &'static str,
    script_path: &'static Path,
    inner: Box<dyn Provider>,
    auth: Arc<Mutex<ResolvedAuth>>,
    models: &'static [ScriptModel],
    refresh_gate: &'static RefreshGate,
}

/// Gate around the auth script's `refresh`. Every `create` mints a fresh
/// `DynamicProvider`, so sub-agents running their own model would each spend
/// the script's rotating refresh token, and a spent one taken twice costs the
/// whole token family. They queue here instead, and the late arrival takes the
/// credentials the winner brought back. That is why the gate hangs off the
/// `'static` per-slug metadata rather than the provider.
#[derive(Default)]
struct RefreshGate {
    lock: smol::lock::Mutex<()>,
    winner: Mutex<Winner>,
}

/// A refresh can hand back byte-identical credentials, so the count, not the
/// bytes, is what tells a parked caller the work is already done. Both live
/// under one lock so they cannot be read apart.
#[derive(Default, Clone)]
struct Winner {
    refreshes: u64,
    auth: Option<ResolvedAuth>,
}

impl RefreshGate {
    async fn refresh(
        &self,
        slug: &str,
        script_path: &Path,
        auth: &Arc<Mutex<ResolvedAuth>>,
    ) -> Result<(), AgentError> {
        let before = self.winner.lock().unwrap().refreshes;
        let _guard = self.lock.lock().await;
        let winner = self.winner.lock().unwrap().clone();
        if winner.refreshes != before {
            debug!("peer refreshed while we waited, skipping script run");
            if let Some(fresh) = winner.auth {
                *auth.lock().unwrap() = fresh;
            }
            return Ok(());
        }
        run_auth_script(slug, script_path, auth, "refresh").await?;
        *self.winner.lock().unwrap() = Winner {
            refreshes: before + 1,
            auth: Some(auth.lock().unwrap().clone()),
        };
        Ok(())
    }
}

async fn run_auth_script(
    slug: &str,
    script_path: &Path,
    auth: &Arc<Mutex<ResolvedAuth>>,
    subcommand: &'static str,
) -> Result<(), AgentError> {
    let script_path = script_path.to_path_buf();
    let auth = auth.clone();
    let slug = slug.to_string();
    smol::unblock(move || {
        // `reload` only re-reads what a login wrote, so it spends no token and
        // must not park the ui behind someone else's refresh.
        let _lock = (subcommand != RELOAD_SUBCOMMAND).then(|| lock_exclusive(&script_path));
        let stdout = run_script(&script_path, subcommand, SCRIPT_TIMEOUT)?;
        let parsed: ScriptResolvedAuth =
            serde_json::from_str(&stdout).map_err(|e| AgentError::Config {
                message: format!("{} {subcommand}: invalid JSON: {e}", script_path.display()),
            })?;
        let mut fresh = parsed.into_resolved(&slug)?;
        let mut guard = auth.lock().unwrap();
        // A script that omits base_url keeps the resolved one; falling back to
        // the provider's default origin would silently repoint the token.
        if fresh.base_url.is_none() {
            fresh.base_url = guard.base_url.take();
        }
        *guard = fresh;
        Ok(())
    })
    .await
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
        Box::pin(async move {
            // First attempt streams through a counting relay: a 401 is only
            // retried when it preceded every event. Replaying deltas onto a
            // channel that already delivered part of the answer would
            // duplicate text in the UI and, after a cancel, in the history.
            let (tx, rx) = flume::unbounded();
            let attempt = async {
                let result = self
                    .inner
                    .stream_message(model, messages, system, tools, &tx, opts, session_id)
                    .await;
                drop(tx);
                result
            };
            let forward = async move {
                let mut forwarded = 0usize;
                while let Ok(ev) = rx.recv_async().await {
                    forwarded += 1;
                    if event_tx.send_async(ev).await.is_err() {
                        break;
                    }
                }
                forwarded
            };
            let (result, forwarded) = futures_lite::future::zip(attempt, forward).await;
            match result {
                // The script mints credentials without the user, so an expired
                // token costs one silent refresh instead of a re-login prompt.
                Err(e) if e.is_auth_error() && forwarded == 0 => {
                    debug!(error = %e, "auth error, refreshing script-backed credentials");
                    match self
                        .refresh_gate
                        .refresh(self.slug, self.script_path, &self.auth)
                        .await
                    {
                        Ok(()) => {
                            self.inner
                                .stream_message(
                                    model, messages, system, tools, event_tx, opts, session_id,
                                )
                                .await
                        }
                        Err(refresh_err) => {
                            warn!(error = %refresh_err, "silent refresh failed, falling back to re-login");
                            Err(e)
                        }
                    }
                }
                result => result,
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        if self.models.is_empty() {
            return self.inner.list_models();
        }
        Box::pin(async {
            Ok(self
                .models
                .iter()
                .map(|m| crate::model::ModelInfo {
                    id: m.id.clone(),
                    context_window: Some(m.context_window),
                    max_output_tokens: Some(m.max_output_tokens),
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
        Box::pin(
            self.refresh_gate
                .refresh(self.slug, self.script_path, &self.auth),
        )
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        // Deliberately ungated: this runs under block_on on the ui thread, and
        // parking it behind someone else's slow refresh script freezes the ui.
        Box::pin(run_auth_script(
            self.slug,
            self.script_path,
            &self.auth,
            RELOAD_SUBCOMMAND,
        ))
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

    const TEST_SLUG: &str = "script-provider";

    #[cfg(unix)]
    const STALE_TOKEN: &str = "Bearer stale";
    #[cfg(unix)]
    const CONSTANT_TOKEN: &str = "Bearer constant";

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
        let resolved = serde_json::from_str::<ScriptResolvedAuth>(with_base)
            .unwrap()
            .into_resolved(TEST_SLUG)
            .unwrap();
        assert_eq!(resolved.base_url.as_deref(), Some("https://example.com"));
        assert_eq!(resolved.headers[0].1, "Bearer tok");

        let without_base = r#"{"headers": {"authorization": "Bearer x"}}"#;
        let resolved = serde_json::from_str::<ScriptResolvedAuth>(without_base)
            .unwrap()
            .into_resolved(TEST_SLUG)
            .unwrap();
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
        let full = r#"{"id": "my-model", "tier": "strong", "supports_tool_examples": true, "max_output_tokens": 32000, "context_window": 200000, "pricing": {"input": 3.0, "output": 15.0, "cache_write": 3.75, "cache_read": 0.30}, "thinking_fields": {"adaptive": {"reasoning_effort": "medium"}, "low": {"reasoning_effort": "low"}}}"#;
        let model: ScriptModel = serde_json::from_str(full).unwrap();
        assert_eq!(model.id, "my-model");
        assert_eq!(model.tier, ModelTier::Strong);
        assert_eq!(model.supports_tool_examples, Some(true));
        assert!(model.pricing.is_some());
        let resolved = model.to_model(
            "dynamic",
            ProviderKind::LlamaCpp,
            model.id.clone(),
            model.tier,
        );
        let mut body = serde_json::json!({});
        crate::ThinkingConfig::Adaptive.apply_local_thinking(&mut body, &resolved);
        assert_eq!(body, serde_json::json!({"reasoning_effort": "medium"}));

        let minimal: ScriptModel = serde_json::from_str(r#"{"id": "custom-v1"}"#).unwrap();
        assert_eq!(minimal.tier, ModelTier::Medium);
        assert_eq!(minimal.supports_tool_examples, None);
        assert_eq!(minimal.max_output_tokens, 16384);
        assert_eq!(minimal.context_window, 128_000);
        assert!(minimal.pricing.is_none());
        assert!(minimal.thinking_fields.is_none());
    }

    #[test]
    fn bad_thinking_fields_keeps_the_model() {
        let described = ScriptDescription {
            modified_ns: 0,
            size: 0,
            info: r#"{"display_name": "T", "base": "llama-cpp", "has_auth": false}"#.into(),
            models: Some(
                r#"[{"id": "typo", "thinking_fields": {"hight": {"reasoning_effort": "high"}}},
                    {"id": "broken", "tier": 7},
                    {"id": "good"}]"#
                    .into(),
            ),
        };
        let meta = build_meta("t".into(), PathBuf::new(), &described).unwrap();
        let ids: Vec<&str> = meta.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["typo", "good"]);
        assert!(meta.models[0].thinking_fields.is_none());
    }

    #[test_case("llama-cpp" ; "llamacpp_keeps")]
    #[test_case("ollama" ; "ollama_keeps")]
    #[test_case("openai" ; "openai_keeps")]
    fn supported_base_keeps_thinking_fields(base: &str) {
        let described = ScriptDescription {
            modified_ns: 0,
            size: 0,
            info: format!(r#"{{"display_name": "T", "base": "{base}", "has_auth": false}}"#),
            models: Some(
                r#"[{"id": "thinking", "thinking_fields": {"high": {"reasoning_effort": "xhigh"}}}]"#
                    .into(),
            ),
        };
        let meta = build_meta("t".into(), PathBuf::new(), &described).unwrap();
        assert_eq!(meta.models.len(), 1);
        assert!(meta.models[0].thinking_fields.is_some());
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

    /// Writes a `refresh` script that counts its runs in `counter`. A rotating
    /// script hands back a new token every run; a non-rotating one repeats the
    /// same bytes, which is how we pin down that the gate trusts its counter and
    /// not a diff of the credentials.
    #[cfg(unix)]
    fn write_counting_refresh_script(dir: &Path, counter: &Path, rotating: bool) -> PathBuf {
        let path = dir.join("counting-provider");
        let token = if rotating {
            "Bearer refreshed-$n"
        } else {
            CONSTANT_TOKEN
        };
        let script = format!(
            "#!/bin/sh\n\
             [ \"$1\" = refresh ] || exit 1\n\
             n=$(( $(cat '{c}' 2>/dev/null || echo 0) + 1 ))\n\
             echo \"$n\" > '{c}'\n\
             printf '{{\"headers\": {{\"authorization\": \"%s\"}}}}' \"{token}\"\n",
            c = counter.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn script_runs(counter: &Path) -> u32 {
        fs::read_to_string(counter).unwrap().trim().parse().unwrap()
    }

    /// Two auth slots, because sub-agents on one slug each build their own
    /// provider and only the gate is shared. Both have to come out holding the
    /// winner's token, since a second run would spend the rotating refresh
    /// token again.
    #[cfg(unix)]
    #[test_case(true, "Bearer refreshed-2" ; "rotating_token")]
    #[test_case(false, CONSTANT_TOKEN ; "unchanged_token")]
    fn refresh_gate_single_flights_concurrent_callers(rotating: bool, final_token: &str) {
        let tmp = TempDir::new().unwrap();
        let counter = tmp.path().join("count");
        let script = write_counting_refresh_script(tmp.path(), &counter, rotating);
        let stale = || {
            Arc::new(Mutex::new(ResolvedAuth::for_test(
                None,
                vec![("authorization".into(), STALE_TOKEN.into())],
            )))
        };
        let (first, second) = (stale(), stale());
        let gate = RefreshGate::default();

        smol::block_on(async {
            let (a, b) = futures_lite::future::zip(
                gate.refresh(TEST_SLUG, &script, &first),
                gate.refresh(TEST_SLUG, &script, &second),
            )
            .await;
            a.unwrap();
            b.unwrap();

            assert_eq!(
                script_runs(&counter),
                1,
                "concurrent 401s share one script run"
            );
            let winner = first.lock().unwrap().headers[0].1.clone();
            assert_ne!(winner, STALE_TOKEN);
            assert_eq!(second.lock().unwrap().headers[0].1, winner);

            // The late caller snapshots the count before locking, so a refresh
            // that overlaps nobody still runs the script.
            gate.refresh(TEST_SLUG, &script, &first).await.unwrap();
        });

        assert_eq!(
            script_runs(&counter),
            2,
            "a refresh that overlaps nobody runs the script again"
        );
        assert_eq!(first.lock().unwrap().headers[0].1, final_token);
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
    #[test_case("xai", ProviderKind::Xai ; "base_xai")]
    fn discover_accepts_all_bases(base: &str, expected: ProviderKind) {
        let tmp = TempDir::new().unwrap();
        let info = format!(r#"{{"display_name": "Test", "base": "{base}", "has_auth": false}}"#);
        write_script(tmp.path(), "custom-test", &info);
        let providers = discover_in(tmp.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].base, expected);
    }
}
