//! Models.dev catalog: fetching, caching, and provider dispatch.
//!
//! The catalog is fetched from `models.dev/api.json` and cached locally. Each
//! provider in the catalog becomes a [`ProviderData`], and models are looked up
//! at stream time via [`CatalogData::lookup`].
//!
//! Per-slug providers (e.g. a user who configures `nvidia/...` directly) get
//! their own [`CatalogProvider`] instance, created from the same
//! [`ProviderData`].

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use flume::Sender;
use isahc::config::{Configurable, VersionNegotiation};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_config::providers::{ProvidersConfig, builtin_provider};
use serde_json::Value;
use tracing::{debug, warn};

use maki_storage::StateDir;
use maki_storage::auth::load_provider_credentials;
use maki_storage::id::SessionRef;

use crate::model::{Model, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::providers::anthropic::shared;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, Timeouts, http_client, opencode, user_agent};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

const MESSAGES_PATH: &str = "/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

const BLOCKED_PROVIDER_IN_CATALOG: &[&str] = &["zai", "zai-coding-plan", "github-copilot"];

/// Builtins with no native client of their own, so [`builtin_provider`]
/// knowing them must not drop them from the catalog.
pub(crate) const CATALOG_BACKED_BUILTINS: &[&str] = opencode::SLUGS;

/// Provider modules own their entry here; the catalog only reads it.
const QUIRKS: &[(&[&str], ProviderQuirks)] = &[(opencode::SLUGS, opencode::QUIRKS)];

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(86400);

const ALLOWED_NPM: &[&str] = &["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    ChatCompletions,
    Messages,
}

/// Behaviour a provider needs beyond what models.dev publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderQuirks {
    pub free_tier: Option<FreeTier>,
    pub session_header: Option<&'static str>,
}

/// A no-key tier that unlocks only zero-priced models, and only after the
/// user opts in via `providers.<config_slug>.enable_free_models`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeTier {
    pub public_key: &'static str,
    pub config_slug: &'static str,
}

impl FreeTier {
    fn opted_in(&self) -> bool {
        ProvidersConfig::load()
            .get(self.config_slug)
            .and_then(|def| def.enable_free_models)
            .unwrap_or(false)
    }
}

fn quirks_for(slug: &str) -> ProviderQuirks {
    QUIRKS
        .iter()
        .find(|(slugs, _)| slugs.contains(&slug))
        .map(|(_, quirks)| *quirks)
        .unwrap_or_default()
}

/// Provider metadata from the catalog, exposed for the login UI.
#[derive(Clone, Debug)]
pub struct ProviderData {
    pub slug: String,
    pub display_name: String,
    /// Environment variable names for API keys
    pub env_keys: Vec<String>,
    /// API base URL
    pub base_url: Option<String>,
    /// NPM package name. Used to determine how to interact with the model.
    pub npm: String,
    /// API format (ChatCompletions or Messages)
    pub api_format: EndpointType,
    /// Models for this provider
    pub models: HashMap<String, CatalogMeta>,
    pub quirks: ProviderQuirks,
}

fn is_free_model(meta: &CatalogMeta) -> bool {
    meta.input_price == 0.0 && meta.output_price == 0.0
}

impl ProviderData {
    pub(crate) fn new(
        slug: String,
        catalog_provider: &schema::CatalogProvider,
        api_format: EndpointType,
        models: HashMap<String, CatalogMeta>,
    ) -> Self {
        Self {
            quirks: quirks_for(&slug),
            slug,
            display_name: catalog_provider.name.clone(),
            env_keys: catalog_provider.env.clone(),
            base_url: catalog_provider.api.clone(),
            npm: catalog_provider.npm.clone(),
            api_format,
            models,
        }
    }

    pub fn load_key_from_storage(&self, state_dir: &StateDir) -> Option<String> {
        let creds = load_provider_credentials(state_dir, &self.slug)?;
        Some(creds.api_key)
    }

    pub fn resolve_api_key(&self, state_dir: &StateDir) -> Option<String> {
        for var in &self.env_keys {
            if let Ok(val) = std::env::var(var) {
                debug!(provider = %self.display_name, var = %var, "api key resolved from env");
                return Some(val);
            }
        }
        if let Some(key) = self.load_key_from_storage(state_dir) {
            debug!(provider = %self.display_name, "api key resolved from storage");
            return Some(key);
        }
        None
    }

    pub fn env_key_set(&self) -> Option<&str> {
        self.env_keys
            .iter()
            .find(|e| std::env::var(e).is_ok())
            .map(|s| s.as_str())
    }

    /// Reads the config fresh so an edit takes effect without a restart.
    pub(crate) fn free_models_enabled(&self) -> bool {
        self.quirks.free_tier.is_some_and(|tier| tier.opted_in())
    }

    pub(crate) fn request_auth(
        &self,
        mut auth: ResolvedAuth,
        session_id: Option<&SessionRef>,
    ) -> ResolvedAuth {
        if let (Some(header), Some(sid)) = (self.quirks.session_header, session_id) {
            auth.set_header(header, sid.to_string());
        }
        auth
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        match self.npm.as_str() {
            "@ai-sdk/anthropic" => vec![("x-api-key".into(), api_key.into())],
            _ => vec![("authorization".into(), format!("Bearer {api_key}"))],
        }
    }

    fn auth_for(&self, api_key: &str) -> Result<ResolvedAuth, AgentError> {
        Ok(ResolvedAuth::new(&self.slug, self.auth_headers(api_key))?
            .with_base_url(self.base_url.clone()))
    }

    pub fn build_auth(&self, state_dir: &StateDir) -> Result<Authentication, AgentError> {
        if let Some(key) = self.resolve_api_key(state_dir) {
            return Ok(Authentication::KeyBased(self.auth_for(&key)?));
        }
        match self.quirks.free_tier {
            Some(tier) => Ok(Authentication::FreeKey(
                self.auth_for(tier.public_key)?,
                tier,
            )),
            None => Ok(Authentication::NoAuth),
        }
    }

    pub fn resolve_auth(&self, state_dir: &StateDir) -> Result<Option<ResolvedAuth>, AgentError> {
        Ok(match self.build_auth(state_dir)? {
            Authentication::KeyBased(auth) | Authentication::FreeKey(auth, _) => Some(auth),
            Authentication::NoAuth => None,
        })
    }

    pub(crate) fn catalog_auth(
        &self,
        state_dir: &StateDir,
        allow_free_fallback: bool,
    ) -> Result<CatalogAuth, AgentError> {
        Ok(match self.build_auth(state_dir)? {
            Authentication::KeyBased(auth) => CatalogAuth::Keyed(auth),
            Authentication::FreeKey(auth, _) if allow_free_fallback => CatalogAuth::FreeOnly(auth),
            Authentication::FreeKey(_, tier) => CatalogAuth::Gated(tier),
            Authentication::NoAuth => {
                let slug = &self.slug;
                return Err(config_error(format!(
                    "no API key configured for provider '{slug}'; run `maki auth login {slug}`"
                )));
            }
        })
    }

    pub fn available_models(
        &self,
        state_dir: &StateDir,
        enable_free_models: bool,
    ) -> Vec<ModelInfo> {
        // A broken `[<slug>.headers]` hides the models instead of killing the
        // whole listing; creating the provider reports the real error.
        let Ok(auth) = self.build_auth(state_dir) else {
            return Vec::new();
        };
        let mut models: Vec<ModelInfo> = self
            .models
            .iter()
            .filter_map(|(model_id, meta)| {
                let is_free = is_free_model(meta);
                if is_free && !enable_free_models {
                    return None;
                }
                let allow_model = match &auth {
                    Authentication::KeyBased(_) => true,
                    Authentication::FreeKey(..) => is_free,
                    Authentication::NoAuth => false,
                };
                if !allow_model {
                    return None;
                }
                Some(meta.model_info(model_id))
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
}

#[derive(Clone, Debug)]
pub struct CatalogMeta {
    pub context: u32,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub supports_thinking: bool,
    pub supports_vision: bool,
}

impl CatalogMeta {
    fn model_info(&self, model_id: &str) -> ModelInfo {
        ModelInfo {
            id: model_id.to_string(),
            context_window: Some(self.context),
            max_output_tokens: Some(self.output),
            pricing: Some(ModelPricing::per_token(
                self.input_price,
                self.output_price,
                self.cache_write,
                self.cache_read,
            )),
            supports_thinking: Some(self.supports_thinking),
            supports_vision: Some(self.supports_vision),
            tier: None,
            provider_info: None,
        }
    }
}

#[derive(Clone)]
pub enum Authentication {
    /// User has a configured API key — all models accessible
    KeyBased(ResolvedAuth),
    /// The provider's public token; unlocks zero-priced models only
    FreeKey(ResolvedAuth, FreeTier),
    /// No authentication available
    NoAuth,
}

pub(crate) struct CatalogData {
    providers: HashMap<String, ProviderData>,
    pub(crate) state_dir: StateDir,
}

impl CatalogData {
    fn empty(state_dir: StateDir) -> Self {
        Self {
            providers: HashMap::new(),
            state_dir,
        }
    }

    fn from_index(index: schema::CatalogIndex, state_dir: &StateDir) -> Self {
        let mut providers = HashMap::new();

        for (provider_id, provider) in index {
            if !ALLOWED_NPM.contains(&provider.npm.as_str()) {
                debug!(npm = %provider.npm, "skipping provider: unsupported npm package");
                continue;
            }
            if BLOCKED_PROVIDER_IN_CATALOG.contains(&provider_id.as_str()) {
                debug!(
                    provider = &provider_id,
                    "skipping providers from the catalog"
                );
                continue;
            }

            let Some(_base_url) = &provider.api else {
                debug!(provider = %provider_id, "skipping: no API URL in catalog");
                continue;
            };

            if builtin_provider(&provider_id).is_some()
                && !CATALOG_BACKED_BUILTINS.contains(&provider_id.as_str())
            {
                debug!(
                    provider = &provider_id,
                    "skipping providers supported by built-in providers"
                );
                continue;
            }

            let api_format = determine_catalog_format(&provider.npm);

            let mut models = HashMap::new();
            for (model_id, model_data) in &provider.models {
                let input_price = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.input)
                    .unwrap_or(0.0);
                let output_price = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.output)
                    .unwrap_or(0.0);

                let context = model_data
                    .limit
                    .as_ref()
                    .and_then(|l| l.context)
                    .unwrap_or(128_000);
                let output = model_data
                    .limit
                    .as_ref()
                    .and_then(|l| l.output)
                    .unwrap_or(64_000);

                let cache_read = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.cache_read)
                    .unwrap_or(0.0);
                let cache_write = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.cache_write)
                    .unwrap_or(0.0);

                let supports_vision = model_data.attachment
                    || model_data
                        .modalities
                        .as_ref()
                        .is_some_and(|m| m.input.iter().any(|s| s == "image"));
                let supports_thinking = model_data.reasoning;

                models.insert(
                    model_id.clone(),
                    CatalogMeta {
                        context,
                        output,
                        input_price,
                        output_price,
                        cache_read,
                        cache_write,
                        supports_thinking,
                        supports_vision,
                    },
                );
            }

            let model_count = models.len();
            let provider_data =
                ProviderData::new(provider_id.clone(), &provider, api_format, models);
            providers.insert(provider_id.clone(), provider_data);

            debug!(
                provider = %provider_id,
                models = model_count,
                format = %provider.npm,
                "catalog provider registered",
            );
        }

        Self {
            providers,
            state_dir: state_dir.clone(),
        }
    }

    pub(crate) fn provider(&self, slug: &str) -> Option<&ProviderData> {
        self.providers.get(slug)
    }

    pub(crate) fn lookup(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<(&CatalogMeta, &ProviderData), AgentError> {
        let provider_data = self
            .providers
            .get(provider)
            .ok_or_else(|| config_error(format!("provider '{provider}' not found in catalog")))?;
        let meta = provider_data.models.get(model_id).ok_or_else(|| {
            config_error(format!(
                "model '{provider}/{model_id}' not found in catalog"
            ))
        })?;
        Ok((meta, provider_data))
    }

    fn all_providers(&self) -> Vec<ProviderData> {
        let mut providers: Vec<ProviderData> = self.providers.values().cloned().collect();
        providers.sort_by_key(|p| p.display_name.to_lowercase());
        providers
    }
}

pub(crate) fn config_error(message: String) -> AgentError {
    AgentError::Config { message }
}

static CATALOG_PROVIDER_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "catalog",
};

static SHARED_CATALOG: OnceLock<Mutex<CatalogData>> = OnceLock::new();

pub(crate) fn init_shared_catalog_if_needed() -> &'static Mutex<CatalogData> {
    SHARED_CATALOG.get_or_init(|| Mutex::new(init_catalog_blocking()))
}

/// Loads the models.dev catalog from the on-disk cache, fetching once if the
/// cache is cold or stale. Blocks, so only call it from startup paths; every
/// other lookup must stay on the `*_if_available` variants.
pub fn warm_catalog() {
    init_shared_catalog_if_needed();
}

/// Returns the list of all providers in alphabetical order.
pub fn catalog_providers() -> Vec<ProviderData> {
    let guard = init_shared_catalog_if_needed().lock().unwrap();
    guard.all_providers()
}

/// Returns the list of catalog providers only if the catalog has already been downloaded.
/// Does NOT trigger downloading.
pub fn catalog_providers_if_available() -> Option<Vec<ProviderData>> {
    let catalog = SHARED_CATALOG.get()?;
    let guard = catalog.lock().ok()?;
    Some(guard.all_providers())
}

/// Returns the ProviderData for a specific catalog provider, if found.
pub fn catalog_provider(provider_id: &str) -> Option<ProviderData> {
    let guard = init_shared_catalog_if_needed().lock().ok()?;
    guard.providers.get(provider_id).cloned()
}

/// Non-blocking variant of [`catalog_provider`]: returns the `ProviderData` only
/// if the catalog has already been downloaded. Never triggers a fetch.
pub fn catalog_provider_if_available(provider_id: &str) -> Option<ProviderData> {
    with_provider_if_available(provider_id, ProviderData::clone)
}

/// Borrows under the lock instead of cloning: the picker calls this once per
/// row through `Model::is_free`, and a `ProviderData` carries its whole models map.
fn with_provider_if_available<T>(slug: &str, f: impl FnOnce(&ProviderData) -> T) -> Option<T> {
    let guard = SHARED_CATALOG.get()?.lock().ok()?;
    guard.providers.get(slug).map(f)
}

/// Non-blocking availability check for catalog-backed providers: true only when
/// the catalog is already warm, contains the slug, and auth resolves (API key or
/// free access). Never triggers a fetch, unlike [`try_create`].
pub fn available_if_warm(slug: &str) -> bool {
    let Some(data) = catalog_provider_if_available(slug) else {
        return false;
    };
    let Ok(state_dir) = StateDir::resolve() else {
        return false;
    };
    matches!(data.resolve_auth(&state_dir), Ok(Some(_)))
}

fn catalog_cache_path() -> Option<PathBuf> {
    let dir = maki_storage::paths::cache_dir().ok()?;
    Some(dir.join(CATALOG_CACHE_FILE))
}

async fn load_cached_catalog_async() -> Option<schema::CatalogIndex> {
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
    let index: schema::CatalogIndex = serde_json::from_str(&text).ok()?;
    debug!("loaded catalog from cache");
    Some(index)
}

async fn save_cached_catalog_async(index: &schema::CatalogIndex) {
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

async fn fetch_remote_catalog_async(
    client: &HttpClient,
) -> Result<schema::CatalogIndex, AgentError> {
    let request = Request::builder()
        .uri(CATALOG_URL)
        .header("user-agent", user_agent())
        .body(())?;

    let mut resp = client.send_async(request).await.map_err(|e| {
        warn!(error = %e, CATALOG_URL, "failed to fetch catalog");
        config_error(format!("failed to fetch catalog from {CATALOG_URL}: {e}"))
    })?;

    let status = resp.status().as_u16();
    if status != 200 {
        // Drain the body so isahc can reuse the connection
        let _ = resp.text().await;
        return Err(AgentError::Api {
            status,
            message: format!("catalog fetch returned HTTP {status}"),
        });
    }

    let text = resp
        .text()
        .await
        .map_err(|e| config_error(format!("failed to read catalog response body: {e}")))?;

    serde_json::from_str(&text)
        .map_err(|e| config_error(format!("failed to parse catalog JSON: {e}")))
}

fn determine_catalog_format(npm: &str) -> EndpointType {
    match npm {
        "@ai-sdk/anthropic" => EndpointType::Messages,
        _ => EndpointType::ChatCompletions,
    }
}

// Try cache first, then fetch from remote.
fn init_catalog_blocking() -> CatalogData {
    let state_dir = match StateDir::resolve() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to resolve state dir");
            return CatalogData::empty(StateDir::from_path("".into()));
        }
    };

    if let Some(index) = smol::block_on(load_cached_catalog_async()) {
        return CatalogData::from_index(index, &state_dir);
    }

    let client = isahc::HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .low_speed_timeout(1, Duration::from_secs(30))
        // curl carries http2 for OTLP.
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .expect("failed to build catalog HTTP client");

    match smol::block_on(fetch_remote_catalog_async(&client)) {
        Ok(index) => {
            smol::block_on(save_cached_catalog_async(&index));
            CatalogData::from_index(index, &state_dir)
        }
        Err(e) => {
            warn!(error = %e, "catalog fetch failed, using empty catalog");
            CatalogData::empty(state_dir)
        }
    }
}

/// Wire layer shared by `CatalogProvider` and `Opencode`, so a header fix
/// lands once.
pub(crate) struct CatalogTransport {
    chat_compat: OpenAiCompatProvider,
    client: HttpClient,
    stream_timeout: Duration,
}

impl CatalogTransport {
    pub(crate) fn new(timeouts: Timeouts) -> Self {
        Self {
            chat_compat: OpenAiCompatProvider::new(&CATALOG_PROVIDER_CONFIG, timeouts),
            client: http_client(timeouts),
            stream_timeout: timeouts.stream,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn stream(
        &self,
        api_format: EndpointType,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        match api_format {
            EndpointType::ChatCompletions => {
                let mut body = self.chat_compat.build_body(model, messages, system, tools);
                opts.thinking
                    .apply_reasoning_effort(&mut body, &dialect::PREFER_HIGH, model);
                self.chat_compat
                    .do_stream(model, &[], &body, event_tx, auth)
                    .await
            }
            EndpointType::Messages => {
                let system_blocks = vec![shared::SystemBlock {
                    r#type: "text",
                    text: system,
                    cache_control: Some(shared::EPHEMERAL),
                }];
                let mut body = shared::build_request_body_with_system(
                    model,
                    messages,
                    &system_blocks,
                    tools,
                    opts.thinking,
                );
                body["model"] = serde_json::json!(model.id);
                body["stream"] = serde_json::json!(true);
                let request = auth
                    .configure_request(
                        Request::builder()
                            .method("POST")
                            .uri(format!(
                                "{}{}",
                                auth.base_url.as_deref().unwrap_or(""),
                                MESSAGES_PATH
                            ))
                            .header("user-agent", user_agent())
                            .header("content-type", "application/json")
                            .header("anthropic-version", ANTHROPIC_VERSION),
                    )
                    .body(serde_json::to_vec(&body)?)?;
                debug!(model = %model.id, "sending Anthropic-format request via catalog");
                let response = self.client.send_async(request).await?;
                if response.status().as_u16() == 200 {
                    crate::providers::anthropic::parse_sse(response, event_tx, self.stream_timeout)
                        .await
                } else {
                    Err(AgentError::from_response(response).await)
                }
            }
        }
    }
}

/// `Provider` for a single catalog sub-provider. Created with a resolved
/// `ProviderData` (from `maki_providers::catalog_provider(slug)`) plus the
/// auth that the models.dev catalog would have used for that sub-provider.
pub struct CatalogProvider {
    data: ProviderData,
    auth: CatalogAuth,
    transport: CatalogTransport,
}

/// Which models the resolved auth unlocks: a real key unlocks all, the
/// no-key `enable_free_models` opt-in unlocks free models only, and `Gated`
/// unlocks nothing. `Gated` holds no credentials at all, so it can never send
/// the public token by accident: discovery lists nothing, and only an actual
/// attempt to stream tells the user to log in or opt in.
pub(crate) enum CatalogAuth {
    Keyed(ResolvedAuth),
    FreeOnly(ResolvedAuth),
    Gated(FreeTier),
}

impl CatalogAuth {
    pub(crate) fn unlocked(&self, slug: &str) -> Result<&ResolvedAuth, AgentError> {
        match self {
            Self::Keyed(auth) | Self::FreeOnly(auth) => Ok(auth),
            Self::Gated(tier) => Err(config_error(format!(
                "provider '{slug}' has no API key; run `maki auth login {slug}` or set providers.{}.enable_free_models = true to use its free models",
                tier.config_slug
            ))),
        }
    }
}

impl CatalogProvider {
    pub fn new(
        data: ProviderData,
        state_dir: &StateDir,
        timeouts: Timeouts,
        allow_free_fallback: bool,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            auth: data.catalog_auth(state_dir, allow_free_fallback)?,
            data,
            transport: CatalogTransport::new(timeouts),
        })
    }
}

impl Provider for CatalogProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.unlocked(&self.data.slug)?.clone();
            let auth = self.data.request_auth(auth, session_id);
            let meta = self
                .data
                .models
                .get(&model.id)
                .ok_or_else(|| AgentError::Config {
                    message: format!(
                        "model '{}' not found from provider '{}'",
                        model.id, self.data.slug
                    ),
                })?;
            let stream_model = Model {
                id: model.id.clone(),
                max_output_tokens: Some(meta.output),
                context_window: meta.context,
                ..model.clone()
            };
            self.transport
                .stream(
                    self.data.api_format,
                    &stream_model,
                    messages,
                    system,
                    tools,
                    event_tx,
                    &auth,
                    &opts,
                )
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            Ok(self
                .data
                .models
                .iter()
                .filter(|(_, meta)| match &self.auth {
                    CatalogAuth::Keyed(_) => true,
                    CatalogAuth::FreeOnly(_) => is_free_model(meta),
                    CatalogAuth::Gated(_) => false,
                })
                .map(|(model_id, meta)| meta.model_info(model_id))
                .collect())
        })
    }
}

#[cfg(test)]
pub(crate) fn seed_catalog_for_tests(index: schema::CatalogIndex, state_dir: StateDir) {
    let _ = SHARED_CATALOG.set(Mutex::new(CatalogData::from_index(index, &state_dir)));
}

#[cfg(test)]
pub(crate) fn warm_empty_catalog_for_tests(state_dir: StateDir) {
    seed_catalog_for_tests(HashMap::new(), state_dir);
}

/// Defers catalog resolution to first use so that provider construction
/// never blocks on a cold-cache models.dev fetch (which would freeze the UI
/// event loop or stall model discovery). Resolution errors, including an
/// unknown slug, surface on the first request instead.
struct LazyCatalogProvider {
    slug: String,
    timeouts: Timeouts,
    inner: OnceLock<Result<CatalogProvider, String>>,
}

impl LazyCatalogProvider {
    async fn resolve(&self) -> Result<&CatalogProvider, AgentError> {
        if self.inner.get().is_none() {
            let slug = self.slug.clone();
            let timeouts = self.timeouts;
            let created = smol::unblock(move || create_resolved(&slug, timeouts)).await;
            let _ = self.inner.set(created.map_err(|e| e.to_string()));
        }
        match self.inner.get().expect("set above") {
            Ok(provider) => Ok(provider),
            Err(message) => Err(AgentError::Config {
                message: message.clone(),
            }),
        }
    }
}

impl Provider for LazyCatalogProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            self.resolve()
                .await?
                .stream_message(model, messages, system, tools, event_tx, opts, session_id)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move { self.resolve().await?.list_models().await })
    }
}

fn create_resolved(slug: &str, timeouts: Timeouts) -> Result<CatalogProvider, AgentError> {
    let data = catalog_provider(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown provider '{slug}'"),
    })?;
    let state_dir = StateDir::resolve().map_err(|e| AgentError::Config {
        message: format!("failed to resolve state dir: {e}"),
    })?;
    let allow_free_fallback = data.free_models_enabled();
    CatalogProvider::new(data, &state_dir, timeouts, allow_free_fallback)
}

/// Try to create a `CatalogProvider` for the given slug. Returns `None` if
/// the catalog is warm and the slug is not in it. With a cold catalog it
/// returns a [`LazyCatalogProvider`] instead of blocking on the fetch;
/// membership is then checked on first use.
pub fn try_create(slug: &str, timeouts: Timeouts) -> Option<Result<Box<dyn Provider>, AgentError>> {
    if SHARED_CATALOG.get().is_some() {
        let data = catalog_provider_if_available(slug)?;
        let state_dir = StateDir::resolve().ok()?;
        let allow_free_fallback = data.free_models_enabled();
        return Some(
            CatalogProvider::new(data, &state_dir, timeouts, allow_free_fallback)
                .map(|c| Box::new(c) as Box<dyn Provider>),
        );
    }
    Some(Ok(Box::new(LazyCatalogProvider {
        slug: slug.to_string(),
        timeouts,
        inner: OnceLock::new(),
    })))
}

/// Look up a single model's metadata in the models.dev catalog, only if the
/// catalog has already been downloaded. Never triggers a fetch — callers
/// (e.g. `Model::from_spec`) must tolerate `None` and fall through, since
/// the catalog may still be warming in the background.
pub fn model_meta_if_available(slug: &str, model_id: &str) -> Option<CatalogMetaView> {
    with_provider_if_available(slug, |data| {
        data.models.get(model_id).map(|meta| CatalogMetaView {
            context: meta.context,
            output: meta.output,
            input_price: meta.input_price,
            output_price: meta.output_price,
            cache_read: meta.cache_read,
            cache_write: meta.cache_write,
            supports_thinking: meta.supports_thinking,
            supports_vision: meta.supports_vision,
        })
    })
    .flatten()
}

/// True when the model belongs to a provider with a [`FreeTier`] and is free
/// by the same [`is_free_model`] definition gating `enable_free_models` (zero
/// input and output price). Never triggers a fetch.
pub(crate) fn free_model_if_available(slug: &str, model_id: &str) -> bool {
    with_provider_if_available(slug, |data| {
        data.quirks.free_tier.is_some() && data.models.get(model_id).is_some_and(is_free_model)
    })
    .unwrap_or(false)
}

/// Metadata shape `Model::from_spec` consumes when a spec resolves to a catalog
/// sub-provider. Public so `maki-providers/src/model.rs` can name it without
/// depending on the catalog-internal `CatalogMeta` struct.
#[derive(Debug, Clone, Copy)]
pub struct CatalogMetaView {
    pub context: u32,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub supports_thinking: bool,
    pub supports_vision: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::schema::{CatalogCost, CatalogIndex, CatalogLimits, CatalogModel, CatalogProvider};
    use std::sync::Arc;

    use super::{
        Authentication, CatalogData, CatalogMeta, EndpointType, ProviderData, ProviderQuirks,
        SessionRef, StateDir, available_if_warm, determine_catalog_format, quirks_for,
    };
    use crate::model::{Model, ModelPricing};
    use crate::provider::Provider;
    use crate::providers::{ResolvedAuth, Timeouts, opencode};
    use crate::{AgentError, ModelFamily, ModelTier, RequestOptions};
    use test_case::test_case;

    const SESSION_HEADER: &str = "x-opencode-session";
    const OPT_IN_HINT: &str = "providers.opencode.enable_free_models = true";

    #[test]
    fn new_rejects_no_auth() {
        let (_tmp, state_dir) = temp_state_dir();
        let data = ProviderData {
            slug: "test".into(),
            display_name: "Test".into(),
            env_keys: vec![],
            base_url: None,
            npm: "@ai-sdk/openai".into(),
            api_format: EndpointType::ChatCompletions,
            models: HashMap::new(),
            quirks: ProviderQuirks::default(),
        };
        let result = super::CatalogProvider::new(data, &state_dir, Timeouts::default(), true);
        assert!(matches!(result, Err(AgentError::Config { .. })));
    }

    #[test_case("opencode",    true,  true  ; "zen_with_session")]
    #[test_case("opencode-go", true,  true  ; "go_with_session")]
    #[test_case("opencode-go", false, false ; "go_without_session")]
    #[test_case("anthropic",   true,  false ; "other_provider_never")]
    fn request_auth_sets_opencode_session_header(slug: &str, with_session: bool, expected: bool) {
        let data = ProviderData {
            slug: slug.into(),
            quirks: quirks_for(slug),
            ..opencode_go_provider_data("UNUSED")
        };
        let session = SessionRef::generate();
        let auth = data.request_auth(
            ResolvedAuth::for_test(None, Vec::new()),
            with_session.then_some(&session),
        );
        let header = auth
            .headers
            .iter()
            .find(|(key, _)| key == SESSION_HEADER)
            .map(|(_, value)| value.as_str());
        assert_eq!(header, expected.then(|| session.to_string()).as_deref());
    }

    fn opencode_go_provider_data(env_key: &str) -> ProviderData {
        ProviderData {
            quirks: opencode::QUIRKS,
            slug: "opencode-go".into(),
            display_name: "Opencode Go".into(),
            env_keys: vec![env_key.into()],
            base_url: Some("https://opencode.ai/zen/go/v1".into()),
            npm: "@ai-sdk/openai-compatible".into(),
            api_format: EndpointType::ChatCompletions,
            models: HashMap::from([
                (
                    "paid-model".into(),
                    CatalogMeta {
                        context: 128_000,
                        output: 64_000,
                        input_price: 1.0,
                        output_price: 2.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        supports_thinking: false,
                        supports_vision: false,
                    },
                ),
                (
                    "free-model".into(),
                    CatalogMeta {
                        context: 128_000,
                        output: 64_000,
                        input_price: 0.0,
                        output_price: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        supports_thinking: false,
                        supports_vision: false,
                    },
                ),
            ]),
        }
    }

    #[test]
    fn gated_free_fallback_hides_models_and_refuses_streaming() {
        let (_tmp, state_dir) = temp_state_dir();
        let data = opencode_go_provider_data("MAKI_TEST_OPENCODE_GO_UNSET_KEY_52814");
        let provider =
            super::CatalogProvider::new(data, &state_dir, Timeouts::default(), false).unwrap();
        assert!(smol::block_on(provider.list_models()).unwrap().is_empty());

        let model = Model {
            id: "free-model".into(),
            provider: Arc::from("opencode-go"),
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            pricing: ModelPricing::default(),
            discovered_free: false,
            max_output_tokens: None,
            context_window: 0,
            thinking_fields: None,
        };
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(provider.stream_message(
            &model,
            &[],
            "",
            &serde_json::json!([]),
            &tx,
            RequestOptions::default(),
            None,
        ));
        assert!(matches!(
            result,
            Err(AgentError::Config { message }) if message.contains(OPT_IN_HINT)
        ));
    }

    #[test]
    fn catalog_provider_list_models_free_fallback_hides_paid_models() {
        let (_tmp, state_dir) = temp_state_dir();
        let data = opencode_go_provider_data("MAKI_TEST_OPENCODE_GO_UNSET_KEY_91472");
        let provider =
            super::CatalogProvider::new(data, &state_dir, Timeouts::default(), true).unwrap();
        let models = smol::block_on(provider.list_models()).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["free-model"]);
    }

    #[test]
    fn catalog_provider_list_models_with_key_shows_all() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("MAKI_TEST_OPENCODE_GO_KEY_41827", "real-key") };
        let data = opencode_go_provider_data("MAKI_TEST_OPENCODE_GO_KEY_41827");
        let provider =
            super::CatalogProvider::new(data, &state_dir, Timeouts::default(), false).unwrap();
        let models = smol::block_on(provider.list_models()).unwrap();
        let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["free-model", "paid-model"]);
        unsafe { std::env::remove_var("MAKI_TEST_OPENCODE_GO_KEY_41827") };
    }

    fn temp_state_dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, state_dir)
    }

    #[test]
    fn catalog_format_messages_for_anthropic() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/anthropic"),
            EndpointType::Messages
        );
    }

    #[test]
    fn catalog_format_chat_for_openai_compat() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/openai-compatible"),
            EndpointType::ChatCompletions
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
                    ..Default::default()
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
                        ..Default::default()
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
    fn catalog_provider_resolve_api_key_from_env() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["MAKI_TEST_UNUSED_VAR_1"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/openai".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        // No env var set — returns None (no OPENCODE_API_KEY in env)
        assert!(provider_data.resolve_api_key(&state_dir).is_none());
    }

    #[test]
    fn catalog_provider_resolve_api_key_anthropic_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["ANTHROPIC_SECRET_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "anthropic".into(),
            &provider,
            EndpointType::Messages,
            HashMap::new(),
        );
        // ANTHROPIC_SECRET_KEY is not set.
        assert!(provider_data.resolve_api_key(&state_dir).is_none());
    }

    #[test]
    fn catalog_provider_build_auth_no_key_returns_none() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec![],
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        // No env vars and no OPENCODE_API_KEY fallback — no auth
        assert!(matches!(
            provider_data.build_auth(&state_dir).unwrap(),
            Authentication::NoAuth
        ));
    }

    #[test]
    fn catalog_provider_build_auth_public_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["OPENCODE_API_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "opencode".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir).unwrap();
        match auth {
            Authentication::FreeKey(resolved, _) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer public");
            }
            _ => panic!("expected FreeKey"),
        }
    }

    #[test]
    fn catalog_provider_build_auth_key_based() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("MAKI_TEST_AUTH_KEY", "sk-real-key") };
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["MAKI_TEST_AUTH_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir).unwrap();
        match auth {
            Authentication::KeyBased(resolved) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer sk-real-key");
            }
            _ => panic!("expected KeyBased"),
        }
        unsafe { std::env::remove_var("MAKI_TEST_AUTH_KEY") };
    }

    #[test]
    fn catalog_provider_build_auth_x_api_key() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("MAKI_TEST_ANTHROPIC_KEY", "sk-ant-key") };
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["MAKI_TEST_ANTHROPIC_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "anthropic".into(),
            &provider,
            EndpointType::Messages,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir).unwrap();
        match auth {
            Authentication::KeyBased(resolved) => {
                assert_eq!(resolved.headers[0].0, "x-api-key");
                assert_eq!(resolved.headers[0].1, "sk-ant-key");
            }
            _ => panic!("expected KeyBased"),
        }
        unsafe { std::env::remove_var("MAKI_TEST_ANTHROPIC_KEY") };
    }

    #[test]
    fn catalog_to_data_filters_nonfree_without_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(2.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["MAKI_TEST_VENDOR_KEY_60924".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        let result = CatalogData::from_index(providers, &state_dir);
        // No key filter — all models pass regardless of key status
        let vendor = result.providers.get("some-vendor").unwrap();
        assert_eq!(vendor.models.len(), 2, "all models included");
    }

    #[test]
    fn catalog_to_data_opencode_free_models_without_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let result = CatalogData::from_index(providers, &state_dir);
        // No key filter — all models pass regardless of key status
        let opencode = result.providers.get("opencode").unwrap();
        assert_eq!(opencode.models.len(), 2, "all models included");
        assert!(matches!(
            opencode.build_auth(&state_dir).unwrap(),
            Authentication::FreeKey(..)
        ));
    }

    #[test_case("free-model", true; "free_opencode_model_is_free")]
    #[test_case("paid-output-model", false; "free_input_paid_output_is_not_free")]
    fn model_is_free_uses_catalog_definition(model_id: &str, expected: bool) {
        let (_tmp, state_dir) = temp_state_dir();
        let models = HashMap::from([
            (
                "free-model".into(),
                CatalogModel {
                    limit: None,
                    cost: Some(CatalogCost {
                        input: Some(0.0),
                        output: Some(0.0),
                        cache_read: None,
                        cache_write: None,
                    }),
                    provider: None,
                    ..Default::default()
                },
            ),
            (
                "paid-output-model".into(),
                CatalogModel {
                    limit: None,
                    cost: Some(CatalogCost {
                        input: Some(0.0),
                        output: Some(25.0),
                        cache_read: None,
                        cache_write: None,
                    }),
                    provider: None,
                    ..Default::default()
                },
            ),
        ]);
        let index: CatalogIndex = HashMap::from([(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        )]);
        super::seed_catalog_for_tests(index, state_dir);

        let model = super::Model::from_spec(&format!("opencode/{model_id}")).unwrap();
        assert_eq!(model.is_free(), expected);
    }

    #[test]
    fn catalog_to_data_opencode_all_models_with_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["MAKI_TEST_OPENCODE_ALL_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_OPENCODE_ALL_81274", "real-key") };
        let result = CatalogData::from_index(providers, &state_dir);

        // With key set, has_api_key is true, so all models pass
        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        assert!(matches!(
            opencode.build_auth(&state_dir).unwrap(),
            Authentication::KeyBased(_)
        ));
        unsafe { std::env::remove_var("MAKI_TEST_OPENCODE_ALL_81274") };
    }

    fn opencode_catalog_with_free_and_paid(_env_var: &str) -> CatalogIndex {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );
        providers
    }

    #[test]
    fn catalog_to_data_opencode_hides_free_models_when_disabled() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = opencode_catalog_with_free_and_paid("unused");
        let result = CatalogData::from_index(index, &state_dir);

        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        assert!(matches!(
            opencode.build_auth(&state_dir).unwrap(),
            Authentication::FreeKey(..)
        ));
        // The public key unlocks only free models, and those need the opt-in.
        assert!(opencode.available_models(&state_dir, false).is_empty());
    }

    #[test]
    fn catalog_to_data_all_models_with_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "cheap".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "freebie".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["MAKI_TEST_VENDOR_KEY_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_VENDOR_KEY_81274", "test-key") };
        let result = CatalogData::from_index(providers, &state_dir);
        unsafe { std::env::remove_var("MAKI_TEST_VENDOR_KEY_81274") };

        assert!(
            result
                .providers
                .get("some-vendor")
                .unwrap()
                .models
                .contains_key("cheap")
        );
        assert!(
            result
                .providers
                .get("some-vendor")
                .unwrap()
                .models
                .contains_key("freebie")
        );
    }

    #[test]
    fn catalog_to_data_skips_providers_without_api_url() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut providers = HashMap::new();
        providers.insert(
            "no-api".into(),
            CatalogProvider {
                name: "No API".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: None,
                models: HashMap::new(),
            },
        );

        let result = CatalogData::from_index(providers, &state_dir);
        assert!(result.providers.is_empty());
    }

    #[test]
    fn catalog_to_data_handles_model_id_collisions() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models: HashMap<String, CatalogModel> = HashMap::new();
        models.insert(
            "shared-model".into(),
            CatalogModel {
                limit: Some(CatalogLimits {
                    context: Some(64_000),
                    input: Some(64_000),
                    output: Some(8_000),
                }),
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();

        // Provider "opencode" has "shared-model"
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models: models.clone(),
            },
        );

        // Provider "other-vendor" also has "shared-model"
        providers.insert(
            "other-vendor".into(),
            CatalogProvider {
                name: "Other".into(),
                env: vec!["MAKI_TEST_OTHER_KEY_COLLISION".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://other.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_OTHER_KEY_COLLISION", "key") };
        let result = CatalogData::from_index(providers, &state_dir);
        unsafe { std::env::remove_var("MAKI_TEST_OTHER_KEY_COLLISION") };

        // Both providers' entries are preserved
        assert!(
            result
                .providers
                .get("opencode")
                .unwrap()
                .models
                .contains_key("shared-model")
        );
        assert!(
            result
                .providers
                .get("other-vendor")
                .unwrap()
                .models
                .contains_key("shared-model")
        );
        assert_eq!(result.providers.len(), 2);

        // lookup prefers the "opencode" provider
        // lookup expects "provider/model_id" format
        let (_meta, provider_data) = result.lookup("opencode", "shared-model").unwrap();
        assert_eq!(provider_data.slug, "opencode");
    }

    #[test]
    fn lookup_finds_opencode_own_models() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "opus".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = CatalogData::from_index(providers, &state_dir);
        let (_meta, provider_data) = data.lookup("opencode", "opus").unwrap();
        assert_eq!(provider_data.slug, "opencode");
    }

    #[test]
    fn lookup_finds_model_id_with_slashes() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["MAKI_TEST_NVIDIA_KEY_LOOKUP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_NVIDIA_KEY_LOOKUP", "key") };
        let data = CatalogData::from_index(providers, &state_dir);
        unsafe { std::env::remove_var("MAKI_TEST_NVIDIA_KEY_LOOKUP") };

        // Entry is stored as ("nvidia", "openai/gpt-oss-120b")
        let (_meta, provider_data) = data.lookup("nvidia", "openai/gpt-oss-120b").unwrap();
        assert_eq!(provider_data.slug, "nvidia");
    }

    #[test]
    fn lookup_spec_is_sub_provider_plus_model_id() {
        let (_tmp, state_dir) = temp_state_dir();
        // Simulates the stream_message pattern:
        // lookup key = "{sub_provider}/{model.id}"
        // e.g. "nvidia/openai/gpt-oss-120b"
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["MAKI_TEST_NVIDIA_DIRECT".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_NVIDIA_DIRECT", "key") };
        let data = CatalogData::from_index(providers, &state_dir);
        unsafe { std::env::remove_var("MAKI_TEST_NVIDIA_DIRECT") };

        // The lookup key constructed by stream_message:
        // format!("{}/{}", sub_provider, model.id)
        // = "nvidia/openai/gpt-oss-120b"
        let _key = format!("{}/{}", "nvidia", "openai/gpt-oss-120b");
        let (_meta, provider_data) = data.lookup("nvidia", "openai/gpt-oss-120b").unwrap();
        assert_eq!(provider_data.slug, "nvidia");
    }

    #[test]
    fn lookup_nested_model_id_uses_sub_provider_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "deepseek-ai/DeepSeek-R1".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "fireworks".into(),
            CatalogProvider {
                name: "Fireworks".into(),
                env: vec!["MAKI_TEST_FIREWORKS_DEEP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://fireworks.ai/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("MAKI_TEST_FIREWORKS_DEEP", "key") };
        let data = CatalogData::from_index(providers, &state_dir);
        unsafe { std::env::remove_var("MAKI_TEST_FIREWORKS_DEEP") };

        // stream_message constructs key as "{sub_provider}/{model.id}"
        // = "fireworks/deepseek-ai/DeepSeek-R1"
        let _key = format!("{}/{}", "fireworks", "deepseek-ai/DeepSeek-R1");
        let (_meta, provider_data) = data.lookup("fireworks", "deepseek-ai/DeepSeek-R1").unwrap();
        assert_eq!(provider_data.slug, "fireworks");
    }

    #[test]
    fn catalog_all_models_filters_keyless_providers() {
        let (_tmp, state_dir) = temp_state_dir();
        let auth_dir = state_dir.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("keyed.json"), r#"{"api_key": "sk-abc123"}"#).unwrap();

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "keyed".into(),
            CatalogProvider {
                name: "Keyed".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://keyed.api/v1".into()),
                models: HashMap::from([(
                    "m1".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                        ..Default::default()
                    },
                )]),
            },
        );
        providers.insert(
            "keyless".into(),
            CatalogProvider {
                name: "Keyless".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://keyless.api/v1".into()),
                models: HashMap::from([(
                    "m2".into(),
                    CatalogModel {
                        limit: None,
                        cost: Some(CatalogCost {
                            input: Some(5.0),
                            output: Some(10.0),
                            cache_read: None,
                            cache_write: None,
                        }),
                        provider: None,
                        ..Default::default()
                    },
                )]),
            },
        );

        let data = CatalogData::from_index(providers, &state_dir);
        assert_eq!(data.providers.len(), 2);

        let keyed = data.provider("keyed").unwrap();
        let keyed_ids: Vec<String> = keyed
            .available_models(&state_dir, true)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(keyed_ids, ["m1"]);
        assert!(
            data.provider("keyless")
                .unwrap()
                .available_models(&state_dir, true)
                .is_empty()
        );
    }

    #[test]
    fn catalog_all_models_public_fallback_shows_only_free() {
        let (_tmp, state_dir) = temp_state_dir();
        // Provider with OPENCODE_API_KEY in env but no key set gets "public" fallback.
        // Only free (zero-cost) models should appear in all_models.
        let mut models = HashMap::new();
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(3.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        // No OPENCODE_API_KEY set in env — falls back to "public"
        let data = CatalogData::from_index(providers, &state_dir);

        let opencode = data.providers.get("opencode").unwrap();
        assert_eq!(opencode.models.len(), 2);
        let result = opencode.available_models(&state_dir, true);
        assert_eq!(
            result.len(),
            1,
            "public fallback should only show free models"
        );
        assert_eq!(result[0].id, "free-model");
        assert_eq!(result[0].pricing.as_ref().unwrap().input, 0.0);
    }

    #[test]
    fn catalog_lookup_finds_model_by_opencode_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "gpt-5.1-codex-mini".into(),
            CatalogModel {
                limit: Some(CatalogLimits {
                    context: Some(128_000),
                    input: None,
                    output: Some(16_384),
                }),
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(5.0),
                    cache_read: Some(0.1),
                    cache_write: Some(0.2),
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "OpenCode Zen".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = CatalogData::from_index(providers, &state_dir);

        let (meta, provider_data) = data.lookup("opencode", "gpt-5.1-codex-mini").unwrap();
        assert_eq!(provider_data.slug, "opencode");
        assert_eq!(meta.context, 128_000);
        assert_eq!(meta.output, 16_384);
    }

    #[test]
    fn catalog_lookup_rejects_unknown_provider_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "OpenCode Zen".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models: HashMap::from([("gpt-5.1-codex-mini".into(), CatalogModel::default())]),
            },
        );

        let data = CatalogData::from_index(providers, &state_dir);

        assert!(
            data.lookup("unknown-provider", "gpt-5.1-codex-mini")
                .is_err()
        );
    }

    #[test]
    fn catalog_lookup_finds_model_by_opencode_go_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "opencode-go".into(),
            CatalogProvider {
                name: "OpenCode Go".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/go/v1".into()),
                models: HashMap::from([("fast-model".into(), CatalogModel::default())]),
            },
        );

        let data = CatalogData::from_index(providers, &state_dir);

        let (_meta, provider_data) = data.lookup("opencode-go", "fast-model").unwrap();
        assert_eq!(provider_data.slug, "opencode-go");
    }

    #[test]
    fn available_if_warm_returns_false_when_catalog_cold() {
        assert!(!available_if_warm("opencode-go"));
    }
}

pub(crate) mod schema {
    //! Serde types for the models.dev catalog JSON (`/api.json`).

    use std::collections::HashMap;

    use serde::{Deserialize, Serialize};

    pub type CatalogIndex = HashMap<String, CatalogProvider>;

    #[derive(Deserialize, Serialize)]
    pub struct CatalogProvider {
        pub name: String,
        #[serde(default)]
        pub env: Vec<String>,
        pub npm: String,
        pub api: Option<String>,
        pub models: HashMap<String, CatalogModel>,
    }

    /// Data types a model supports on input and output (e.g. text, image).
    #[derive(Default, Deserialize, Serialize, Clone)]
    pub struct CatalogModalities {
        #[serde(default)]
        pub input: Vec<String>,
        #[serde(default)]
        pub output: Vec<String>,
    }

    #[derive(Default, Deserialize, Serialize, Clone)]
    pub struct CatalogModel {
        pub limit: Option<CatalogLimits>,
        #[serde(default)]
        pub cost: Option<CatalogCost>,
        #[serde(default)]
        pub provider: Option<CatalogShape>,
        #[serde(default)]
        pub attachment: bool,
        #[serde(default)]
        pub reasoning: bool,
        #[serde(default)]
        pub modalities: Option<CatalogModalities>,
    }

    #[derive(Deserialize, Serialize, Clone)]
    pub struct CatalogLimits {
        #[serde(default)]
        pub context: Option<u32>,
        #[serde(default)]
        pub input: Option<u32>,
        #[serde(default)]
        pub output: Option<u32>,
    }

    #[derive(Deserialize, Serialize, Clone)]
    pub struct CatalogCost {
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
    pub struct CatalogShape {
        #[serde(default)]
        pub shape: Option<String>,
    }
}
