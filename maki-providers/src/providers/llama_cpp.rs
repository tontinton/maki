use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use flume::Sender;
use isahc::ReadResponseExt;
use isahc::config::Configurable;
use serde_json::Value;
use tracing::{debug, warn};

use crate::model::{Model, ModelEntry};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const HOST_ENV: &str = "LLAMA_CPP_HOST";
const API_KEY_ENV: &str = "LLAMA_CPP_API_KEY";
const HOST_NOT_SET: &str = "LLAMA_CPP_HOST not set";
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "http://localhost:8080/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "LlamaCpp",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[]
}

static CACHED_CONTEXT_WINDOW: OnceLock<Option<u32>> = OnceLock::new();

/// Probe the running llama.cpp server for its actual context size by reading
/// `default_generation_settings.n_ctx` from `/props`. Cached for the life of
/// the process; returns `None` on any failure so callers fall back to a static
/// default.
pub(crate) fn fetch_context_window() -> Option<u32> {
    *CACHED_CONTEXT_WINDOW.get_or_init(probe_context_window)
}

fn probe_context_window() -> Option<u32> {
    let host = std::env::var(HOST_ENV).ok()?;
    let host = host.trim_end_matches('/');
    let client = isahc::HttpClient::builder()
        .connect_timeout(PROBE_CONNECT_TIMEOUT)
        .timeout(PROBE_TOTAL_TIMEOUT)
        .build()
        .ok()?;

    let n_ctx = probe_props_n_ctx(&client, host)?;
    debug!(n_ctx, "llama.cpp context window detected");
    Some(n_ctx)
}

fn probe_props_n_ctx(client: &isahc::HttpClient, host: &str) -> Option<u32> {
    let body = http_get(client, &format!("{host}/props"))?;
    parse_props_n_ctx(&body)
}

fn http_get(client: &isahc::HttpClient, url: &str) -> Option<String> {
    let mut builder = isahc::Request::get(url);
    if let Some(key) = first_api_key() {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let request = builder.body(()).ok()?;

    let mut resp = match client.send(request) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, %url, "failed to probe llama.cpp; using fallback context window");
            return None;
        }
    };

    let status = resp.status().as_u16();
    if status != 200 {
        warn!(status, %url, "llama.cpp probe returned non-200; using fallback context window");
        return None;
    }

    resp.text().ok()
}

fn parse_props_n_ctx(body: &str) -> Option<u32> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    parsed
        .get("default_generation_settings")
        .and_then(|s| s.get("n_ctx"))
        .or_else(|| parsed.get("n_ctx"))
        .and_then(Value::as_u64)
        .map(|n| n as u32)
}

fn first_api_key() -> Option<String> {
    KeyPool::from_env(API_KEY_ENV).ok().map(|p| p.current().to_string())
}

pub struct LlamaCpp {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl LlamaCpp {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let key_pool = KeyPool::from_env(API_KEY_ENV).ok();
        Self::from_env(timeouts, key_pool, std::env::var(HOST_ENV).ok())
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn from_env(
        timeouts: super::Timeouts,
        key_pool: Option<KeyPool>,
        host: Option<String>,
    ) -> Result<Self, AgentError> {
        let base_url = match host {
            Some(h) => format!("{h}/v1"),
            None => {
                return Err(AgentError::Config {
                    message: HOST_NOT_SET.into(),
                });
            }
        };
        let headers = match key_pool.as_ref().map(|p| p.current().to_string()) {
            Some(key) => vec![("authorization".into(), format!("Bearer {key}"))],
            None => Vec::new(),
        };
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth {
                base_url: Some(base_url),
                headers,
            })),
            key_pool,
            system_prefix: None,
        })
    }
}

impl Provider for LlamaCpp {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let body = self.compat.build_body(model, messages, system, tools);
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_list_models(&auth).await
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self.key_pool.as_ref().is_some_and(|p| {
                p.rotate_headers(&self.auth, |key| {
                    vec![("authorization".into(), format!("Bearer {key}"))]
                })
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TIMEOUTS: super::super::Timeouts = super::super::Timeouts {
        connect: std::time::Duration::from_secs(10),
        low_speed: std::time::Duration::from_secs(30),
        stream: std::time::Duration::from_secs(300),
    };

    #[test]
    fn from_env_without_host_or_api_key_errors() {
        match LlamaCpp::from_env(TEST_TIMEOUTS, None, None) {
            Err(AgentError::Config { message }) => assert_eq!(message, HOST_NOT_SET),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected error when host and api_key are None"),
        }
    }

    #[test]
    fn from_env_with_host_builds_auth() {
        let llama = LlamaCpp::from_env(TEST_TIMEOUTS, None, Some("http://x:1234".into())).unwrap();
        let auth = llama.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(auth.headers.is_empty());
    }

    #[test]
    fn parse_props_n_ctx_reads_nested() {
        let body = r#"{"default_generation_settings":{"n_ctx":8192,"temperature":0.8},"total_slots":1}"#;
        assert_eq!(parse_props_n_ctx(body), Some(8192));
    }

    #[test]
    fn parse_props_n_ctx_falls_back_to_top_level() {
        let body = r#"{"n_ctx":4096,"total_slots":1}"#;
        assert_eq!(parse_props_n_ctx(body), Some(4096));
    }

    #[test]
    fn parse_props_n_ctx_handles_garbage() {
        assert_eq!(parse_props_n_ctx("not json"), None);
        assert_eq!(parse_props_n_ctx("{}"), None);
    }

    #[test]
    fn from_env_with_api_key_uses_host_with_auth() {
        let pool = KeyPool::from_keys(vec!["test-key".into()]);
        let llama = LlamaCpp::from_env(TEST_TIMEOUTS, Some(pool), Some("http://local:1234".into()))
            .unwrap();
        let auth = llama.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://local:1234/v1"));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }
}
