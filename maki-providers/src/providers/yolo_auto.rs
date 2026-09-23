use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::Value;

use maki_config::providers::Protocol;

use crate::model::{Model, ModelFamily, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GENERIC_DISCOVERY_NOTE, GeneratedDocs, LoginConfig,
    NO_CURATED_MODELS, Native, ProviderSpec,
};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts};

const SLUG: &str = "yolo-auto";
const DISPLAY_NAME: &str = "Yolo-Auto";
const ENV_VAR: &str = "YOLO_AUTO_API_KEY";
const BASE_URL: &str = "https://yolo-auto.com/v1";
const DEFAULT_MODEL: &str = "yolo-auto/yolo";
const LOGIN_URL: &str = "https://yolo-auto.com";
const MAX_TOKENS_FIELD: &str = "max_tokens";
const FEATURES: &str = "OpenAI-compatible chat completions with tool calling and reasoning effort levels. Flat-rate plans, no per-token billing";

/// The catalogue is the API's, not ours: the two aliases named below are
/// stable, and every other id the key can reach arrives from `/v1/models`,
/// which the compat lister already reads for the enforced window.
const CATALOG_NOTE: &str = "The `yolo` and `yolo-small` aliases are always available, and the rest of the catalogue is served live from `/v1/models`. Plans are flat-rate, so no per-token rate is quoted.";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: SLUG,
    api_key_env: ENV_VAR,
    base_url: BASE_URL,
    max_tokens_field: MAX_TOKENS_FIELD,
    include_stream_usage: true,
    provider_name: DISPLAY_NAME,
};

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 262_144,
    models_toml: NO_CURATED_MODELS,
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
        catalog: CatalogDoc::Discovered(GENERIC_DISCOVERY_NOTE),
        trailing_notes: &[CATALOG_NOTE],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(YoloAuto::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(YoloAuto::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());

pub struct YoloAuto {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl YoloAuto {
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

impl Provider for YoloAuto {
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

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
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
