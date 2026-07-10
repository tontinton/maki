use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use flume::Sender;
use isahc::config::Configurable;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{Model, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, EffortScale, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::{ResolvedAuth, http_client};
use crate::providers::anthropic::shared;

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(86400);

const MESSAGES_PATH: &str = "/messages";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointType {
    ChatCompletions,
    Messages,
}

type CatalogIndex = HashMap<String, CatalogProvider>;

#[derive(Deserialize, Serialize)]
struct CatalogProvider {
    name: String,
    #[serde(default)]
    env: Vec<String>,
    npm: String,
    api: Option<String>,
    models: HashMap<String, CatalogModel>,
}

impl CatalogProvider {
    fn resolve_api_key(&self) -> String {
        for var in &self.env {
            if let Ok(val) = std::env::var(var) {
                debug!(provider = %self.name, var = %var, "api key resolved from env");
                return val;
            }
            debug!(provider = %self.name, var = %var, "env var not set");
        }
        if self.env.iter().any(|v| v == "OPENCODE_API_KEY") {
            debug!(provider = %self.name, "using public fallback key");
            return "public".to_string();
        }
        debug!(provider = %self.name, "no api key available");
        String::new()
    }

    fn has_api_key(&self) -> bool {
        let found = self.env.iter().any(|v| std::env::var(v).is_ok());
        debug!(provider = %self.name, has_api_key = found, "api key presence check");
        found
    }

    fn build_auth(&self) -> ResolvedAuth {
        let api_key = self.resolve_api_key();
        let headers = match self.npm.as_str() {
            "@ai-sdk/anthropic" => vec![("x-api-key".into(), api_key)],
            _ => vec![("authorization".into(), format!("Bearer {api_key}"))],
        };
        ResolvedAuth {
            base_url: self.api.clone(),
            headers,
        }
    }
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogModel {
    limit: Option<CatalogLimits>,
    #[serde(default)]
    cost: Option<CatalogCost>,
    #[serde(default)]
    provider: Option<CatalogShape>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogLimits {
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    input: Option<u32>,
    #[serde(default)]
    output: Option<u32>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogShape {
    #[serde(default)]
    shape: Option<String>,
}

#[derive(Clone)]
struct CatalogMeta {
    provider_id: String,
    api_format: EndpointType,
    context: u32,
    output: u32,
    input_price: f64,
    output_price: f64,
    cache_read: f64,
    cache_write: f64,
}

type ModelWithProvider = (String, String);

struct CatalogData {
    entries: HashMap<ModelWithProvider, CatalogMeta>,
    auths: HashMap<String, ResolvedAuth>,
}

impl CatalogData {
    fn lookup(&self, key: &str) -> Result<(CatalogMeta, ResolvedAuth), AgentError> {
        let (provider, model_id) = key.split_once('/').ok_or_else(|| AgentError::Config {
            message: format!("invalid model key '{key}': expected 'provider/model_id'"),
        })?;
        let meta = self
            .entries
            .get(&(provider.to_string(), model_id.to_string()))
            .cloned()
            .ok_or_else(|| AgentError::Config {
                message: format!("model '{key}' not found in catalog"),
            })?;
        let auth =
            self.auths
                .get(&meta.provider_id)
                .cloned()
                .ok_or_else(|| AgentError::Config {
                    message: format!(
                        "auth for provider '{}' not found in catalog",
                        meta.provider_id
                    ),
                })?;
        Ok((meta, auth))
    }

    fn all_models(&self) -> Vec<ModelInfo> {
        let mut models: Vec<ModelInfo> = self
            .entries
            .iter()
            .map(|((provider, model_id), meta)| ModelInfo {
                id: format!("{provider}/{model_id}"),
                context_window: Some(meta.context),
                max_output_tokens: Some(meta.output),
                pricing: Some(ModelPricing {
                    input: meta.input_price,
                    output: meta.output_price,
                    cache_read: meta.cache_read,
                    cache_write: meta.cache_write,
                    fast: None,
                }),
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
}

static CATALOG_CHAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode (Catalog)",
};

pub struct Opencode {
    client: HttpClient,
    chat_compat: OpenAiCompatProvider,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

static CATALOG: OnceLock<Mutex<CatalogData>> = OnceLock::new();

impl Opencode {
    fn new_impl(timeouts: super::Timeouts, auth: Option<Arc<Mutex<ResolvedAuth>>>) -> Self {
        CATALOG.get_or_init(|| Mutex::new(init_catalog_blocking()));
        Self {
            client: http_client(timeouts),
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        }
    }

    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Ok(Self::new_impl(timeouts, None))
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self::new_impl(timeouts, Some(auth))
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        Ok(CATALOG.get().unwrap().lock().unwrap().all_models())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_chat_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let mut body = self.chat_compat.build_body(model, messages, system, tools);
        opts.thinking
            .apply_reasoning_effort(&mut body, EffortScale::PreferHigh);
        self.chat_compat
            .do_stream(model, &[], &body, event_tx, auth)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_messages(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let system_blocks = vec![shared::SystemBlock {
            r#type: "text",
            text: system,
            cache_control: Some(shared::EPHEMERAL),
        }];
        let mut body = shared::build_request_body_with_system(
            model,
            messages,
            &system_blocks,
            tools,
            opts.thinking,
        );
        body["model"] = json!(model.id);
        body["stream"] = json!(true);
        let json_body = serde_json::to_vec(&body)?;
        let request = auth
            .configure_request(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "{}{}",
                        auth.base_url.as_deref().unwrap_or(""),
                        MESSAGES_PATH
                    ))
                    .header("user-agent", super::user_agent())
                    .header("content-type", "application/json")
                    .header("anthropic-version", "2023-06-01"),
            )
            .body(json_body)?;

        debug!(model = %model.id, "sending Anthropic-format request via catalog");

        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            crate::providers::anthropic::parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }
}

impl Provider for Opencode {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let model_for_stream = model.clone();

            let model_id = &model_for_stream.id;
            let (sub_provider, actual_id) =
                model_id.split_once('/').unwrap_or(("opencode", model_id));

            let (meta, auth) = {
                let guard = CATALOG.get().unwrap().lock().unwrap();
                let (meta, auth) = guard.lookup(&format!("{sub_provider}/{actual_id}"))?;
                // Dynamic provider auth (e.g. from Lua) overrides the opencode route
                let auth = match (&self.auth, meta.provider_id.as_str()) {
                    (Some(provider_auth), "opencode") => provider_auth.lock().unwrap().clone(),
                    _ => auth,
                };
                (meta, auth)
            };

            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);

            let model = Model {
                id: actual_id.to_string(),
                max_output_tokens: meta.output,
                context_window: meta.context,
                ..model_for_stream
            };

            match meta.api_format {
                EndpointType::ChatCompletions => {
                    self.handle_catalog_chat_completions(
                        &model, messages, system, tools, event_tx, &auth, &opts,
                    )
                    .await
                }
                EndpointType::Messages => {
                    self.handle_catalog_messages(
                        &model, messages, system, tools, event_tx, &auth, &opts,
                    )
                    .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}

// --- Catalog helpers ---

fn catalog_cache_path() -> Option<PathBuf> {
    let dir = maki_storage::paths::cache_dir().ok()?;
    Some(dir.join(CATALOG_CACHE_FILE))
}

async fn load_cached_catalog_async() -> Option<CatalogIndex> {
    let path = catalog_cache_path()?;
    let meta = smol::unblock({
        let path = path.clone();
        move || fs::metadata(&path)
    })
    .await
    .ok()?;

    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > CATALOG_CACHE_TTL {
        debug!("catalog cache expired");
        return None;
    }

    let text = smol::unblock(move || fs::read_to_string(&path))
        .await
        .ok()?;
    let index: CatalogIndex = serde_json::from_str(&text).ok()?;
    debug!("loaded catalog from cache");
    Some(index)
}

async fn save_cached_catalog_async(index: &CatalogIndex) {
    let path = match catalog_cache_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let dir = dir.to_path_buf();
        let _ = smol::unblock(move || fs::create_dir_all(&dir)).await;
    }
    let text = match serde_json::to_string_pretty(index) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "failed to serialize catalog for cache");
            return;
        }
    };
    smol::unblock(move || {
        if let Err(e) = fs::write(&path, &text) {
            warn!(error = %e, path = %path.display(), "failed to write catalog cache");
        } else {
            debug!(path = %path.display(), "cached catalog");
        }
    })
    .await;
}

async fn fetch_remote_catalog_async(client: &HttpClient) -> Result<CatalogIndex, AgentError> {
    let request = Request::builder()
        .uri(CATALOG_URL)
        .header("user-agent", super::user_agent())
        .body(())?;

    let mut resp = client.send_async(request).await.map_err(|e| {
        warn!(error = %e, CATALOG_URL, "failed to fetch catalog");
        AgentError::Config {
            message: format!("failed to fetch catalog from {CATALOG_URL}: {e}"),
        }
    })?;

    let status = resp.status().as_u16();
    if status != 200 {
        return Err(AgentError::Api {
            status,
            message: format!("catalog fetch returned HTTP {status}"),
        });
    }

    let text = resp.text().await.map_err(|e| AgentError::Config {
        message: format!("failed to read catalog response body: {e}"),
    })?;

    serde_json::from_str(&text).map_err(|e| AgentError::Config {
        message: format!("failed to parse catalog JSON: {e}"),
    })
}

fn determine_catalog_format(npm: &str) -> EndpointType {
    match npm {
        "@ai-sdk/anthropic" => EndpointType::Messages,
        _ => EndpointType::ChatCompletions,
    }
}

const ALLOWED_NPM: &[&str] = &["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"];

fn catalog_to_data(index: CatalogIndex, enable_free_models: bool) -> CatalogData {
    let mut entries: HashMap<ModelWithProvider, CatalogMeta> = HashMap::new();
    let mut auths = HashMap::new();

    for (provider_id, provider) in &index {
        if !ALLOWED_NPM.contains(&provider.npm.as_str()) {
            debug!(npm = %provider.npm, "skipping provider: unsupported npm package");
            continue;
        }

        let Some(_base_url) = &provider.api else {
            debug!(provider = %provider_id, "skipping: no API URL in catalog");
            continue;
        };

        let has_key = provider.has_api_key();
        let auth = provider.build_auth();

        let mut model_count = 0u32;
        for (model_id, model_data) in &provider.models {
            let input_price = model_data
                .cost
                .as_ref()
                .and_then(|c| c.input)
                .unwrap_or(0.0);
            let output_price = model_data
                .cost
                .as_ref()
                .and_then(|c| c.output)
                .unwrap_or(0.0);
            let is_free = input_price == 0.0 && output_price == 0.0;

            if is_free && !enable_free_models {
                continue;
            }

            if !(has_key || provider_id == "opencode" && is_free) {
                continue;
            }

            let api_format = determine_catalog_format(&provider.npm);

            let context = model_data
                .limit
                .as_ref()
                .and_then(|l| l.context)
                .unwrap_or(128_000);
            let output = model_data
                .limit
                .as_ref()
                .and_then(|l| l.output)
                .unwrap_or(64_000);

            let cache_read = model_data
                .cost
                .as_ref()
                .and_then(|c| c.cache_read)
                .unwrap_or(0.0);
            let cache_write = model_data
                .cost
                .as_ref()
                .and_then(|c| c.cache_write)
                .unwrap_or(0.0);

            let key = (provider_id.clone(), model_id.clone());
            entries.insert(
                key,
                CatalogMeta {
                    provider_id: provider_id.clone(),
                    api_format,
                    context,
                    output,
                    input_price,
                    output_price,
                    cache_read,
                    cache_write,
                },
            );
            model_count += 1;
        }

        if model_count > 0 {
            auths.insert(provider_id.clone(), auth);
            debug!(
                provider = %provider_id,
                models = model_count,
                has_key,
                "catalog provider registered",
            );
        }
    }

    CatalogData { entries, auths }
}

fn init_catalog_blocking() -> CatalogData {
    let enable_free_models = maki_config::providers::ProvidersConfig::load()
        .get("opencode")
        .and_then(|d| d.enable_free_models)
        .unwrap_or(false);
    // Try cache first (fast, no network)
    if let Some(index) = smol::block_on(load_cached_catalog_async()) {
        debug!("using cached catalog");
        return catalog_to_data(index, enable_free_models);
    }

    // Fetch from remote (blocks the current thread)
    let client = isahc::HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .low_speed_timeout(1, Duration::from_secs(30))
        .build()
        .expect("failed to build catalog HTTP client");

    match smol::block_on(fetch_remote_catalog_async(&client)) {
        Ok(index) => {
            smol::block_on(save_cached_catalog_async(&index));
            catalog_to_data(index, enable_free_models)
        }
        Err(e) => {
            warn!(error = %e, "catalog fetch failed, using empty catalog");
            CatalogData {
                entries: HashMap::new(),
                auths: HashMap::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_format_messages_for_anthropic() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/anthropic"),
            EndpointType::Messages
        );
    }

    #[test]
    fn catalog_format_chat_for_openai_compat() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/openai-compatible"),
            EndpointType::ChatCompletions
        );
    }

    #[test]
    fn catalog_provider_roundtrip_json() {
        let provider = CatalogProvider {
            name: "Test Provider".into(),
            env: vec!["TEST_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://test.api/v1".into()),
            models: HashMap::from([(
                "test-model".into(),
                CatalogModel {
                    limit: Some(CatalogLimits {
                        context: Some(128_000),
                        input: None,
                        output: Some(64_000),
                    }),
                    cost: Some(CatalogCost {
                        input: Some(0.5),
                        output: Some(1.5),
                        cache_read: Some(0.1),
                        cache_write: Some(0.2),
                    }),
                    provider: None,
                },
            )]),
        };

        let json = serde_json::to_string_pretty(&provider).unwrap();
        let deserialized: CatalogProvider = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "Test Provider");
        assert_eq!(deserialized.npm, "@ai-sdk/openai-compatible");
        assert!(deserialized.models.contains_key("test-model"));
        let model = &deserialized.models["test-model"];
        let cost = model.cost.as_ref().unwrap();
        assert_eq!(cost.input, Some(0.5));
        assert_eq!(cost.output, Some(1.5));
    }

    #[test]
    fn catalog_index_roundtrip_json() {
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "test-provider".into(),
            CatalogProvider {
                name: "Test".into(),
                env: vec![],
                npm: "@ai-sdk/openai".into(),
                api: Some("https://test.api/v1".into()),
                models: HashMap::from([(
                    "test-model".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                    },
                )]),
            },
        );

        let json = serde_json::to_string_pretty(&providers).unwrap();
        let deserialized: CatalogIndex = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.len(), 1);
        assert!(deserialized.contains_key("test-provider"));
    }

    #[test]
    fn catalog_provider_missing_optional_fields() {
        let json = r#"{
            "name": "Minimal",
            "npm": "@ai-sdk/openai",
            "models": {}
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        assert_eq!(provider.name, "Minimal");
        assert!(provider.env.is_empty());
        assert!(provider.api.is_none());
        assert!(provider.models.is_empty());
    }

    #[test]
    fn catalog_model_missing_cost_and_provider() {
        let json = r#"{
            "name": "Test",
            "npm": "@ai-sdk/openai",
            "api": "https://test.api/v1",
            "models": {
                "m1": { "limit": {"context": 64000} }
            }
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        let model = &provider.models["m1"];
        assert_eq!(model.limit.as_ref().unwrap().context, Some(64000));
        assert!(model.cost.is_none());
        assert!(model.provider.is_none());
    }

    #[test]
    fn catalog_provider_resolve_api_key_from_env() {
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["MAKI_TEST_UNUSED_VAR_1".into()],
            npm: "@ai-sdk/openai".into(),
            api: None,
            models: HashMap::new(),
        };
        // No env var set — returns empty (no OPENCODE_API_KEY in env)
        assert!(provider.resolve_api_key().is_empty());
    }

    #[test]
    fn catalog_provider_resolve_api_key_anthropic_fallback() {
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["ANTHROPIC_SECRET_KEY".into()],
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        // ANTHROPIC_SECRET_KEY is not set.
        assert!(provider.resolve_api_key().is_empty());
    }

    #[test]
    fn catalog_provider_build_auth_bearer_default() {
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec![],
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let auth = provider.build_auth();
        assert_eq!(auth.headers[0].0, "authorization");
        assert_eq!(auth.headers[0].1, "Bearer ");
        assert_eq!(auth.base_url.as_deref(), None);
    }

    #[test]
    fn catalog_provider_build_auth_x_api_key() {
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec![],
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let auth = provider.build_auth();
        assert_eq!(auth.headers[0].0, "x-api-key");
        assert_eq!(auth.headers[0].1, "");
        assert_eq!(auth.base_url.as_deref(), None);
    }

    #[test]
    fn catalog_to_data_filters_nonfree_without_key() {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(2.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["MAKI_TEST_VENDOR_KEY_60924".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        let result = catalog_to_data(providers, true);
        // Without env var, NO models should pass (not even free ones for non-opencode)
        assert!(
            result.entries.is_empty(),
            "should be empty: {:?}",
            result.entries.keys()
        );
    }

    #[test]
    fn catalog_to_data_opencode_free_models_without_key() {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["MAKI_TEST_OPENCODE_FREE_60924".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let result = catalog_to_data(providers, true);
        // Without key set, has_api_key is false, so only free models pass
        // but has_api_key is false, so only free models pass
        assert!(
            result
                .entries
                .contains_key(&("opencode".into(), "free-model".into()))
        );
        assert!(
            !result
                .entries
                .contains_key(&("opencode".into(), "paid-model".into()))
        );
        // Route should exist
        assert!(result.auths.contains_key("opencode"));
    }

    #[test]
    fn catalog_to_data_opencode_all_models_with_key() {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["MAKI_TEST_OPENCODE_ALL_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_OPENCODE_ALL_81274", "real-key") };
        let result = catalog_to_data(providers, true);
        unsafe { std::env::remove_var("MAKI_TEST_OPENCODE_ALL_81274") };

        // With key set, has_api_key is true, so all models pass
        assert!(
            result
                .entries
                .contains_key(&("opencode".into(), "free-model".into()))
        );
        assert!(
            result
                .entries
                .contains_key(&("opencode".into(), "paid-model".into()))
        );
        assert!(result.auths.contains_key("opencode"));
    }

    fn opencode_catalog_with_free_and_paid(env_var: &str) -> CatalogIndex {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec![env_var.into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );
        providers
    }

    const DISABLE_FREE_TEST_KEY: &str = "MAKI_TEST_OPENCODE_DISABLE_FREE_94421";

    #[test]
    fn catalog_to_data_opencode_hides_free_models_when_disabled() {
        unsafe { std::env::set_var(DISABLE_FREE_TEST_KEY, "real-key") };
        let index = opencode_catalog_with_free_and_paid(DISABLE_FREE_TEST_KEY);
        let result = catalog_to_data(index, false);
        unsafe { std::env::remove_var(DISABLE_FREE_TEST_KEY) };

        assert!(
            !result
                .entries
                .contains_key(&("opencode".into(), "free-model".into())),
            "free model should be hidden when enable_free_models=false: {:?}",
            result.entries.keys()
        );
        assert!(
            result
                .entries
                .contains_key(&("opencode".into(), "paid-model".into()))
        );
        assert!(result.auths.contains_key("opencode"));
    }

    #[test]
    fn catalog_to_data_opencode_no_models_without_key_when_disabled() {
        let index = opencode_catalog_with_free_and_paid(DISABLE_FREE_TEST_KEY);
        let result = catalog_to_data(index, false);

        assert!(
            result.entries.is_empty(),
            "no models should be listed without a key when enable_free_models=false: {:?}",
            result.entries.keys()
        );
        assert!(!result.auths.contains_key("opencode"));
    }

    #[test]
    fn catalog_to_data_all_models_with_key() {
        let mut models = HashMap::new();
        models.insert(
            "cheap".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "freebie".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["MAKI_TEST_VENDOR_KEY_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        // Set the env var so has_key = true
        unsafe { std::env::set_var("MAKI_TEST_VENDOR_KEY_81274", "test-key") };
        let result = catalog_to_data(providers, true);
        unsafe { std::env::remove_var("MAKI_TEST_VENDOR_KEY_81274") };

        assert!(
            result
                .entries
                .contains_key(&("some-vendor".into(), "cheap".into()))
        );
        assert!(
            result
                .entries
                .contains_key(&("some-vendor".into(), "freebie".into()))
        );
    }

    #[test]
    fn catalog_to_data_skips_providers_without_api_url() {
        let mut providers = HashMap::new();
        providers.insert(
            "no-api".into(),
            CatalogProvider {
                name: "No API".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: None,
                models: HashMap::new(),
            },
        );

        let result = catalog_to_data(providers, true);
        assert!(result.entries.is_empty());
        assert!(result.auths.is_empty());
    }

    #[test]
    fn catalog_to_data_handles_model_id_collisions() {
        let mut models: HashMap<String, CatalogModel> = HashMap::new();
        models.insert(
            "shared-model".into(),
            CatalogModel {
                limit: Some(CatalogLimits {
                    context: Some(64_000),
                    input: Some(64_000),
                    output: Some(8_000),
                }),
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();

        // Provider "opencode" has "shared-model"
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models: models.clone(),
            },
        );

        // Provider "other-vendor" also has "shared-model"
        providers.insert(
            "other-vendor".into(),
            CatalogProvider {
                name: "Other".into(),
                env: vec!["MAKI_TEST_OTHER_KEY_COLLISION".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://other.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_OTHER_KEY_COLLISION", "key") };
        let result = catalog_to_data(providers, true);
        unsafe { std::env::remove_var("MAKI_TEST_OTHER_KEY_COLLISION") };

        // Both providers' entries are preserved
        assert!(
            result
                .entries
                .contains_key(&("opencode".into(), "shared-model".into()))
        );
        assert!(
            result
                .entries
                .contains_key(&("other-vendor".into(), "shared-model".into()))
        );
        assert_eq!(result.entries.len(), 2);

        // lookup prefers the "opencode" provider
        // lookup expects \"provider/model_id\" format
        let (meta, _) = result.lookup("opencode/shared-model").unwrap();
        assert_eq!(meta.provider_id, "opencode");
    }

    #[test]
    fn lookup_finds_opencode_own_models() {
        let mut models = HashMap::new();
        models.insert(
            "opus".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = catalog_to_data(providers, true);
        let (meta, _) = data.lookup("opencode/opus").unwrap();
        assert_eq!(meta.provider_id, "opencode");
    }

    #[test]
    fn lookup_finds_model_id_with_slashes() {
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["MAKI_TEST_NVIDIA_KEY_LOOKUP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_NVIDIA_KEY_LOOKUP", "key") };
        let data = catalog_to_data(providers, true);
        unsafe { std::env::remove_var("MAKI_TEST_NVIDIA_KEY_LOOKUP") };

        // Entry is stored as ("nvidia", "openai/gpt-oss-120b")
        let (meta, _) = data.lookup("nvidia/openai/gpt-oss-120b").unwrap();
        assert_eq!(meta.provider_id, "nvidia");
    }

    #[test]
    fn lookup_spec_is_sub_provider_plus_model_id() {
        // Simulates the stream_message pattern:
        // lookup key = "{sub_provider}/{model.id}"
        // e.g. "nvidia/openai/gpt-oss-120b"
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["MAKI_TEST_NVIDIA_DIRECT".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_NVIDIA_DIRECT", "key") };
        let data = catalog_to_data(providers, true);
        unsafe { std::env::remove_var("MAKI_TEST_NVIDIA_DIRECT") };

        // The lookup key constructed by stream_message:
        // format!("{}/{}", sub_provider, model.id)
        // = "nvidia/openai/gpt-oss-120b"
        let key = format!("{}/{}", "nvidia", "openai/gpt-oss-120b");
        let (meta, _) = data.lookup(&key).unwrap();
        assert_eq!(meta.provider_id, "nvidia");
    }

    #[test]
    fn lookup_nested_model_id_uses_sub_provider_key() {
        let mut models = HashMap::new();
        models.insert(
            "deepseek-ai/DeepSeek-R1".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "fireworks".into(),
            CatalogProvider {
                name: "Fireworks".into(),
                env: vec!["MAKI_TEST_FIREWORKS_DEEP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://fireworks.ai/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_FIREWORKS_DEEP", "key") };
        let data = catalog_to_data(providers, true);
        unsafe { std::env::remove_var("MAKI_TEST_FIREWORKS_DEEP") };

        // stream_message constructs key as "{sub_provider}/{model.id}"
        // = "fireworks/deepseek-ai/DeepSeek-R1"
        let key = format!("{}/{}", "fireworks", "deepseek-ai/DeepSeek-R1");
        let (meta, _) = data.lookup(&key).unwrap();
        assert_eq!(meta.provider_id, "fireworks");
    }
}
