use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use futures_lite::io::AsyncBufRead;
use isahc::config::{Configurable, VersionNegotiation};
use isahc::http::request::Builder;
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use maki_storage::StateDir;
use maki_storage::auth::{OAuthTokens, load_tokens, lock_tokens, save_tokens};

use crate::AgentError;

pub(crate) mod anthropic;
pub(crate) mod aperture;
pub(crate) mod catalog;
pub(crate) mod copilot;
pub mod custom;
pub(crate) mod deepseek;
pub mod dynamic;
pub(crate) mod google;
pub(crate) mod llama_cpp;
pub(crate) mod local;
pub(crate) mod mistral;
pub(crate) mod ollama;
pub(crate) mod openai;
pub(crate) mod openai_compat;
pub mod opencode;
pub(crate) mod openrouter;
pub(crate) mod regolo;
pub(crate) mod synthetic;
pub(crate) mod tensorx;
pub(crate) mod xai;
pub(crate) mod zai;

const LOW_SPEED_BYTES_PER_SEC: u32 = 1;
const UNMAPPED_SSE_ERROR_STATUS: u16 = 400;
const UNAUTHORIZED_STATUS: u16 = 401;
const AUTHORIZATION_HEADER: &str = "authorization";

fn bearer_value(api_key: &str) -> String {
    format!("Bearer {api_key}")
}

pub(crate) fn user_agent() -> &'static str {
    concat!(
        "maki/v",
        env!("CARGO_PKG_VERSION"),
        "-g",
        env!("GIT_SHORT_HASH")
    )
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub stream: Duration,
    pub low_speed: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            stream: Duration::from_secs(300),
            low_speed: Duration::from_secs(30),
        }
    }
}

/// Reading, refreshing and writing tokens has to happen as one turn. Whoever
/// queued behind a peer here is holding a copy the peer already spent, and
/// replaying a rotated refresh token gets the whole family revoked, so the
/// tokens are loaded again once the lock is in hand.
pub(crate) fn refreshed_tokens(
    dir: &StateDir,
    provider: &str,
    refresh: impl FnOnce(&OAuthTokens) -> Result<OAuthTokens, AgentError>,
) -> Result<OAuthTokens, AgentError> {
    let _lock = lock_tokens(dir, provider);
    let current = load_tokens(dir, provider).ok_or_else(|| AgentError::Api {
        status: UNAUTHORIZED_STATUS,
        message: format!("{provider} OAuth tokens not found on disk"),
    })?;
    if !current.is_expired() {
        return Ok(current);
    }
    let fresh = refresh(&current)?;
    save_tokens(dir, provider, &fresh)?;
    Ok(fresh)
}

#[derive(Clone)]
pub struct ResolvedAuth {
    pub base_url: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Header names that came from `[<slug>.headers]`. They win over anything
    /// the provider sets afterwards, so a key rotation cannot drop a gateway
    /// credential that replaced the built-in auth header.
    config_headers: Vec<String>,
}

impl ResolvedAuth {
    /// The only way to build auth, so every provider picks up
    /// `[<slug>.headers]` from `providers.toml`. Skipping it would silently
    /// ignore the user's config, which is why there is no slug-less
    /// constructor outside of tests.
    pub fn new(slug: &str, headers: Vec<(String, String)>) -> Result<Self, AgentError> {
        let mut auth = Self {
            base_url: None,
            headers,
            config_headers: Vec::new(),
        };
        if let Some(def) = maki_config::providers::ProvidersConfig::load().get(slug) {
            auth.apply_config_headers(slug, &def.headers)?;
        }
        Ok(auth)
    }

    /// Fold `[<slug>.headers]` in, expanding `${VAR}` from the environment. An
    /// unset or empty variable fails the whole provider (see
    /// `maki_config::expand_env`), matching the MCP path.
    fn apply_config_headers(
        &mut self,
        slug: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<(), AgentError> {
        for (name, value) in headers {
            let expanded = maki_config::expand_env(value).map_err(|var| {
                AgentError::Config {
                    message: format!(
                        "provider '{slug}' header '{name}': environment variable '{var}' is unset or empty"
                    ),
                }
            })?;
            self.set_header(name, expanded);
            self.config_headers.push(name.clone());
        }
        Ok(())
    }

    pub fn bearer(slug: &str, api_key: &str) -> Result<Self, AgentError> {
        Self::new(
            slug,
            vec![(AUTHORIZATION_HEADER.into(), bearer_value(api_key))],
        )
    }

    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.base_url = base_url;
        self
    }

    /// Set the header carrying the API key, unless `[<slug>.headers]` already
    /// owns that name: the config value is the one the gateway expects.
    fn set_key_header(&mut self, name: &str, value: String) {
        if self
            .config_headers
            .iter()
            .any(|configured| configured.eq_ignore_ascii_case(name))
        {
            return;
        }
        self.set_header(name, value);
    }

    /// Replace a same-name header (case-insensitive) instead of appending a
    /// second one: `Builder::header` appends, so a configured `Authorization`
    /// next to the built-in bearer would send two credentials.
    pub(crate) fn set_header(&mut self, name: &str, value: String) {
        match self
            .headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.1 = value,
            None => self.headers.push((name.to_string(), value)),
        }
    }

    /// Apply all auth headers to an HTTP request builder.
    pub fn configure_request(&self, builder: Builder) -> Builder {
        self.headers.iter().fold(builder, |b, (key, value)| {
            b.header(key.as_str(), value.as_str())
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: Option<String>, headers: Vec<(String, String)>) -> Self {
        Self {
            base_url,
            headers,
            config_headers: Vec::new(),
        }
    }
}

pub(crate) fn with_prefix<'a>(
    prefix: &Option<String>,
    system: &'a str,
    buf: &'a mut String,
) -> &'a str {
    match prefix {
        Some(p) => {
            *buf = format!("{p}\n\n{system}");
            buf
        }
        None => system,
    }
}

pub(crate) fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[derive(Deserialize)]
pub(crate) struct SseErrorPayload {
    pub error: SseErrorDetail,
}

#[derive(Deserialize)]
pub(crate) struct SseErrorDetail {
    #[serde(default)]
    pub r#type: String,
    /// Providers send this as a string, a number or `null`, and rejecting any one of those shapes
    /// throws away the whole error.
    #[serde(default)]
    pub code: Value,
    pub message: String,
}

/// A streamed error rides inside a plain 200 response, so this tag is the only clue we get about
/// what went wrong and whether waiting will help.
pub(crate) fn sse_error_status(tag: &str) -> Option<u16> {
    Some(match tag {
        "overloaded_error" | "server_is_overloaded" => 529,
        "service_unavailable_error" => 503,
        "api_error" | "server_error" => 500,
        "rate_limit_error" | "rate_limit_exceeded" | "tokens" => 429,
        "request_too_large" => 413,
        "not_found_error" => 404,
        "permission_error" => 403,
        "billing_error" | "insufficient_quota" => 402,
        "authentication_error" | "invalid_api_key" => 401,
        _ => return None,
    })
}

impl SseErrorPayload {
    pub fn into_agent_error(self) -> AgentError {
        let status = sse_error_status(self.error.code.as_str().unwrap_or_default())
            .or_else(|| sse_error_status(&self.error.r#type))
            .unwrap_or(UNMAPPED_SSE_ERROR_STATUS);
        AgentError::Api {
            status,
            message: self.error.message,
        }
    }
}

pub(crate) async fn next_sse_line<R: AsyncBufRead + Unpin>(
    lines: &mut futures_lite::io::Lines<R>,
    deadline: &mut Instant,
    stream_timeout: Duration,
) -> Result<Option<String>, AgentError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = futures_lite::future::or(
        async { lines.next().await.transpose().map_err(AgentError::from) },
        async {
            smol::Timer::after(remaining).await;
            Err(AgentError::Timeout {
                secs: stream_timeout.as_secs(),
            })
        },
    )
    .await;
    if let Ok(Some(_)) = &result {
        *deadline = Instant::now() + stream_timeout;
    }
    result
}

pub(crate) fn http_client(timeouts: Timeouts) -> isahc::HttpClient {
    isahc::HttpClient::builder()
        .connect_timeout(timeouts.connect)
        .low_speed_timeout(LOW_SPEED_BYTES_PER_SEC, timeouts.low_speed)
        // The workspace enables curl's http2 feature for OTLP over gRPC, which
        // would otherwise flip provider streaming to h2 over TLS. Streaming is
        // tuned for HTTP/1.1, so pin it.
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .expect("failed to build HTTP client")
}

#[derive(Clone, Debug)]
pub struct KeyPool {
    keys: Arc<Vec<String>>,
    index: Arc<AtomicUsize>,
}

impl KeyPool {
    pub fn from_env(env_var: &str) -> Result<Self, AgentError> {
        let raw = std::env::var(env_var).map_err(|_| AgentError::Config {
            message: format!("{env_var} not set"),
        })?;
        let keys: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if keys.is_empty() {
            return Err(AgentError::Config {
                message: format!("{env_var} is empty"),
            });
        }
        Ok(Self {
            keys: Arc::new(keys),
            index: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn resolve(slug: &str, env_var: &str) -> Result<Self, AgentError> {
        if let Ok(pool) = Self::from_env(env_var) {
            debug!(slug, keys = pool.len(), "resolved API key from env");
            return Ok(pool);
        }
        if let Some(key) = Self::key_from_file(slug) {
            debug!(slug, "resolved API key from saved credentials");
            return Ok(Self::from_keys(vec![key]));
        }
        if let Some(key) = Self::key_from_config(slug) {
            debug!(slug, "resolved API key from providers.toml");
            return Ok(Self::from_keys(vec![key]));
        }
        Err(AgentError::Config {
            message: format!(
                "{env_var} not set and no saved credentials for '{slug}' — run `maki auth login {slug}`"
            ),
        })
    }

    fn key_from_file(slug: &str) -> Option<String> {
        let dir = maki_storage::StateDir::resolve().ok()?;
        maki_storage::auth::load_provider_credentials(&dir, slug).map(|c| c.api_key)
    }

    fn key_from_config(slug: &str) -> Option<String> {
        maki_config::providers::ProvidersConfig::load()
            .get(slug)
            .and_then(|d| d.api_key.clone())
    }

    pub(crate) fn from_keys(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys),
            index: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn current(&self) -> &str {
        &self.keys[self.index.load(Ordering::Relaxed) % self.keys.len()]
    }

    pub fn rotate(&self) -> bool {
        if self.keys.len() <= 1 {
            return false;
        }
        self.index.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Rotate to the next key and refresh only the header carrying it, so the
    /// resolved `base_url` and any `[<slug>.headers]` survive the rotation.
    pub fn rotate_key_header(
        &self,
        auth: &Mutex<ResolvedAuth>,
        name: &str,
        build: impl FnOnce(&str) -> String,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        auth.lock()
            .unwrap()
            .set_key_header(name, build(self.current()));
        true
    }

    pub fn rotate_bearer(&self, auth: &Mutex<ResolvedAuth>) -> bool {
        self.rotate_key_header(auth, AUTHORIZATION_HEADER, bearer_value)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::AsyncBufReadExt;
    use test_case::test_case;

    const ERROR_MESSAGE: &str = "Our servers are currently overloaded. Please try again later.";
    const PARSE_FAILED: &str = "SSE error payload should deserialize";

    // Codex only admits the overload in `code`, and anything we cannot place has to stay a plain
    // 400 so a user mistake is not retried forever: https://github.com/tontinton/maki/issues/777
    #[test_case(r#""type":"service_unavailable_error","code":"server_is_overloaded""#, 529, true  ; "code_beats_type")]
    #[test_case(r#""type":"service_unavailable_error""#,                               503, true  ; "absent_code")]
    #[test_case(r#""type":"service_unavailable_error","code":null"#,                   503, true  ; "null_code")]
    #[test_case(r#""type":"rate_limit_error","code":429"#,                             429, true  ; "numeric_code")]
    #[test_case(r#""type":"invalid_request_error","code":"invalid_value""#,            400, false ; "unknown_tags")]
    fn sse_error_payload_status(tags: &str, status: u16, retryable: bool) {
        let payload: SseErrorPayload = serde_json::from_str(&format!(
            r#"{{"error":{{{tags},"message":"{ERROR_MESSAGE}"}}}}"#
        ))
        .expect(PARSE_FAILED);
        let err = payload.into_agent_error();

        assert_eq!(
            err.to_string(),
            format!("API error ({status}): {ERROR_MESSAGE}")
        );
        assert_eq!(err.is_retryable(), retryable);
    }

    #[test_case("a b", "a%20b" ; "space")]
    #[test_case("a:b", "a%3Ab" ; "colon")]
    #[test_case("abc", "abc"   ; "passthrough")]
    fn urlenc_encodes(input: &str, expected: &str) {
        assert_eq!(urlenc(input), expected);
    }

    struct NeverReader;

    impl futures_lite::io::AsyncRead for NeverReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
    }

    impl futures_lite::io::AsyncBufRead for NeverReader {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            std::task::Poll::Pending
        }

        fn consume(self: std::pin::Pin<&mut Self>, _amt: usize) {}
    }

    #[test]
    fn next_sse_line_expired_deadline_returns_timeout() {
        smol::block_on(async {
            let mut lines = NeverReader.lines();
            let mut past = Instant::now() - Duration::from_secs(1);
            let stream_timeout = Duration::from_secs(300);
            let err = next_sse_line(&mut lines, &mut past, stream_timeout)
                .await
                .unwrap_err();
            assert!(matches!(err, AgentError::Timeout { .. }));
        })
    }

    #[test]
    fn key_pool_single_key_current() {
        let pool = KeyPool::from_keys(vec!["sk-1".into()]);
        assert_eq!(pool.current(), "sk-1");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn key_pool_single_key_rotate_returns_false() {
        let pool = KeyPool::from_keys(vec!["sk-1".into()]);
        assert!(!pool.rotate());
        assert_eq!(pool.current(), "sk-1");
    }

    #[test]
    fn key_pool_multi_key_rotates() {
        let pool = KeyPool::from_keys(vec!["sk-1".into(), "sk-2".into(), "sk-3".into()]);
        assert_eq!(pool.current(), "sk-1");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-2");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-3");
    }

    #[test]
    fn key_pool_wraps_around() {
        let pool = KeyPool::from_keys(vec!["a".into(), "b".into()]);
        pool.rotate();
        pool.rotate();
        assert_eq!(pool.current(), "a");
    }

    #[test]
    fn resolve_from_env() {
        let env_var = format!("MAKI_TEST_KEY_{}", fastrand::u32(..));
        unsafe { std::env::set_var(&env_var, "from-env") };
        let pool = KeyPool::resolve("test_slug", &env_var).unwrap();
        unsafe { std::env::remove_var(&env_var) };
        assert_eq!(pool.current(), "from-env");
    }

    #[test]
    fn resolve_env_supports_comma_separated() {
        let env_var = format!("MAKI_TEST_MULTI_{}", fastrand::u32(..));
        unsafe { std::env::set_var(&env_var, "sk-1, sk-2, sk-3") };
        let pool = KeyPool::resolve("test_slug", &env_var).unwrap();
        unsafe { std::env::remove_var(&env_var) };
        assert_eq!(pool.current(), "sk-1");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-2");
    }

    #[test]
    fn resolve_returns_error_when_nothing_found() {
        let slug = format!("test_resolve_none_{}", fastrand::u32(..));
        let env_var = format!("MAKI_TEST_KEY_NONE_{}", fastrand::u32(..));
        let result = KeyPool::resolve(&slug, &env_var);
        assert!(result.is_err());
        let msg = format!("{result:?}");
        assert!(msg.contains(&env_var) || msg.contains(&slug));
    }

    const TEST_SLUG: &str = "gateway";
    const GATEWAY_HEADER: &str = "CF-Access-Client-Id";
    const GATEWAY_ID: &str = "client-id";
    const GATEWAY_URL: &str = "https://gw.internal/v1";
    const GATEWAY_CRED: &str = "Basic gateway-cred";

    fn config_headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn header_value(auth: &ResolvedAuth, name: &str) -> Option<String> {
        auth.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn test_bearer(key: &str) -> ResolvedAuth {
        ResolvedAuth::for_test(None, vec![(AUTHORIZATION_HEADER.into(), bearer_value(key))])
    }

    #[test]
    fn config_headers_append_unknown_and_replace_same_name() {
        let mut auth = test_bearer("sk-1");
        auth.apply_config_headers(
            TEST_SLUG,
            &config_headers(&[
                (GATEWAY_HEADER, GATEWAY_ID),
                // Case differs from the built-in header on purpose: appending
                // instead of replacing would send two credentials.
                ("Authorization", "Basic other"),
            ]),
        )
        .unwrap();
        assert_eq!(auth.headers.len(), 2);
        assert_eq!(
            header_value(&auth, AUTHORIZATION_HEADER).as_deref(),
            Some("Basic other")
        );
        assert_eq!(
            header_value(&auth, GATEWAY_HEADER).as_deref(),
            Some(GATEWAY_ID)
        );
    }

    #[test]
    fn config_headers_unset_var_names_slug_header_and_var() {
        let var = format!("MAKI_TEST_GATEWAY_UNSET_{}", fastrand::u32(..));
        let mut auth = test_bearer("sk-1");
        let err = auth
            .apply_config_headers(
                TEST_SLUG,
                &config_headers(&[(GATEWAY_HEADER, &format!("${{{var}}}"))]),
            )
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(TEST_SLUG), "got: {msg}");
        assert!(msg.contains(GATEWAY_HEADER), "got: {msg}");
        assert!(msg.contains(&var), "got: {msg}");
    }

    #[test]
    fn rotate_bearer_keeps_base_url_and_config_headers() {
        let pool = KeyPool::from_keys(vec!["sk-1".into(), "sk-2".into()]);
        let mut auth = test_bearer(pool.current());
        auth.base_url = Some(GATEWAY_URL.into());
        auth.apply_config_headers(TEST_SLUG, &config_headers(&[(GATEWAY_HEADER, GATEWAY_ID)]))
            .unwrap();

        let auth = Mutex::new(auth);
        assert!(pool.rotate_bearer(&auth));

        let auth = auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some(GATEWAY_URL));
        assert_eq!(
            header_value(&auth, AUTHORIZATION_HEADER).as_deref(),
            Some("Bearer sk-2")
        );
        assert_eq!(
            header_value(&auth, GATEWAY_HEADER).as_deref(),
            Some(GATEWAY_ID)
        );
    }

    #[test]
    fn rotate_bearer_keeps_a_configured_auth_header() {
        let pool = KeyPool::from_keys(vec!["sk-1".into(), "sk-2".into()]);
        let mut auth = test_bearer(pool.current());
        auth.apply_config_headers(
            TEST_SLUG,
            &config_headers(&[("Authorization", GATEWAY_CRED)]),
        )
        .unwrap();

        let auth = Mutex::new(auth);
        assert!(pool.rotate_bearer(&auth));

        // The gateway credential replaced the built-in bearer, so rotating the
        // key must not put `Bearer sk-2` back and lock the user out.
        let auth = auth.lock().unwrap();
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(
            header_value(&auth, AUTHORIZATION_HEADER).as_deref(),
            Some(GATEWAY_CRED)
        );
    }
}
