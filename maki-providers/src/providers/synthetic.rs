use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::Value;

use maki_config::providers::Protocol;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec,
};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts};

const SLUG: &str = "synthetic";
const DISPLAY_NAME: &str = "Synthetic";
const ENV_VAR: &str = "SYNTHETIC_API_KEY";
const BASE_URL: &str = "https://api.synthetic.new/openai/v1";
const DEFAULT_MODEL: &str = "synthetic/hf:moonshotai/Kimi-K2.5";
const LOGIN_URL: &str = "https://synthetic.new";
const MAX_TOKENS_FIELD: &str = "max_completion_tokens";
const FEATURES: &str = "Reasoning effort support (low/medium/high), open-weight models";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: SLUG,
    api_key_env: ENV_VAR,
    base_url: BASE_URL,
    max_tokens_field: MAX_TOKENS_FIELD,
    include_stream_usage: false,
    provider_name: DISPLAY_NAME,
};

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Synthetic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(32_000),
    fallback_context_window: 128_000,
    models: models(),
    pricing_schedule: None,
    native: Some(Native {
        new: create,
        with_auth: create_with_auth,
        aperture: Some(ApertureRoute {
            path_prefix: DEFAULT_PATH_PREFIX,
        }),
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Synthetic::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Synthetic::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["hf:moonshotai/Kimi-K2.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Synthetic,
            vision: false,
            default: true,
            pricing: ModelPricing::per_million(0.45, 3.40, 0.00, 0.00),
            max_output_tokens: Some(131072),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["hf:deepseek-ai/DeepSeek-V3.2"],
            tier: ModelTier::Medium,
            family: ModelFamily::Synthetic,
            vision: false,
            default: true,
            pricing: ModelPricing::per_million(0.56, 1.68, 0.00, 0.00),
            max_output_tokens: Some(131072),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["hf:zai-org/GLM-4.7-Flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Synthetic,
            vision: false,
            default: true,
            pricing: ModelPricing::per_million(0.10, 0.50, 0.00, 0.00),
            max_output_tokens: Some(131072),
            context_window: 200_000,
        },
    ];
    MODELS
}

pub struct Synthetic {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Synthetic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve(CONFIG.slug, CONFIG.api_key_env)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(
                CONFIG.slug,
                pool.current(),
            )?)),
            key_pool: Some(pool),
            system_prefix: None,
        })
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
}

impl Provider for Synthetic {
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
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking
                .apply_reasoning_effort(&mut body, &dialect::STANDARD, model);
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

    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.key_pool.as_ref()?,
            &self.auth,
            KeyHeader::Bearer,
        ))
    }
}
