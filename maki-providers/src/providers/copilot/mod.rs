use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use maki_storage::id::SessionRef;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::anthropic::shared;
use super::openai::responses;
use super::openai_compat;
use crate::model::{Model, ModelFamily, ModelInfo, ModelPricing, ModelTier, lookup_entry};
use crate::model_registry;
use crate::provider::{BoxFuture, Provider};
use crate::providers::{ResolvedAuth, Timeouts};
use crate::spec::{AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec};
use crate::{
    AgentError, Effort, EffortDialect, Message, ProviderEvent, RequestOptions, StreamResponse,
    ThinkingConfig, dialect,
};
use maki_config::providers::Protocol;

pub mod auth;

const SLUG: &str = "copilot";
const DISPLAY_NAME: &str = "Copilot";
const ENV_VAR: &str = "GH_COPILOT_TOKEN";
const DEFAULT_API_ENDPOINT: &str = "https://api.githubcopilot.com";
const DEFAULT_MODEL: &str = "copilot/gpt-5.6-terra";
const LOGIN_URL: &str = "https://github.com/settings/copilot";
/// Prose, not a value: the real endpoint is discovered over GraphQL.
const DOC_API_URL: &str =
    "https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)";
const FEATURES: &str = "Native Copilot Chat HTTP API with model endpoint discovery";
const AUTH_NOTE: &str = "(or run `maki auth login copilot` to import a token from gh CLI, the Copilot client, or the system keyring)";

/// No Aperture route: Copilot's auth is GraphQL-discovered, not a clean proxy
/// target.
pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: false,
    supports_deferred_tools: false,
    sends_thinking,
    thinking_fields_fallback: |_| None,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(100_000),
    fallback_context_window: 200_000,
    models_toml: include_str!("../../../models/copilot.toml"),
    pricing_schedule: None,
    build: Build::Native(Native {
        new: create,
        with_auth: create_with_auth,
    }),
    aperture: None,
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: DEFAULT_API_ENDPOINT,
        default_model: DEFAULT_MODEL,
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[DOC_API_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVarWith(AUTH_NOTE),
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Copilot::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Copilot::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());
const GRAPHQL_QUERY: &str = "query { viewer { copilotEndpoints { api } } }";
/// Also pins the `/models` shape: `2025-10-01` leaves out `billing.token_prices`,
/// and by `2026-07-01` its `cache_price` is called `cache_read_price`.
const API_VERSION_HEADER: &str = "2026-08-01";
const EDITOR_VERSION_HEADER: &str = concat!("Maki/", env!("CARGO_PKG_VERSION"));
/// Without it the gateway falls back to the `copilot-language-server`
/// integrator, which intermittently rejects newer models such as gpt-6-luna.
const INTEGRATION_ID_HEADER: &str = "vscode-chat";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const MESSAGES_PATH: &str = "/v1/messages";
const MODELS_PATH: &str = "/models";

/// Scales `/models` AI-credit prices (1 credit = $0.01) to USD per 1M tokens.
const AIC_TO_USD_PER_MILLION: f64 = 10_000.0;
/// `copilot_usage.total_nano_aiu` is billed in nano AI credits.
const NANO_AIU_PER_USD: f64 = 100_000_000_000.0;

pub struct Copilot {
    client: Client,
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
        self.remember(models);
        Ok(self
            .models
            .lock()
            .unwrap()
            .get(model_id)
            .map(CopilotModel::endpoint)
            .unwrap_or_else(|| guess_endpoint(model_id)))
    }

    /// Merges rather than replaces: `/models` sometimes omits a model it
    /// served a moment ago, and forgetting it would reroute that model to
    /// a guessed endpoint mid-session.
    fn remember(&self, models: Vec<CopilotModel>) {
        self.models
            .lock()
            .unwrap()
            .extend(models.into_iter().map(|model| (model.id.clone(), model)));
    }

    async fn fetch_models(&self) -> Result<Vec<CopilotModel>, AgentError> {
        let auth = self.auth().await?;
        let request = copilot_request(
            self.client.get(format!("{}{MODELS_PATH}", auth.endpoint)),
            &auth,
            None,
        )
        .build()?;
        let body_text = super::send_request(&self.client, request, |response| async move {
            if !response.status().is_success() {
                return Err(AgentError::from_response(response).await);
            }
            Ok(response.text().await?)
        })
        .await?;

        let body: CopilotModelsResponse = serde_json::from_str(&body_text)?;
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
            .body(serde_json::to_vec(&body)?)
            .build()?;
        super::send_request(&self.client, request, |response| async move {
            if response.status().is_success() {
                openai_compat::parse_sse(
                    super::body_reader(response),
                    event_tx,
                    self.stream_timeout,
                    false,
                )
                .await
            } else {
                Err(AgentError::from_response(response).await)
            }
        })
        .await
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
        let reasoning_info = model_registry::provider_info::<CopilotModelInfo>(SLUG, &model.id)
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
        let body = messages_body(model, messages, system, tools, thinking);
        let request = self
            .build_post(&auth, MESSAGES_PATH, Some("conversation-agent"), &body)?
            .header("anthropic-version", "2023-06-01")
            .body(serde_json::to_vec(&body)?)
            .build()?;
        super::send_request(&self.client, request, |response| async move {
            if response.status().is_success() {
                super::anthropic::parse_sse(
                    super::body_reader(response),
                    event_tx,
                    self.stream_timeout,
                )
                .await
            } else {
                Err(AgentError::from_response(response).await)
            }
        })
        .await
    }

    fn build_post(
        &self,
        auth: &CopilotAuth,
        path: &str,
        interaction_type: Option<&str>,
        body: &Value,
    ) -> Result<RequestBuilder, AgentError> {
        debug!(
            path,
            body_bytes = serde_json::to_vec(body)?.len(),
            "sending Copilot API request"
        );
        Ok(copilot_request(
            self.client.post(format!("{}{path}", auth.endpoint)),
            auth,
            interaction_type,
        ))
    }
}

/// Prices a turn from the `copilot_usage` bill Copilot attaches to every stream,
/// next to the usage counts: on the final chat completions chunk, on
/// `message_delta`, and on `response.completed`, always at the top level of the
/// event rather than inside `usage`. It already reflects the context tier the
/// request landed in, which raises every rate past a per-model prompt size, so
/// it beats any table we could keep. Read loosely so an unexpected shape only
/// leaves the turn unpriced instead of failing the whole event.
pub(crate) fn copilot_usd(copilot_usage: &Value) -> Option<f64> {
    copilot_usage["total_nano_aiu"]
        .as_f64()
        .map(|nano_aiu| nano_aiu / NANO_AIU_PER_USD)
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
    cache_read_price: f64,
    cache_write_price: Option<f64>,
    max_prompt_tokens: Option<u32>,
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
            context_window: self.context_window(),
            max_output_tokens: self.capabilities.limits.max_output_tokens,
            pricing: self.pricing(),
            supports_thinking: Some(self.supports_thinking()),
            supports_vision: Some(self.capabilities.supports.vision),
            tier: self
                .model_picker_category
                .and_then(CopilotModelCategory::tier),
            provider_info: Some(Arc::new(reasoning)),
            extra: None,
            effort: None,
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
            endpoint: self.endpoint(),
        }
    }

    /// `max_context_window_tokens` spans the long-context tier where a model
    /// has one; the default tier's window is its prompt cap plus the output.
    fn context_window(&self) -> Option<u32> {
        let limits = &self.capabilities.limits;
        self.billing
            .token_prices
            .as_ref()
            .and_then(|prices| prices.default.as_ref())
            .and_then(|default| default.max_prompt_tokens)
            .zip(limits.max_output_tokens)
            .map(|(prompt, output)| prompt.saturating_add(output))
            .or(limits.max_context_window_tokens)
    }

    /// `/models` reports prices in AI credits per billing batch (1 credit =
    /// $0.01), scaled to USD per 1M tokens for [`ModelPricing`]. Only the
    /// default context tier is read. A model without a cache write price
    /// inherits it from the static spec by id prefix, so cost accounting
    /// matches the offline path.
    fn pricing(&self) -> Option<ModelPricing> {
        let token_prices = self.billing.token_prices.as_ref()?;
        let default = token_prices.default.as_ref()?;
        let batch_size = f64::from(token_prices.batch_size);
        if batch_size == 0.0 {
            return None;
        }
        let usd_per_million = AIC_TO_USD_PER_MILLION / batch_size;
        let cache_write = default.cache_write_price.map_or_else(
            || {
                lookup_entry(SPEC.models(), &self.id)
                    .and_then(|entry| entry.pricing.as_ref())
                    .map_or(0.0, |pricing| pricing.cache_write)
            },
            |price| price * usd_per_million,
        );
        Some(ModelPricing::per_million(
            default.input_price * usd_per_million,
            default.output_price * usd_per_million,
            cache_write,
            default.cache_read_price * usd_per_million,
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
    endpoint: Endpoint,
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

async fn discover_api_endpoint(client: &Client, token: &str, graphql_url: &str) -> String {
    match try_discover_api_endpoint(client, token, graphql_url).await {
        Ok(endpoint) => endpoint,
        Err(err) => {
            warn!(error = %err, fallback = DEFAULT_API_ENDPOINT, "Copilot endpoint discovery failed");
            DEFAULT_API_ENDPOINT.to_owned()
        }
    }
}

async fn try_discover_api_endpoint(
    client: &Client,
    token: &str,
    graphql_url: &str,
) -> Result<String, AgentError> {
    let body = json!({ "query": GRAPHQL_QUERY });
    let request = client
        .post(graphql_url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("user-agent", super::user_agent())
        .body(serde_json::to_vec(&body)?)
        .build()?;
    let body_text = super::send_request(client, request, |response| async move {
        if !response.status().is_success() {
            return Err(AgentError::from_response(response).await);
        }
        Ok(response.text().await?)
    })
    .await?;

    let parsed: GraphQlResponse = serde_json::from_str(&body_text)?;
    parsed
        .data
        .map(|data| data.viewer.copilot_endpoints.api)
        .ok_or_else(|| AgentError::Config {
            message: "Copilot endpoint discovery response contained no data".into(),
        })
}

fn copilot_request(
    builder: RequestBuilder,
    auth: &CopilotAuth,
    interaction_type: Option<&str>,
) -> RequestBuilder {
    let builder = builder
        .header("authorization", format!("Bearer {}", auth.token))
        .header("content-type", "application/json")
        .header("editor-version", EDITOR_VERSION_HEADER)
        .header("copilot-integration-id", INTEGRATION_ID_HEADER)
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
        (
            "copilot-integration-id".into(),
            INTEGRATION_ID_HEADER.into(),
        ),
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

fn messages_body(
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    thinking: ThinkingConfig,
) -> Value {
    let system = [shared::SystemBlock {
        r#type: "text",
        text: system,
        cache_control: Some(shared::EPHEMERAL),
    }];
    let mut body =
        shared::build_request_body_with_system(model, messages, &system, tools, thinking, None);
    body["model"] = json!(model.id);
    body["stream"] = json!(true);
    body
}

fn effort_dialect(info: &CopilotModelInfo) -> EffortDialect<'_> {
    EffortDialect {
        name: None,
        supported: if info.reasoning_efforts.is_empty() {
            dialect::PREFER_HIGH.supported
        } else {
            &info.reasoning_efforts
        },
        adaptive: (!info.adaptive_thinking).then_some(Effort::High),
        off: info.reasoning_off.then_some(dialect::OFF),
    }
}

fn sends_thinking(model: &Model, choice: ThinkingConfig) -> ThinkingConfig {
    model_registry::provider_info::<CopilotModelInfo>(SLUG, &model.id)
        .map_or(choice, |info| listed_sends_thinking(&info, choice))
}

/// Only the responses endpoint renders thinking through the dialect. The
/// messages endpoint spells it natively and chat completions carries no
/// reasoning at all.
fn listed_sends_thinking(info: &CopilotModelInfo, choice: ThinkingConfig) -> ThinkingConfig {
    if info.endpoint == Endpoint::Responses {
        effort_dialect(info).sends(choice)
    } else {
        choice
    }
}

fn guess_endpoint(model_id: &str) -> Endpoint {
    if model_id.starts_with("claude-") {
        Endpoint::Messages
    } else if model_id.contains("codex") || gpt_major(model_id).is_some_and(|major| major >= 5) {
        Endpoint::Responses
    } else {
        Endpoint::ChatCompletions
    }
}

fn gpt_major(model_id: &str) -> Option<u32> {
    model_id
        .strip_prefix("gpt-")?
        .split(['.', '-'])
        .next()?
        .parse()
        .ok()
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
            self.remember(models);
            Ok(infos)
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            *self.auth.lock().unwrap() = None;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProviderRegistry;
    use crate::{ContentBlock, Role, TokenUsage};
    use test_case::test_case;

    const OPUS_CACHE_WRITE: f64 = 6.25;
    const CLAUDE_SPEC: &str = "copilot/claude-opus-5";
    const REPLY: &str = "reply";
    const SOL: &str = "gpt-6.1-sol";

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

    #[test_case("gpt-6-luna", Endpoint::Responses ; "gpt_6")]
    #[test_case("gpt-5.6-terra", Endpoint::Responses ; "gpt_5")]
    #[test_case("gpt-10-mini", Endpoint::Responses ; "two_digit_major")]
    #[test_case("gpt-5.3-codex", Endpoint::Responses ; "codex")]
    #[test_case("gpt-4.1", Endpoint::ChatCompletions ; "gpt_4")]
    #[test_case("gpt-4o", Endpoint::ChatCompletions ; "gpt_4o")]
    #[test_case("claude-opus-5", Endpoint::Messages ; "claude")]
    #[test_case("gemini-3.7-flash", Endpoint::ChatCompletions ; "other")]
    fn guess_endpoint_for_unlisted_model(model_id: &str, expected: Endpoint) {
        assert_eq!(guess_endpoint(model_id), expected);
    }

    #[test]
    fn requests_identify_integrator() {
        let auth = CopilotAuth {
            token: "token".into(),
            endpoint: DEFAULT_API_ENDPOINT.into(),
        };
        let request = copilot_request(Client::new().get(DEFAULT_API_ENDPOINT), &auth, None)
            .build()
            .unwrap();
        assert_eq!(
            request.headers()["copilot-integration-id"],
            INTEGRATION_ID_HEADER
        );
        assert!(copilot_headers(&auth, None).contains(&(
            "copilot-integration-id".into(),
            INTEGRATION_ID_HEADER.into()
        )));
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

    /// On the responses endpoint `adaptive` is the dialect's level, snapped to
    /// what the model lists. The Claude models report `adaptive_thinking` and
    /// take the messages endpoint, which spells the mode natively.
    #[test_case(RESPONSES_PATH, json!({"reasoning_effort": ["low", "high", "max"]}), Some(Effort::High); "responses sends a fixed level")]
    #[test_case(RESPONSES_PATH, json!({"reasoning_effort": ["low", "medium"]}), Some(Effort::Medium); "the level snaps to what the model lists")]
    #[test_case(RESPONSES_PATH, json!({"reasoning_effort": ["low", "high"], "adaptive_thinking": true}), None; "reported adaptive stays")]
    #[test_case(MESSAGES_PATH, json!({"max_thinking_budget": 32_000}), None; "messages spells it natively")]
    fn adaptive_effort_follows_the_endpoint_and_the_listing(
        endpoint: &str,
        supports: Value,
        expected: Option<Effort>,
    ) {
        let listed: CopilotModel = serde_json::from_value(json!({
            "id": SOL,
            "supported_endpoints": [endpoint],
            "capabilities": {"type": "chat", "supports": supports}
        }))
        .unwrap();
        assert_eq!(
            listed_sends_thinking(&listed.reasoning_info(), ThinkingConfig::Adaptive).level(),
            expected
        );
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
        let defaults: Vec<_> = SPEC
            .models()
            .iter()
            .filter(|entry| entry.default && entry.tier == tier)
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].prefixes[0], expected_prefix);
        assert_eq!(
            ProviderRegistry::find_default_for_tier("copilot", tier)
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
            "id": "claude-opus-5",
            "billing": {
                "token_prices": {
                    "batch_size": 500_000,
                    "default": {"input_price": 500.0, "output_price": 3000.0, "cache_read_price": 50.0, "cache_write_price": 62.5}
                }
            }
        }))
        .unwrap();

        let pricing = model.pricing().unwrap();
        assert_eq!(pricing.input, 10.0);
        assert_eq!(pricing.output, 60.0);
        assert_eq!(pricing.cache_read, 1.0);
        assert_eq!(pricing.cache_write, 1.25);
        assert!(pricing.fast.is_none());
    }

    #[test_case(json!({"max_context_window_tokens": 1_000_000, "max_output_tokens": 128_000}), json!({"batch_size": 1_000_000, "default": {"max_prompt_tokens": 200_000}}) => Some(328_000) ; "default_tier")]
    #[test_case(json!({"max_context_window_tokens": 200_000, "max_output_tokens": 64_000}), Value::Null => Some(200_000) ; "no_tiers")]
    fn context_window_is_the_default_tier(limits: Value, token_prices: Value) -> Option<u32> {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "claude-opus-5.5",
            "capabilities": {"type": "chat", "limits": limits},
            "billing": {"token_prices": token_prices}
        }))
        .unwrap();
        model.model_info().context_window
    }

    #[test]
    fn pricing_inherits_cache_write_from_manifest_by_prefix() {
        let billing = |id: &str| {
            json!({
                "id": id,
                "billing": {
                    "token_prices": {
                        "batch_size": 500_000,
                        "default": {"input_price": 250.0, "output_price": 1250.0, "cache_read_price": 25.0}
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

    #[test_case(json!(7_576_685_000_u64), 0.07576685 ; "long_context_luna_turn")]
    #[test_case(json!(0), 0.0 ; "free_turn")]
    #[test_case(json!(375_000.5), 0.000003750005 ; "float_total")]
    fn copilot_usd_converts_nano_credits_to_usd(nano_aiu: Value, expected_usd: f64) {
        let usd = copilot_usd(&json!({"total_nano_aiu": nano_aiu})).unwrap();
        assert!((usd - expected_usd).abs() < 1e-12);
    }

    #[test_case(Value::Null ; "no_billing_block")]
    #[test_case(json!({"token_details": []}) ; "no_total")]
    #[test_case(json!({"total_nano_aiu": "375000"}) ; "string_total")]
    #[test_case(json!("billed") ; "not_an_object")]
    fn copilot_usd_unusable_leaves_the_turn_unpriced(copilot_usage: Value) {
        assert!(copilot_usd(&copilot_usage).is_none());
    }

    #[test_case(json!({"id": "gpt-5"}) ; "no billing")]
    #[test_case(json!({"id": "gpt-5", "billing": {"token_prices": {"batch_size": 0, "default": {"input_price": 1.0, "output_price": 1.0, "cache_read_price": 0.1}}}}) ; "zero batch size")]
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
            endpoint: Endpoint::Responses,
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
        assert_eq!(
            body,
            json!({"reasoning": {"effort": "medium", "summary": "auto"}})
        );
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn messages_body_drops_unsigned_thinking_without_cache_control() {
        let model = Model::from_spec(CLAUDE_SPEC).unwrap();
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "gpt reasoning summary".into(),
                    signature: None,
                },
                ContentBlock::Text { text: REPLY.into() },
            ],
            ..Default::default()
        }];

        let body = messages_body(&model, &messages, "system", &json!([]), ThinkingConfig::Off);

        assert_eq!(
            body["messages"],
            json!([{"role": "assistant", "content": [{"type": "text", "text": REPLY, "cache_control": {"type": "ephemeral"}}]}])
        );
    }

    #[test]
    fn messages_body_marks_system_tools_and_recent_messages_for_cache() {
        let model = Model::from_spec(CLAUDE_SPEC).unwrap();
        let messages = vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message::user("latest".into()),
        ];
        let tools =
            json!([{"name": "tool", "description": "test", "input_schema": {"type": "object"}}]);

        let body = messages_body(&model, &messages, "system", &tools, ThinkingConfig::Off);

        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
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
