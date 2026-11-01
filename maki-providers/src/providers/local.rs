use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use flume::Sender;
use futures::future::join_all;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use maki_config::providers::{Protocol, ProvidersConfig};

use crate::model::{Model, ModelEffort, ModelInfo, ModelPricing};
use crate::model_registry;
use crate::provider::{BoxFuture, Provider};
use crate::types::ThinkingFallback;
use crate::{
    AgentError, Effort, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

use super::openai::responses;
use super::openai_compat::{DEFAULT_MAX_TOKENS_FIELD, OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth};

const OLLAMA_DEFAULT_TAG: &str = "latest";
const OLLAMA_THINKING_CAPABILITY: &str = "thinking";

pub(crate) struct LocalEndpointConfig {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub host_env: &'static str,
    pub api_key_env: &'static str,
    pub default_host: &'static str,
    pub default_model: &'static str,
    pub cloud_fallback_url: Option<&'static str>,
    pub discovery_mode: DiscoveryMode,
    pub compat: OpenAiCompatConfig,
    pub thinking_fallback: ThinkingFallback,
}

impl LocalEndpointConfig {
    pub(crate) fn sends_thinking(&self, model: &Model, choice: ThinkingConfig) -> ThinkingConfig {
        self.thinking_fallback.sends(model, choice)
    }

    pub(crate) fn fields_fallback(&self) -> Option<ThinkingFallback> {
        (maki_config::providers::resolve_protocol(
            self.slug,
            ProvidersConfig::load_or_default().get(self.slug),
        ) != Some(Protocol::OpenaiResponses))
        .then_some(self.thinking_fallback)
    }
}

fn resolve_protocol_for_local(slug: &str) -> Option<Protocol> {
    maki_config::providers::resolve_protocol(
        slug,
        maki_config::providers::ProvidersConfig::load().get(slug),
    )
}

pub(crate) struct LocalEndpoint {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
    thinking_fallback: ThinkingFallback,
    discovery_mode: DiscoveryMode,
    protocol: Option<Protocol>,
}

impl LocalEndpoint {
    pub fn new(
        cfg: &'static LocalEndpointConfig,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        let key_pool = KeyPool::resolve(cfg.slug, cfg.api_key_env).ok();
        let host = maki_config::providers::ProvidersConfig::load()
            .get(cfg.slug)
            .and_then(|d| d.base_url.clone())
            .or_else(|| std::env::var(cfg.host_env).ok());
        Self::build(
            cfg,
            timeouts,
            key_pool,
            host,
            resolve_protocol_for_local(cfg.slug),
        )
    }

    pub(crate) fn with_auth(
        cfg: &'static LocalEndpointConfig,
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&cfg.compat, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
            thinking_fallback: cfg.thinking_fallback,
            discovery_mode: cfg.discovery_mode,
            protocol: resolve_protocol_for_local(cfg.slug),
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn build(
        cfg: &'static LocalEndpointConfig,
        timeouts: super::Timeouts,
        key_pool: Option<KeyPool>,
        host: Option<String>,
        protocol: Option<Protocol>,
    ) -> Result<Self, AgentError> {
        let api_key = key_pool.as_ref().map(|p| p.current().to_string());
        let base_url = match host {
            Some(h) => format!("{h}/v1"),
            None if api_key.is_some() && cfg.cloud_fallback_url.is_some() => {
                cfg.cloud_fallback_url.unwrap().to_string()
            }
            None => {
                return Err(AgentError::Config {
                    message: format!("{} not set", cfg.host_env),
                });
            }
        };
        let headers = match api_key {
            Some(key) => vec![("authorization".into(), format!("Bearer {key}"))],
            None => Vec::new(),
        };
        let compat_config = &cfg.compat;
        let auth = ResolvedAuth::new(cfg.slug, headers)?.with_base_url(Some(base_url));
        Ok(Self {
            compat: OpenAiCompatProvider::new(compat_config, timeouts),
            auth: Arc::new(Mutex::new(auth)),
            key_pool,
            system_prefix: None,
            thinking_fallback: cfg.thinking_fallback,
            discovery_mode: cfg.discovery_mode,
            protocol,
        })
    }
}

impl Provider for LocalEndpoint {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();

            if matches!(self.protocol, Some(Protocol::OpenaiResponses)) {
                let mut buf = String::new();
                let system = super::with_prefix(&self.system_prefix, system, &mut buf);
                let mut body = responses::build_body(model, messages, system, tools);
                body["return_progress"] = serde_json::Value::Bool(true);
                // TODO: wire thinking budget into responses API when llama.cpp supports it
                return responses::do_stream(
                    self.compat.client(),
                    model,
                    &body,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await;
            }

            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body =
                self.compat
                    .build_body(model, messages, system, tools, opts.thinking, auth.top_p);

            opts.thinking
                .apply_thinking(&mut body, model, self.thinking_fallback);

            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            match self.discovery_mode {
                DiscoveryMode::None => self.compat.do_list_models(&auth).await,
                DiscoveryMode::LlamaCpp => self.discover_llamacpp_models(&auth).await,
                DiscoveryMode::Ollama => self.discover_ollama_models(&auth).await,
            }
        })
    }

    /// Ollama picks the window when it loads a model, from free VRAM unless a
    /// Modelfile or `OLLAMA_CONTEXT_LENGTH` pins it. Only `/api/ps` tells, and
    /// only while the model is loaded.
    fn served_context_window<'a>(&'a self, model: &'a Model) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move {
            if !matches!(self.discovery_mode, DiscoveryMode::Ollama) {
                return None;
            }
            let auth = self.auth.lock().unwrap().clone();
            let loaded = ollama_loaded_models(&self.compat, &auth, self.api_root(&auth)).await;
            let window = ollama_served_window(&loaded, &model.id)?;
            model_registry::record_context_window(&self.compat.config().slug, &model.id, window);
            Some(window)
        })
    }

    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.key_pool.as_ref()?,
            &self.auth,
            KeyHeader::Bearer,
        ))
    }
}

const LLAMACPP_DEFAULT_CTX: u32 = 128_000;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum DiscoveryMode {
    #[default]
    None,
    LlamaCpp,
    Ollama,
}

enum ServerMode {
    Router,
    Single,
    Legacy,
}

#[derive(Deserialize)]
struct LlamaCppModelsResponse {
    #[serde(default)]
    data: Vec<LlamaCppModelData>,
}

#[derive(Deserialize)]
struct LlamaCppModelData {
    id: String,
    #[serde(default)]
    meta: Option<LlamaCppMeta>,
    #[serde(default)]
    status: Option<LlamaCppStatus>,
    #[serde(default)]
    max_model_len: Option<u32>,
    #[serde(default)]
    architecture: Option<LlamaCppArchitecture>,
}

#[derive(Deserialize)]
struct LlamaCppArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

#[derive(Deserialize)]
struct LlamaCppMeta {
    #[serde(default)]
    n_ctx: u32,
}

#[derive(Deserialize)]
struct LlamaCppStatus {
    #[serde(default)]
    args: Vec<String>,
}

impl LocalEndpoint {
    async fn discover_llamacpp_models(
        &self,
        auth: &ResolvedAuth,
    ) -> Result<Vec<ModelInfo>, AgentError> {
        let root = self.api_root(auth);

        let props: serde_json::Value = serde_json::from_str(
            &self
                .compat
                .get_text(auth, &format!("{root}/props?autoload=false"))
                .await?,
        )?;

        let models_text = self
            .compat
            .get_text(auth, &format!("{root}/v1/models"))
            .await?;
        let body: LlamaCppModelsResponse = serde_json::from_str(&models_text)?;

        let mode = if props["role"].as_str() == Some("router") {
            ServerMode::Router
        } else if body.data.first().is_some_and(|m| m.max_model_len.is_some()) {
            ServerMode::Legacy
        } else {
            ServerMode::Single
        };

        let props_n_ctx = props["n_ctx"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);

        let props_vision = llamacpp_props_vision(&props, &mode);

        let mut models: Vec<ModelInfo> = body
            .data
            .into_iter()
            .filter_map(|m| {
                let arch = m.architecture.as_ref();
                let has_text_input = arch
                    .map(|a| a.input_modalities.iter().any(|m| m == "text"))
                    .unwrap_or(true);
                let has_text_output = arch
                    .map(|a| a.output_modalities.iter().any(|m| m == "text"))
                    .unwrap_or(true);
                if !has_text_input || !has_text_output {
                    return None;
                }
                let context_window = llamacpp_extract_ctx_from_model(&m, &mode, props_n_ctx);
                let supports_vision = arch
                    .map(|a| a.input_modalities.iter().any(|m| m == "image"))
                    .or(props_vision);
                Some(ModelInfo {
                    id: m.id,
                    context_window: Some(context_window),
                    max_output_tokens: None,
                    pricing: Some(ModelPricing::ZERO),
                    supports_thinking: None,
                    supports_vision,
                    tier: None,
                    provider_info: None,
                    extra: None,
                    effort: None,
                })
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

fn llamacpp_extract_ctx_from_model(
    model: &LlamaCppModelData,
    mode: &ServerMode,
    props_n_ctx: u32,
) -> u32 {
    match mode {
        ServerMode::Router => {
            if let Some(ctx) = model
                .meta
                .as_ref()
                .and_then(|m| (m.n_ctx > 0).then_some(m.n_ctx))
            {
                return ctx;
            }
            if let Some(args) = model.status.as_ref().map(|s| &s.args) {
                if let Some(ctx) = llamacpp_extract_ctx_arg(args, "--ctx-size") {
                    return ctx;
                }
                if let Some(ctx) = llamacpp_extract_ctx_arg(args, "--fit-ctx") {
                    return ctx;
                }
            }
            LLAMACPP_DEFAULT_CTX
        }
        ServerMode::Single => model
            .meta
            .as_ref()
            .and_then(|m| (m.n_ctx > 0).then_some(m.n_ctx))
            .unwrap_or(LLAMACPP_DEFAULT_CTX),
        ServerMode::Legacy => model
            .max_model_len
            .filter(|&v| v > 0)
            .or_else(|| (props_n_ctx > 0).then_some(props_n_ctx))
            .unwrap_or(LLAMACPP_DEFAULT_CTX),
    }
}

fn llamacpp_extract_ctx_arg(args: &[String], flag: &str) -> Option<u32> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1)?.parse().ok()
}

fn llamacpp_props_vision(props: &Value, mode: &ServerMode) -> Option<bool> {
    match mode {
        // router props describe the router, not the individual models it serves
        ServerMode::Router => None,
        ServerMode::Single | ServerMode::Legacy => {
            props.pointer("/modalities/vision").and_then(Value::as_bool)
        }
    }
}

// Ollama model discovery via POST /api/show

#[derive(Deserialize)]
struct OllamaModelsResponse {
    #[serde(default)]
    data: Vec<OllamaModelData>,
}

#[derive(Deserialize)]
struct OllamaModelData {
    id: String,
}

#[derive(Deserialize)]
struct OllamaShowResponse {
    #[serde(default)]
    parameters: Option<String>,
    #[serde(default)]
    model_info: Option<serde_json::Map<String, Value>>,
    /// e.g. `["completion", "vision", "tools"]`; absent on older Ollama servers.
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    /// What `think` takes, absent on models without the metadata.
    #[serde(default)]
    thinking: Option<OllamaThinking>,
}

/// `false` turns thinking off, `true` on, and a name picks a level.
#[derive(Deserialize)]
struct OllamaThinking {
    #[serde(default)]
    values: Vec<Value>,
}

impl OllamaThinking {
    fn takes(&self, toggle: bool) -> bool {
        self.values.contains(&Value::Bool(toggle))
    }
}

#[derive(Default)]
struct OllamaModelDetails {
    context_window: Option<u32>,
    supports_vision: Option<bool>,
    supports_thinking: Option<bool>,
    effort: Option<ModelEffort>,
}

impl LocalEndpoint {
    fn api_root<'a>(&'a self, auth: &'a ResolvedAuth) -> &'a str {
        let base = auth
            .base_url
            .as_deref()
            .unwrap_or(&self.compat.config().base_url);
        base.strip_suffix("/v1").unwrap_or(base)
    }

    async fn discover_ollama_models(
        &self,
        auth: &ResolvedAuth,
    ) -> Result<Vec<ModelInfo>, AgentError> {
        let root = self.api_root(auth);

        let models_text = self
            .compat
            .get_text(auth, &format!("{root}/v1/models"))
            .await?;
        let body: OllamaModelsResponse = serde_json::from_str(&models_text)?;

        let compat = &self.compat;
        let futures: Vec<_> = body
            .data
            .iter()
            .map(|m| ollama_fetch_model_details(compat, auth, root, &m.id))
            .collect();
        let (details, loaded) =
            futures::join!(join_all(futures), ollama_loaded_models(compat, auth, root));

        let mut models: Vec<ModelInfo> = body
            .data
            .into_iter()
            .zip(details)
            .map(|(m, d)| ModelInfo {
                context_window: ollama_served_window(&loaded, &m.id).or(d.context_window),
                id: m.id,
                max_output_tokens: None,
                pricing: Some(ModelPricing::ZERO),
                supports_thinking: d.supports_thinking,
                supports_vision: d.supports_vision,
                tier: None,
                provider_info: None,
                extra: None,
                effort: d.effort,
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

async fn ollama_fetch_model_details(
    compat: &OpenAiCompatProvider,
    auth: &ResolvedAuth,
    root: &str,
    model_id: &str,
) -> OllamaModelDetails {
    let show_url = format!("{root}/api/show");
    let body = json!({"model": model_id});
    let Ok(json_body) = serde_json::to_vec(&body) else {
        return OllamaModelDetails::default();
    };

    let text = match compat
        .post_text(auth, &show_url, "application/json", &json_body)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(model = model_id, error = %e, "Ollama POST /api/show failed");
            return OllamaModelDetails::default();
        }
    };
    let show: OllamaShowResponse = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            warn!(model = model_id, error = %e, "Failed to parse Ollama /api/show response");
            return OllamaModelDetails::default();
        }
    };

    ollama_model_details(&show)
}

/// A Modelfile's `num_ctx` is what Ollama loads with, but never past the
/// length the model was trained on.
fn ollama_model_details(show: &OllamaShowResponse) -> OllamaModelDetails {
    let trained = show
        .model_info
        .as_ref()
        .and_then(ollama_extract_context_length);
    let declared = show.parameters.as_deref().and_then(ollama_extract_num_ctx);
    let context_window = declared.into_iter().chain(trained).min();

    OllamaModelDetails {
        context_window,
        supports_vision: ollama_extract_vision(show.capabilities.as_deref()),
        supports_thinking: ollama_supports_thinking(show),
        effort: show.thinking.as_ref().map(ollama_effort),
    }
}

/// A model whose `thinking.values` holds only `false` cannot think. Without
/// the metadata the capability is all there is to go by.
fn ollama_supports_thinking(show: &OllamaShowResponse) -> Option<bool> {
    match &show.thinking {
        Some(thinking) => Some(thinking.values.iter().any(|v| *v != Value::Bool(false))),
        None => show
            .capabilities
            .as_deref()
            .map(|c| c.iter().any(|c| c == OLLAMA_THINKING_CAPABILITY)),
    }
}

/// Ollama hands a level name as it is to a model that lists names. A model
/// that only toggles reads any level as `true`, which Ollama calls medium, so
/// medium is the one level it gets.
fn ollama_effort(thinking: &OllamaThinking) -> ModelEffort {
    let names: Vec<Value> = thinking
        .values
        .iter()
        .filter(|v| v.is_string())
        .cloned()
        .collect();
    let supported = if names.is_empty() && thinking.takes(true) {
        vec![Effort::Medium]
    } else {
        ModelEffort::known_levels(&names)
    };
    ModelEffort {
        supported,
        send_off: Some(thinking.takes(false)),
    }
}

fn ollama_extract_context_length(info: &serde_json::Map<String, Value>) -> Option<u32> {
    info.iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| {
            info.iter()
                .find(|(k, _)| *k == "context_length")
                .and_then(|(_, v)| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
        })
}

/// `None` when the server didn't report capabilities at all (older Ollama),
/// so discovery falls back to the family default instead of claiming "no
/// vision" for a model it never actually checked.
fn ollama_extract_vision(capabilities: Option<&[String]>) -> Option<bool> {
    capabilities.map(|caps| caps.iter().any(|c| c == "vision"))
}

fn ollama_extract_num_ctx(params: &str) -> Option<u32> {
    for line in params.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("num_ctx ")
            && let Ok(ctx) = value.trim().parse::<u32>()
            && ctx > 0
        {
            return Some(ctx);
        }
    }
    None
}

#[derive(Deserialize)]
struct OllamaPsResponse {
    #[serde(default)]
    models: Vec<OllamaPsModel>,
}

#[derive(Deserialize)]
struct OllamaPsModel {
    name: String,
    #[serde(default)]
    context_length: u32,
}

/// Empty when nothing is loaded, or when the server has no `/api/ps`, like
/// llama.cpp behind `OLLAMA_HOST`.
async fn ollama_loaded_models(
    compat: &OpenAiCompatProvider,
    auth: &ResolvedAuth,
    root: &str,
) -> Vec<OllamaPsModel> {
    let text = match compat.get_text(auth, &format!("{root}/api/ps")).await {
        Ok(text) => text,
        Err(e) => {
            debug!(error = %e, "Ollama GET /api/ps failed");
            return Vec::new();
        }
    };
    match serde_json::from_str::<OllamaPsResponse>(&text) {
        Ok(ps) => ps.models,
        Err(e) => {
            warn!(error = %e, "Failed to parse Ollama /api/ps response");
            Vec::new()
        }
    }
}

fn ollama_served_window(loaded: &[OllamaPsModel], model_id: &str) -> Option<u32> {
    let wanted = ollama_tagged(model_id);
    loaded
        .iter()
        .find(|m| ollama_tagged(&m.name) == wanted)
        .map(|m| m.context_length)
        .filter(|&window| window > 0)
}

/// A name without a tag means `:latest` to Ollama. Only the last path segment
/// can hold the tag, since a registry host may carry a port.
fn ollama_tagged(name: &str) -> Cow<'_, str> {
    let last_segment = name.rsplit('/').next().unwrap_or(name);
    if last_segment.contains(':') {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("{name}:{OLLAMA_DEFAULT_TAG}"))
    }
}

const OLLAMA_SLUG: &str = "ollama";
const LLAMACPP_SLUG: &str = "llama-cpp";
pub(crate) const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
pub(crate) const LLAMACPP_BASE_URL: &str = "http://localhost:8080/v1";

pub(crate) const OLLAMA: LocalEndpointConfig = LocalEndpointConfig {
    slug: OLLAMA_SLUG,
    display_name: "Ollama",
    host_env: "OLLAMA_HOST",
    api_key_env: "OLLAMA_API_KEY",
    default_host: "http://localhost:11434",
    default_model: "ollama/qwen3",
    cloud_fallback_url: Some("https://ollama.com/v1"),
    discovery_mode: DiscoveryMode::Ollama,
    compat: OpenAiCompatConfig {
        slug: Cow::Borrowed(OLLAMA_SLUG),
        api_key_env: Cow::Borrowed(""),
        base_url: Cow::Borrowed(OLLAMA_BASE_URL),
        max_tokens_field: Cow::Borrowed(DEFAULT_MAX_TOKENS_FIELD),
        include_stream_usage: true,
        usage_cost: false,
        provider_name: Cow::Borrowed("Ollama"),
    },
    // Ollama ignores the budget field, so effort is the only thing it hears.
    thinking_fallback: ThinkingFallback::Dialect(&crate::dialect::OLLAMA),
};

pub(crate) const LLAMACPP: LocalEndpointConfig = LocalEndpointConfig {
    slug: LLAMACPP_SLUG,
    display_name: "LlamaCpp",
    host_env: "LLAMA_CPP_HOST",
    api_key_env: "LLAMA_CPP_API_KEY",
    default_host: "http://localhost:8080",
    default_model: "llama-cpp/default",
    cloud_fallback_url: None,
    discovery_mode: DiscoveryMode::LlamaCpp,
    compat: OpenAiCompatConfig {
        slug: Cow::Borrowed(LLAMACPP_SLUG),
        api_key_env: Cow::Borrowed(""),
        base_url: Cow::Borrowed(LLAMACPP_BASE_URL),
        max_tokens_field: Cow::Borrowed(DEFAULT_MAX_TOKENS_FIELD),
        include_stream_usage: true,
        usage_cost: false,
        provider_name: Cow::Borrowed("LlamaCpp"),
    },
    thinking_fallback: ThinkingFallback::BudgetField,
};

#[cfg(test)]
mod tests {
    use super::super::Timeouts;
    use super::*;

    /// maki-config keeps its own list of the slugs whose thinking keys it lets
    /// `providers.toml` set. A rename here would silently stop reading them and
    /// start reporting them as dropped.
    #[test]
    fn local_slugs_read_their_declared_thinking_keys() {
        for cfg in [OLLAMA, LLAMACPP] {
            assert!(maki_config::providers::overlays_local_thinking(cfg.slug));
        }
    }

    #[test]
    fn from_env_without_host_or_api_key_errors() {
        match LocalEndpoint::build(&OLLAMA, Timeouts::default(), None, None, None) {
            Err(AgentError::Config { message }) => {
                assert_eq!(message, "OLLAMA_HOST not set");
            }
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }

    #[test]
    fn from_env_with_host_builds_auth() {
        let ep = LocalEndpoint::build(
            &OLLAMA,
            Timeouts::default(),
            None,
            Some("http://x:1234".into()),
            None,
        )
        .unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(auth.headers.is_empty());
    }

    #[test]
    fn from_env_with_api_key_uses_cloud_for_ollama() {
        let pool = KeyPool::from_keys(vec!["test-key".into()]);
        let ep =
            LocalEndpoint::build(&OLLAMA, Timeouts::default(), Some(pool), None, None).unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("https://ollama.com/v1"));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn from_env_both_host_and_api_key_uses_host_with_auth() {
        let pool = KeyPool::from_keys(vec!["test-key".into()]);
        let ep = LocalEndpoint::build(
            &OLLAMA,
            Timeouts::default(),
            Some(pool),
            Some("http://local:1234".into()),
            None,
        )
        .unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://local:1234/v1"));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn llamacpp_without_host_errors() {
        match LocalEndpoint::build(&LLAMACPP, Timeouts::default(), None, None, None) {
            Err(AgentError::Config { message }) => {
                assert_eq!(message, "LLAMA_CPP_HOST not set");
            }
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }

    #[test]
    fn llamacpp_with_host_builds_auth() {
        let ep = LocalEndpoint::build(
            &LLAMACPP,
            Timeouts::default(),
            None,
            Some("http://x:1234".into()),
            None,
        )
        .unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(auth.headers.is_empty());
    }

    #[test]
    fn llamacpp_no_cloud_fallback() {
        let pool = KeyPool::from_keys(vec!["key".into()]);
        match LocalEndpoint::build(&LLAMACPP, Timeouts::default(), Some(pool), None, None) {
            Err(AgentError::Config { message }) => {
                assert_eq!(message, "LLAMA_CPP_HOST not set");
            }
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }

    #[test]
    fn ollama_uses_ollama_discovery() {
        let ep = LocalEndpoint::build(
            &OLLAMA,
            Timeouts::default(),
            None,
            Some("http://x:1234".into()),
            None,
        )
        .unwrap();
        assert!(matches!(ep.discovery_mode, DiscoveryMode::Ollama));
    }

    #[test]
    fn llamacpp_uses_llamacpp_discovery() {
        let ep = LocalEndpoint::build(
            &LLAMACPP,
            Timeouts::default(),
            None,
            Some("http://x:1234".into()),
            None,
        )
        .unwrap();
        assert!(matches!(ep.discovery_mode, DiscoveryMode::LlamaCpp));
    }

    mod extract_ctx {
        use super::super::*;

        fn model_with_meta(n_ctx: u32) -> LlamaCppModelData {
            LlamaCppModelData {
                id: "test".into(),
                meta: Some(LlamaCppMeta { n_ctx }),
                status: None,
                max_model_len: None,
                architecture: None,
            }
        }

        fn model_with_status(args: Vec<String>) -> LlamaCppModelData {
            LlamaCppModelData {
                id: "test".into(),
                meta: None,
                status: Some(LlamaCppStatus { args }),
                max_model_len: None,
                architecture: None,
            }
        }

        fn model_with_max_model_len(v: u32) -> LlamaCppModelData {
            LlamaCppModelData {
                id: "test".into(),
                meta: None,
                status: None,
                max_model_len: Some(v),
                architecture: None,
            }
        }

        fn model_empty() -> LlamaCppModelData {
            LlamaCppModelData {
                id: "test".into(),
                meta: None,
                status: None,
                max_model_len: None,
                architecture: None,
            }
        }

        #[test]
        fn router_mode_uses_meta_n_ctx() {
            let model = model_with_meta(32768);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Router, 0),
                32768
            );
        }

        #[test]
        fn router_mode_falls_back_to_ctx_size_arg() {
            let model = model_with_status(vec![
                "--model".into(),
                "foo.gguf".into(),
                "--ctx-size".into(),
                "16384".into(),
            ]);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Router, 0),
                16384
            );
        }

        #[test]
        fn router_mode_falls_back_to_fit_ctx_arg() {
            let model = model_with_status(vec![
                "--model".into(),
                "foo.gguf".into(),
                "--fit-ctx".into(),
                "24000".into(),
            ]);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Router, 0),
                24000
            );
        }

        #[test]
        fn router_mode_prefers_ctx_size_over_fit_ctx() {
            let model = model_with_status(vec![
                "--ctx-size".into(),
                "8192".into(),
                "--fit-ctx".into(),
                "16384".into(),
            ]);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Router, 0),
                8192
            );
        }

        #[test]
        fn router_mode_prefers_meta_over_args() {
            let mut model = model_with_meta(4096);
            model.status = Some(LlamaCppStatus {
                args: vec!["--ctx-size".into(), "65536".into()],
            });
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Router, 0),
                4096
            );
        }

        #[test]
        fn router_mode_defaults_when_no_info() {
            let model = model_empty();
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Router, 0),
                LLAMACPP_DEFAULT_CTX
            );
        }

        #[test]
        fn single_mode_uses_meta_n_ctx() {
            let model = model_with_meta(131072);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Single, 0),
                131072
            );
        }

        #[test]
        fn single_mode_defaults_when_no_meta() {
            let model = model_empty();
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Single, 0),
                LLAMACPP_DEFAULT_CTX
            );
        }

        #[test]
        fn legacy_mode_uses_max_model_len() {
            let model = model_with_max_model_len(4096);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Legacy, 0),
                4096
            );
        }

        #[test]
        fn legacy_mode_falls_back_to_props_n_ctx() {
            let model = model_empty();
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Legacy, 8192),
                8192
            );
        }

        #[test]
        fn legacy_mode_prefers_max_model_len_over_props_n_ctx() {
            let model = model_with_max_model_len(2048);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Legacy, 8192),
                2048
            );
        }

        #[test]
        fn legacy_mode_ignores_zero_max_model_len() {
            let model = model_with_max_model_len(0);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Legacy, 4096),
                4096
            );
        }

        #[test]
        fn legacy_mode_defaults_when_no_info() {
            let model = model_empty();
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Legacy, 0),
                LLAMACPP_DEFAULT_CTX
            );
        }

        #[test]
        fn zero_n_ctx_treated_as_absent() {
            let model = model_with_meta(0);
            assert_eq!(
                llamacpp_extract_ctx_from_model(&model, &ServerMode::Single, 0),
                LLAMACPP_DEFAULT_CTX
            );
        }
    }

    mod llamacpp_extract_ctx_arg {
        use super::super::*;

        #[test]
        fn extracts_value_after_flag() {
            let args = vec!["--ctx-size".into(), "4096".into()];
            assert_eq!(llamacpp_extract_ctx_arg(&args, "--ctx-size"), Some(4096));
        }

        #[test]
        fn returns_none_for_missing_flag() {
            let args = vec!["--model".into(), "foo.gguf".into()];
            assert_eq!(llamacpp_extract_ctx_arg(&args, "--ctx-size"), None);
        }

        #[test]
        fn returns_none_for_flag_at_end() {
            let args = vec!["--ctx-size".into()];
            assert_eq!(llamacpp_extract_ctx_arg(&args, "--ctx-size"), None);
        }

        #[test]
        fn returns_none_for_non_numeric_value() {
            let args = vec!["--ctx-size".into(), "abc".into()];
            assert_eq!(llamacpp_extract_ctx_arg(&args, "--ctx-size"), None);
        }

        #[test]
        fn finds_flag_among_others() {
            let args = vec![
                "--model".into(),
                "foo.gguf".into(),
                "--ctx-size".into(),
                "16384".into(),
                "--threads".into(),
                "8".into(),
            ];
            assert_eq!(llamacpp_extract_ctx_arg(&args, "--ctx-size"), Some(16384));
        }
    }

    mod llamacpp_props_vision {
        use super::super::*;

        #[test]
        fn reads_vision_from_modalities() {
            let props = json!({"modalities": {"vision": true, "audio": false}});
            assert_eq!(
                llamacpp_props_vision(&props, &ServerMode::Single),
                Some(true)
            );
        }

        #[test]
        fn returns_false_when_vision_disabled() {
            let props = json!({"modalities": {"vision": false, "audio": false}});
            assert_eq!(
                llamacpp_props_vision(&props, &ServerMode::Single),
                Some(false)
            );
        }

        #[test]
        fn returns_none_without_modalities() {
            let props = json!({"total_slots": 4});
            assert_eq!(llamacpp_props_vision(&props, &ServerMode::Single), None);
        }

        #[test]
        fn ignores_props_in_router_mode() {
            let props = json!({"role": "router", "modalities": {"vision": true}});
            assert_eq!(llamacpp_props_vision(&props, &ServerMode::Router), None);
        }
    }

    mod ollama_show_response {
        use test_case::test_case;

        use super::super::*;
        use crate::model::ModelEffort;

        const PARSE_FAILED: &str = "failed to parse /api/show body";
        const CAPABILITIES_PLACEHOLDER: &str = "__CAPABILITIES__";
        const THINKING_PLACEHOLDER: &str = "__THINKING__";
        const CONTEXT_LENGTH: u32 = 128_000;
        const DECLARED_NUM_CTX: u32 = 2048;
        const OVER_TRAINED_NUM_CTX: u32 = 200_000;

        /// Shaped like a real Ollama `POST /api/show` body, so a rename or a
        /// nesting change of any field discovery reads breaks these tests.
        const SHOW_BODY: &str = r##"{
  "license": "Apache License Version 2.0, January 2004",
  "modelfile": "# Modelfile generated by \"ollama show\"\nFROM /root/.ollama/blobs/sha256-abc",
  "parameters": "stop \"<|im_start|>\"\nnum_ctx 2048",
  "template": "{{ .Prompt }}",
  "details": {
    "parent_model": "",
    "format": "gguf",
    "family": "qwen25vl",
    "families": ["qwen25vl", "clip"],
    "parameter_size": "8.3B",
    "quantization_level": "Q4_K_M"
  },
  "model_info": {
    "general.architecture": "qwen25vl",
    "qwen25vl.context_length": 128000,
    "qwen25vl.embedding_length": 3584
  },
  __CAPABILITIES__
  __THINKING__
  "modified_at": "2026-01-01T12:00:00.000000000-05:00"
}"##;

        const THINKING_NAMED: &str = r#""thinking": {"values":[false,"low","medium","xhigh"],"default":"medium"},
"#;
        const THINKING_BOOLEAN: &str = r#""thinking": {"values":[false,true],"default":true},
"#;
        const THINKING_OFF_ONLY: &str = r#""thinking": {"values":[false],"default":false},
"#;
        const THINKING_NO_OFF: &str = r#""thinking": {"values":["low","high"],"default":"low"},
"#;

        fn fill(body: &str, capabilities: &str, thinking: &str) -> String {
            body.replace(CAPABILITIES_PLACEHOLDER, capabilities)
                .replace(THINKING_PLACEHOLDER, thinking)
        }

        fn expected(supported: Vec<Effort>, send_off: bool) -> Option<ModelEffort> {
            Some(ModelEffort {
                supported,
                send_off: Some(send_off),
            })
        }

        fn named_levels() -> Vec<Effort> {
            vec![Effort::Low, Effort::Medium, Effort::XHigh]
        }

        const CAPS_VISION: &str = r#""capabilities": ["completion", "vision"],
"#;
        const CAPS_TOOLS: &str = r#""capabilities": ["completion", "tools"],
"#;
        const CAPS_THINKING: &str = r#""capabilities": ["completion", "thinking"],
"#;

        #[test_case("", "", None, None, None ; "old_server_says_nothing")]
        #[test_case(CAPS_VISION, "", Some(false), None, Some(true) ; "vision_only")]
        #[test_case(CAPS_TOOLS, "", Some(false), None, Some(false) ; "no_vision_only")]
        #[test_case(CAPS_THINKING, "", Some(true), None, Some(false) ; "thinking_capability_without_values")]
        #[test_case("", THINKING_NAMED, Some(true), expected(named_levels(), true), None ; "named_levels")]
        #[test_case("", THINKING_BOOLEAN, Some(true), expected(vec![Effort::Medium], true), None ; "a_toggle_is_one_level")]
        #[test_case("", THINKING_OFF_ONLY, Some(false), expected(Vec::new(), true), None ; "off_only_cannot_think")]
        #[test_case("", THINKING_NO_OFF, Some(true), expected(vec![Effort::Low, Effort::High], false), None ; "no_off_sends_nothing_for_off")]
        fn parses_details(
            capabilities_field: &str,
            thinking_field: &str,
            expected_thinking: Option<bool>,
            expected_effort: Option<ModelEffort>,
            expected_vision: Option<bool>,
        ) {
            let body = fill(SHOW_BODY, capabilities_field, thinking_field);
            let show: OllamaShowResponse = serde_json::from_str(&body).expect(PARSE_FAILED);

            let details = ollama_model_details(&show);
            assert_eq!(details.supports_thinking, expected_thinking);
            assert_eq!(details.effort, expected_effort);
            assert_eq!(details.supports_vision, expected_vision);
            assert_eq!(details.context_window, Some(DECLARED_NUM_CTX));
        }

        #[test_case(Some(DECLARED_NUM_CTX), Some(CONTEXT_LENGTH), Some(DECLARED_NUM_CTX) ; "num_ctx_below_trained_wins")]
        #[test_case(Some(OVER_TRAINED_NUM_CTX), Some(CONTEXT_LENGTH), Some(CONTEXT_LENGTH) ; "num_ctx_capped_at_trained")]
        #[test_case(None, Some(CONTEXT_LENGTH), Some(CONTEXT_LENGTH) ; "trained_without_num_ctx")]
        #[test_case(Some(DECLARED_NUM_CTX), None, Some(DECLARED_NUM_CTX) ; "num_ctx_without_trained")]
        #[test_case(None, None, None ; "neither")]
        fn context_window_is_num_ctx_capped_at_trained(
            num_ctx: Option<u32>,
            trained: Option<u32>,
            expected: Option<u32>,
        ) {
            let show: OllamaShowResponse = serde_json::from_value(json!({
                "parameters": num_ctx.map(|n| format!("num_ctx {n}")),
                "model_info": trained.map(|n| json!({ "llama.context_length": n })),
            }))
            .expect(PARSE_FAILED);

            assert_eq!(ollama_model_details(&show).context_window, expected);
        }
    }

    mod ollama_served {
        use test_case::test_case;

        use super::super::*;
        use crate::Timeouts;
        use crate::test_support::{Canned, Requests, serve};

        const PATH_PS: &str = "/api/ps";
        const SERVED: u32 = 4096;
        const SHOW_TRAINED: u32 = 131_072;
        const PS_LOADED: &str = r#"{"models":[{"name":"llama3.2:latest","context_length":4096}]}"#;
        const PS_NONE: &str = r#"{"models":[]}"#;
        const MODELS_BODY: &str = r#"{"data":[{"id":"llama3.2:latest"}]}"#;
        const SHOW_BODY: &str = r#"{"model_info":{"llama.context_length":131072}}"#;

        fn served(names: &[&str]) -> Vec<OllamaPsModel> {
            names
                .iter()
                .map(|name| OllamaPsModel {
                    name: (*name).into(),
                    context_length: SERVED,
                })
                .collect()
        }

        #[test_case(&["llama3.2:latest"], "llama3.2", Some(SERVED) ; "untagged_id_is_latest")]
        #[test_case(&["llama3.2"], "llama3.2:latest", Some(SERVED) ; "untagged_name_is_latest")]
        #[test_case(&["qwen3:8b"], "qwen3:32b", None ; "sibling_tag_does_not_match")]
        #[test_case(&["host:5000/team/model"], "host:5000/team/model:latest", Some(SERVED) ; "port_is_not_a_tag")]
        #[test_case(&[], "llama3.2", None ; "nothing_loaded")]
        fn matches_loaded_model(loaded: &[&str], model_id: &str, expected: Option<u32>) {
            assert_eq!(ollama_served_window(&served(loaded), model_id), expected);
        }

        fn endpoint(
            cfg: &'static LocalEndpointConfig,
            script: &'static [Canned],
        ) -> (LocalEndpoint, Requests) {
            let (base_url, requests) = serve(script);
            let host = base_url.strip_suffix("/v1").map(str::to_owned);
            let ep = LocalEndpoint::build(cfg, Timeouts::default(), None, host, None).unwrap();
            (ep, requests)
        }

        const LOADED: &[Canned] = &[Canned::at(PATH_PS, Canned::json(200, PS_LOADED))];
        const NO_PS: &[Canned] = &[Canned::at(PATH_PS, Canned::json(404, "{}"))];

        #[test_case(LOADED, Some(SERVED) ; "loaded_model_reports_and_records")]
        #[test_case(NO_PS, None ; "server_without_api_ps")]
        #[tokio::test]
        async fn ollama_reports_the_served_window(
            script: &'static [Canned],
            expected: Option<u32>,
        ) {
            const ID: &str = "llama3.2";
            model_registry::set_known_models(OLLAMA_SLUG, vec![ModelInfo::id_only(ID.into())]);
            let (ep, _) = endpoint(&OLLAMA, script);
            let model = Model::from_spec(&format!("ollama/{ID}")).unwrap();

            assert_eq!(ep.served_context_window(&model).await, expected);
            assert_eq!(
                model_registry::discovered(OLLAMA_SLUG, ID).and_then(|info| info.context_window),
                expected
            );
        }

        /// A stray request on every turn would go unnoticed, so pin it.
        #[tokio::test]
        async fn llamacpp_never_asks() {
            let (ep, requests) = endpoint(&LLAMACPP, LOADED);
            let model = Model::from_spec("llama-cpp/default").unwrap();

            assert_eq!(ep.served_context_window(&model).await, None);
            assert!(requests.lock().unwrap().is_empty());
        }

        const DISCOVERY_LOADED: &[Canned] = &[
            Canned::at("/v1/models", Canned::json(200, MODELS_BODY)),
            Canned::at("/api/show", Canned::json(200, SHOW_BODY)),
            Canned::at(PATH_PS, Canned::json(200, PS_LOADED)),
        ];
        const DISCOVERY_UNLOADED: &[Canned] = &[
            Canned::at("/v1/models", Canned::json(200, MODELS_BODY)),
            Canned::at("/api/show", Canned::json(200, SHOW_BODY)),
            Canned::at(PATH_PS, Canned::json(200, PS_NONE)),
        ];

        #[test_case(DISCOVERY_LOADED, SERVED ; "loaded_model_lists_its_served_window")]
        #[test_case(DISCOVERY_UNLOADED, SHOW_TRAINED ; "unloaded_model_lists_its_declared_window")]
        #[tokio::test]
        async fn discovery_prefers_the_served_window(script: &'static [Canned], expected: u32) {
            let (ep, _) = endpoint(&OLLAMA, script);
            let models = ep.list_models().await.unwrap();

            assert_eq!(models[0].context_window, Some(expected));
        }
    }

    mod ollama_extract_context_length {
        use super::super::*;

        fn make_model_info(pairs: &[(&str, u64)]) -> serde_json::Map<String, Value> {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                map.insert(k.to_string(), Value::Number(serde_json::Number::from(*v)));
            }
            map
        }

        #[test]
        fn extracts_llama_context_length() {
            let info = make_model_info(&[("llama.context_length", 8192)]);
            assert_eq!(ollama_extract_context_length(&info), Some(8192));
        }

        #[test]
        fn extracts_gemma_context_length() {
            let info = make_model_info(&[("gemma3.context_length", 131072)]);
            assert_eq!(ollama_extract_context_length(&info), Some(131072));
        }

        #[test]
        fn extracts_plain_context_length() {
            let info = make_model_info(&[("context_length", 4096)]);
            assert_eq!(ollama_extract_context_length(&info), Some(4096));
        }

        #[test]
        fn prefers_architecture_specific_over_plain() {
            let info = make_model_info(&[("llama.context_length", 8192), ("context_length", 4096)]);
            assert_eq!(ollama_extract_context_length(&info), Some(8192));
        }

        #[test]
        fn returns_none_when_missing() {
            let info = make_model_info(&[("llama.embedding_length", 4096)]);
            assert_eq!(ollama_extract_context_length(&info), None);
        }

        #[test]
        fn returns_none_for_empty_map() {
            let info = serde_json::Map::new();
            assert_eq!(ollama_extract_context_length(&info), None);
        }

        #[test]
        fn ignores_non_numeric_value() {
            let mut map = serde_json::Map::new();
            map.insert(
                "llama.context_length".to_string(),
                Value::String("8192".into()),
            );
            assert_eq!(ollama_extract_context_length(&map), None);
        }
    }

    mod ollama_extract_num_ctx {
        use super::super::*;

        #[test]
        fn extracts_num_ctx_from_params() {
            let params = "temperature 0.7\nnum_ctx 4096\nseed 0";
            assert_eq!(ollama_extract_num_ctx(params), Some(4096));
        }

        #[test]
        fn handles_leading_whitespace() {
            let params = "  num_ctx  8192  ";
            assert_eq!(ollama_extract_num_ctx(params), Some(8192));
        }

        #[test]
        fn ignores_zero_num_ctx() {
            let params = "num_ctx 0";
            assert_eq!(ollama_extract_num_ctx(params), None);
        }

        #[test]
        fn returns_none_when_missing() {
            let params = "temperature 0.7\nseed 0";
            assert_eq!(ollama_extract_num_ctx(params), None);
        }

        #[test]
        fn returns_none_for_empty_string() {
            assert_eq!(ollama_extract_num_ctx(""), None);
        }

        #[test]
        fn returns_none_for_non_numeric_value() {
            let params = "num_ctx abc";
            assert_eq!(ollama_extract_num_ctx(params), None);
        }

        #[test]
        fn finds_num_ctx_among_other_params() {
            let params = "\
                temperature 0.8
                top_k 40
                top_p 0.9
                num_ctx 131072
                seed 1234
                ";
            assert_eq!(ollama_extract_num_ctx(params), Some(131072));
        }
    }
}
