use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason,
    StreamResponse, ThinkingConfig, TokenUsage,
};

use super::{KeyPool, ResolvedAuth, http_client, next_sse_line};

const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const ENV_VAR: &str = "GEMINI_API_KEY";
const FLASH_MAX_THINKING: u32 = 24_576;
const PRO_MAX_THINKING: u32 = 32_768;

/// The generic per-model max, capped by Google's documented `thinkingBudget`
/// hard limits per family.
fn max_thinking(model: &Model) -> u32 {
    let cap = if model.id.contains("flash") {
        FLASH_MAX_THINKING
    } else {
        PRO_MAX_THINKING
    };
    model.max_thinking_budget().map_or(cap, |m| m.min(cap))
}

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "google",
    display_name: "Google",
    protocol: maki_config::providers::Protocol::Google,
    default_base_url: BASE_URL,
    default_api_key_env: ENV_VAR,
    default_model: "google/gemini-2.5-pro",
    plans: None,
    login_url: Some("https://aistudio.google.com/apikey"),
    needs_url: false,
});

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["gemini-2.5-pro"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gemini,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.25,
                output: 5.00,
                cache_write: 0.00,
                cache_read: 0.31,
                fast: None,
            },
            max_output_tokens: 65_536,
            context_window: 1_048_576,
        },
        ModelEntry {
            prefixes: &["gemini-2.5-flash"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gemini,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.04,
                fast: None,
            },
            max_output_tokens: 65_536,
            context_window: 1_048_576,
        },
        ModelEntry {
            prefixes: &["gemini-2.0-flash-lite"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gemini,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 0.075,
                output: 0.30,
                cache_write: 0.00,
                cache_read: 0.01,
                fast: None,
            },
            max_output_tokens: 65_536,
            context_window: 1_048_576,
        },
    ]
}

fn resolve_auth_from_key(key: &str) -> ResolvedAuth {
    ResolvedAuth {
        base_url: None,
        headers: vec![("x-goog-api-key".into(), key.to_string())],
    }
}

pub struct Google {
    client: HttpClient,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    stream_timeout: Duration,
}

impl Google {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("google", ENV_VAR)?;
        let resolved = resolve_auth_from_key(pool.current());
        Ok(Self {
            client: http_client(timeouts),
            auth: Arc::new(Mutex::new(resolved)),
            key_pool: Some(pool),
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: http_client(timeouts),
            auth,
            key_pool: None,
            stream_timeout: timeouts.stream,
        }
    }

    fn build_request(&self, method: &str, url: &str) -> isahc::http::request::Builder {
        let auth = self.auth.lock().unwrap();
        auth.configure_request(
            Request::builder()
                .method(method)
                .uri(url)
                .header("user-agent", super::user_agent()),
        )
    }

    fn api_key(&self) -> String {
        let auth = self.auth.lock().unwrap();
        auth.headers
            .iter()
            .find(|(k, _)| k == "x-goog-api-key")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn stream_url(&self, model_id: &str) -> String {
        let base = {
            let auth = self.auth.lock().unwrap();
            auth.base_url.as_deref().unwrap_or(BASE_URL).to_string()
        };
        let encoded = super::urlenc(model_id);
        format!("{base}/models/{encoded}:streamGenerateContent?alt=sse")
    }

    fn models_url(&self) -> String {
        let base = {
            let auth = self.auth.lock().unwrap();
            auth.base_url.as_deref().unwrap_or(BASE_URL).to_string()
        };
        let key = self.api_key();
        format!("{base}/models?key={key}&pageSize=1000")
    }

    fn build_body(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        thinking: ThinkingConfig,
    ) -> Value {
        let mut body = json!({
            "contents": convert_messages(messages),
        });

        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }

        thinking.apply_google_thinking(&mut body, max_thinking(model));

        if let Some(max_output) = model.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_output);
        }

        let tool_decls = convert_tools(tools);
        if !tool_defs_empty(&tool_decls) {
            body["tools"] = json!([{"functionDeclarations": tool_decls}]);
        }

        body
    }

    async fn do_stream(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let body = self.build_body(model, messages, system, tools, thinking);
        let url = self.stream_url(&model.id);
        let json_body = serde_json::to_vec(&body)?;

        let request = self
            .build_request("POST", &url)
            .header("content-type", "application/json")
            .body(json_body)?;

        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }
}

impl Provider for Google {
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
        Box::pin(self.do_stream(model, messages, system, tools, event_tx, opts.thinking))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        let url = self.models_url();
        let request = self.build_request("GET", &url).body(()).unwrap();
        let client = self.client.clone();
        Box::pin(async move {
            let mut response = client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }
            let body_text = response.text().await?;
            let models_response: ModelsListResponse = serde_json::from_str(&body_text)?;
            let mut infos: Vec<crate::model::ModelInfo> = models_response
                .models
                .into_iter()
                .filter(|m| {
                    m.supported_generation_methods
                        .iter()
                        .any(|m| m == "generateContent")
                })
                .map(|m| {
                    let id = m
                        .name
                        .strip_prefix("models/")
                        .map(String::from)
                        .unwrap_or(m.name);
                    crate::model::ModelInfo::id_only(id)
                })
                .collect();
            infos.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(infos)
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = KeyPool::resolve("google", ENV_VAR)?;
            *self.auth.lock().unwrap() = resolve_auth_from_key(pool.current());
            Ok(())
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, resolve_auth_from_key)))
        })
    }
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let tool_names: std::collections::HashMap<&str, &str> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();

    let mut out: Vec<Value> = Vec::new();

    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
        };

        let mut parts: Vec<Value> = Vec::new();
        // Gemini is strict about function-response turns: tool-returned
        // images get their own user turn instead of mixing inlineData into
        // the functionResponse content.
        let has_tool_results = msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        let mut image_parts: Vec<Value> = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    parts.push(json!({"text": text}));
                }
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    let mut part = json!({"text": thinking, "thought": true});
                    if let Some(sig) = signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                ContentBlock::RedactedThinking { .. } => {}
                ContentBlock::ToolUse { id: _, name, input } => {
                    parts.push(json!({
                        "functionCall": {
                            "name": name,
                            "args": input,
                        }
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let mut response_val = serde_json::from_str(content)
                        .unwrap_or_else(|_| json!({"result": content}));
                    if *is_error {
                        response_val = json!({"error": response_val});
                    }
                    let name = tool_names
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("unknown");
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": response_val,
                        }
                    }));
                }
                ContentBlock::Image { source } => {
                    let part = json!({
                        "inlineData": {
                            "mimeType": source.media_type.mime(),
                            "data": source.data,
                        }
                    });
                    if has_tool_results {
                        image_parts.push(part);
                    } else {
                        parts.push(part);
                    }
                }
            }
        }

        if !parts.is_empty() {
            out.push(json!({"role": role, "parts": parts}));
        }
        if !image_parts.is_empty() {
            out.push(json!({"role": "user", "parts": image_parts}));
        }
    }

    out
}

fn convert_tools(tools: &Value) -> Vec<Value> {
    let Some(arr) = tools.as_array() else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let description = t.get("description")?.as_str().unwrap_or("");
            let parameters = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "name": name,
                "description": description,
                "parameters": strip_additional_properties(parameters),
            }))
        })
        .collect()
}

fn strip_additional_properties(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.remove("additionalProperties");
            map.values_mut()
                .for_each(|v| *v = strip_additional_properties(std::mem::take(v)));
            Value::Object(map)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(strip_additional_properties).collect())
        }
        other => other,
    }
}

fn tool_defs_empty(tool_decls: &[Value]) -> bool {
    tool_decls.is_empty()
}

// --- SSE response types ---

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseCandidate {
    content: Option<SseContent>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseContent {
    parts: Option<Vec<SsePart>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsePart {
    text: Option<String>,
    thought: Option<bool>,
    thought_signature: Option<String>,
    function_call: Option<SseFunctionCall>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseFunctionCall {
    name: String,
    args: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseUsageMetadata {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
    #[serde(default)]
    cached_content_token_count: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseResponse {
    candidates: Option<Vec<SseCandidate>>,
    usage_metadata: Option<SseUsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsListResponse {
    models: Vec<ApiModelInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiModelInfo {
    name: String,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.strip_prefix(' ').unwrap_or(d),
            _ => continue,
        };

        let chunk: SseResponse = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to parse Gemini SSE chunk");
                continue;
            }
        };

        if let Some(meta) = chunk.usage_metadata {
            usage.input = meta.prompt_token_count;
            usage.output = meta.candidates_token_count;
            if let Some(cached) = meta.cached_content_token_count {
                usage.cache_read = cached;
            }
        }

        let Some(candidates) = chunk.candidates else {
            continue;
        };

        for candidate in candidates {
            if let Some(reason) = candidate.finish_reason {
                stop_reason = Some(StopReason::from_google(&reason)).or(stop_reason);
            }

            let Some(content) = candidate.content else {
                continue;
            };
            let Some(parts) = content.parts else {
                continue;
            };

            for part in parts {
                if let Some(func_call) = part.function_call {
                    let id = format!("call_{}", func_call.name);
                    let input = func_call.args.unwrap_or_default();
                    event_tx
                        .send_async(ProviderEvent::ToolUseStart {
                            id: id.clone(),
                            name: func_call.name.clone(),
                        })
                        .await?;
                    content_blocks.push(ContentBlock::ToolUse {
                        id,
                        name: func_call.name,
                        input,
                    });
                    stop_reason = Some(StopReason::ToolUse);
                } else if let Some(text) = part.text {
                    if part.thought.unwrap_or(false) {
                        if !text.is_empty() {
                            event_tx
                                .send_async(ProviderEvent::ThinkingDelta { text: text.clone() })
                                .await?;
                        }
                        content_blocks.push(ContentBlock::Thinking {
                            thinking: text,
                            signature: part.thought_signature,
                        });
                    } else if !text.is_empty() {
                        event_tx
                            .send_async(ProviderEvent::TextDelta { text: text.clone() })
                            .await?;
                        content_blocks.push(ContentBlock::Text { text });
                    }
                }
            }
        }
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use test_case::test_case;

    const GEMINI_API_KEY: &str = "test-key";

    fn test_auth() -> Arc<Mutex<ResolvedAuth>> {
        Arc::new(Mutex::new(ResolvedAuth {
            base_url: None,
            headers: vec![("x-goog-api-key".into(), GEMINI_API_KEY.into())],
        }))
    }

    fn test_timeouts() -> super::super::Timeouts {
        super::super::Timeouts {
            connect: Duration::from_secs(5),
            low_speed: Duration::from_secs(30),
            stream: Duration::from_secs(300),
        }
    }

    fn test_model() -> Model {
        Model {
            id: "gemini-2.5-flash".into(),
            provider: crate::provider::ProviderKind::Google,
            dynamic_slug: None,
            tier: ModelTier::Medium,
            family: ModelFamily::Gemini,
            supports_vision_override: Some(true),
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            pricing: ModelPricing::default(),
            max_output_tokens: Some(8192),
            context_window: 1_048_576,
        }
    }

    #[test]
    fn google_build_body_basic() {
        let google = Google::with_auth(test_auth(), test_timeouts());
        let model = test_model();
        let messages = vec![Message::user("hello".into())];
        let body = google.build_body(
            &model,
            &messages,
            "be helpful",
            &json!([]),
            ThinkingConfig::Off,
        );

        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn google_build_body_thinking_adaptive() {
        let google = Google::with_auth(test_auth(), test_timeouts());
        let messages = vec![Message::user("think".into())];
        let body = google.build_body(
            &test_model(),
            &messages,
            "",
            &json!([]),
            ThinkingConfig::Adaptive,
        );

        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn google_build_body_thinking_budget() {
        let google = Google::with_auth(test_auth(), test_timeouts());
        let messages = vec![Message::user("think hard".into())];
        let body = google.build_body(
            &test_model(),
            &messages,
            "",
            &json!([]),
            ThinkingConfig::Budget(8192),
        );

        // Clamped to the model's max thinking budget (half of 8192 output tokens).
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
    }

    #[test_case("STOP", StopReason::EndTurn ; "stop")]
    #[test_case("MAX_TOKENS", StopReason::MaxTokens ; "max_tokens")]
    #[test_case("SAFETY", StopReason::EndTurn ; "safety")]
    #[test_case("RECITATION", StopReason::EndTurn ; "recitation")]
    #[test_case("unknown", StopReason::EndTurn ; "unknown")]
    fn stop_reason_from_google(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_google(input), expected);
    }

    #[test]
    fn convert_messages_user_and_assistant() {
        let messages = vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "hi there".into(),
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["parts"][0]["text"], "hello");
        assert_eq!(result[1]["role"], "model");
        assert_eq!(result[1]["parts"][0]["text"], "hi there");
    }

    #[test]
    fn convert_messages_thinking_block() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("sig123".into()),
            }],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["text"], "hmm");
        assert_eq!(result[0]["parts"][0]["thought"], true);
        assert_eq!(result[0]["parts"][0]["thoughtSignature"], "sig123");
    }

    #[test]
    fn convert_messages_tool_use_and_result() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "/tmp/a"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "file contents".into(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
    }

    #[test]
    fn convert_messages_tool_returned_image_gets_own_user_turn() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "[image: pic.png 1KB]".into(),
                    is_error: false,
                },
                ContentBlock::Image {
                    source: crate::ImageSource::new(
                        crate::ImageMediaType::Png,
                        std::sync::Arc::from("aGVsbG8="),
                    ),
                },
            ],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 2);
        assert!(result[0]["parts"][0].get("functionResponse").is_some());
        assert!(result[0]["parts"].as_array().unwrap().len() == 1);
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[1]["parts"][0]["inlineData"]["mimeType"], "image/png");
    }

    #[test]
    fn convert_messages_chat_pasted_image_stays_in_same_turn() {
        // Split the turn and the question drifts apart from its picture.
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
                ContentBlock::Image {
                    source: crate::ImageSource::new(
                        crate::ImageMediaType::Webp,
                        std::sync::Arc::from("aGVsbG8="),
                    ),
                },
            ],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 1);
        let parts = result[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/webp");
    }

    #[test]
    fn convert_tools_basic() {
        let tools = json!([{
            "name": "bash",
            "description": "run a command",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "additionalProperties": false},
                    "opts": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"verbose": {"type": "boolean"}}
                    }
                },
                "additionalProperties": false
            }
        }]);
        let result = convert_tools(&tools);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["name"], "bash");
        assert_eq!(result[0]["description"], "run a command");
        assert!(result[0]["parameters"]["properties"]["cmd"].is_object());
        assert!(
            result[0]["parameters"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            result[0]["parameters"]["properties"]["cmd"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            result[0]["parameters"]["properties"]["opts"]
                .get("additionalProperties")
                .is_none()
        );
    }

    #[test]
    fn strip_additional_properties_recursive() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "inner": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"x": {"type": "number", "additionalProperties": false}}
                },
                "list": {
                    "type": "array",
                    "items": {"type": "string", "additionalProperties": false}
                }
            }
        });
        let cleaned = strip_additional_properties(schema);
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(
            cleaned["properties"]["inner"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            cleaned["properties"]["inner"]["properties"]["x"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            cleaned["properties"]["list"]["items"]
                .get("additionalProperties")
                .is_none()
        );
    }

    #[test]
    fn models_list_has_defaults() {
        let models = models();
        assert!(!models.is_empty());
        for entry in models {
            assert!(!entry.prefixes.is_empty());
            assert!(entry.max_output_tokens > 0);
            assert!(entry.context_window >= entry.max_output_tokens);
        }
    }

    fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
        let body = isahc::AsyncBody::from_bytes_static(data);
        isahc::Response::builder().status(200).body(body).unwrap()
    }

    #[test]
    fn parse_sse_plain_text() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":10}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(result.usage.input, 5);
        assert_eq!(result.usage.output, 10);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Text { text } if text == "hello"
        ));
    }

    #[test]
    fn parse_sse_thinking_part() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"thinking...\",\"thought\":true,\"thoughtSignature\":\"sig1\"}]}},{\"content\":{\"parts\":[{\"text\":\"answer\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":20}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Thinking { thinking, signature } if thinking == "thinking..." && signature.as_deref() == Some("sig1")
        ));
        assert!(matches!(
            &result.message.content[1],
            ContentBlock::Text { text } if text == "answer"
        ));
    }

    #[test]
    fn parse_sse_tool_call() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"bash\",\"args\":{\"cmd\":\"ls\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":15}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::ToolUse { name, .. } if name == "bash"
        ));
    }

    #[test]
    fn parse_sse_cached_tokens() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":50}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.usage.input, 100);
        assert_eq!(result.usage.output, 10);
        assert_eq!(result.usage.cache_read, 50);
    }
}
