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
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Self::from_env(
            timeouts,
            std::env::var(API_KEY_ENV).ok(),
            std::env::var(HOST_ENV).ok(),
        )
    }

    fn from_env(
        timeouts: super::Timeouts,
        api_key: Option<String>,
        host: Option<String>,
    ) -> Result<Self, AgentError> {
        if let Some(api_key) = api_key {
            return Ok(Self {
                compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
                auth: ResolvedAuth {
                    base_url: Some(CLOUD_BASE_URL.into()),
                    headers: vec![("authorization".into(), format!("Bearer {api_key}"))],
                },
            });
        }

        let host = host.ok_or(AgentError::Config {
            message: HOST_NOT_SET.into(),
        })?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
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

    const TEST_TIMEOUTS: super::super::Timeouts = super::super::Timeouts {
        connect: std::time::Duration::from_secs(10),
        stream: std::time::Duration::from_secs(300),
    };

    #[test]
    fn from_env_without_host_or_api_key_errors() {
        match Ollama::from_env(TEST_TIMEOUTS, None, None) {
            Err(AgentError::Config { message }) => assert_eq!(message, HOST_NOT_SET),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected error when host and api_key are None"),
        }
    }

    #[test]
    fn from_env_with_host_builds_auth() {
        let ollama = Ollama::from_env(TEST_TIMEOUTS, None, Some("http://x:1234".into())).unwrap();
        assert_eq!(ollama.auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(ollama.auth.headers.is_empty());
    }

    #[test]
    fn from_env_with_api_key_uses_cloud() {
        let ollama = Ollama::from_env(TEST_TIMEOUTS, Some("test-key".into()), None).unwrap();
        assert_eq!(ollama.auth.base_url.as_deref(), Some(CLOUD_BASE_URL));
        assert_eq!(ollama.auth.headers.len(), 1);
        assert_eq!(ollama.auth.headers[0].0, "authorization");
        assert_eq!(ollama.auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn from_env_api_key_takes_precedence_over_host() {
        let ollama = Ollama::from_env(
            TEST_TIMEOUTS,
            Some("test-key".into()),
            Some("http://local:1234".into()),
        )
        .unwrap();
        assert_eq!(ollama.auth.base_url.as_deref(), Some(CLOUD_BASE_URL));
        assert_eq!(ollama.auth.headers[0].1, "Bearer test-key");
    }
}
