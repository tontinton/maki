use flume::Sender;
use serde_json::Value;

use crate::model::{Model, ModelEntry};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

pub(crate) const DEFAULT_MAX_OUTPUT: u32 = 16384;
pub(crate) const DEFAULT_CONTEXT: u32 = 128_000;
const HOST_ENV: &str = "OLLAMA_HOST";
const API_KEY_ENV: &str = "OLLAMA_API_KEY";
const CLOUD_BASE_URL: &str = "https://ollama.com/v1";
const HOST_NOT_SET: &str = "OLLAMA_HOST not set (or set OLLAMA_API_KEY for cloud)";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "http://localhost:11434/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Ollama",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[]
}

pub struct Ollama {
    compat: OpenAiCompatProvider,
    auth: ResolvedAuth,
}

impl Ollama {
    pub fn new() -> Result<Self, AgentError> {
        if let Ok(api_key) = std::env::var(API_KEY_ENV) {
            return Ok(Self {
                compat: OpenAiCompatProvider::new(&CONFIG),
                auth: ResolvedAuth {
                    base_url: Some(CLOUD_BASE_URL.into()),
                    headers: vec![("authorization".into(), format!("Bearer {api_key}"))],
                },
            });
        }

        let host = std::env::var(HOST_ENV).map_err(|_| AgentError::Config {
            message: HOST_NOT_SET.into(),
        })?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG),
            auth: ResolvedAuth {
                base_url: Some(format!("{host}/v1")),
                headers: Vec::new(),
            },
        })
    }
}

impl Provider for Ollama {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let body = self.compat.build_body(model, messages, system, tools);
            self.compat
                .do_stream(model, &[], &body, event_tx, &self.auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.compat.do_list_models(&self.auth))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn new_without_host_or_api_key_errors() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var(HOST_ENV);
            std::env::remove_var(API_KEY_ENV);
        }
        match Ollama::new() {
            Err(AgentError::Config { message }) => assert_eq!(message, HOST_NOT_SET),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected error when {HOST_ENV} and {API_KEY_ENV} are unset"),
        }
    }

    #[test]
    fn new_with_host_builds_auth() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var(API_KEY_ENV);
            std::env::set_var(HOST_ENV, "http://x:1234");
        }
        let ollama = Ollama::new().expect("should build when host set");
        assert_eq!(ollama.auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(ollama.auth.headers.is_empty());
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe { std::env::remove_var(HOST_ENV) };
    }

    #[test]
    fn new_with_api_key_uses_cloud() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var(HOST_ENV);
            std::env::set_var(API_KEY_ENV, "test-key");
        }
        let ollama = Ollama::new().expect("should build with API key");
        assert_eq!(ollama.auth.base_url.as_deref(), Some(CLOUD_BASE_URL));
        assert_eq!(ollama.auth.headers.len(), 1);
        assert_eq!(ollama.auth.headers[0].0, "authorization");
        assert_eq!(ollama.auth.headers[0].1, "Bearer test-key");
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe { std::env::remove_var(API_KEY_ENV) };
    }

    #[test]
    fn new_api_key_takes_precedence_over_host() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe {
            std::env::set_var(HOST_ENV, "http://local:1234");
            std::env::set_var(API_KEY_ENV, "test-key");
        }
        let ollama = Ollama::new().expect("should build");
        assert_eq!(ollama.auth.base_url.as_deref(), Some(CLOUD_BASE_URL));
        assert_eq!(ollama.auth.headers[0].1, "Bearer test-key");
        // SAFETY: single-threaded test section guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var(HOST_ENV);
            std::env::remove_var(API_KEY_ENV);
        }
    }
}
