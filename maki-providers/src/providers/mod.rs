use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use futures_lite::io::AsyncBufRead;
use isahc::config::Configurable;
use isahc::http::request::Builder;
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::AgentError;
use crate::model::Model;

pub(crate) mod anthropic;
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
pub(crate) mod synthetic;
pub(crate) mod tensorx;
pub(crate) mod zai;

const LOW_SPEED_BYTES_PER_SEC: u32 = 1;

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

#[derive(Clone)]
pub struct ResolvedAuth {
    pub base_url: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl ResolvedAuth {
    pub fn bearer(api_key: &str) -> Self {
        Self {
            base_url: None,
            headers: vec![("authorization".into(), format!("Bearer {api_key}"))],
        }
    }

    /// Apply all auth headers to an HTTP request builder.
    pub fn configure_request(&self, builder: Builder) -> Builder {
        self.headers.iter().fold(builder, |b, (key, value)| {
            b.header(key.as_str(), value.as_str())
        })
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

/// Apply per-model body overrides after all typed thinking setup. Three
/// operations run in order: `defaults` (additive, only fills absent keys),
/// `replace` (deep-merge, overwrites existing), `filter` (strips keys).
/// `protected` is the conversation field for this provider's protocol
/// (`messages`, `input`, or `contents`); none of the three can touch it.
///
/// Nothing happens when `body_override` is `None`, so the no-override path
/// stays zero-cost.
pub(crate) fn apply_body_overrides(body: &mut Value, model: &Model, protected: &[&str]) {
    let Some(ov) = model.body_override.as_ref() else {
        return;
    };
    if let Some(defaults) = &ov.defaults {
        shallow_inject(body, defaults, protected);
    }
    if let Some(replace) = &ov.replace {
        deep_merge(body, replace, protected);
    }
    if !ov.filter.is_empty() {
        filter_body(body, &ov.filter, protected);
    }
}

fn shallow_inject(body: &mut Value, defaults: &Value, protected: &[&str]) {
    let (Some(body_obj), Some(defaults_obj)) = (body.as_object_mut(), defaults.as_object()) else {
        return;
    };
    for (k, v) in defaults_obj {
        if protected.contains(&k.as_str()) {
            continue;
        }
        if !body_obj.contains_key(k) {
            body_obj.insert(k.clone(), v.clone());
        }
    }
}

fn deep_merge(body: &mut Value, replace: &Value, protected: &[&str]) {
    let (Some(body_obj), Some(replace_obj)) = (body.as_object_mut(), replace.as_object()) else {
        return;
    };
    for (k, v) in replace_obj {
        if protected.contains(&k.as_str()) {
            continue;
        }
        match body_obj.get_mut(k) {
            Some(existing) if existing.is_object() && v.is_object() => {
                if let (Some(e), Some(r)) = (existing.as_object_mut(), v.as_object()) {
                    for (rk, rv) in r {
                        e.insert(rk.clone(), rv.clone());
                    }
                }
            }
            _ => {
                body_obj.insert(k.clone(), v.clone());
            }
        }
    }
}

fn filter_body(body: &mut Value, filter: &[String], protected: &[&str]) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for k in filter {
        if protected.contains(&k.as_str()) {
            continue;
        }
        obj.remove(k);
    }
}

#[derive(Deserialize)]
pub(crate) struct SseErrorPayload {
    pub error: SseErrorDetail,
}

#[derive(Deserialize)]
pub(crate) struct SseErrorDetail {
    #[serde(default)]
    pub r#type: String,
    pub message: String,
}

impl SseErrorPayload {
    pub fn into_agent_error(self) -> AgentError {
        let status = match self.error.r#type.as_str() {
            "overloaded_error" => 529,
            "api_error" | "server_error" => 500,
            "rate_limit_error" | "rate_limit_exceeded" | "tokens" => 429,
            "request_too_large" => 413,
            "not_found_error" => 404,
            "permission_error" => 403,
            "billing_error" | "insufficient_quota" => 402,
            "authentication_error" | "invalid_api_key" => 401,
            _ => 400,
        };
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

    pub fn rotate_auth(
        &self,
        auth: &Mutex<ResolvedAuth>,
        build: impl FnOnce(&str) -> ResolvedAuth,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        *auth.lock().unwrap() = build(self.current());
        true
    }

    pub fn rotate_headers(
        &self,
        auth: &Mutex<ResolvedAuth>,
        build: impl FnOnce(&str) -> Vec<(String, String)>,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        auth.lock().unwrap().headers = build(self.current());
        true
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

    #[test]
    fn shallow_inject_fills_absent_keys_only() {
        let mut body = serde_json::json!({"temperature": 0.1, "messages": []});
        let defaults = serde_json::json!({"temperature": 0.7, "max_tokens": 8192});
        shallow_inject(&mut body, &defaults, &["messages"]);
        assert_eq!(body["temperature"], 0.1);
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn shallow_inject_empty_is_noop() {
        let mut body = serde_json::json!({"x": 1});
        shallow_inject(&mut body, &serde_json::json!({}), &[]);
        assert_eq!(body, serde_json::json!({"x": 1}));
    }

    #[test]
    fn shallow_inject_nested_value_lands_whole() {
        let mut absent = serde_json::json!({"a": 1});
        shallow_inject(
            &mut absent,
            &serde_json::json!({"chat_template_kwargs": {"x": 1}}),
            &[],
        );
        assert_eq!(absent["chat_template_kwargs"]["x"], 1);

        let mut present = serde_json::json!({"chat_template_kwargs": {"y": 1}});
        shallow_inject(
            &mut present,
            &serde_json::json!({"chat_template_kwargs": {"x": 999}}),
            &[],
        );
        assert_eq!(present["chat_template_kwargs"]["y"], 1);
        assert!(present["chat_template_kwargs"].get("x").is_none());
    }

    #[test]
    fn filter_body_removes_listed_keys() {
        let mut body = serde_json::json!({
            "context_management": {"x": 1},
            "keep": true,
            "also_drop": null
        });
        filter_body(
            &mut body,
            &["context_management".into(), "also_drop".into()],
            &[],
        );
        assert_eq!(body, serde_json::json!({"keep": true}));
    }

    #[test]
    fn filter_body_missing_key_is_noop() {
        let mut body = serde_json::json!({"keep": true});
        filter_body(&mut body, &["context_management".into()], &[]);
        assert_eq!(body, serde_json::json!({"keep": true}));
    }

    #[test]
    fn apply_body_overrides_defaults_then_replace_then_filter() {
        let model = Model {
            id: "test-model".into(),
            provider: Arc::from("anthropic"),
            tier: crate::model::ModelTier::Medium,
            family: crate::model::ModelFamily::Claude,
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            supports_vision_override: None,
            pricing: Default::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
            thinking_dialect: None,
            thinking_fields: None,
            body_override: Some(crate::types::BodyOverride {
                defaults: Some(serde_json::json!({"temperature": 0.7, "poison": "bad"})),
                replace: Some(serde_json::json!({"temperature": 0.1})),
                filter: vec!["poison".into()],
            }),
        };
        let mut body = serde_json::json!({"messages": [], "temperature": 0.5});
        apply_body_overrides(&mut body, &model, &["messages"]);
        // Replace overwrites the existing temperature (not the defaults value).
        assert_eq!(body["temperature"], 0.1);
        assert_eq!(body["messages"], serde_json::json!([]));
        assert!(body.get("poison").is_none());
    }

    #[test]
    fn apply_body_overrides_no_override_is_noop() {
        let model = Model {
            id: "test-model".into(),
            provider: Arc::from("anthropic"),
            tier: crate::model::ModelTier::Medium,
            family: crate::model::ModelFamily::Claude,
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            supports_vision_override: None,
            pricing: Default::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
            thinking_dialect: None,
            thinking_fields: None,
            body_override: None,
        };
        let mut body = serde_json::json!({"messages": [], "temperature": 0.5});
        apply_body_overrides(&mut body, &model, &["messages"]);
        assert_eq!(
            body,
            serde_json::json!({"messages": [], "temperature": 0.5})
        );
    }

    #[test]
    fn apply_body_overrides_protected_key_survives_filter() {
        let model = Model {
            id: "test-model".into(),
            provider: Arc::from("anthropic"),
            tier: crate::model::ModelTier::Medium,
            family: crate::model::ModelFamily::Claude,
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            supports_vision_override: None,
            pricing: Default::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
            thinking_dialect: None,
            thinking_fields: None,
            body_override: Some(crate::types::BodyOverride {
                defaults: None,
                replace: None,
                filter: vec!["messages".into()],
            }),
        };
        let mut body = serde_json::json!({"messages": [{"role": "user"}]});
        apply_body_overrides(&mut body, &model, &["messages"]);
        assert_eq!(body["messages"], serde_json::json!([{"role": "user"}]));
    }

    #[test]
    fn apply_body_overrides_replace_deep_merges_nested() {
        let model = Model {
            id: "test-model".into(),
            provider: Arc::from("anthropic"),
            tier: crate::model::ModelTier::Medium,
            family: crate::model::ModelFamily::Claude,
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            supports_vision_override: None,
            pricing: Default::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
            thinking_dialect: None,
            thinking_fields: None,
            body_override: Some(crate::types::BodyOverride {
                defaults: None,
                replace: Some(serde_json::json!({"generationConfig": {"thinkingBudget": 8192}})),
                filter: vec![],
            }),
        };
        let mut body =
            serde_json::json!({"messages": [], "generationConfig": {"includeThoughts": true}});
        apply_body_overrides(&mut body, &model, &["messages"]);
        assert_eq!(body["generationConfig"]["thinkingBudget"], 8192);
        assert_eq!(body["generationConfig"]["includeThoughts"], true);
    }
}
