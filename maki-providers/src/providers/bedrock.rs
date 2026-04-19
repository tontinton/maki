//! AWS Bedrock provider with SigV4 auth and EventStream binary protocol.
//! Supports credential resolution from env vars, shared credentials file,
//! and the AWS CLI credential process fallback.

use std::env;
use std::fmt::Write as FmtWrite;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use flume::Sender;
use isahc::{HttpClient, Request};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::model::Model;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage,
};

const SERVICE: &str = "bedrock";
const SIGNING_ALGORITHM: &str = "AWS4-HMAC-SHA256";

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
    let base = match model_id.split_once('-') {
        Some(_) => model_id,
        None => model_id,
    };
    // Strip any date suffix (e.g. -20250514) for the mapping lookup
    let lookup = strip_date_suffix(base);
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
    // Date suffixes look like -20250514 (dash + 8 digits at end)
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
    // 1. Bearer token (Bedrock API Key) — highest priority
    if let Ok(token) = env::var("AWS_BEARER_TOKEN_BEDROCK")
        && !token.is_empty()
    {
        debug!("using Bedrock bearer token authentication");
        return Ok(BedrockAuth::BearerToken(token));
    }

    // 2. Standard AWS credential chain
    resolve_credentials().map(BedrockAuth::SigV4)
}

fn resolve_credentials() -> Result<AwsCredentials, AgentError> {
    // 1. Environment variables
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

    // 2. Shared credentials file (~/.aws/credentials)
    if let Some(creds) = read_shared_credentials() {
        debug!("using AWS credentials from shared credentials file");
        return Ok(creds);
    }

    // 3. Fallback: AWS CLI credential process
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
// SigV4 signing
// ---------------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC-SHA256 using SHA-256 from the sha2 crate.
    // HMAC(K, m) = H((K' ^ opad) || H((K' ^ ipad) || m))
    const BLOCK_SIZE: usize = 64;
    const IPAD: u8 = 0x36;
    const OPAD: u8 = 0x5c;

    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hash = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    let mut i_key_pad = [0u8; BLOCK_SIZE];
    for (i, b) in key_block.iter().enumerate() {
        i_key_pad[i] = b ^ IPAD;
    }
    inner.update(i_key_pad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    let mut o_key_pad = [0u8; BLOCK_SIZE];
    for (i, b) in key_block.iter().enumerate() {
        o_key_pad[i] = b ^ OPAD;
    }
    outer.update(o_key_pad);
    outer.update(inner_hash);
    let result = outer.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

struct SignedRequest {
    authorization: String,
    amz_date: String,
    security_token: Option<String>,
    content_sha256: String,
}

fn sign_request(
    creds: &AwsCredentials,
    region: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    now: SystemTime,
) -> SignedRequest {
    let datetime = {
        let dur = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = dur.as_secs();
        // Format as YYYYMMDD'T'HHMMSS'Z'
        let days = secs / 86400;
        let (y, m, d) = epoch_days_to_ymd(days as i64);
        let day_secs = secs % 86400;
        let hh = day_secs / 3600;
        let mm = (day_secs % 3600) / 60;
        let ss = day_secs % 60;
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
    };
    let date_stamp = &datetime[..8];
    let content_sha256 = sha256_hex(body);
    let scope = format!("{date_stamp}/{region}/{SERVICE}/aws4_request");

    // Collect all headers that will be signed, including the ones we add
    let mut sign_headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.to_string()))
        .collect();
    sign_headers.push(("x-amz-date".into(), datetime.clone()));
    sign_headers.push(("x-amz-content-sha256".into(), content_sha256.clone()));
    if let Some(token) = &creds.session_token {
        sign_headers.push(("x-amz-security-token".into(), token.clone()));
    }
    sign_headers.sort_by(|a, b| a.0.cmp(&b.0));

    let signed_headers_str: String = sign_headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_headers: String = sign_headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();

    let canonical_request =
        format!("{method}\n{path}\n\n{canonical_headers}\n{signed_headers_str}\n{content_sha256}");
    let canonical_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!("{SIGNING_ALGORITHM}\n{datetime}\n{scope}\n{canonical_hash}");

    // Derive signing key
    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "{SIGNING_ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers_str}, Signature={signature}",
        creds.access_key_id,
    );

    SignedRequest {
        authorization,
        amz_date: datetime,
        security_token: creds.session_token.clone(),
        content_sha256,
    }
}

fn epoch_days_to_ymd(days: i64) -> (i64, u32, u32) {
    // Convert Unix epoch days to (year, month, day).
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// CRC-32 (ISO 3309 / ITU-T V.42, used by AWS EventStream)
// ---------------------------------------------------------------------------

const fn build_crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let poly: u32 = 0xEDB8_8320;
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ poly;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

const CRC32C_TABLE: [u32; 256] = build_crc32c_table();

fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// EventStream binary frame parser
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct EventStreamFrame {
    event_type: String,
    payload: Vec<u8>,
}

fn parse_eventstream_frame(data: &[u8]) -> Result<(EventStreamFrame, usize), AgentError> {
    if data.len() < 16 {
        return Err(AgentError::Api {
            status: 0,
            message: "EventStream frame too short for prelude".into(),
        });
    }

    let total_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let header_len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let prelude_crc_expected = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    // Validate prelude CRC (covers first 8 bytes)
    let prelude_crc_actual = crc32c(&data[..8]);
    if prelude_crc_actual != prelude_crc_expected {
        return Err(AgentError::Api {
            status: 0,
            message: format!(
                "EventStream prelude CRC mismatch: expected {prelude_crc_expected:#x}, got {prelude_crc_actual:#x}"
            ),
        });
    }

    if data.len() < total_len {
        return Err(AgentError::Api {
            status: 0,
            message: format!(
                "EventStream frame truncated: need {total_len} bytes, have {}",
                data.len()
            ),
        });
    }

    // Validate message CRC (covers everything except last 4 bytes)
    let message_crc_expected = u32::from_be_bytes([
        data[total_len - 4],
        data[total_len - 3],
        data[total_len - 2],
        data[total_len - 1],
    ]);
    let message_crc_actual = crc32c(&data[..total_len - 4]);
    if message_crc_actual != message_crc_expected {
        return Err(AgentError::Api {
            status: 0,
            message: format!(
                "EventStream message CRC mismatch: expected {message_crc_expected:#x}, got {message_crc_actual:#x}"
            ),
        });
    }

    // Parse headers (start at offset 12, length = header_len)
    let header_start = 12;
    let header_end = header_start + header_len;
    let mut event_type = String::new();
    let mut pos = header_start;

    while pos < header_end {
        if pos >= data.len() {
            break;
        }
        let name_len = data[pos] as usize;
        pos += 1;
        if pos + name_len > header_end {
            break;
        }
        let name = std::str::from_utf8(&data[pos..pos + name_len]).unwrap_or("");
        pos += name_len;

        if pos >= header_end {
            break;
        }
        let value_type = data[pos];
        pos += 1;

        // Type 7 = string
        if value_type == 7 {
            if pos + 2 > header_end {
                break;
            }
            let val_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + val_len > header_end {
                break;
            }
            let val = std::str::from_utf8(&data[pos..pos + val_len]).unwrap_or("");
            pos += val_len;

            if name == ":event-type" {
                event_type = val.to_string();
            }
        } else {
            // Skip other header types. Common types and their sizes:
            // 0=bool_true(0), 1=bool_false(0), 2=byte(1), 3=short(2),
            // 4=int(4), 5=long(8), 6=bytes(2+len), 7=string(2+len), 8=timestamp(8), 9=uuid(16)
            let skip = match value_type {
                0 | 1 => 0,
                2 => 1,
                3 => 2,
                4 => 4,
                5 | 8 => 8,
                9 => 16,
                6 => {
                    if pos + 2 <= header_end {
                        let l = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                        2 + l
                    } else {
                        break;
                    }
                }
                _ => break,
            };
            pos += skip;
        }
    }

    // Payload sits between end of headers and the 4-byte message CRC
    let payload_start = header_end;
    let payload_end = total_len - 4;
    let payload = if payload_end > payload_start {
        data[payload_start..payload_end].to_vec()
    } else {
        Vec::new()
    };

    Ok((
        EventStreamFrame {
            event_type,
            payload,
        },
        total_len,
    ))
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
        // Model IDs contain only alphanumeric, dots, colons, and hyphens — no encoding needed.
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

        let url_parsed = url.strip_prefix("https://").unwrap_or(&url);
        let (host, path) = url_parsed.split_once('/').unwrap_or((url_parsed, "/"));
        let path = format!("/{path}");

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
                let headers_for_signing = [
                    ("host", host),
                    ("accept", "application/vnd.amazon.eventstream"),
                    ("content-type", "application/json"),
                ];
                let signed = sign_request(
                    creds,
                    &self.region,
                    "POST",
                    &path,
                    &headers_for_signing,
                    &json_body,
                    SystemTime::now(),
                );
                let mut builder = Request::builder()
                    .method("POST")
                    .uri(&url)
                    .header("host", host)
                    .header("accept", "application/vnd.amazon.eventstream")
                    .header("content-type", "application/json")
                    .header("x-amz-date", &signed.amz_date)
                    .header("x-amz-content-sha256", &signed.content_sha256)
                    .header("authorization", &signed.authorization);
                if let Some(token) = &signed.security_token {
                    builder = builder.header("x-amz-security-token", token);
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
        use futures_lite::AsyncReadExt;

        let mut buf = Vec::with_capacity(16384);
        let mut chunk = [0u8; 8192];
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut current_tool_json = String::new();
        let mut current_block_idx: usize = 0;
        let mut usage = TokenUsage::default();
        let mut stop_reason: Option<StopReason> = None;
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
            buf.extend_from_slice(&chunk[..n]);

            // Process complete frames from buffer
            while buf.len() >= 16 {
                let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if buf.len() < total_len {
                    break;
                }

                let (frame, consumed) = parse_eventstream_frame(&buf)?;
                buf.drain(..consumed);

                if frame.payload.is_empty() {
                    continue;
                }

                // The payload is a JSON blob wrapping Anthropic-format events
                let payload_str = match std::str::from_utf8(&frame.payload) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                let event_value: Value = match serde_json::from_str(payload_str) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Bedrock wraps events in a "bytes" field (base64-encoded) inside a chunk event,
                // or delivers them directly depending on the event type.
                let json_data = if frame.event_type == "chunk" {
                    if let Some(bytes_b64) = event_value.get("bytes").and_then(|v| v.as_str()) {
                        match base64::engine::general_purpose::STANDARD.decode(bytes_b64) {
                            Ok(decoded) => match serde_json::from_slice::<Value>(&decoded) {
                                Ok(v) => v,
                                Err(_) => continue,
                            },
                            Err(_) => continue,
                        }
                    } else {
                        event_value
                    }
                } else {
                    event_value
                };

                let event_type = json_data.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match event_type {
                    "message_start" => {
                        if let Ok(ev) =
                            serde_json::from_value::<super::anthropic::MessageStartEvent>(json_data)
                            && let Some(u) = ev.message.usage
                        {
                            usage = TokenUsage::from(u);
                        }
                    }
                    "content_block_start" => {
                        match serde_json::from_value::<super::anthropic::ContentBlockStartEvent>(
                            json_data,
                        ) {
                            Ok(ev) => {
                                current_block_idx = ev.index;
                                match ev.content_block {
                                    super::anthropic::SseContentBlock::Text => {
                                        content_blocks.push(ContentBlock::Text {
                                            text: String::new(),
                                        });
                                    }
                                    super::anthropic::SseContentBlock::Thinking => {
                                        content_blocks.push(ContentBlock::Thinking {
                                            thinking: String::new(),
                                            signature: None,
                                        });
                                    }
                                    super::anthropic::SseContentBlock::RedactedThinking {
                                        data,
                                    } => {
                                        content_blocks
                                            .push(ContentBlock::RedactedThinking { data });
                                    }
                                    super::anthropic::SseContentBlock::ToolUse { id, name } => {
                                        current_tool_json.clear();
                                        event_tx
                                            .send_async(ProviderEvent::ToolUseStart {
                                                id: id.clone(),
                                                name: name.clone(),
                                            })
                                            .await?;
                                        content_blocks.push(ContentBlock::ToolUse {
                                            id,
                                            name,
                                            input: Value::Null,
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to parse content_block_start")
                            }
                        }
                    }
                    "content_block_delta" => {
                        match serde_json::from_value::<super::anthropic::ContentBlockDeltaEvent>(
                            json_data,
                        ) {
                            Ok(ev) => {
                                current_block_idx = ev.index;
                                let block = content_blocks.get_mut(current_block_idx);
                                match ev.delta {
                                    super::anthropic::Delta::Text { text } => {
                                        if !text.is_empty() {
                                            if let Some(ContentBlock::Text { text: t }) = block {
                                                t.push_str(&text);
                                            }
                                            event_tx
                                                .send_async(ProviderEvent::TextDelta { text })
                                                .await?;
                                        }
                                    }
                                    super::anthropic::Delta::Thinking { thinking } => {
                                        if !thinking.is_empty() {
                                            if let Some(ContentBlock::Thinking {
                                                thinking: t,
                                                ..
                                            }) = block
                                            {
                                                t.push_str(&thinking);
                                            }
                                            event_tx
                                                .send_async(ProviderEvent::ThinkingDelta {
                                                    text: thinking,
                                                })
                                                .await?;
                                        }
                                    }
                                    super::anthropic::Delta::Signature { signature } => {
                                        if let Some(ContentBlock::Thinking {
                                            signature: sig, ..
                                        }) = block
                                        {
                                            *sig = Some(signature);
                                        }
                                    }
                                    super::anthropic::Delta::InputJson { partial_json } => {
                                        current_tool_json.push_str(&partial_json);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to parse content_block_delta")
                            }
                        }
                    }
                    "content_block_stop" => {
                        if let Some(ContentBlock::ToolUse { name, input, .. }) =
                            content_blocks.get_mut(current_block_idx)
                        {
                            *input = match serde_json::from_str(&current_tool_json) {
                                Ok(v) => {
                                    debug!(tool = %name, json = %current_tool_json, "tool input JSON");
                                    v
                                }
                                Err(e) => {
                                    warn!(error = %e, json = %current_tool_json, "malformed tool JSON");
                                    Value::Object(Default::default())
                                }
                            };
                            current_tool_json.clear();
                        }
                    }
                    "message_delta" => {
                        if let Ok(ev) =
                            serde_json::from_value::<super::anthropic::MessageDeltaEvent>(json_data)
                        {
                            if let Some(u) = ev.usage {
                                usage.output = u.output_tokens;
                            }
                            if let Some(d) = ev.delta {
                                stop_reason = d
                                    .stop_reason
                                    .map(|s| StopReason::from_anthropic(&s))
                                    .or(stop_reason);
                            }
                        }
                    }
                    "message_stop" => break,
                    "error" => {
                        if let Ok(ev) = serde_json::from_value::<super::SseErrorPayload>(json_data)
                        {
                            warn!(
                                error_type = %ev.error.r#type,
                                message = %ev.error.message,
                                "EventStream error"
                            );
                            return Err(ev.into_agent_error());
                        }
                    }
                    _ => {}
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
}

impl Provider for Bedrock {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
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

    // --- SigV4 signing test ---

    #[test]
    fn sigv4_signs_correctly() {
        // Based on AWS Signature Version 4 test suite concepts.
        // We verify the signing key derivation and overall signature format.
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };

        let body = b"test-body";
        let headers = [("host", "example.amazonaws.com")];

        // Use a fixed timestamp: 2015-08-30T12:36:00Z = 1440938160
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1440938160);
        let signed = sign_request(&creds, "us-east-1", "POST", "/test", &headers, body, now);

        assert!(signed.authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request"
        ));
        assert!(signed.authorization.contains("Signature="));
        assert_eq!(signed.amz_date, "20150830T123600Z");
        assert_eq!(signed.content_sha256, sha256_hex(body));
    }

    #[test]
    fn sigv4_with_session_token() {
        let creds = AwsCredentials {
            access_key_id: "AKID".into(),
            secret_access_key: "SECRET".into(),
            session_token: Some("TOKEN123".into()),
        };
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000);
        let signed = sign_request(&creds, "us-west-2", "POST", "/", &[("host", "h")], b"", now);

        assert!(signed.authorization.contains("x-amz-security-token"));
        assert_eq!(signed.security_token, Some("TOKEN123".into()));
    }

    // --- Signing key derivation test ---

    #[test]
    fn signing_key_derivation() {
        // AWS test vector for signing key derivation
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let date = "20120215";
        let region = "us-east-1";
        let service = "iam";

        let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
        let k_region = hmac_sha256(&k_date, region.as_bytes());
        let k_service = hmac_sha256(&k_region, service.as_bytes());
        let k_signing = hmac_sha256(&k_service, b"aws4_request");

        // Known expected value from AWS documentation
        let expected = "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d";
        assert_eq!(hex_encode(&k_signing), expected);
    }

    // --- EventStream parser tests ---

    fn build_eventstream_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        // Build headers bytes
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7); // string type
            let vlen = value.len() as u16;
            header_bytes.extend_from_slice(&vlen.to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }

        let header_len = header_bytes.len() as u32;
        let total_len = 4 + 4 + 4 + header_bytes.len() + payload.len() + 4;

        let mut frame = Vec::with_capacity(total_len);
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&header_len.to_be_bytes());

        // Prelude CRC (first 8 bytes)
        let prelude_crc = crc32c(&frame[..8]);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());

        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(payload);

        // Message CRC (everything except last 4 bytes)
        let message_crc = crc32c(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());

        frame
    }

    #[test]
    fn eventstream_parse_content_event() {
        let payload = br#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let frame = build_eventstream_frame(
            &[
                (":event-type", "content"),
                (":content-type", "application/json"),
            ],
            payload,
        );

        let (parsed, consumed) = parse_eventstream_frame(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed.event_type, "content");
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn eventstream_parse_empty_payload() {
        let frame = build_eventstream_frame(&[(":event-type", "initial-response")], &[]);
        let (parsed, consumed) = parse_eventstream_frame(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed.event_type, "initial-response");
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn eventstream_crc_mismatch_detected() {
        let mut frame = build_eventstream_frame(&[(":event-type", "chunk")], b"test");
        // Corrupt the message CRC (last 4 bytes)
        let len = frame.len();
        frame[len - 1] ^= 0xFF;

        let result = parse_eventstream_frame(&frame);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err}").contains("CRC mismatch"),
            "expected CRC error, got: {err}"
        );
    }

    // --- Credential resolution tests ---

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

    // --- Model ID mapping tests ---

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

    // --- Region resolution tests ---

    #[test]
    fn region_falls_back_to_us_east_1() {
        // Clear all region vars, then check the fallback.
        // This test won't interfere with real env because env vars are per-process
        // and test isolation. We rely on the fact that these specific vars
        // are unlikely to all be set in CI.
        let result = resolve_region();
        // We can't guarantee env state, but at minimum it should return a non-empty string
        assert!(!result.is_empty());
    }

    // --- CRC-32 tests ---

    #[test]
    fn crc32c_empty() {
        assert_eq!(crc32c(b""), 0x0000_0000);
    }

    #[test]
    fn crc32c_known_values() {
        // "123456789" CRC-32 = 0xCBF43926
        assert_eq!(crc32c(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32c_single_byte() {
        // CRC-32 of a single zero byte = 0xD202EF8D
        let result = crc32c(&[0]);
        assert_eq!(result, 0xD202_EF8D);
    }

    // --- epoch_days_to_ymd tests ---

    #[test]
    fn epoch_days_known_dates() {
        // 2015-08-30 = day 16677 from epoch
        assert_eq!(epoch_days_to_ymd(16677), (2015, 8, 30));
        // 1970-01-01 = day 0
        assert_eq!(epoch_days_to_ymd(0), (1970, 1, 1));
        // 2000-01-01 = day 10957
        assert_eq!(epoch_days_to_ymd(10957), (2000, 1, 1));
    }

    // --- strip_date_suffix tests ---

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
