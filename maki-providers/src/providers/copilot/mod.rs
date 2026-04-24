use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use futures_lite::io::BufReader;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::openai::responses;
use super::openai_compat;
use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

pub mod auth;

const DEFAULT_API_ENDPOINT: &str = "https://api.githubcopilot.com";
const GRAPHQL_QUERY: &str = "query { viewer { copilotEndpoints { api } } }";
const API_VERSION_HEADER: &str = "2025-10-01";
const EDITOR_VERSION_HEADER: &str = concat!("Maki/", env!("CARGO_PKG_VERSION"));
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const MESSAGES_PATH: &str = "/v1/messages";
const MODELS_PATH: &str = "/models";

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["gpt-5-mini", "gpt-5 mini", "claude-haiku-4.5"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.2", "gpt-4.1", "claude-sonnet-4.5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &[
                "gpt-5.4",
                "gpt-5.3-codex",
                "claude-opus-4.6",
                "grok-code-fast-1",
            ],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
    ]
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

        let token = auth::load_token()?;
        let endpoint = discover_api_endpoint(&self.client, &token).await;
        let auth = CopilotAuth { token, endpoint };
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
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let body = responses::build_body(model, messages, system, tools);
        let resolved = super::ResolvedAuth {
            base_url: Some(auth.endpoint.clone()),
            headers: copilot_headers(&auth, Some("conversation-agent")),
        };
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
            "max_tokens": model.max_output_tokens,
            "system": [{"type": "text", "text": system}],
            "messages": anthropic_messages(messages),
            "tools": tools,
            "stream": true,
        });
        thinking.apply_to_body(&mut body);

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
    is_chat_default: bool,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    supported_endpoints: Vec<String>,
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

#[derive(Clone, Default, Deserialize)]
struct CopilotModelPolicy {
    #[serde(default)]
    state: String,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelCapabilities {
    #[serde(default, rename = "type")]
    model_type: String,
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

async fn discover_api_endpoint(client: &HttpClient, token: &str) -> String {
    match try_discover_api_endpoint(client, token).await {
        Ok(endpoint) => endpoint,
        Err(err) => {
            warn!(error = %err, fallback = DEFAULT_API_ENDPOINT, "Copilot endpoint discovery failed");
            DEFAULT_API_ENDPOINT.to_owned()
        }
    }
}

async fn try_discover_api_endpoint(client: &HttpClient, token: &str) -> Result<String, AgentError> {
    let body = json!({ "query": GRAPHQL_QUERY });
    let request = Request::builder()
        .method("POST")
        .uri("https://api.github.com/graphql")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
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
        .header("x-github-api-version", API_VERSION_HEADER);

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
        thinking: ThinkingConfig,
        _session_id: Option<&'a str>,
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
                    self.stream_responses(model, messages, system, tools, event_tx)
                        .await
                }
                Endpoint::Messages => {
                    self.stream_messages(model, messages, system, tools, event_tx, thinking)
                        .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async move {
            let models = self.fetch_models().await?;
            let ids = models
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>();
            let mut guard = self.models.lock().unwrap();
            guard.clear();
            guard.extend(models.into_iter().map(|model| (model.id.clone(), model)));
            Ok(ids)
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
    use super::*;

    #[test]
    fn endpoint_prefers_messages_then_responses_then_chat() {
        let mut model = CopilotModel {
            id: "claude-sonnet-4.5".into(),
            policy: None,
            capabilities: CopilotModelCapabilities {
                model_type: "chat".into(),
            },
            is_chat_default: false,
            model_picker_enabled: true,
            supported_endpoints: vec![CHAT_COMPLETIONS_PATH.into(), MESSAGES_PATH.into()],
        };
        assert_eq!(model.endpoint(), Endpoint::Messages);

        model.supported_endpoints = vec![RESPONSES_PATH.into()];
        assert_eq!(model.endpoint(), Endpoint::Responses);

        model.supported_endpoints.clear();
        assert_eq!(model.endpoint(), Endpoint::ChatCompletions);
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
            },
            is_chat_default: false,
            model_picker_enabled: true,
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
