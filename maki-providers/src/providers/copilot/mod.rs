use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use futures_lite::io::BufReader;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::anthropic::shared;
use super::openai::responses;
use super::openai_compat;
use crate::model::{
    Model, ModelEntry, ModelFamily, ModelInfo, ModelPricing, ModelTier, lookup_entry,
};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, Effort, EffortDialect, Message, ProviderEvent, RequestOptions, StreamResponse,
    ThinkingConfig, dialect,
};

pub mod auth;

const SLUG: &str = "copilot";
const DEFAULT_API_ENDPOINT: &str = "https://api.githubcopilot.com";

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: SLUG,
    display_name: "Copilot",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: DEFAULT_API_ENDPOINT,
    default_api_key_env: "GH_COPILOT_TOKEN",
    default_model: "copilot/gpt-5.6-terra",
    plans: None,
    login_url: Some("https://github.com/settings/copilot"),
    needs_url: false,
});
const GRAPHQL_QUERY: &str = "query { viewer { copilotEndpoints { api } } }";
const API_VERSION_HEADER: &str = "2025-10-01";
const EDITOR_VERSION_HEADER: &str = concat!("Maki/", env!("CARGO_PKG_VERSION"));
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const MESSAGES_PATH: &str = "/v1/messages";
const MODELS_PATH: &str = "/models";

/// Scales `/models` AI-credit prices (1 credit = $0.01) to USD per 1M tokens.
const AIC_TO_USD_PER_MILLION: f64 = 10_000.0;

/// Fallback pricing used until `/models` reports `billing.token_prices` (or
/// for offline runs). The API wins via discovered metadata; these mirror
/// GitHub's published rates (usage-based billing since June 2026,
/// docs.github.com/copilot/reference/copilot-billing/models-and-pricing), at
/// the default context tier.
pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["gpt-5-mini"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.25, 2.00, 0.00, 0.025),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4-mini"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.75, 4.50, 0.00, 0.075),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4-nano"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.20, 1.25, 0.00, 0.02),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-haiku-4.5"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(1.00, 5.00, 1.25, 0.10),
            max_output_tokens: Some(64_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gemini-3.5-flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(1.50, 9.00, 0.00, 0.15),
            max_output_tokens: Some(65_536),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gemini-3.6-flash"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.75, 3.75, 0.00, 0.075),
            max_output_tokens: Some(65_536),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gemini-3.7-flash"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.75, 3.75, 0.00, 0.075),
            max_output_tokens: Some(65_536),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["mai-code-1-flash-picker"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.75, 4.50, 0.00, 0.075),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4.5", "claude-sonnet-4.6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(3.00, 15.00, 3.75, 0.30),
            max_output_tokens: Some(64_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(2.00, 10.00, 2.50, 0.20),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(5.00, 30.00, 0.00, 0.50),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["kimi-k2.7-code"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(0.95, 4.00, 0.00, 0.19),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["kimi-k3"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(3.00, 15.00, 0.00, 0.30),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gemini-3.1-pro-preview"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(2.00, 12.00, 0.00, 0.20),
            max_output_tokens: Some(65_536),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-luna"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_token(0.20, 1.20, 0.25, 0.02),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(2.50, 15.00, 0.00, 0.25),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-sol"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(5.00, 30.00, 6.25, 0.50),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-terra"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_token(2.00, 12.00, 2.50, 0.20),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.3-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(1.75, 14.00, 0.00, 0.175),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &[
                "claude-opus-5",
                "claude-opus-4.8",
                "claude-opus-4.7",
                "claude-opus-4.6",
                "claude-opus-4.5",
            ],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_token(5.00, 25.00, 6.25, 0.50),
            max_output_tokens: Some(64_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4.8-fast", "claude-fable-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(10.00, 50.00, 12.50, 1.00),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["grok-4.5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(2.00, 6.00, 0.00, 0.50),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["grok-4.6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(2.00, 6.00, 0.00, 0.50),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
    ];
    MODELS
}

pub struct Copilot {
    client: HttpClient,
    stream_timeout: Duration,
    auth: Arc<Mutex<Option<CopilotAuth>>>,
    resolved_auth: Option<Arc<Mutex<super::ResolvedAuth>>>,
    system_prefix: Option<String>,
    models: Arc<Mutex<HashMap<String, CopilotModel>>>,
}

impl Copilot {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        auth::load_token()?;
        Ok(Self {
            client: super::http_client(timeouts),
            stream_timeout: timeouts.stream,
            auth: Arc::default(),
            resolved_auth: None,
            system_prefix: None,
            models: Arc::default(),
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: super::http_client(timeouts),
            stream_timeout: timeouts.stream,
            auth: Arc::default(),
            resolved_auth: Some(auth),
            system_prefix: None,
            models: Arc::default(),
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn auth(&self) -> Result<CopilotAuth, AgentError> {
        if let Some(auth) = &self.resolved_auth {
            return copilot_auth_from_resolved(&auth.lock().unwrap());
        }

        if let Some(auth) = self.auth.lock().unwrap().clone() {
            return Ok(auth);
        }

        let creds = auth::load_token()?;
        let host = creds.host.as_deref().unwrap_or("github.com");
        let endpoint =
            discover_api_endpoint(&self.client, &creds.api_key, &auth::graphql_url(host)).await;
        let auth = CopilotAuth {
            token: creds.api_key,
            endpoint,
        };
        *self.auth.lock().unwrap() = Some(auth.clone());
        Ok(auth)
    }

    async fn model_endpoint(&self, model_id: &str) -> Result<Endpoint, AgentError> {
        if let Some(model) = self.models.lock().unwrap().get(model_id).cloned() {
            return Ok(model.endpoint());
        }

        let models = self.fetch_models().await?;
        let mut guard = self.models.lock().unwrap();
        guard.clear();
        guard.extend(models.into_iter().map(|model| (model.id.clone(), model)));
        Ok(guard
            .get(model_id)
            .map(CopilotModel::endpoint)
            .unwrap_or_else(|| guess_endpoint(model_id)))
    }

    async fn fetch_models(&self) -> Result<Vec<CopilotModel>, AgentError> {
        let auth = self.auth().await?;
        let request = copilot_request(
            Request::builder()
                .method("GET")
                .uri(format!("{}{MODELS_PATH}", auth.endpoint)),
            &auth,
            None,
        )
        .body(())?;

        let mut response = self.client.send_async(request).await?;
        if !response.status().is_success() {
            return Err(AgentError::from_response(response).await);
        }

        let body: CopilotModelsResponse = serde_json::from_str(&response.text().await?)?;
        let mut models = body
            .data
            .into_iter()
            .filter_map(
                |value| match serde_json::from_value::<CopilotModel>(value) {
                    Ok(model) => Some(model),
                    Err(err) => {
                        warn!(error = %err, "skipping malformed Copilot model metadata");
                        None
                    }
                },
            )
            .filter(CopilotModel::is_enabled_chat_model)
            .collect::<Vec<_>>();

        if let Some(default_pos) = models.iter().position(|model| model.is_chat_default) {
            let default_model = models.remove(default_pos);
            models.insert(0, default_model);
        }

        Ok(models)
    }

    async fn stream_chat_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let wire_tools = openai_compat::convert_tools(tools);
        let mut body = json!({
            "model": model.id,
            "messages": openai_compat::convert_messages(messages, system),
            "n": 1,
            "stream": true,
            "temperature": 0.1,
        });
        if wire_tools.as_array().is_some_and(|tools| !tools.is_empty()) {
            body["tools"] = wire_tools;
        }

        let request = self
            .build_post(
                &auth,
                CHAT_COMPLETIONS_PATH,
                Some("conversation-agent"),
                &body,
            )?
            .body(serde_json::to_vec(&body)?)?;
        let response = self.client.send_async(request).await?;
        if response.status().is_success() {
            openai_compat::parse_sse(
                BufReader::new(response.into_body()),
                event_tx,
                self.stream_timeout,
            )
            .await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn stream_responses(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let mut body = responses::build_body(model, messages, system, tools);
        let reasoning_info = crate::model_registry::provider_info::<CopilotModelInfo>(
            "copilot", &model.id,
        )
        .or_else(|| {
            self.models
                .lock()
                .unwrap()
                .get(&model.id)
                .map(CopilotModel::reasoning_info)
                .map(Arc::new)
        });
        if let Some(info) = reasoning_info {
            responses::apply_responses_reasoning(
                &mut body,
                thinking,
                model,
                &effort_dialect(&info),
            );
        }
        let resolved =
            super::ResolvedAuth::new(SLUG, copilot_headers(&auth, Some("conversation-agent")))?
                .with_base_url(Some(auth.endpoint.clone()));
        responses::do_stream(
            &self.client,
            model,
            &body,
            event_tx,
            &resolved,
            self.stream_timeout,
        )
        .await
    }

    async fn stream_messages(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let mut body = json!({
            "model": model.id,
            "max_tokens": model.max_output_tokens.unwrap_or(shared::FALLBACK_MAX_TOKENS),
            "system": [{"type": "text", "text": system}],
            "messages": anthropic_messages(messages),
            "tools": tools,
            "stream": true,
        });
        thinking.apply_to_body(&mut body, model);

        let request = self
            .build_post(&auth, MESSAGES_PATH, Some("conversation-agent"), &body)?
            .header("anthropic-version", "2023-06-01")
            .body(serde_json::to_vec(&body)?)?;
        let response = self.client.send_async(request).await?;
        if response.status().is_success() {
            super::anthropic::parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    fn build_post(
        &self,
        auth: &CopilotAuth,
        path: &str,
        interaction_type: Option<&str>,
        body: &Value,
    ) -> Result<isahc::http::request::Builder, AgentError> {
        debug!(
            path,
            body_bytes = serde_json::to_vec(body)?.len(),
            "sending Copilot API request"
        );
        Ok(copilot_request(
            Request::builder()
                .method("POST")
                .uri(format!("{}{path}", auth.endpoint)),
            auth,
            interaction_type,
        ))
    }
}

#[derive(Clone)]
struct CopilotAuth {
    token: String,
    endpoint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
}

#[derive(Clone, Deserialize)]
struct CopilotModel {
    id: String,
    #[serde(default)]
    policy: Option<CopilotModelPolicy>,
    #[serde(default)]
    capabilities: CopilotModelCapabilities,
    #[serde(default)]
    billing: CopilotModelBilling,
    #[serde(default)]
    is_chat_default: bool,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    model_picker_category: Option<CopilotModelCategory>,
    #[serde(default)]
    supported_endpoints: Vec<String>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelBilling {
    #[serde(default)]
    token_prices: Option<CopilotTokenPrices>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotTokenPrices {
    #[serde(default)]
    batch_size: u32,
    #[serde(default)]
    default: Option<CopilotTokenPriceTier>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotTokenPriceTier {
    #[serde(default)]
    input_price: f64,
    #[serde(default)]
    output_price: f64,
    #[serde(default)]
    cache_price: f64,
}

impl CopilotModel {
    fn is_enabled_chat_model(&self) -> bool {
        self.model_picker_enabled
            && self.capabilities.model_type == "chat"
            && self
                .policy
                .as_ref()
                .is_none_or(|policy| policy.state == "enabled")
    }

    fn model_info(&self) -> ModelInfo {
        let reasoning = self.reasoning_info();
        ModelInfo {
            id: self.id.clone(),
            context_window: self.capabilities.limits.max_context_window_tokens,
            max_output_tokens: self.capabilities.limits.max_output_tokens,
            pricing: self.pricing(),
            supports_thinking: Some(self.supports_thinking()),
            supports_vision: Some(self.capabilities.supports.vision),
            tier: self
                .model_picker_category
                .and_then(CopilotModelCategory::tier),
            provider_info: Some(Arc::new(reasoning)),
        }
    }

    /// The chat completions body carries no reasoning field, so declaring
    /// thinking there would offer the user a setting the request drops.
    fn supports_thinking(&self) -> bool {
        let supports = &self.capabilities.supports;
        self.endpoint() != Endpoint::ChatCompletions
            && (!supports.reasoning_effort.is_empty()
                || supports.adaptive_thinking
                || supports.max_thinking_budget.is_some()
                || supports.min_thinking_budget.is_some())
    }

    fn reasoning_info(&self) -> CopilotModelInfo {
        let mut reasoning_efforts = self
            .capabilities
            .supports
            .reasoning_effort
            .iter()
            .filter_map(|effort| effort.parse().ok())
            .collect::<Vec<_>>();
        reasoning_efforts.sort_unstable();
        reasoning_efforts.dedup();
        CopilotModelInfo {
            reasoning_off: self
                .capabilities
                .supports
                .reasoning_effort
                .iter()
                .any(|effort| effort == dialect::OFF),
            reasoning_efforts,
            adaptive_thinking: self.capabilities.supports.adaptive_thinking,
        }
    }

    /// `/models` reports prices in AI credits per billing batch (1 credit =
    /// $0.01), scaled to USD per 1M tokens for [`ModelPricing`]. The endpoint
    /// exposes only the default context tier and cached-input reads; cache
    /// writes are inherited from the static manifest by id prefix so cost
    /// accounting matches the offline path.
    fn pricing(&self) -> Option<ModelPricing> {
        let token_prices = self.billing.token_prices.as_ref()?;
        let default = token_prices.default.as_ref()?;
        let batch_size = f64::from(token_prices.batch_size);
        if batch_size == 0.0 {
            return None;
        }
        let usd_per_million = AIC_TO_USD_PER_MILLION / batch_size;
        let manifest_cache_write =
            lookup_entry(models(), &self.id).map_or(0.0, |entry| entry.pricing.cache_write);
        Some(ModelPricing::per_token(
            default.input_price * usd_per_million,
            default.output_price * usd_per_million,
            manifest_cache_write,
            default.cache_price * usd_per_million,
        ))
    }

    fn endpoint(&self) -> Endpoint {
        if self
            .supported_endpoints
            .iter()
            .any(|endpoint| endpoint == MESSAGES_PATH)
        {
            Endpoint::Messages
        } else if self
            .supported_endpoints
            .iter()
            .any(|endpoint| endpoint == RESPONSES_PATH)
        {
            Endpoint::Responses
        } else {
            Endpoint::ChatCompletions
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CopilotModelCategory {
    Lightweight,
    Versatile,
    Powerful,
    #[serde(other)]
    Unknown,
}

impl CopilotModelCategory {
    const fn tier(self) -> Option<ModelTier> {
        match self {
            Self::Lightweight => Some(ModelTier::Weak),
            Self::Versatile => Some(ModelTier::Medium),
            Self::Powerful => Some(ModelTier::Strong),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelPolicy {
    #[serde(default)]
    state: String,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelCapabilities {
    #[serde(default, rename = "type")]
    model_type: String,
    #[serde(default)]
    limits: CopilotModelLimits,
    #[serde(default)]
    supports: CopilotModelSupports,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelLimits {
    max_context_window_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelSupports {
    #[serde(default)]
    reasoning_effort: Vec<String>,
    #[serde(default)]
    adaptive_thinking: bool,
    max_thinking_budget: Option<u32>,
    min_thinking_budget: Option<u32>,
    #[serde(default)]
    vision: bool,
}

#[derive(Debug)]
struct CopilotModelInfo {
    reasoning_efforts: Vec<Effort>,
    reasoning_off: bool,
    adaptive_thinking: bool,
}

#[derive(Deserialize)]
struct CopilotModelsResponse {
    #[serde(default)]
    data: Vec<Value>,
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
}

#[derive(Deserialize)]
struct GraphQlData {
    viewer: GraphQlViewer,
}

#[derive(Deserialize)]
struct GraphQlViewer {
    #[serde(rename = "copilotEndpoints")]
    copilot_endpoints: GraphQlCopilotEndpoints,
}

#[derive(Deserialize)]
struct GraphQlCopilotEndpoints {
    api: String,
}

async fn discover_api_endpoint(client: &HttpClient, token: &str, graphql_url: &str) -> String {
    match try_discover_api_endpoint(client, token, graphql_url).await {
        Ok(endpoint) => endpoint,
        Err(err) => {
            warn!(error = %err, fallback = DEFAULT_API_ENDPOINT, "Copilot endpoint discovery failed");
            DEFAULT_API_ENDPOINT.to_owned()
        }
    }
}

async fn try_discover_api_endpoint(
    client: &HttpClient,
    token: &str,
    graphql_url: &str,
) -> Result<String, AgentError> {
    let body = json!({ "query": GRAPHQL_QUERY });
    let request = Request::builder()
        .method("POST")
        .uri(graphql_url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("user-agent", super::user_agent())
        .body(serde_json::to_vec(&body)?)?;

    let mut response = client.send_async(request).await?;
    if !response.status().is_success() {
        return Err(AgentError::from_response(response).await);
    }

    let parsed: GraphQlResponse = serde_json::from_str(&response.text().await?)?;
    parsed
        .data
        .map(|data| data.viewer.copilot_endpoints.api)
        .ok_or_else(|| AgentError::Config {
            message: "Copilot endpoint discovery response contained no data".into(),
        })
}

fn copilot_request(
    builder: isahc::http::request::Builder,
    auth: &CopilotAuth,
    interaction_type: Option<&str>,
) -> isahc::http::request::Builder {
    let builder = builder
        .header("authorization", format!("Bearer {}", auth.token))
        .header("content-type", "application/json")
        .header("editor-version", EDITOR_VERSION_HEADER)
        .header("x-github-api-version", API_VERSION_HEADER)
        .header("user-agent", super::user_agent());

    if let Some(interaction_type) = interaction_type {
        builder
            .header("x-initiator", "agent")
            .header("x-interaction-type", interaction_type)
            .header("openai-intent", interaction_type)
    } else {
        builder
    }
}

fn copilot_headers(auth: &CopilotAuth, interaction_type: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {}", auth.token)),
        ("content-type".into(), "application/json".into()),
        ("editor-version".into(), EDITOR_VERSION_HEADER.into()),
        ("x-github-api-version".into(), API_VERSION_HEADER.into()),
    ];
    if let Some(interaction_type) = interaction_type {
        headers.extend([
            ("x-initiator".into(), "agent".into()),
            ("x-interaction-type".into(), interaction_type.into()),
            ("openai-intent".into(), interaction_type.into()),
        ]);
    }
    headers
}

fn copilot_auth_from_resolved(auth: &super::ResolvedAuth) -> Result<CopilotAuth, AgentError> {
    let token = auth
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.strip_prefix("Bearer "))
        .map(str::to_owned)
        .ok_or_else(|| AgentError::Config {
            message: "dynamic Copilot provider missing Bearer authorization header".into(),
        })?;

    Ok(CopilotAuth {
        token,
        endpoint: auth
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_API_ENDPOINT.into()),
    })
}

fn anthropic_messages(messages: &[Message]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                json!({
                    "role": message.role,
                    "content": message.content,
                })
            })
            .collect(),
    )
}

fn effort_dialect(info: &CopilotModelInfo) -> EffortDialect<'_> {
    EffortDialect {
        supported: if info.reasoning_efforts.is_empty() {
            dialect::PREFER_HIGH.supported
        } else {
            &info.reasoning_efforts
        },
        adaptive: (!info.adaptive_thinking).then_some(Effort::High),
        off: info.reasoning_off.then_some(dialect::OFF),
    }
}

fn guess_endpoint(model_id: &str) -> Endpoint {
    if model_id.starts_with("claude-") {
        Endpoint::Messages
    } else if model_id.contains("gpt-5") || model_id.contains("codex") {
        Endpoint::Responses
    } else {
        Endpoint::ChatCompletions
    }
}

impl Provider for Copilot {
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
            let mut prefixed_system = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut prefixed_system);
            let endpoint = self.model_endpoint(&model.id).await?;
            debug!(model = %model.id, ?endpoint, "running Copilot request");
            match endpoint {
                Endpoint::ChatCompletions => {
                    self.stream_chat_completions(model, messages, system, tools, event_tx)
                        .await
                }
                Endpoint::Responses => {
                    self.stream_responses(model, messages, system, tools, event_tx, opts.thinking)
                        .await
                }
                Endpoint::Messages => {
                    self.stream_messages(model, messages, system, tools, event_tx, opts.thinking)
                        .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let models = self.fetch_models().await?;
            let infos = models
                .iter()
                .map(CopilotModel::model_info)
                .collect::<Vec<_>>();
            let mut guard = self.models.lock().unwrap();
            guard.clear();
            guard.extend(models.into_iter().map(|model| (model.id.clone(), model)));
            Ok(infos)
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            *self.auth.lock().unwrap() = None;
            self.models.lock().unwrap().clear();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    const OPUS_CACHE_WRITE: f64 = 6.25;

    use super::*;
    use crate::TokenUsage;
    use crate::manifest::ManifestRegistry;
    use test_case::test_case;

    #[test]
    fn endpoint_prefers_messages_then_responses_then_chat() {
        let mut model = CopilotModel {
            id: "claude-sonnet-4.5".into(),
            policy: None,
            capabilities: CopilotModelCapabilities {
                model_type: "chat".into(),
                ..Default::default()
            },
            billing: CopilotModelBilling::default(),
            is_chat_default: false,
            model_picker_enabled: true,
            model_picker_category: None,
            supported_endpoints: vec![CHAT_COMPLETIONS_PATH.into(), MESSAGES_PATH.into()],
        };
        assert_eq!(model.endpoint(), Endpoint::Messages);

        model.supported_endpoints = vec![RESPONSES_PATH.into()];
        assert_eq!(model.endpoint(), Endpoint::Responses);

        model.supported_endpoints.clear();
        assert_eq!(model.endpoint(), Endpoint::ChatCompletions);
    }

    #[test]
    fn parses_discovered_capabilities_and_category() {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "gpt-5.6-sol",
            "model_picker_enabled": true,
            "model_picker_category": "powerful",
            "supported_endpoints": ["/responses"],
            "capabilities": {
                "type": "chat",
                "limits": {
                    "max_context_window_tokens": 1_050_000,
                    "max_output_tokens": 128_000
                },
                "supports": {
                    "reasoning_effort": ["none", "low", "medium", "high"],
                    "adaptive_thinking": true,
                    "max_thinking_budget": 64_000,
                    "min_thinking_budget": 1_024,
                    "vision": true
                }
            }
        }))
        .unwrap();

        let info = model.model_info();
        assert_eq!(info.context_window, Some(1_050_000));
        assert_eq!(info.max_output_tokens, Some(128_000));
        assert_eq!(info.supports_thinking, Some(true));
        assert_eq!(info.supports_vision, Some(true));
        assert_eq!(info.tier, Some(ModelTier::Strong));
        let provider_info = info
            .provider_info
            .unwrap()
            .downcast::<CopilotModelInfo>()
            .unwrap();
        assert_eq!(
            provider_info.reasoning_efforts,
            vec![Effort::Low, Effort::Medium, Effort::High]
        );
        assert!(provider_info.reasoning_off);
        assert!(provider_info.adaptive_thinking);
    }

    #[test]
    fn unknown_category_keeps_model_without_tier() {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "gpt-6",
            "model_picker_enabled": true,
            "model_picker_category": "reasoning",
            "capabilities": { "type": "chat" }
        }))
        .unwrap();

        assert!(model.is_enabled_chat_model());
        assert_eq!(model.model_info().tier, None);
    }

    #[test_case(RESPONSES_PATH, true; "responses honors reasoning")]
    #[test_case(MESSAGES_PATH, true; "messages honors thinking")]
    #[test_case(CHAT_COMPLETIONS_PATH, false; "chat completions drops reasoning")]
    fn thinking_support_follows_endpoint(endpoint: &str, expected: bool) {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "reasoner",
            "supported_endpoints": [endpoint],
            "capabilities": {
                "type": "chat",
                "supports": {"reasoning_effort": ["low", "high"], "adaptive_thinking": true}
            }
        }))
        .unwrap();

        assert_eq!(model.model_info().supports_thinking, Some(expected));
    }

    #[test_case(ModelTier::Weak, "gpt-5.6-luna"; "weak defaults to luna")]
    #[test_case(ModelTier::Medium, "gpt-5.6-terra"; "medium defaults to terra")]
    #[test_case(ModelTier::Strong, "claude-opus-5"; "strong defaults to opus")]
    fn manifest_has_exactly_one_default_per_tier(tier: ModelTier, expected_prefix: &str) {
        let defaults: Vec<_> = models()
            .iter()
            .filter(|entry| entry.default && entry.tier == tier)
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].prefixes[0], expected_prefix);
        assert_eq!(
            ManifestRegistry::find_default_for_tier("copilot", tier)
                .unwrap()
                .prefixes[0],
            expected_prefix
        );
    }

    #[test_case("copilot/gpt-5.6-luna", 1_000_000, 1_000_000, 0.20 + 1.20; "luna default rates")]
    #[test_case("copilot/gpt-5.4-mini", 100_000, 100_000, 0.075 + 0.45; "gpt-5.4-mini beats gpt-5.4 prefix")]
    #[test_case("copilot/claude-opus-4.8-fast", 100_000, 100_000, 1.00 + 5.00; "opus 4.8 fast beats opus prefix")]
    fn manifest_models_report_cost(spec: &str, input: u32, output: u32, expected: f64) {
        let usage = TokenUsage {
            input,
            output,
            cache_creation: 0,
            cache_read: 0,
            cost: None,
        };
        let cost = Model::from_spec(spec)
            .unwrap()
            .list_cost(&usage, false)
            .unwrap();
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn token_prices_convert_aic_per_batch_to_usd_per_million() {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "gpt-5",
            "billing": {
                "token_prices": {
                    "batch_size": 500_000,
                    "default": {"input_price": 500.0, "output_price": 3000.0, "cache_price": 50.0}
                }
            }
        }))
        .unwrap();

        let pricing = model.pricing().unwrap();
        assert_eq!(pricing.input, 10.0);
        assert_eq!(pricing.output, 60.0);
        assert_eq!(pricing.cache_read, 1.0);
        assert!(pricing.fast.is_none());
    }

    #[test]
    fn pricing_inherits_cache_write_from_manifest_by_prefix() {
        let billing = |id: &str| {
            json!({
                "id": id,
                "billing": {
                    "token_prices": {
                        "batch_size": 500_000,
                        "default": {"input_price": 250.0, "output_price": 1250.0, "cache_price": 25.0}
                    }
                }
            })
        };

        let opus: CopilotModel = serde_json::from_value(billing("claude-opus-5")).unwrap();
        let pricing = opus.pricing().unwrap();
        assert_eq!(pricing.input, 5.0);
        assert_eq!(pricing.output, 25.0);
        assert_eq!(pricing.cache_read, 0.5);
        assert_eq!(pricing.cache_write, OPUS_CACHE_WRITE);

        let unmatched: CopilotModel = serde_json::from_value(billing("gpt-5")).unwrap();
        assert_eq!(unmatched.pricing().unwrap().cache_write, 0.0);
    }

    #[test_case(json!({"id": "gpt-5"}) ; "no billing")]
    #[test_case(json!({"id": "gpt-5", "billing": {"token_prices": {"batch_size": 0, "default": {"input_price": 1.0, "output_price": 1.0, "cache_price": 0.1}}}}) ; "zero batch size")]
    fn pricing_falls_back_when_billing_unusable(billing: Value) {
        let model: CopilotModel = serde_json::from_value(billing).unwrap();
        assert!(model.pricing().is_none());
        assert!(model.model_info().pricing.is_none());
    }

    #[test]
    fn responses_reasoning_uses_effort_object_and_explicit_none() {
        let model = Model::from_spec("copilot/gpt-5.4").unwrap();
        let info = CopilotModelInfo {
            reasoning_efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            reasoning_off: true,
            adaptive_thinking: false,
        };
        let dialect = effort_dialect(&info);

        let mut body = json!({});
        responses::apply_responses_reasoning(&mut body, ThinkingConfig::Off, &model, &dialect);
        assert_eq!(body, json!({"reasoning": {"effort": "none"}}));

        let mut body = json!({});
        responses::apply_responses_reasoning(
            &mut body,
            ThinkingConfig::Effort(Effort::Medium),
            &model,
            &dialect,
        );
        assert_eq!(body, json!({"reasoning": {"effort": "medium"}}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn filters_enabled_chat_models() {
        let enabled = CopilotModel {
            id: "gpt-5.4".into(),
            policy: Some(CopilotModelPolicy {
                state: "enabled".into(),
            }),
            capabilities: CopilotModelCapabilities {
                model_type: "chat".into(),
                ..Default::default()
            },
            billing: CopilotModelBilling::default(),
            is_chat_default: false,
            model_picker_enabled: true,
            model_picker_category: None,
            supported_endpoints: vec![],
        };
        assert!(enabled.is_enabled_chat_model());

        let disabled = CopilotModel {
            policy: Some(CopilotModelPolicy {
                state: "pending".into(),
            }),
            ..enabled
        };
        assert!(!disabled.is_enabled_chat_model());
    }
}
