use std::sync::{Arc, Mutex};

use flume::Sender;
use futures::future::join_all;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use maki_config::providers::Protocol;

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::openai::responses;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

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
    pub thinking_budget_field: bool,
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
    thinking_budget_field: bool,
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
            thinking_budget_field: cfg.thinking_budget_field,
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
        Ok(Self {
            compat: OpenAiCompatProvider::new(compat_config, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth {
                base_url: Some(base_url),
                headers,
            })),
            key_pool,
            system_prefix: None,
            thinking_budget_field: cfg.thinking_budget_field,
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
            let mut body = self.compat.build_body(model, messages, system, tools);

            if self.thinking_budget_field {
                opts.thinking.apply_local_thinking(&mut body, model);
            }

            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            match self.discovery_mode {
                DiscoveryMode::None => self.compat.do_list_models(&auth).await,
                DiscoveryMode::LlamaCpp => self.discover_llamacpp_models(&auth).await,
                DiscoveryMode::Ollama => self.discover_ollama_models(&auth).await,
            }
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self.key_pool.as_ref().is_some_and(|p| {
                p.rotate_headers(&self.auth, |key| {
                    vec![("authorization".into(), format!("Bearer {key}"))]
                })
            }))
        })
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
    ) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let base = auth
            .base_url
            .as_deref()
            .unwrap_or(self.compat.config().base_url);
        let root = base.strip_suffix("/v1").unwrap_or(base);

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

        let mut models: Vec<crate::model::ModelInfo> = body
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
                let supports_vision = arch.map(|a| a.input_modalities.iter().any(|m| m == "image"));
                Some(crate::model::ModelInfo {
                    id: m.id,
                    context_window: Some(context_window),
                    max_output_tokens: None,
                    pricing: Some(crate::model::ModelPricing::ZERO),
                    supports_thinking: None,
                    supports_vision,
                    provider_info: None,
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
}

impl LocalEndpoint {
    async fn discover_ollama_models(
        &self,
        auth: &ResolvedAuth,
    ) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let base = auth
            .base_url
            .as_deref()
            .unwrap_or(self.compat.config().base_url);
        let root = base.strip_suffix("/v1").unwrap_or(base);

        let models_text = self
            .compat
            .get_text(auth, &format!("{root}/v1/models"))
            .await?;
        let body: OllamaModelsResponse = serde_json::from_str(&models_text)?;

        let compat = &self.compat;
        let futures: Vec<_> = body
            .data
            .iter()
            .map(|m| ollama_fetch_context_window(compat, auth, root, &m.id))
            .collect();
        let context_windows = join_all(futures).await;

        let mut models: Vec<crate::model::ModelInfo> = body
            .data
            .into_iter()
            .zip(context_windows)
            .map(|(m, context_window)| crate::model::ModelInfo {
                id: m.id,
                context_window,
                max_output_tokens: None,
                pricing: Some(crate::model::ModelPricing::ZERO),
                supports_thinking: None,
                supports_vision: None,
                provider_info: None,
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

async fn ollama_fetch_context_window(
    compat: &OpenAiCompatProvider,
    auth: &ResolvedAuth,
    root: &str,
    model_id: &str,
) -> Option<u32> {
    let show_url = format!("{root}/api/show");
    let body = json!({"model": model_id});
    let json_body = serde_json::to_vec(&body).ok()?;

    let text = match compat
        .post_text(auth, &show_url, "application/json", &json_body)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(model = model_id, error = %e, "Ollama POST /api/show failed");
            return None;
        }
    };
    let show: OllamaShowResponse = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            warn!(model = model_id, error = %e, "Failed to parse Ollama /api/show response");
            return None;
        }
    };

    if let Some(ref info) = show.model_info
        && let Some(ctx) = ollama_extract_context_length(info)
    {
        return Some(ctx);
    }

    if let Some(ref params) = show.parameters
        && let Some(ctx) = ollama_extract_num_ctx(params)
    {
        return Some(ctx);
    }

    None
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

pub(crate) const OLLAMA: LocalEndpointConfig = LocalEndpointConfig {
    slug: "ollama",
    display_name: "Ollama",
    host_env: "OLLAMA_HOST",
    api_key_env: "OLLAMA_API_KEY",
    default_host: "http://localhost:11434",
    default_model: "ollama/qwen3",
    cloud_fallback_url: Some("https://ollama.com/v1"),
    discovery_mode: DiscoveryMode::Ollama,
    compat: OpenAiCompatConfig {
        api_key_env: "",
        base_url: "http://localhost:11434/v1",
        max_tokens_field: "max_tokens",
        include_stream_usage: true,
        provider_name: "Ollama",
    },
    thinking_budget_field: false,
};

pub(crate) const LLAMACPP: LocalEndpointConfig = LocalEndpointConfig {
    slug: "llama-cpp",
    display_name: "LlamaCpp",
    host_env: "LLAMA_CPP_HOST",
    api_key_env: "LLAMA_CPP_API_KEY",
    default_host: "http://localhost:8080",
    default_model: "llama-cpp/default",
    cloud_fallback_url: None,
    discovery_mode: DiscoveryMode::LlamaCpp,
    compat: OpenAiCompatConfig {
        api_key_env: "",
        base_url: "http://localhost:8080/v1",
        max_tokens_field: "max_tokens",
        include_stream_usage: true,
        provider_name: "LlamaCpp",
    },
    thinking_budget_field: true,
};

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TIMEOUTS: super::super::Timeouts = super::super::Timeouts {
        connect: std::time::Duration::from_secs(10),
        low_speed: std::time::Duration::from_secs(30),
        stream: std::time::Duration::from_secs(300),
    };

    #[test]
    fn from_env_without_host_or_api_key_errors() {
        match LocalEndpoint::build(&OLLAMA, TEST_TIMEOUTS, None, None, None) {
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
            TEST_TIMEOUTS,
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
        let ep = LocalEndpoint::build(&OLLAMA, TEST_TIMEOUTS, Some(pool), None, None).unwrap();
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
            TEST_TIMEOUTS,
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
        match LocalEndpoint::build(&LLAMACPP, TEST_TIMEOUTS, None, None, None) {
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
            TEST_TIMEOUTS,
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
        match LocalEndpoint::build(&LLAMACPP, TEST_TIMEOUTS, Some(pool), None, None) {
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
            TEST_TIMEOUTS,
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
            TEST_TIMEOUTS,
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
