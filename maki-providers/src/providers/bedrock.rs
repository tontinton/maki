//! AWS Bedrock provider using `aws-sigv4` for request signing and
//! `aws-smithy-eventstream` for binary frame decoding. Shares event
//! processing logic with the Anthropic provider.

use std::env;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use base64::Engine;
use bytes::BytesMut;
use flume::Sender;
use isahc::{HttpClient, Request};
use serde_json::{Value, json};
use tracing::debug;

use crate::model::Model;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, Message as ChatMessage, ProviderEvent, StreamResponse, ThinkingConfig,
};

use super::anthropic::{EventAction, EventProcessingState, process_anthropic_event};

const SERVICE: &str = "bedrock-runtime";

// ---------------------------------------------------------------------------
// Model registry
// ---------------------------------------------------------------------------

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["claude-haiku-4-5"],
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 5.00,
                cache_write: 1.25,
                cache_read: 0.10,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-6"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-7"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
    ]
}

/// Maps maki model IDs to Bedrock model identifiers.
fn bedrock_model_id(model_id: &str) -> String {
    if model_id.contains('.') || model_id.contains(':') {
        return model_id.to_string();
    }
    let lookup = strip_date_suffix(model_id);
    let (mapped, version) = match lookup {
        "claude-haiku-4-5" => ("anthropic.claude-haiku-4-5", "v1:0"),
        "claude-sonnet-4-5" => ("anthropic.claude-sonnet-4-5", "v2:0"),
        "claude-sonnet-4-6" => ("anthropic.claude-sonnet-4-6", "v1:0"),
        "claude-opus-4-6" => ("anthropic.claude-opus-4-6", "v1:0"),
        "claude-opus-4-7" => ("anthropic.claude-opus-4-7", "v1:0"),
        _ => return format!("anthropic.{model_id}"),
    };
    format!("{mapped}-{version}")
}

fn strip_date_suffix(s: &str) -> &str {
    if s.len() > 9 {
        let candidate = &s[s.len() - 9..];
        if candidate.starts_with('-') && candidate[1..].bytes().all(|b| b.is_ascii_digit()) {
            return &s[..s.len() - 9];
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum BedrockAuth {
    BearerToken(String),
    SigV4(AwsCredentials),
}

#[derive(Clone)]
struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

fn resolve_auth() -> Result<BedrockAuth, AgentError> {
    if let Ok(token) = env::var("AWS_BEARER_TOKEN_BEDROCK")
        && !token.is_empty()
    {
        debug!("using Bedrock bearer token authentication");
        return Ok(BedrockAuth::BearerToken(token));
    }
    resolve_credentials().map(BedrockAuth::SigV4)
}

fn resolve_credentials() -> Result<AwsCredentials, AgentError> {
    if let (Ok(key), Ok(secret)) = (
        env::var("AWS_ACCESS_KEY_ID"),
        env::var("AWS_SECRET_ACCESS_KEY"),
    ) {
        debug!("using AWS credentials from environment variables");
        return Ok(AwsCredentials {
            access_key_id: key,
            secret_access_key: secret,
            session_token: env::var("AWS_SESSION_TOKEN").ok(),
        });
    }

    if let Some(creds) = read_shared_credentials() {
        debug!("using AWS credentials from shared credentials file");
        return Ok(creds);
    }

    if let Some(creds) = cli_credential_process() {
        debug!("using AWS credentials from CLI credential process");
        return Ok(creds);
    }

    Err(AgentError::Config {
        message: "no AWS credentials found: set AWS_BEARER_TOKEN_BEDROCK, \
                  AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY, configure ~/.aws/credentials, \
                  or ensure `aws configure export-credentials` works"
            .into(),
    })
}

fn read_shared_credentials() -> Option<AwsCredentials> {
    let path = env::var("AWS_SHARED_CREDENTIALS_FILE")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".aws").join("credentials")))?;
    let content = std::fs::read_to_string(&path).ok()?;
    let profile = env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into());
    parse_ini_profile(&content, &profile)
}

fn parse_ini_profile(content: &str, profile: &str) -> Option<AwsCredentials> {
    let header = format!("[{profile}]");
    let mut in_section = false;
    let mut key_id = None;
    let mut secret = None;
    let mut token = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == header;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            match k {
                "aws_access_key_id" => key_id = Some(v.to_string()),
                "aws_secret_access_key" => secret = Some(v.to_string()),
                "aws_session_token" => token = Some(v.to_string()),
                _ => {}
            }
        }
    }

    Some(AwsCredentials {
        access_key_id: key_id?,
        secret_access_key: secret?,
        session_token: token,
    })
}

fn cli_credential_process() -> Option<AwsCredentials> {
    let output = Command::new("aws")
        .args(["configure", "export-credentials", "--format", "process"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(AwsCredentials {
        access_key_id: parsed.get("AccessKeyId")?.as_str()?.to_string(),
        secret_access_key: parsed.get("SecretAccessKey")?.as_str()?.to_string(),
        session_token: parsed
            .get("SessionToken")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn resolve_region() -> String {
    env::var("AWS_BEDROCK_REGION")
        .or_else(|_| env::var("AWS_DEFAULT_REGION"))
        .or_else(|_| env::var("AWS_REGION"))
        .unwrap_or_else(|_| "us-east-1".into())
}

// ---------------------------------------------------------------------------
// SigV4 signing (via aws-sigv4 crate)
// ---------------------------------------------------------------------------

fn sign_request_headers(
    creds: &AwsCredentials,
    region: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Vec<(String, String)>, AgentError> {
    let identity = Credentials::new(
        &creds.access_key_id,
        &creds.secret_access_key,
        creds.session_token.clone(),
        None,
        "maki-bedrock",
    )
    .into();

    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SERVICE)
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| AgentError::Config {
            message: format!("SigV4 signing params: {e}"),
        })?
        .into();

    let signable = SignableRequest::new(
        "POST",
        url,
        headers.iter().copied(),
        SignableBody::Bytes(body),
    )
    .map_err(|e| AgentError::Config {
        message: format!("SigV4 signable request: {e}"),
    })?;

    let (instructions, _signature) = sign(signable, &signing_params)
        .map_err(|e| AgentError::Config {
            message: format!("SigV4 signing failed: {e}"),
        })?
        .into_parts();

    Ok(instructions
        .headers()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect())
}

// ---------------------------------------------------------------------------
// Provider implementation
// ---------------------------------------------------------------------------

pub struct Bedrock {
    client: HttpClient,
    auth: Arc<Mutex<BedrockAuth>>,
    region: String,
    stream_timeout: Duration,
}

impl Bedrock {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let auth = resolve_auth()?;
        let region = resolve_region();
        Ok(Self {
            client: super::http_client(timeouts),
            auth: Arc::new(Mutex::new(auth)),
            region,
            stream_timeout: timeouts.stream,
        })
    }

    fn endpoint_url(&self, model_id: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/invoke-with-response-stream",
            self.region, model_id
        )
    }

    async fn do_stream_request(
        &self,
        model: &Model,
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let bedrock_id = bedrock_model_id(&model.id);
        let url = self.endpoint_url(&bedrock_id);
        let json_body = serde_json::to_vec(body)?;

        let host = url
            .strip_prefix("https://")
            .unwrap_or(&url)
            .split('/')
            .next()
            .unwrap_or("");

        let auth = self.auth.lock().unwrap().clone();
        let request = match &auth {
            BedrockAuth::BearerToken(token) => Request::builder()
                .method("POST")
                .uri(&url)
                .header("host", host)
                .header("accept", "application/vnd.amazon.eventstream")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(json_body)?,
            BedrockAuth::SigV4(creds) => {
                let base_headers = [
                    ("host", host),
                    ("accept", "application/vnd.amazon.eventstream"),
                    ("content-type", "application/json"),
                ];
                let signed_headers =
                    sign_request_headers(creds, &self.region, &url, &base_headers, &json_body)?;

                let mut builder = Request::builder()
                    .method("POST")
                    .uri(&url)
                    .header("host", host)
                    .header("accept", "application/vnd.amazon.eventstream")
                    .header("content-type", "application/json");
                for (k, v) in &signed_headers {
                    builder = builder.header(k.as_str(), v.as_str());
                }
                builder.body(json_body)?
            }
        };

        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            self.parse_event_stream(response, event_tx).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn parse_event_stream(
        &self,
        mut response: isahc::Response<isahc::AsyncBody>,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        use bytes::BufMut;
        use futures_lite::AsyncReadExt;

        let mut buf = BytesMut::with_capacity(16384);
        let mut chunk = [0u8; 8192];
        let mut decoder = MessageFrameDecoder::new();
        let mut state = EventProcessingState::new();
        let mut deadline = Instant::now() + self.stream_timeout;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let read_result = futures_lite::future::or(
                async { response.body_mut().read(&mut chunk).await },
                async {
                    smol::Timer::after(remaining).await;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "stream timeout",
                    ))
                },
            )
            .await;

            let n = match read_result {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(AgentError::Timeout {
                        secs: self.stream_timeout.as_secs(),
                    });
                }
                Err(e) => return Err(AgentError::from(e)),
            };
            deadline = Instant::now() + self.stream_timeout;
            buf.put_slice(&chunk[..n]);

            // Decode complete frames
            loop {
                match decoder.decode_frame(&mut buf) {
                    Ok(DecodedFrame::Complete(msg)) => {
                        if matches!(
                            self.handle_frame(&msg, &mut state, event_tx).await?,
                            Some(EventAction::Stop)
                        ) {
                            return Ok(state.into_stream_response());
                        }
                    }
                    Ok(DecodedFrame::Incomplete) => break,
                    Err(e) => {
                        return Err(AgentError::Api {
                            status: 0,
                            message: format!("EventStream decode error: {e}"),
                        });
                    }
                }
            }
        }

        Ok(state.into_stream_response())
    }

    async fn handle_frame(
        &self,
        msg: &aws_smithy_types::event_stream::Message,
        state: &mut EventProcessingState,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<Option<EventAction>, AgentError> {
        if msg.payload().is_empty() {
            return Ok(None);
        }

        // Extract event type from headers
        let event_type = msg
            .headers()
            .iter()
            .find(|h| h.name().as_str() == ":event-type")
            .and_then(|h| h.value().as_string().ok())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();

        // Bedrock wraps Anthropic events in a "chunk" frame with base64-encoded "bytes" field
        let json_data = if event_type == "chunk" {
            let payload_str = std::str::from_utf8(msg.payload()).map_err(|_| AgentError::Api {
                status: 0,
                message: "EventStream payload not valid UTF-8".into(),
            })?;
            let wrapper: Value = serde_json::from_str(payload_str)?;
            if let Some(bytes_b64) = wrapper.get("bytes").and_then(|v| v.as_str()) {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes_b64)
                    .map_err(|e| AgentError::Api {
                        status: 0,
                        message: format!("base64 decode error: {e}"),
                    })?;
                serde_json::from_slice::<Value>(&decoded)?
            } else {
                wrapper
            }
        } else {
            let payload_str = match std::str::from_utf8(msg.payload()) {
                Ok(s) => s,
                Err(_) => return Ok(None),
            };
            match serde_json::from_str(payload_str) {
                Ok(v) => v,
                Err(_) => return Ok(None),
            }
        };

        let anthropic_event_type = json_data
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let action = process_anthropic_event(&anthropic_event_type, json_data, state, event_tx).await?;
        Ok(Some(action))
    }
}

impl Provider for Bedrock {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [ChatMessage],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let wire_messages: Value = serde_json::to_value(
                messages
                    .iter()
                    .map(|msg| {
                        json!({
                            "role": msg.role,
                            "content": msg.content,
                        })
                    })
                    .collect::<Vec<_>>(),
            )?;

            let mut body = json!({
                "anthropic_version": "bedrock-2023-05-31",
                "max_tokens": model.max_output_tokens,
                "system": system,
                "messages": wire_messages,
                "tools": tools,
            });

            thinking.apply_to_body(&mut body);

            debug!(model = %model.id, num_messages = messages.len(), ?thinking, "sending Bedrock request");
            self.do_stream_request(model, &body, event_tx).await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async {
            Ok(models()
                .iter()
                .flat_map(|e| e.prefixes.iter())
                .map(|p| (*p).to_string())
                .collect())
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let auth = resolve_auth()?;
            *self.auth.lock().unwrap() = auth;
            debug!("reloaded AWS Bedrock credentials");
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ini_default_profile() {
        let ini = "[default]\naws_access_key_id = AKID123\naws_secret_access_key = SECRET456\n";
        let creds = parse_ini_profile(ini, "default").unwrap();
        assert_eq!(creds.access_key_id, "AKID123");
        assert_eq!(creds.secret_access_key, "SECRET456");
        assert!(creds.session_token.is_none());
    }

    #[test]
    fn parse_ini_named_profile_with_token() {
        let ini = "\
[default]
aws_access_key_id = DEFAULT_KEY
aws_secret_access_key = DEFAULT_SECRET

[profile-dev]
aws_access_key_id = DEV_KEY
aws_secret_access_key = DEV_SECRET
aws_session_token = DEV_TOKEN
";
        let creds = parse_ini_profile(ini, "profile-dev").unwrap();
        assert_eq!(creds.access_key_id, "DEV_KEY");
        assert_eq!(creds.secret_access_key, "DEV_SECRET");
        assert_eq!(creds.session_token, Some("DEV_TOKEN".into()));
    }

    #[test]
    fn parse_ini_missing_profile() {
        let ini = "[default]\naws_access_key_id = X\naws_secret_access_key = Y\n";
        assert!(parse_ini_profile(ini, "nonexistent").is_none());
    }

    #[test]
    fn bedrock_model_id_standard_mapping() {
        assert_eq!(
            bedrock_model_id("claude-sonnet-4-5"),
            "anthropic.claude-sonnet-4-5-v2:0"
        );
        assert_eq!(
            bedrock_model_id("claude-haiku-4-5"),
            "anthropic.claude-haiku-4-5-v1:0"
        );
        assert_eq!(
            bedrock_model_id("claude-opus-4-7"),
            "anthropic.claude-opus-4-7-v1:0"
        );
    }

    #[test]
    fn bedrock_model_id_with_date_suffix() {
        assert_eq!(
            bedrock_model_id("claude-sonnet-4-6-20250514"),
            "anthropic.claude-sonnet-4-6-v1:0"
        );
    }

    #[test]
    fn bedrock_model_id_passthrough_qualified() {
        let qualified = "anthropic.claude-sonnet-4-5-v2:0";
        assert_eq!(bedrock_model_id(qualified), qualified);

        let with_dot = "us.anthropic.claude-sonnet-4-5-v2:0";
        assert_eq!(bedrock_model_id(with_dot), with_dot);
    }

    #[test]
    fn bedrock_model_id_unknown_falls_back() {
        assert_eq!(
            bedrock_model_id("claude-unknown-99"),
            "anthropic.claude-unknown-99"
        );
    }

    #[test]
    fn region_falls_back_to_us_east_1() {
        let result = resolve_region();
        assert!(!result.is_empty());
    }

    #[test]
    fn strip_date_suffix_works() {
        assert_eq!(
            strip_date_suffix("claude-sonnet-4-6-20250514"),
            "claude-sonnet-4-6"
        );
        assert_eq!(strip_date_suffix("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(strip_date_suffix("short"), "short");
    }
}
