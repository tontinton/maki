use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use flume::Sender;

use isahc::config::Configurable;
use isahc::prelude::ReadResponseExt;
use isahc::{HttpClient, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StreamResponse,
    ThinkingConfig,
};

use super::{ResolvedAuth, http_client};

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

    fn build_auth(&self) -> super::ResolvedAuth {
        let api_key = self.resolve_api_key();
        let headers = match self.npm.as_str() {
            "@ai-sdk/anthropic" => vec![("x-api-key".into(), api_key)],
            _ => vec![("authorization".into(), format!("Bearer {api_key}"))],
        };
        super::ResolvedAuth {
            base_url: self.api.clone(),
            headers,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct CatalogModel {
    limit: Option<CatalogLimits>,
    #[serde(default)]
    cost: Option<CatalogCost>,
    #[serde(default)]
    provider: Option<CatalogShape>,
}

#[derive(Deserialize, Serialize)]
struct CatalogLimits {
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    input: Option<u32>,
    #[serde(default)]
    output: Option<u32>,
}

#[derive(Deserialize, Serialize)]
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

#[derive(Deserialize, Serialize)]
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
    #[allow(dead_code)]
    input_price: f64,
    #[allow(dead_code)]
    output_price: f64,
    #[allow(dead_code)]
    cache_read: f64,
    #[allow(dead_code)]
    cache_write: f64,
}

#[derive(Clone)]
struct CatalogRoute {
    auth: super::ResolvedAuth,
}

impl Default for CatalogMeta {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            api_format: EndpointType::ChatCompletions,
            context: 128_000,
            output: 64_000,
            input_price: 0.0,
            output_price: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }
}

#[derive(Default)]
struct CatalogData {
    entries: HashMap<String, CatalogMeta>,
    routes: HashMap<String, CatalogRoute>,
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
    catalog: OnceLock<HashMap<String, CatalogMeta>>,
    catalog_routes: OnceLock<HashMap<String, CatalogRoute>>,
    stream_timeout: Duration,
}

impl Opencode {
    fn ensure_catalog(&self) -> &HashMap<String, CatalogMeta> {
        self.catalog.get_or_init(|| {
            let mut data = fetch_catalog().unwrap_or_default();

            // If a dynamic provider resolved auth (e.g. via Lua script), override
            // the opencode route's auth so env vars aren't required.
            if let (Some(auth), Some(route)) = (&self.auth, data.routes.get_mut("opencode")) {
                route.auth = auth.lock().unwrap().clone();
            }

            let _ = self.catalog_routes.set(data.routes);
            data.entries
        })
    }

    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            client: http_client(timeouts),
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: None,
            system_prefix: None,
            catalog: OnceLock::new(),
            catalog_routes: OnceLock::new(),
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            client: http_client(timeouts),
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: Some(auth),
            system_prefix: None,
            catalog: OnceLock::new(),
            catalog_routes: OnceLock::new(),
            stream_timeout: timeouts.stream,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_chat_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        route: &CatalogRoute,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let mut body = self.chat_compat.build_body(model, messages, system, tools);
        match opts.thinking {
            ThinkingConfig::Off => {}
            _ => {
                body["thinking"] = json!({"type": "enabled"});
            }
        }
        self.chat_compat
            .do_stream(model, &[], &body, event_tx, &route.auth)
            .await
    }

    async fn handle_catalog_messages(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        route: &CatalogRoute,
    ) -> Result<StreamResponse, AgentError> {
        let body = build_anthropic_body(model, messages, system, tools);
        let json_body = serde_json::to_vec(&body)?;
        let mut rb = Request::builder()
            .method("POST")
            .uri(format!(
                "{}{}",
                route.auth.base_url.as_deref().unwrap_or(""),
                MESSAGES_PATH
            ))
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        for (key, value) in &route.auth.headers {
            rb = rb.header(key.as_str(), value.as_str());
        }
        let request = rb.body(json_body)?;

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
        let catalog = self.ensure_catalog();
        let meta = catalog.get(&model.id).cloned();
        let route = meta.as_ref().and_then(|m| {
            let r = self
                .catalog_routes
                .get()
                .and_then(|routes| routes.get(&m.provider_id).cloned());
            if r.is_some() {
                debug!(model = %model.id, provider = %m.provider_id, "model route resolved");
            } else {
                debug!(model = %model.id, provider = %m.provider_id, "model route not found");
            }
            r
        });
        let model_for_stream = model.clone();

        Box::pin(async move {
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);

            let (meta, route) = match (meta, route) {
                (Some(m), Some(r)) => (m, r),
                _ => {
                    return Err(AgentError::Config {
                        message: format!("model '{}' not found in catalog", model_for_stream.id),
                    });
                }
            };

            let model = Model {
                max_output_tokens: meta.output,
                context_window: meta.context,
                ..model_for_stream
            };

            match meta.api_format {
                EndpointType::ChatCompletions => {
                    self.handle_catalog_chat_completions(
                        &model, messages, system, tools, event_tx, &route, &opts,
                    )
                    .await
                }
                EndpointType::Messages => {
                    self.handle_catalog_messages(&model, messages, system, tools, event_tx, &route)
                        .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        let catalog = self.ensure_catalog();
        let mut ids: Vec<String> = catalog.keys().cloned().collect();
        ids.sort();
        Box::pin(async move { Ok(ids) })
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

fn load_cached_catalog() -> Option<CatalogIndex> {
    let path = catalog_cache_path()?;
    let meta = fs::metadata(&path).ok()?;

    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > CATALOG_CACHE_TTL {
        debug!("catalog cache expired");
        return None;
    }

    let text = fs::read_to_string(&path).ok()?;
    let index: CatalogIndex = serde_json::from_str(&text).ok()?;
    debug!("loaded catalog from cache");
    Some(index)
}

fn save_cached_catalog(index: &CatalogIndex) {
    let path = match catalog_cache_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(index) {
        Ok(text) => {
            if let Err(e) = fs::write(&path, &text) {
                warn!(error = %e, path = %path.display(), "failed to write catalog cache");
            } else {
                debug!(path = %path.display(), "cached catalog");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize catalog for cache"),
    }
}

fn fetch_remote_catalog(client: &HttpClient) -> Result<CatalogIndex, AgentError> {
    let mut resp = client.get(CATALOG_URL).map_err(|e| {
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

    let text = resp.text().map_err(|e| AgentError::Config {
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

fn catalog_to_data(index: CatalogIndex) -> CatalogData {
    let mut entries = HashMap::new();
    let mut routes = HashMap::new();

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

            entries.insert(
                model_id.clone(),
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
            routes.insert(provider_id.clone(), CatalogRoute { auth });
            debug!(
                provider = %provider_id,
                models = model_count,
                has_key,
                "catalog provider registered",
            );
        }
    }

    CatalogData { entries, routes }
}

fn fetch_catalog() -> Result<CatalogData, AgentError> {
    if let Some(index) = load_cached_catalog() {
        return Ok(catalog_to_data(index));
    }

    let client = isahc::HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .low_speed_timeout(1, Duration::from_secs(30))
        .build()
        .map_err(|e| AgentError::Config {
            message: format!("failed to build HTTP client for catalog fetch: {e}"),
        })?;

    let index = fetch_remote_catalog(&client)?;
    save_cached_catalog(&index);
    Ok(catalog_to_data(index))
}

// --- Anthropic-format body building ---

pub(crate) fn build_anthropic_body(
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
) -> Value {
    let wire_messages = convert_to_anthropic_messages(messages);
    let wire_tools = convert_to_anthropic_tools(tools);

    let system_blocks: Vec<Value> = if system.is_empty() {
        vec![]
    } else {
        vec![json!({"type": "text", "text": system})]
    };

    let mut body = json!({
        "model": model.id,
        "max_tokens": model.max_output_tokens,
        "stream": true,
        "messages": wire_messages,
    });
    if !system_blocks.is_empty() {
        body["system"] = json!(system_blocks);
    }
    if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
        body["tools"] = wire_tools;
    }
    body
}

pub(crate) fn convert_to_anthropic_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content: Vec<Value> = msg
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => {
                        json!({"type": "text", "text": text})
                    }
                    ContentBlock::Image { source } => {
                        json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": match source.media_type {
                                    crate::ImageMediaType::Png => "image/png",
                                    crate::ImageMediaType::Jpeg => "image/jpeg",
                                    crate::ImageMediaType::Gif => "image/gif",
                                    crate::ImageMediaType::Webp => "image/webp",
                                },
                                "data": source.data,
                            }
                        })
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        })
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": content,
                            "is_error": is_error,
                        })
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        json!({"type": "thinking", "thinking": thinking})
                    }
                    ContentBlock::RedactedThinking { data } => {
                        json!({"type": "redacted_thinking", "data": data})
                    }
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

pub(crate) fn convert_to_anthropic_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    Value::Array(arr.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_to_anthropic_messages_basic() {
        use crate::types::Message as Msg;
        let messages = vec![Msg::user("hello".into())];
        let result = convert_to_anthropic_messages(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"][0]["type"], "text");
        assert_eq!(result[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn convert_to_anthropic_messages_with_tools() {
        use crate::types::{ContentBlock as CB, Message as Msg, Role};
        let messages = vec![
            Msg {
                role: Role::Assistant,
                content: vec![
                    CB::Text {
                        text: "I'll check".into(),
                    },
                    CB::ToolUse {
                        id: "tc_1".into(),
                        name: "bash".into(),
                        input: json!({"cmd": "ls"}),
                    },
                ],
                ..Default::default()
            },
            Msg {
                role: Role::User,
                content: vec![CB::ToolResult {
                    tool_use_id: "tc_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let result = convert_to_anthropic_messages(&messages);

        assert_eq!(result[0]["role"], "assistant");
        assert_eq!(result[0]["content"][0]["text"], "I'll check");
        assert_eq!(result[0]["content"][1]["type"], "tool_use");
        assert_eq!(result[0]["content"][1]["id"], "tc_1");
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[1]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn build_anthropic_body_includes_system_and_model() {
        let model = Model::from_spec("opencode/claude-sonnet-4-6").unwrap();
        let messages = vec![Message::user("hi".into())];
        let body = build_anthropic_body(&model, &messages, "be helpful", &json!([]));

        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["system"][0]["text"], "be helpful");
        assert_eq!(body["max_tokens"], 128000);
        assert_eq!(body["stream"], true);
    }

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

        let result = catalog_to_data(providers);
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
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let result = catalog_to_data(providers);
        // Without OPENCODE_API_KEY, "public" is used as default key (auth only),
        // but has_api_key is false, so only free models pass
        assert!(result.entries.contains_key("free-model"));
        assert!(!result.entries.contains_key("paid-model"));
        // Route should exist
        assert!(result.routes.contains_key("opencode"));
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
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("OPENCODE_API_KEY", "real-key") };
        let result = catalog_to_data(providers);
        unsafe { std::env::remove_var("OPENCODE_API_KEY") };

        // With OPENCODE_API_KEY set, has_api_key is true, so all models pass
        assert!(result.entries.contains_key("free-model"));
        assert!(result.entries.contains_key("paid-model"));
        assert!(result.routes.contains_key("opencode"));
    }

    #[test]
    fn catalog_to_data_all_models_with_key() {
        let mut models = HashMap::new();
        models.insert(
            "cheap".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.5),
                    output: Some(1.5),
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
        let result = catalog_to_data(providers);
        unsafe { std::env::remove_var("MAKI_TEST_VENDOR_KEY_81274") };

        assert!(result.entries.contains_key("cheap"));
        assert!(result.entries.contains_key("freebie"));
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

        let result = catalog_to_data(providers);
        assert!(result.entries.is_empty());
        assert!(result.routes.is_empty());
    }
}
