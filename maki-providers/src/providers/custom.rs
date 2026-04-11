use crate::model::{Model, ModelEntry};
use crate::provider::{BoxFuture, Provider};
use crate::providers::ResolvedAuth;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};
use flume::Sender;
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};
static CONFIG: OnceLock<CustomConfig> = OnceLock::new();
static COMPAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: true,
    provider_name: "Custom",
};
pub struct CustomConfig {
    pub base_url: String,
    pub api_key: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
}
pub fn set_config(config: CustomConfig) {
    let _ = CONFIG.set(config);
}
fn get_config() -> Option<&'static CustomConfig> {
    CONFIG.get()
}

pub fn get_defaults() -> (u32, u32) {
    let config = get_config();
    (
        config.map(|c| c.context_window).unwrap_or(1_000_000),
        config.map(|c| c.max_output_tokens).unwrap_or(16_384),
    )
}
pub struct Custom {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
}
impl Custom {
    pub fn new() -> Result<Self, AgentError> {
        let config = get_config().ok_or_else(|| AgentError::Config {
            message: "Custom provider not configured. Set custom_openai_base_url and custom_openai_api_key in [provider] section of config.toml".into(),
        })?;
        let mut auth = ResolvedAuth::bearer(&config.api_key);
        auth.base_url = Some(config.base_url.clone());
        Ok(Self {
            compat: OpenAiCompatProvider::new(&COMPAT_CONFIG),
            auth: Arc::new(Mutex::new(auth)),
        })
    }
}
impl Provider for Custom {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _thinking: ThinkingConfig,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let body = self.compat.build_body(model, messages, system, tools);
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_stream(model, &body, event_tx, &auth).await
        })
    }
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_list_models(&auth).await
        })
    }
}
pub(crate) fn models() -> &'static [ModelEntry] {
    &[]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_config_stores_values() {
        set_config(CustomConfig {
            base_url: "http://test.example/v1".into(),
            api_key: "test-key".into(),
            context_window: 128_000,
            max_output_tokens: 16384,
        });
        let config = get_config().unwrap();
        assert_eq!(config.base_url, "http://test.example/v1");
        assert_eq!(config.api_key, "test-key");
    }
    #[test]
    fn get_defaults_returns_expected() {
        let (cw, mot) = get_defaults();
        assert_eq!(cw, 1_000_000);
        assert_eq!(mot, 16_384);
    }
}
