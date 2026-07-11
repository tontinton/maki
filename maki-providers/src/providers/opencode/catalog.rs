//! Catalog data model and HTTP loading for the models.dev provider index.
//!
//! `CatalogIndex` is the raw JSON shape returned by `https://models.dev/api.json`.
//! The Zen and Go backends transform this into a uniform [`crate::opencode::backend::Catalog`].

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use isahc::config::Configurable;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::AgentError;
use crate::providers::ResolvedAuth;
use crate::providers::user_agent;

pub(super) const CATALOG_URL: &str = "https://models.dev/api.json";
pub(super) const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
pub(super) const CATALOG_CACHE_TTL: Duration = Duration::from_secs(86400);
pub(super) const ALLOWED_NPM: &[&str] = &["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"];

const DEFAULT_CONTEXT: u32 = 128_000;
const DEFAULT_OUTPUT: u32 = 64_000;

pub(super) type CatalogIndex = HashMap<String, CatalogProvider>;
pub(super) type ModelKey = (String, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointType {
    ChatCompletions,
    Messages,
}

impl EndpointType {
    pub(super) fn from_npm(npm: &str) -> Self {
        if npm == "@ai-sdk/anthropic" {
            Self::Messages
        } else {
            Self::ChatCompletions
        }
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct CatalogProvider {
    pub name: String,
    #[serde(default)]
    pub env: Vec<String>,
    pub npm: String,
    pub api: Option<String>,
    pub models: HashMap<String, CatalogModel>,
}

impl CatalogProvider {
    pub(super) fn build_auth(&self, api_key: String, api_format: EndpointType) -> ResolvedAuth {
        let headers = match api_format {
            EndpointType::Messages => vec![("x-api-key".into(), api_key)],
            EndpointType::ChatCompletions => {
                vec![("authorization".into(), format!("Bearer {api_key}"))]
            }
        };
        ResolvedAuth {
            base_url: self.api.clone(),
            headers,
        }
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct CatalogModel {
    #[serde(default)]
    pub limit: Option<CatalogLimits>,
    #[serde(default)]
    pub cost: Option<CatalogCost>,
    #[serde(default)]
    pub provider: Option<CatalogShape>,
    #[serde(default)]
    pub attachment: Option<bool>,
    #[serde(default)]
    pub modalities: Option<CatalogModalities>,
    #[serde(default)]
    pub reasoning: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct CatalogLimits {
    #[serde(default)]
    pub context: Option<u32>,
    #[serde(default)]
    pub input: Option<u32>,
    #[serde(default)]
    pub output: Option<u32>,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct CatalogCost {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct CatalogShape {
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub npm: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct CatalogModalities {
    #[serde(default)]
    input: Vec<String>,
}

pub(super) const PUBLIC_KEY: &str = "public";
pub(super) const ZEN_CATALOG_KEY: &str = "opencode";
pub(super) const ZEN_MAKI_SLUG: &str = "opencode-zen";
pub(super) const GO_PROVIDER_ID: &str = "opencode-go";

fn saved_key(state_dir: &maki_storage::StateDir, slug: &str) -> Option<String> {
    maki_storage::auth::load_provider_credentials(state_dir, slug).map(|c| c.api_key)
}

pub(super) fn maki_slug_for(provider_id: &str) -> &str {
    if provider_id == ZEN_CATALOG_KEY {
        ZEN_MAKI_SLUG
    } else {
        provider_id
    }
}

fn resolve_provider_key(
    provider: &CatalogProvider,
    state_dir: &maki_storage::StateDir,
    saved_key_slug: &str,
    read_env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    for var in &provider.env {
        if let Some(val) = read_env(var) {
            debug!(provider = %provider.name, var = %var, "api key resolved from env");
            return Some(val);
        }
        debug!(provider = %provider.name, var = %var, "env var not set");
    }
    if provider.env.iter().any(|v| v == "OPENCODE_API_KEY")
        && let Some(key) = saved_key(state_dir, saved_key_slug)
    {
        debug!(provider = %provider.name, slug = saved_key_slug, "api key resolved from saved credentials");
        return Some(key);
    }
    debug!(provider = %provider.name, "no api key available");
    None
}

pub(super) fn resolve_provider_keys(
    index: &CatalogIndex,
    state_dir: &maki_storage::StateDir,
) -> HashMap<String, Option<String>> {
    index
        .iter()
        .map(|(provider_id, provider)| {
            let slug = maki_slug_for(provider_id);
            let key = resolve_provider_key(provider, state_dir, slug, |v| std::env::var(v).ok());
            (provider_id.clone(), key)
        })
        .collect()
}

/// A single resolved model entry, shared by Zen and Go.
#[derive(Clone)]
pub(crate) struct Meta {
    pub provider_id: String,
    pub api_format: EndpointType,
    pub context: u32,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub vision: bool,
    pub supports_thinking: bool,
}

pub(super) fn model_meta(
    model: &CatalogModel,
    provider_id: &str,
    api_format: EndpointType,
) -> Meta {
    let limit = model.limit.as_ref();
    let cost = model.cost.as_ref();
    let resolved_api_format = model
        .provider
        .as_ref()
        .and_then(|p| p.npm.as_deref())
        .map(EndpointType::from_npm)
        .unwrap_or(api_format);
    let vision = model.attachment.unwrap_or(false)
        || model
            .modalities
            .as_ref()
            .is_some_and(|m| m.input.iter().any(|s| s == "image"));

    Meta {
        provider_id: provider_id.to_string(),
        api_format: resolved_api_format,
        context: limit.and_then(|l| l.context).unwrap_or(DEFAULT_CONTEXT),
        output: limit.and_then(|l| l.output).unwrap_or(DEFAULT_OUTPUT),
        input_price: cost.and_then(|c| c.input).unwrap_or(0.0),
        output_price: cost.and_then(|c| c.output).unwrap_or(0.0),
        cache_read: cost.and_then(|c| c.cache_read).unwrap_or(0.0),
        cache_write: cost.and_then(|c| c.cache_write).unwrap_or(0.0),
        vision,
        supports_thinking: model.reasoning.unwrap_or(false),
    }
}

fn catalog_cache_path() -> Option<PathBuf> {
    let dir = maki_storage::paths::cache_dir().ok()?;
    Some(dir.join(CATALOG_CACHE_FILE))
}

pub(super) async fn load_cached_async() -> Option<CatalogIndex> {
    let path = catalog_cache_path()?;
    let meta = smol::unblock({
        let path = path.clone();
        move || fs::metadata(&path)
    })
    .await
    .ok()?;

    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > CATALOG_CACHE_TTL {
        debug!("catalog cache expired");
        return None;
    }

    let text = smol::unblock(move || fs::read_to_string(&path))
        .await
        .ok()?;
    let index: CatalogIndex = serde_json::from_str(&text).ok()?;
    debug!("loaded catalog from cache");
    Some(index)
}

pub(super) async fn save_cached_async(index: &CatalogIndex) {
    let path = match catalog_cache_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let dir = dir.to_path_buf();
        let _ = smol::unblock(move || fs::create_dir_all(&dir)).await;
    }
    let text = match serde_json::to_string_pretty(index) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "failed to serialize catalog for cache");
            return;
        }
    };
    smol::unblock(move || {
        if let Err(e) = fs::write(&path, &text) {
            warn!(error = %e, path = %path.display(), "failed to write catalog cache");
        } else {
            debug!(path = %path.display(), "cached catalog");
        }
    })
    .await;
}

pub(super) fn http_client(context: &str) -> isahc::HttpClient {
    isahc::HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .low_speed_timeout(1, Duration::from_secs(30))
        .build()
        .unwrap_or_else(|e| panic!("failed to build {context} HTTP client: {e}"))
}

pub(super) async fn fetch_remote_async(client: &HttpClient) -> Result<CatalogIndex, AgentError> {
    let request = Request::builder()
        .uri(CATALOG_URL)
        .header("user-agent", user_agent())
        .body(())?;

    let mut resp = client.send_async(request).await.map_err(|e| {
        warn!(error = %e, CATALOG_URL, "failed to fetch catalog");
        AgentError::Config {
            message: format!("failed to fetch catalog from {CATALOG_URL}: {e}"),
        }
    })?;

    let status = resp.status().as_u16();
    if status != 200 {
        return Err(AgentError::Api {
            status,
            message: format!("catalog fetch returned HTTP {status}"),
        });
    }

    let text = resp.text().await.map_err(|e| AgentError::Config {
        message: format!("failed to read catalog response body: {e}"),
    })?;

    serde_json::from_str(&text).map_err(|e| AgentError::Config {
        message: format!("failed to parse catalog JSON: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_storage::auth::ProviderCredentials;
    use tempfile::TempDir;

    fn empty_state_dir() -> (TempDir, maki_storage::StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = maki_storage::StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn state_dir_with_opencode_key(key: &str) -> (TempDir, maki_storage::StateDir) {
        let (tmp, dir) = empty_state_dir();
        maki_storage::auth::save_provider_credentials(
            &dir,
            "opencode-zen",
            &ProviderCredentials {
                api_key: key.to_string(),
            },
        )
        .unwrap();
        (tmp, dir)
    }

    #[test]
    fn endpoint_type_dispatch() {
        assert_eq!(
            EndpointType::from_npm("@ai-sdk/anthropic"),
            EndpointType::Messages
        );
        assert_eq!(
            EndpointType::from_npm("@ai-sdk/openai-compatible"),
            EndpointType::ChatCompletions
        );
    }

    #[test]
    fn build_auth_header_variants() {
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec![],
            npm: "@ai-sdk/anthropic".into(),
            api: Some("https://test.api/v1".into()),
            models: HashMap::new(),
        };
        let messages_auth = provider.build_auth("key-123".into(), EndpointType::Messages);
        assert_eq!(
            messages_auth.headers,
            vec![("x-api-key".into(), "key-123".into())]
        );
        let chat_auth = provider.build_auth("key-123".into(), EndpointType::ChatCompletions);
        assert_eq!(
            chat_auth.headers,
            vec![("authorization".into(), "Bearer key-123".into())]
        );
    }

    #[test]
    fn catalog_provider_roundtrip_json() {
        let provider = CatalogProvider {
            name: "Test Provider".into(),
            env: vec!["TEST_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://test.api/v1".into()),
            models: HashMap::from([(
                "test-model".into(),
                CatalogModel {
                    limit: Some(CatalogLimits {
                        context: Some(128_000),
                        input: None,
                        output: Some(64_000),
                    }),
                    cost: Some(CatalogCost {
                        input: Some(0.5),
                        output: Some(1.5),
                        cache_read: Some(0.1),
                        cache_write: Some(0.2),
                    }),
                    provider: None,
                    attachment: None,
                    modalities: None,
                    reasoning: None,
                },
            )]),
        };

        let json = serde_json::to_string_pretty(&provider).unwrap();
        let deserialized: CatalogProvider = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "Test Provider");
        assert_eq!(deserialized.npm, "@ai-sdk/openai-compatible");
        assert!(deserialized.models.contains_key("test-model"));
        let model = &deserialized.models["test-model"];
        let cost = model.cost.as_ref().unwrap();
        assert_eq!(cost.input, Some(0.5));
        assert_eq!(cost.output, Some(1.5));
    }

    #[test]
    fn catalog_index_roundtrip_json() {
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "test-provider".into(),
            CatalogProvider {
                name: "Test".into(),
                env: vec![],
                npm: "@ai-sdk/openai".into(),
                api: Some("https://test.api/v1".into()),
                models: HashMap::from([(
                    "test-model".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                        attachment: None,
                        modalities: None,
                        reasoning: None,
                    },
                )]),
            },
        );

        let json = serde_json::to_string_pretty(&providers).unwrap();
        let deserialized: CatalogIndex = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.len(), 1);
        assert!(deserialized.contains_key("test-provider"));
    }

    #[test]
    fn catalog_provider_missing_optional_fields() {
        let json = r#"{
            "name": "Minimal",
            "npm": "@ai-sdk/openai",
            "models": {}
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        assert_eq!(provider.name, "Minimal");
        assert!(provider.env.is_empty());
        assert!(provider.api.is_none());
        assert!(provider.models.is_empty());
    }

    #[test]
    fn catalog_model_missing_cost_and_provider() {
        let json = r#"{
            "name": "Test",
            "npm": "@ai-sdk/openai",
            "api": "https://test.api/v1",
            "models": {
                "m1": { "limit": {"context": 64000} }
            }
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        let model = &provider.models["m1"];
        assert_eq!(model.limit.as_ref().unwrap().context, Some(64000));
        assert!(model.cost.is_none());
        assert!(model.provider.is_none());
    }

    #[test]
    fn resolve_provider_key_returns_none_when_env_unset() {
        let (_tmp, state_dir) = empty_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["MAKI_TEST_UNUSED_VAR_1".into()],
            npm: "@ai-sdk/openai".into(),
            api: None,
            models: HashMap::new(),
        };
        let key = resolve_provider_key(&provider, &state_dir, "opencode", |v| {
            assert_eq!(v, "MAKI_TEST_UNUSED_VAR_1");
            None
        });
        assert!(key.is_none());
    }

    #[test]
    fn resolve_provider_key_returns_saved_when_env_unset() {
        let (_tmp, state_dir) = state_dir_with_opencode_key("from-saved");
        let provider = CatalogProvider {
            name: "Opencode".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/v1".into()),
            models: HashMap::new(),
        };
        let key = resolve_provider_key(&provider, &state_dir, "opencode-zen", |_| None);
        assert_eq!(key, Some("from-saved".into()));
    }

    #[test]
    fn resolve_provider_key_env_takes_priority_over_saved() {
        let (_tmp, state_dir) = state_dir_with_opencode_key("from-saved");
        let provider = CatalogProvider {
            name: "Opencode".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/v1".into()),
            models: HashMap::new(),
        };
        let key = resolve_provider_key(&provider, &state_dir, "opencode-zen", |v| {
            if v == "OPENCODE_API_KEY" {
                Some("from-env".into())
            } else {
                None
            }
        });
        assert_eq!(key, Some("from-env".into()));
    }

    #[test]
    fn resolve_provider_key_returns_none_when_nothing_available() {
        let (_tmp, state_dir) = empty_state_dir();
        let provider = CatalogProvider {
            name: "Opencode".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/v1".into()),
            models: HashMap::new(),
        };
        let key = resolve_provider_key(&provider, &state_dir, "opencode-zen", |_| None);
        assert!(key.is_none());
    }

    use test_case::test_case;

    #[test_case(Some(true),  None,                        true  ; "vision_from_attachment_true")]
    #[test_case(None,        Some(&["image"][..]),        true  ; "vision_from_modalities_image")]
    #[test_case(None,        Some(&["text", "audio"][..]), false ; "vision_not_from_nonimage_modalities")]
    #[test_case(None,        None,                        false ; "vision_default_false")]
    fn model_meta_vision(
        attachment: Option<bool>,
        modalities_input: Option<&[&str]>,
        expected: bool,
    ) {
        let modalities = modalities_input.map(|input| CatalogModalities {
            input: input.iter().copied().map(String::from).collect(),
        });
        let model = CatalogModel {
            limit: None,
            cost: None,
            provider: None,
            attachment,
            modalities,
            reasoning: None,
        };
        let meta = model_meta(&model, "test", EndpointType::ChatCompletions);
        assert_eq!(meta.vision, expected);
    }

    #[test]
    fn model_meta_resolves_per_model_api_format() {
        let model_with_npm_override = CatalogModel {
            limit: None,
            cost: None,
            provider: Some(CatalogShape {
                shape: None,
                npm: Some("@ai-sdk/anthropic".into()),
            }),
            attachment: None,
            modalities: None,
            reasoning: None,
        };
        let meta = model_meta(
            &model_with_npm_override,
            "test",
            EndpointType::ChatCompletions,
        );
        assert_eq!(meta.api_format, EndpointType::Messages);
    }
}
