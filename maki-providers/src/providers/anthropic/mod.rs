//! Anthropic allows 4 cache breakpoints per request. We place them on: the last tool
//! definition, the system prompt, and the last block of the 2 most recent messages.

pub(crate) mod bedrock;
pub(crate) mod shared;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::KeyPool;

const API_VERSION: &str = "2023-06-01";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=1000";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

const ENV_VAR: &str = "ANTHROPIC_API_KEY";

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "anthropic",
    display_name: "Anthropic",
    protocol: maki_config::providers::Protocol::Anthropic,
    default_base_url: "https://api.anthropic.com/v1/messages",
    default_api_key_env: ENV_VAR,
    default_model: "anthropic/claude-sonnet-4-6",
    plans: None,
    login_url: Some("https://console.anthropic.com/settings/keys"),
    needs_url: false,
});

pub(crate) use shared::models;

/// Returns whether the fast-mode beta header must be attached. We re-check
/// `supports_fast()` here rather than trusting `opts.fast` alone, so a stale UI
/// flag can never bill an ineligible model at the premium fast-mode rate.
fn apply_fast_mode(body: &mut Value, model: &Model, opts: RequestOptions) -> bool {
    let on = opts.fast && model.supports_fast();
    if on {
        body["speed"] = json!("fast");
    }
    on
}

fn resolve_auth_from_key(key: &str) -> super::ResolvedAuth {
    super::ResolvedAuth {
        base_url: Some("https://api.anthropic.com/v1/messages".into()),
        headers: vec![("x-api-key".into(), key.to_string())],
    }
}

pub struct Anthropic {
    client: HttpClient,
    auth: Arc<Mutex<super::ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

impl Anthropic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("anthropic", ENV_VAR)?;
        let resolved = resolve_auth_from_key(pool.current());
        debug!(keys = pool.len(), "using API key authentication");
        Ok(Self {
            client: super::http_client(timeouts),
            auth: Arc::new(Mutex::new(resolved)),
            key_pool: Some(pool),
            system_prefix: None,
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: super::http_client(timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn build_request(&self, method: &str, url: Option<&str>) -> isahc::http::request::Builder {
        let auth = self.auth.lock().unwrap();
        let url = url.unwrap_or_else(|| auth.base_url.as_deref().unwrap_or(MESSAGES_URL));
        let mut builder = Request::builder()
            .method(method)
            .uri(url)
            .header("anthropic-version", API_VERSION)
            .header("user-agent", super::user_agent());
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder
    }

    async fn do_stream_request(
        &self,
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
        fast: bool,
        long_context: bool,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(body)?;
        let mut builder = self
            .build_request("POST", None)
            .header("content-type", "application/json");
        let mut betas = Vec::new();
        if fast {
            betas.push(FAST_MODE_BETA);
        }
        if long_context {
            betas.push(shared::LONG_CONTEXT_BETA);
        }
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }
        let request = builder.body(json_body)?;
        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn do_list_models(&self) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut url = MODELS_URL.to_string();
            if let Some(cursor) = &after_id {
                url.push_str(&format!("&after_id={cursor}"));
            }

            let request = self.build_request("GET", Some(&url)).body(())?;
            let mut response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }

            let body_text = response.text().await?;
            let page: ModelsPage = serde_json::from_str(&body_text)?;
            for m in page.data {
                if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                    models.push(crate::model::ModelInfo::id_only(format!(
                        "{}{}",
                        m.id,
                        shared::LONG_CONTEXT_SUFFIX
                    )));
                }
                models.push(crate::model::ModelInfo::id_only(m.id));
            }

            if !page.has_more {
                break;
            }
            after_id = page.last_id;
        }

        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

impl Provider for Anthropic {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let system_blocks = if let Some(prefix) = &self.system_prefix {
                vec![
                    shared::SystemBlock {
                        r#type: "text",
                        text: prefix,
                        cache_control: None,
                    },
                    shared::SystemBlock {
                        r#type: "text",
                        text: system,
                        cache_control: Some(shared::EPHEMERAL),
                    },
                ]
            } else {
                vec![shared::SystemBlock {
                    r#type: "text",
                    text: system,
                    cache_control: Some(shared::EPHEMERAL),
                }]
            };

            let mut body = shared::build_request_body_with_system(
                model,
                messages,
                &system_blocks,
                tools,
                opts.thinking,
            );
            body["model"] = json!(shared::strip_long_context(&model.id));
            body["stream"] = json!(true);
            let fast = apply_fast_mode(&mut body, model, opts);
            let long_context = model.id.ends_with(shared::LONG_CONTEXT_SUFFIX);

            debug!(model = %model.id, num_messages = messages.len(), thinking = ?opts.thinking, fast, long_context, "sending API request");
            self.do_stream_request(&body, event_tx, fast, long_context)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = KeyPool::resolve("anthropic", ENV_VAR)?;
            *self.auth.lock().unwrap() = resolve_auth_from_key(pool.current());
            debug!("reloaded Anthropic auth from env");
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

#[derive(Deserialize)]
struct ApiModelInfo {
    id: String,
    #[serde(default)]
    max_input_tokens: u32,
}

#[derive(Deserialize)]
struct ModelsPage {
    data: Vec<ApiModelInfo>,
    has_more: bool,
    last_id: Option<String>,
}

pub(crate) async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();
    let mut parser = shared::EventParser::new();
    let mut current_event = String::new();
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        if let Some(rest) = line.strip_prefix("event:") {
            current_event = rest.strip_prefix(' ').unwrap_or(rest).to_string();
            continue;
        }

        let data = match line.strip_prefix("data:") {
            Some(d) => d.strip_prefix(' ').unwrap_or(d),
            None => continue,
        };

        if parser
            .process(&current_event, data, event_tx)
            .await?
            .is_break()
        {
            break;
        }
    }

    Ok(parser.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, ProviderEvent, Role, StopReason, TokenUsage};
    use serde_json::{Value, json};
    use shared::build_wire_messages;
    use std::time::Duration;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

    fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
        let body = isahc::AsyncBody::from_bytes_static(data);
        isahc::Response::builder().status(200).body(body).unwrap()
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":8}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(
                resp.usage,
                TokenUsage {
                    input: 42,
                    output: 10,
                    cache_creation: 5,
                    cache_read: 8
                }
            );
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));

            let mut deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let ProviderEvent::TextDelta { text: t } = e {
                    deltas.push(t);
                }
            }
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_no_space_after_colon() {
        smol::block_on(async {
            let sse_data = b"\
event:message_start\n\
data:{\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\
\n\
event:content_block_start\n\
data:{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event:content_block_delta\n\
data:{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\
\n\
event:message_delta\n\
data:{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\
\n\
event:message_stop\n\
data:{\"type\":\"message_stop\"}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "OK")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert_eq!(resp.usage.input, 7);
            assert_eq!(resp.usage.output, 1);
        })
    }

    #[test]
    fn parse_sse_tool_use() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"echo hi\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "tu_1");
            assert_eq!(tools[0].1, "bash");

            let starts: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("tu_1".to_string(), "bash".to_string())]);
        })
    }

    #[test]
    fn cache_control_placement() {
        let single = vec![Message::user("only".into())];
        let wire = build_wire_messages(&single);
        let json: Value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        let multi = vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                    },
                ],
                ..Default::default()
            },
        ];
        let wire = build_wire_messages(&multi);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert!(json[0]["content"][0].get("cache_control").is_none());
        assert_eq!(
            json[1]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(json[2]["content"][0].get("cache_control").is_none());
        assert_eq!(
            json[2]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn apply_fast_mode_sets_speed_on_capable_model() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(
            &mut body,
            &model,
            RequestOptions {
                fast: true,
                ..Default::default()
            },
        );
        assert!(header);
        assert_eq!(body["speed"], json!("fast"));
    }

    #[test]
    fn apply_fast_mode_ignores_stale_flag_on_ineligible_model() {
        // Sonnet is not fast-capable, so opts.fast=true must still skip `speed`.
        let model = Model::from_spec("anthropic/claude-sonnet-4-5").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(
            &mut body,
            &model,
            RequestOptions {
                fast: true,
                ..Default::default()
            },
        );
        assert!(!header);
        assert!(body.get("speed").is_none());
    }

    #[test]
    fn apply_fast_mode_off_when_not_requested() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(&mut body, &model, RequestOptions::default());
        assert!(!header);
        assert!(body.get("speed").is_none());
    }

    #[test]
    fn long_context_spec_resolves_to_1m_window() {
        let model = Model::from_spec("anthropic/claude-opus-4-8-1m").unwrap();
        assert_eq!(model.id, "claude-opus-4-8-1m");
        assert_eq!(model.context_window, shared::LONG_CONTEXT_WINDOW);
        assert!(model.id.ends_with(shared::LONG_CONTEXT_SUFFIX));
        // The API has never heard of `-1m`, so strip it before sending.
        assert_eq!(shared::strip_long_context(&model.id), "claude-opus-4-8");
    }

    #[test]
    fn list_models_adds_1m_variant_from_max_input_tokens() {
        // The real /v1/models payload hides the 1M window in `max_input_tokens`.
        let page: ModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {"id": "claude-opus-4-8", "max_input_tokens": 1000000},
                    {"id": "claude-opus-4-5-20251101", "max_input_tokens": 200000}
                ],
                "has_more": false,
                "last_id": null
            }"#,
        )
        .unwrap();

        let mut models = Vec::new();
        for m in page.data {
            if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                models.push(format!("{}{}", m.id, shared::LONG_CONTEXT_SUFFIX));
            }
            models.push(m.id);
        }
        models.sort();

        assert_eq!(
            models,
            vec![
                "claude-opus-4-5-20251101".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-opus-4-8-1m".to_string(),
            ]
        );
    }

    #[test]
    fn parse_sse_overloaded_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_unparseable_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: not-json\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 400);
                    assert_eq!(message, "not-json");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_yields_empty_object() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_2\",\"name\":\"read\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{broken\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(*tools[0].2, Value::Object(Default::default()));
        })
    }

    #[test]
    fn parse_sse_thinking_blocks() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" think\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, signature }
                    if thinking == "Let me think" && *signature == Some("sig123".to_string()))
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
            );

            let thinking_deltas: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me", " think"]);
        })
    }

    #[test]
    fn parse_sse_redacted_thinking() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_data\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::RedactedThinking { data } if data == "opaque_data")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hi")
            );
        })
    }
}
