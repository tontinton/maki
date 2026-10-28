use std::env;
use std::error::Error;
use std::fmt::Display;
use std::future::Future;
use std::iter;
use std::ops::ControlFlow;
use std::time::{Duration, SystemTime};

use aws_config::Region;
use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_config::environment::EnvironmentVariableRegionProvider;
use aws_config::meta::region::RegionProviderChain;
use aws_config::profile::ProfileFileRegionProvider;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_sigv4::http_request::{self as sigv4, SignableBody, SignableRequest, SigningSettings};
use aws_sigv4::sign::v4;
use base64::Engine;
use flume::Sender;
use maki_storage::id::SessionRef;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Request};
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::shared;

pub(crate) const BEARER_TOKEN_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";
const SKIP_AUTH_ENV: &str = "CLAUDE_CODE_SKIP_BEDROCK_AUTH";
const BASE_URL_ENV: &str = "ANTHROPIC_BEDROCK_BASE_URL";
const BEDROCK_API_VERSION: &str = "bedrock-2023-05-31";
const MIN_EVENTSTREAM_FRAME: usize = 16;
const DEFAULT_REGION: &str = "us-east-1";
const SIGNING_NAME: &str = "bedrock";
const CONTENT_TYPE: &str = "content-type";
const JSON: &str = "application/json";
const FORBIDDEN: u16 = 403;
/// Temporary credentials are replaced this long before they expire, so a
/// request signed with them cannot outlive them. Credentials that live less
/// than twice as long are replaced halfway instead.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Claude Code's limit, for a `credential_process` waiting on input it never
/// gets.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(60);
const NO_CREDENTIALS_HINT: &str = "set AWS_PROFILE (after `aws sso login` for SSO), AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or AWS_BEARER_TOKEN_BEDROCK";

fn io_error(kind: std::io::ErrorKind, e: impl Into<Box<dyn Error + Send + Sync>>) -> AgentError {
    AgentError::Io(std::io::Error::new(kind, e))
}

enum Auth {
    /// `AWS_BEARER_TOKEN_BEDROCK`, a Bedrock API key.
    ApiKey(String),
    /// `CLAUDE_CODE_SKIP_BEDROCK_AUTH=1`: the gateway at the base URL signs.
    Gateway,
    /// SigV4 with credentials from the AWS SDK's default chain.
    SigV4(CredentialCache),
}

impl Auth {
    fn name(&self) -> &'static str {
        match self {
            Self::ApiKey(_) => "api key",
            Self::Gateway => "gateway",
            Self::SigV4(_) => "sigv4",
        }
    }
}

/// An empty key counts as unset, as it does for Claude Code.
fn choose_auth(env_key: Option<String>, skip_auth: bool) -> Auth {
    match (env_key.filter(|key| !key.is_empty()), skip_auth) {
        (Some(key), _) => Auth::ApiKey(key),
        (None, true) => Auth::Gateway,
        (None, false) => Auth::SigV4(CredentialCache::default()),
    }
}

pub(crate) fn is_enabled() -> bool {
    env::var("CLAUDE_CODE_USE_BEDROCK").is_ok_and(|v| v == "1")
}

fn resolve_bedrock_top_p() -> Option<f64> {
    // Bedrock is built from the anthropic branch, so it honours the anthropic
    // slug's `top_p`; there is no `bedrock` section to read.
    maki_config::providers::top_p_for(super::SLUG)
}

fn auth_from_env() -> Auth {
    let auth = choose_auth(
        env::var(BEARER_TOKEN_ENV).ok(),
        env::var(SKIP_AUTH_ENV).is_ok_and(|v| v == "1"),
    );
    debug!(auth = auth.name(), "Bedrock auth");
    auth
}

#[derive(Default)]
struct CredentialCache {
    /// Held across a lookup, so requests that arrive meanwhile wait for its
    /// result, and a forget waits for it instead of being overwritten by it.
    state: Mutex<Cached>,
}

#[derive(Default)]
enum Cached {
    #[default]
    Empty,
    Fresh {
        credentials: Credentials,
        /// `None` for credentials that do not expire.
        refresh_at: Option<SystemTime>,
    },
    /// The chain found nothing. Kept until the next re-auth, so each request
    /// does not wait out the instance metadata lookup again.
    Missing(String),
}

impl CredentialCache {
    async fn get<F, Fut>(&self, now: SystemTime, lookup: F) -> Result<Credentials, AgentError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Credentials, CredentialsError>>,
    {
        let mut cached = self.state.lock().await;
        match &*cached {
            Cached::Fresh {
                credentials,
                refresh_at,
            } if refresh_at.is_none_or(|at| now < at) => return Ok(credentials.clone()),
            Cached::Missing(message) => return Err(no_credentials(message)),
            Cached::Fresh { .. } | Cached::Empty => {}
        }
        let error = match lookup().await {
            Ok(credentials) => {
                debug!(expiry = ?credentials.expiry(), "resolved AWS credentials for Bedrock");
                *cached = Cached::Fresh {
                    refresh_at: refresh_at(&credentials, now),
                    credentials: credentials.clone(),
                };
                return Ok(credentials);
            }
            Err(error) => error,
        };
        let message = error_chain(&error);
        if let Cached::Fresh { credentials, .. } = &*cached
            && !expired(credentials, now)
        {
            warn!(error = %message, "refreshing AWS credentials failed, keeping the current ones");
            return Ok(credentials.clone());
        }
        if matches!(error, CredentialsError::CredentialsNotLoaded(_)) {
            *cached = Cached::Missing(message.clone());
        }
        Err(no_credentials(&message))
    }

    async fn forget(&self) {
        *self.state.lock().await = Cached::Empty;
    }
}

fn refresh_at(credentials: &Credentials, now: SystemTime) -> Option<SystemTime> {
    let expiry = credentials.expiry()?;
    let lifetime = expiry.duration_since(now).unwrap_or_default();
    Some(expiry - REFRESH_MARGIN.min(lifetime / 2))
}

fn expired(credentials: &Credentials, now: SystemTime) -> bool {
    credentials.expiry().is_some_and(|expiry| expiry <= now)
}

fn no_credentials(message: &str) -> AgentError {
    AgentError::Config {
        message: format!("no AWS credentials for Bedrock ({message}): {NO_CREDENTIALS_HINT}"),
    }
}

/// Env keys, profiles (static keys, SSO, `credential_process`, assumed roles,
/// `aws login`), web identity, container and instance credentials. The chain
/// is built per lookup, so its profile files are read fresh.
async fn lookup_credentials(region: Region) -> Result<Credentials, CredentialsError> {
    let lookup = async {
        let chain = DefaultCredentialsChain::builder()
            .region(region)
            .build()
            .await;
        chain.provide_credentials().await
    };
    timeout(LOOKUP_TIMEOUT, lookup)
        .await
        .unwrap_or_else(|_| Err(CredentialsError::provider_timed_out(LOOKUP_TIMEOUT)))
}

fn invoke_request(client: &Client, url: &str) -> Result<Request, AgentError> {
    let request = client
        .post(url)
        .header("user-agent", super::super::user_agent())
        .header(CONTENT_TYPE, JSON)
        .build()?;
    Ok(request)
}

fn set_header(request: &mut Request, name: &str, value: &str) -> Result<(), AgentError> {
    let name = HeaderName::try_from(name).map_err(signing_error)?;
    let value = HeaderValue::try_from(value).map_err(signing_error)?;
    request.headers_mut().insert(name, value);
    Ok(())
}

/// Signs the method, URL and headers `request` already holds, so the signature
/// covers what goes out. `content-type` ends up signed next to `host` and the
/// date, `user-agent` is left out because proxies rewrite it.
fn sign_request(
    request: &mut Request,
    credentials: Credentials,
    region: &Region,
    body: &[u8],
    time: SystemTime,
) -> Result<(), AgentError> {
    let identity = credentials.into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region.as_ref())
        .name(SIGNING_NAME)
        .time(time)
        .settings(SigningSettings::default())
        .build()
        .map_err(signing_error)?
        .into();
    let headers = request
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.url().as_str(),
        headers,
        SignableBody::Bytes(body),
    )
    .map_err(signing_error)?;
    let (instructions, _) = sigv4::sign(signable, &params)
        .map_err(signing_error)?
        .into_parts();
    instructions
        .headers()
        .try_for_each(|(name, value)| set_header(request, name, value))
}

fn signing_error(e: impl Display) -> AgentError {
    AgentError::Config {
        message: format!("signing the Bedrock request: {e}"),
    }
}

/// The SDK's errors keep the useful part (an expired SSO session, a missing
/// profile) in their sources.
fn error_chain(error: &(dyn Error + 'static)) -> String {
    iter::successors(Some(error), |&e| e.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

fn decode_eventstream_frame(buf: &[u8]) -> Result<(usize, Option<Vec<u8>>), AgentError> {
    if buf.len() < MIN_EVENTSTREAM_FRAME {
        return Err(io_error(
            std::io::ErrorKind::UnexpectedEof,
            "eventstream frame too short",
        ));
    }

    let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;

    if buf.len() < total_len {
        return Err(io_error(
            std::io::ErrorKind::UnexpectedEof,
            "incomplete eventstream frame",
        ));
    }

    let prelude_size = 12; // total_len(4) + headers_len(4) + prelude_crc(4)
    let message_crc_size = 4;
    let headers_end = prelude_size + headers_len;
    let payload_end = total_len - message_crc_size;

    let headers_bytes = &buf[prelude_size..headers_end];
    let payload_bytes = &buf[headers_end..payload_end];

    let mut event_type = None;
    let mut exception_type = None;
    let mut pos = 0;

    while pos < headers_bytes.len() {
        let name_len = headers_bytes[pos] as usize;
        pos += 1;
        if pos + name_len > headers_bytes.len() {
            break;
        }
        let name = std::str::from_utf8(&headers_bytes[pos..pos + name_len]).unwrap_or("");
        pos += name_len;

        if pos >= headers_bytes.len() {
            break;
        }
        let header_type = headers_bytes[pos];
        pos += 1;

        if header_type == 7 {
            if pos + 2 > headers_bytes.len() {
                break;
            }
            let value_len =
                u16::from_be_bytes([headers_bytes[pos], headers_bytes[pos + 1]]) as usize;
            pos += 2;
            if pos + value_len > headers_bytes.len() {
                break;
            }
            let value = std::str::from_utf8(&headers_bytes[pos..pos + value_len]).unwrap_or("");
            pos += value_len;

            match name {
                ":event-type" => event_type = Some(value.to_string()),
                ":exception-type" => exception_type = Some(value.to_string()),
                _ => {}
            }
        } else {
            break;
        }
    }

    if let Some(exc) = exception_type {
        let message = std::str::from_utf8(payload_bytes)
            .unwrap_or("unknown error")
            .to_string();
        let status = match exc.as_str() {
            "ThrottlingException" => 429,
            "ValidationException" => 400,
            _ => 500,
        };
        return Err(AgentError::api(status, message));
    }

    let payload = if event_type.as_deref() == Some("chunk") && !payload_bytes.is_empty() {
        Some(payload_bytes.to_vec())
    } else {
        None
    };

    Ok((total_len, payload))
}

pub(crate) struct Bedrock {
    client: Client,
    auth: Auth,
    base_url: Option<String>,
    top_p: Option<f64>,
    /// Resolved on the first request, so building the provider reads nothing.
    region: OnceCell<Region>,
}

impl Bedrock {
    pub fn new(timeouts: super::super::Timeouts) -> Self {
        Self {
            client: super::super::http_client(timeouts),
            auth: auth_from_env(),
            base_url: env::var(BASE_URL_ENV).ok(),
            top_p: resolve_bedrock_top_p(),
            region: OnceCell::new(),
        }
    }

    async fn authorize(
        &self,
        request: &mut Request,
        body: &[u8],
        region: &Region,
    ) -> Result<(), AgentError> {
        match &self.auth {
            Auth::ApiKey(key) => set_header(
                request,
                super::super::AUTHORIZATION_HEADER,
                &super::super::bearer_value(key),
            ),
            Auth::Gateway => Ok(()),
            Auth::SigV4(cache) => {
                let now = SystemTime::now();
                let credentials = cache
                    .get(now, || lookup_credentials(region.clone()))
                    .await?;
                sign_request(request, credentials, region, body, now)
            }
        }
    }

    /// The next request asks the credential chain again, which picks up a new
    /// `aws sso login` or rotated keys. The region stays until a restart.
    async fn forget_credentials(&self) {
        if let Auth::SigV4(cache) = &self.auth {
            cache.forget().await;
        }
    }

    /// Claude Code's order: `AWS_REGION`, `AWS_DEFAULT_REGION`, the profile's
    /// region, then `us-east-1`.
    async fn region(&self) -> &Region {
        self.region
            .get_or_init(|| async {
                let chain =
                    RegionProviderChain::first_try(EnvironmentVariableRegionProvider::new())
                        .or_else(ProfileFileRegionProvider::new());
                let region = chain
                    .region()
                    .await
                    .unwrap_or_else(|| Region::from_static(DEFAULT_REGION));
                debug!(%region, "resolved Bedrock region");
                region
            })
            .await
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
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let region = self.region().await;
            let requested_id = env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| model.id.clone());
            let long_context = requested_id.ends_with(shared::LONG_CONTEXT_SUFFIX);
            let model_id = shared::strip_long_context(&requested_id).to_string();

            let mut body = shared::build_request_body_with_system(
                model,
                messages,
                &[shared::SystemBlock {
                    r#type: "text",
                    text: system,
                    cache_control: Some(shared::EPHEMERAL),
                }],
                tools,
                opts.thinking,
                self.top_p,
            );
            // Fast mode lives only on the direct API, so Bedrock skips `opts.fast`
            // and never sends the `speed` param.
            body["anthropic_version"] = json!(BEDROCK_API_VERSION);
            let has_examples = tools
                .as_array()
                .is_some_and(|arr| arr.iter().any(|t| t.get("input_examples").is_some()));
            let mut betas = Vec::new();
            if has_examples {
                betas.push(shared::BETA_TOOL_EXAMPLES_BEDROCK);
            }
            if shared::has_deferred_tools(tools) {
                betas.push(shared::BETA_DEFERRED_TOOLS_BEDROCK);
            }
            if long_context {
                betas.push(shared::LONG_CONTEXT_BETA);
            }
            if !betas.is_empty() {
                body["anthropic_beta"] = json!(betas);
            }

            let encoded_model = super::super::urlenc(&model_id);
            let url = match &self.base_url {
                Some(base) => format!("{base}/model/{encoded_model}/invoke-with-response-stream"),
                None => format!(
                    "https://bedrock-runtime.{region}.amazonaws.com/model/{encoded_model}/invoke-with-response-stream"
                ),
            };

            let json_body = serde_json::to_vec(&body)?;
            let mut request = invoke_request(&self.client, &url)?;
            self.authorize(&mut request, &json_body, region).await?;
            *request.body_mut() = Some(json_body.into());

            debug!(model = %model_id, %region, "sending Bedrock request");

            let mut response = self.client.execute(request).await?;
            let status = response.status().as_u16();
            if status != 200 {
                // SigV4 rejections are 403s, which never reach `refresh_auth`.
                if status == FORBIDDEN {
                    self.forget_credentials().await;
                }
                return Err(AgentError::from_response(response).await);
            }

            let mut parser = shared::EventParser::new();
            let mut frame_buf = Vec::new();

            while let Some(chunk) = response.chunk().await? {
                frame_buf.extend_from_slice(&chunk);

                while frame_buf.len() >= MIN_EVENTSTREAM_FRAME {
                    let peek_total = u32::from_be_bytes([
                        frame_buf[0],
                        frame_buf[1],
                        frame_buf[2],
                        frame_buf[3],
                    ]) as usize;
                    if frame_buf.len() < peek_total {
                        break;
                    }

                    let (consumed, payload) = decode_eventstream_frame(&frame_buf)?;
                    frame_buf.drain(..consumed);

                    let Some(payload) = payload else {
                        continue;
                    };

                    let (event_type, json) = decode_event_payload(&payload)?;

                    if let ControlFlow::Break(()) =
                        parser.process(&event_type, &json, event_tx).await?
                    {
                        return Ok(parser.finish());
                    }
                }
            }

            Ok(parser.finish())
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async {
            let models: Vec<crate::model::ModelInfo> = super::SPEC
                .models()
                .iter()
                .map(|entry| crate::model::ModelInfo::id_only(entry.prefixes[0].to_string()))
                .collect();
            Ok(models)
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.forget_credentials().await;
            Ok(())
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.refresh_auth()
    }
}

fn decode_event_payload(payload: &[u8]) -> Result<(String, String), AgentError> {
    let json_str =
        std::str::from_utf8(payload).map_err(|e| io_error(std::io::ErrorKind::InvalidData, e))?;
    let outer: Value = serde_json::from_str(json_str)?;

    let (parsed, json) = if let Some(b64) = outer.get("bytes").and_then(|b| b.as_str()) {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| io_error(std::io::ErrorKind::InvalidData, e))?;
        let json =
            String::from_utf8(decoded).map_err(|e| io_error(std::io::ErrorKind::InvalidData, e))?;
        let parsed: Value = serde_json::from_str(&json)?;
        (parsed, json)
    } else {
        (outer, json_str.to_string())
    };

    let event_type = parsed
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    Ok((event_type, json))
}

#[cfg(test)]
mod tests {
    use std::future::{Ready, ready};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::UNIX_EPOCH;

    use reqwest::header::HeaderMap;

    use super::*;
    use crate::{ContentBlock, ProviderEvent};
    use test_case::test_case;

    const ACCESS_KEY: &str = "AKID";
    const OTHER_ACCESS_KEY: &str = "AKID2";
    const SECRET_KEY: &str = "SECRET";
    const API_KEY: &str = "bedrock-api-key";
    const REGION: &str = "us-east-1";
    const CHAIN_FAILED: &str = "sso endpoint unreachable";
    /// 2024-01-01T00:00:00Z, when the signature below was pinned.
    const SIGNED_AT: Duration = Duration::from_secs(1_704_067_200);

    fn credentials(access_key: &str, expiry: Option<SystemTime>) -> Credentials {
        Credentials::new(access_key, SECRET_KEY, None, expiry, "test")
    }

    fn signed_headers(url: &str, session_token: Option<&str>) -> HeaderMap {
        let credentials = Credentials::new(
            ACCESS_KEY,
            SECRET_KEY,
            session_token.map(String::from),
            None,
            "test",
        );
        let mut request = invoke_request(&Client::new(), url).unwrap();
        sign_request(
            &mut request,
            credentials,
            &Region::from_static(REGION),
            b"{}",
            UNIX_EPOCH + SIGNED_AT,
        )
        .unwrap();
        request.headers().clone()
    }

    #[test]
    fn sigv4_signing_encodes_path_segments() {
        // The URL below has '%3A' from a real Bedrock model ID. The pinned signature
        // was computed against the doubly encoded '%253A' canonical path, so this
        // test fails the moment someone stops re-encoding the path.
        let url = "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke-with-response-stream";
        let headers = signed_headers(url, None);
        let auth = headers[super::super::super::AUTHORIZATION_HEADER]
            .to_str()
            .unwrap();
        assert!(
            auth.contains(
                "Signature=ff69b69725fa6fa2de10b57605b9fca403f357980e082d138d1772abe10fbea2"
            ),
            "unexpected signature: {auth}"
        );
    }

    #[test_case(None ; "without_session_token")]
    #[test_case(Some("TOKEN") ; "with_session_token")]
    fn sigv4_signing(session_token: Option<&str>) {
        let headers = signed_headers(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/invoke",
            session_token,
        );
        let auth = headers[super::super::super::AUTHORIZATION_HEADER]
            .to_str()
            .unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKID/20240101/us-east-1/bedrock/aws4_request"
        ));
        let expected_signed = if session_token.is_some() {
            "SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"
        } else {
            "SignedHeaders=content-type;host;x-amz-date"
        };
        assert!(
            auth.contains(expected_signed),
            "Authorization missing expected SignedHeaders: {auth}"
        );
        assert_eq!(
            headers
                .get("x-amz-security-token")
                .map(|v| v.to_str().unwrap()),
            session_token
        );
    }

    #[test_case(Some(API_KEY), true, "api key" ; "key_beats_gateway")]
    #[test_case(Some(""), false, "sigv4" ; "empty_key_is_unset")]
    #[test_case(None, true, "gateway" ; "gateway")]
    #[test_case(None, false, "sigv4" ; "credential_chain")]
    fn auth_order(env_key: Option<&str>, skip_auth: bool, expected: &str) {
        assert_eq!(
            choose_auth(env_key.map(String::from), skip_auth).name(),
            expected
        );
    }

    #[test_case(None, None ; "static_credentials_never_refresh")]
    #[test_case(Some(3600), Some(3300) ; "five_minutes_early")]
    #[test_case(Some(240), Some(120) ; "short_lived_halfway")]
    fn refresh_at_cases(expires_in: Option<u64>, refresh_in: Option<u64>) {
        let now = SystemTime::now();
        let expiry = expires_in.map(|secs| now + Duration::from_secs(secs));
        assert_eq!(
            refresh_at(&credentials(ACCESS_KEY, expiry), now),
            refresh_in.map(|secs| now + Duration::from_secs(secs))
        );
    }

    fn answer(
        calls: &AtomicUsize,
        result: Result<Credentials, CredentialsError>,
    ) -> impl FnOnce() -> Ready<Result<Credentials, CredentialsError>> + '_ {
        move || {
            calls.fetch_add(1, Ordering::Relaxed);
            ready(result)
        }
    }

    fn access_key(result: Result<Credentials, AgentError>) -> String {
        result.unwrap().access_key_id().to_owned()
    }

    #[tokio::test]
    async fn cache_reuses_fresh_credentials() {
        let (cache, calls, now) = (
            CredentialCache::default(),
            AtomicUsize::new(0),
            SystemTime::now(),
        );
        for _ in 0..2 {
            let found = cache
                .get(now, answer(&calls, Ok(credentials(ACCESS_KEY, None))))
                .await;
            assert_eq!(access_key(found), ACCESS_KEY);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cache_refreshes_inside_the_margin() {
        let (cache, calls, now) = (
            CredentialCache::default(),
            AtomicUsize::new(0),
            SystemTime::now(),
        );
        let expiry = Some(now + Duration::from_secs(600));
        cache
            .get(now, answer(&calls, Ok(credentials(ACCESS_KEY, expiry))))
            .await
            .unwrap();
        let later = now + Duration::from_secs(420);
        let found = cache
            .get(
                later,
                answer(&calls, Ok(credentials(OTHER_ACCESS_KEY, None))),
            )
            .await;
        assert_eq!(access_key(found), OTHER_ACCESS_KEY);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_credentials_that_still_work() {
        let (cache, calls, now) = (
            CredentialCache::default(),
            AtomicUsize::new(0),
            SystemTime::now(),
        );
        let expiry = Some(now + Duration::from_secs(600));
        cache
            .get(now, answer(&calls, Ok(credentials(ACCESS_KEY, expiry))))
            .await
            .unwrap();
        let later = now + Duration::from_secs(420);
        let failed = Err(CredentialsError::provider_error(CHAIN_FAILED));
        let found = cache.get(later, answer(&calls, failed)).await;
        assert_eq!(access_key(found), ACCESS_KEY);
    }

    #[tokio::test]
    async fn missing_credentials_are_remembered_until_forgotten() {
        let (cache, calls, now) = (
            CredentialCache::default(),
            AtomicUsize::new(0),
            SystemTime::now(),
        );
        for _ in 0..2 {
            let missing = Err(CredentialsError::not_loaded_no_source());
            assert!(cache.get(now, answer(&calls, missing)).await.is_err());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        cache.forget().await;
        let found = cache
            .get(now, answer(&calls, Ok(credentials(ACCESS_KEY, None))))
            .await;
        assert_eq!(access_key(found), ACCESS_KEY);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn transient_failures_are_not_remembered() {
        let (cache, calls, now) = (
            CredentialCache::default(),
            AtomicUsize::new(0),
            SystemTime::now(),
        );
        let failed = Err(CredentialsError::provider_error(CHAIN_FAILED));
        let error = cache.get(now, answer(&calls, failed)).await.unwrap_err();
        assert!(error.to_string().contains(CHAIN_FAILED), "{error}");
        let found = cache
            .get(now, answer(&calls, Ok(credentials(ACCESS_KEY, None))))
            .await;
        assert_eq!(access_key(found), ACCESS_KEY);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    fn build_eventstream_frame(header_name: &str, header_value: &str, payload: &[u8]) -> Vec<u8> {
        let mut header_buf = Vec::new();
        header_buf.push(header_name.len() as u8);
        header_buf.extend_from_slice(header_name.as_bytes());
        header_buf.push(7);
        header_buf.extend_from_slice(&(header_value.len() as u16).to_be_bytes());
        header_buf.extend_from_slice(header_value.as_bytes());

        let headers_len = header_buf.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;

        let mut frame = Vec::new();
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&headers_len.to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes());
        frame.extend_from_slice(&header_buf);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&0u32.to_be_bytes());
        frame
    }

    #[test]
    fn decode_eventstream_chunk_returns_payload() {
        let payload = b"{\"type\":\"content_block_delta\"}";
        let frame = build_eventstream_frame(":event-type", "chunk", payload);
        let (consumed, data) = decode_eventstream_frame(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(data.unwrap(), payload);
    }

    #[test]
    fn decode_eventstream_metadata_returns_none() {
        let frame = build_eventstream_frame(":event-type", "initial-response", b"");
        let (consumed, data) = decode_eventstream_frame(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert!(data.is_none());
    }

    #[test_case("ValidationException", 400, "Access denied" ; "validation_exception")]
    #[test_case("ThrottlingException", 429, "Rate exceeded" ; "throttling_exception")]
    #[test_case("UnknownException", 500, "oops" ; "unknown_maps_to_500")]
    fn decode_eventstream_exception(exc_type: &str, expected_status: u16, msg: &str) {
        let frame = build_eventstream_frame(":exception-type", exc_type, msg.as_bytes());
        let err = decode_eventstream_frame(&frame).unwrap_err();
        match err {
            AgentError::Api {
                status, message, ..
            } => {
                assert_eq!(status, expected_status);
                assert_eq!(message, msg);
            }
            other => panic!("expected Api error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_parser_text_stream() {
        let (tx, rx) = flume::unbounded();
        let mut parser = shared::EventParser::new();

        let steps: &[(&str, &str)] = &[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
        ];
        for (event, data) in steps {
            assert!(
                parser
                    .process(event, data, &tx)
                    .await
                    .unwrap()
                    .is_continue()
            );
        }
        assert!(
            parser
                .process("message_stop", r#"{"type":"message_stop"}"#, &tx)
                .await
                .unwrap()
                .is_break()
        );

        let resp = parser.finish();
        assert_eq!(resp.usage.input, 10);
        assert!(matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello"));
        assert!(
            rx.drain()
                .any(|e| matches!(e, ProviderEvent::TextDelta { text } if text == "Hello"))
        );
    }
}
