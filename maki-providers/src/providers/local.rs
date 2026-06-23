use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

pub(crate) struct LocalEndpointConfig {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub host_env: &'static str,
    pub api_key_env: &'static str,
    pub default_host: &'static str,
    pub default_model: &'static str,
    pub cloud_fallback_url: Option<&'static str>,
    pub compat: OpenAiCompatConfig,
    pub thinking_budget_field: bool,
}

pub(crate) struct LocalEndpoint {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
    thinking_budget_field: bool,
}

impl LocalEndpoint {
    pub fn new(
        cfg: &'static LocalEndpointConfig,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        let key_pool = KeyPool::resolve(cfg.slug, cfg.api_key_env).ok();
        let config = maki_config::providers::ProvidersConfig::load();
        let host = config
            .get(cfg.slug)
            .and_then(|d| d.base_url.clone())
            .or_else(|| std::env::var(cfg.host_env).ok());
        Self::build(cfg, timeouts, key_pool, host)
    }

    pub(crate) fn with_auth(
        cfg: &'static LocalEndpointConfig,
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&cfg.compat, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
            thinking_budget_field: cfg.thinking_budget_field,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn build(
        cfg: &'static LocalEndpointConfig,
        timeouts: super::Timeouts,
        key_pool: Option<KeyPool>,
        host: Option<String>,
    ) -> Result<Self, AgentError> {
        let api_key = key_pool.as_ref().map(|p| p.current().to_string());
        let base_url = match host {
            Some(h) => format!("{h}/v1"),
            None if api_key.is_some() && cfg.cloud_fallback_url.is_some() => {
                cfg.cloud_fallback_url.unwrap().to_string()
            }
            None => {
                return Err(AgentError::Config {
                    message: format!("{} not set", cfg.host_env),
                });
            }
        };
        let headers = match api_key {
            Some(key) => vec![("authorization".into(), format!("Bearer {key}"))],
            None => Vec::new(),
        };
        let compat_config = &cfg.compat;
        Ok(Self {
            compat: OpenAiCompatProvider::new(compat_config, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth {
                base_url: Some(base_url),
                headers,
            })),
            key_pool,
            system_prefix: None,
            thinking_budget_field: cfg.thinking_budget_field,
        })
    }
}

impl Provider for LocalEndpoint {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);

            if self.thinking_budget_field {
                let budget = match opts.thinking {
                    ThinkingConfig::Off => 0,
                    ThinkingConfig::Adaptive => -1,
                    ThinkingConfig::Budget(n) => n as i64,
                };
                body["thinking_budget_tokens"] = json!(budget);
            }

            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
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

pub(crate) const OLLAMA: LocalEndpointConfig = LocalEndpointConfig {
    slug: "ollama",
    display_name: "Ollama",
    host_env: "OLLAMA_HOST",
    api_key_env: "OLLAMA_API_KEY",
    default_host: "http://localhost:11434",
    default_model: "ollama/qwen3",
    cloud_fallback_url: Some("https://ollama.com/v1"),
    compat: OpenAiCompatConfig {
        api_key_env: "",
        base_url: "http://localhost:11434/v1",
        max_tokens_field: "max_tokens",
        include_stream_usage: true,
        provider_name: "Ollama",
    },
    thinking_budget_field: false,
};

pub(crate) const LLAMACPP: LocalEndpointConfig = LocalEndpointConfig {
    slug: "llama-cpp",
    display_name: "LlamaCpp",
    host_env: "LLAMA_CPP_HOST",
    api_key_env: "LLAMA_CPP_API_KEY",
    default_host: "http://localhost:8080",
    default_model: "llama-cpp/default",
    cloud_fallback_url: None,
    compat: OpenAiCompatConfig {
        api_key_env: "",
        base_url: "http://localhost:8080/v1",
        max_tokens_field: "max_tokens",
        include_stream_usage: true,
        provider_name: "LlamaCpp",
    },
    thinking_budget_field: true,
};

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
        match LocalEndpoint::build(&OLLAMA, TEST_TIMEOUTS, None, None) {
            Err(AgentError::Config { message }) => {
                assert_eq!(message, "OLLAMA_HOST not set");
            }
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }

    #[test]
    fn from_env_with_host_builds_auth() {
        let ep = LocalEndpoint::build(&OLLAMA, TEST_TIMEOUTS, None, Some("http://x:1234".into()))
            .unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(auth.headers.is_empty());
    }

    #[test]
    fn from_env_with_api_key_uses_cloud_for_ollama() {
        let pool = KeyPool::from_keys(vec!["test-key".into()]);
        let ep = LocalEndpoint::build(&OLLAMA, TEST_TIMEOUTS, Some(pool), None).unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("https://ollama.com/v1"));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn from_env_both_host_and_api_key_uses_host_with_auth() {
        let pool = KeyPool::from_keys(vec!["test-key".into()]);
        let ep = LocalEndpoint::build(
            &OLLAMA,
            TEST_TIMEOUTS,
            Some(pool),
            Some("http://local:1234".into()),
        )
        .unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://local:1234/v1"));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn llamacpp_without_host_errors() {
        match LocalEndpoint::build(&LLAMACPP, TEST_TIMEOUTS, None, None) {
            Err(AgentError::Config { message }) => {
                assert_eq!(message, "LLAMA_CPP_HOST not set");
            }
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }

    #[test]
    fn llamacpp_with_host_builds_auth() {
        let ep = LocalEndpoint::build(&LLAMACPP, TEST_TIMEOUTS, None, Some("http://x:1234".into()))
            .unwrap();
        let auth = ep.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234/v1"));
        assert!(auth.headers.is_empty());
    }

    #[test]
    fn llamacpp_no_cloud_fallback() {
        let pool = KeyPool::from_keys(vec!["key".into()]);
        match LocalEndpoint::build(&LLAMACPP, TEST_TIMEOUTS, Some(pool), None) {
            Err(AgentError::Config { message }) => {
                assert_eq!(message, "LLAMA_CPP_HOST not set");
            }
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }
}
