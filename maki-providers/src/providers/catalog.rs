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

/// Priced per subscription, so every model in it reads as free. The `zai` entry
/// carries the pay as you go rates for the same models.
const BLOCKED_PROVIDER_IN_CATALOG: &[&str] = &["zai-coding-plan"];

/// models.dev ids for providers we ship a client for under another name, so
/// their metadata lands under the slug a model spec is written with.
const BUILTIN_CATALOG_IDS: &[(&str, &str)] =
    &[("github-copilot", "copilot"), ("regolo-ai", "regolo")];

/// Builtins with no native client of their own, so [`builtin_provider`]
/// knowing them must not drop them from the catalog.
pub(crate) const CATALOG_BACKED_BUILTINS: &[&str] = opencode::SLUGS;

/// Provider modules own their entry here; the catalog only reads it.
const QUIRKS: &[(&[&str], ProviderQuirks)] = &[(opencode::SLUGS, opencode::QUIRKS)];

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(86400);

const ALLOWED_NPM: &[&str] = &["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"];

const IMAGE_MODALITY: &str = "image";

/// Used only where the catalog is the whole story. A builtin has its manifest
/// fallbacks to reach for instead, which are per provider and so beat a guess.
const DEFAULT_CONTEXT: u32 = 128_000;
const DEFAULT_OUTPUT: u32 = 64_000;

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
    /// Read live so an edit takes effect without a restart, and read leniently
    /// because this runs per request: `load()` exits the process on a parse
    /// error, which is fine at startup and fatal in the middle of a turn.
    fn opted_in(&self) -> bool {
        ProvidersConfig::load_or_default()
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

/// An unpriced entry counts as free, the same reading `enable_free_models` has
/// always had. Only catalog providers reach this, and a free tier is the one
/// place models.dev is reliably explicit about a zero.
fn is_free_model(meta: &CatalogMeta) -> bool {
    meta.pricing
        .as_ref()
        .is_none_or(|pricing| pricing.input == 0.0 && pricing.output == 0.0)
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

/// What models.dev publishes about one model. Every field is `Option` because
/// "the catalog says no" and "the catalog does not say" have to stay apart: a
/// builtin has a manifest and a curated table to fall through to, and collapsing
/// the two here would let a thin upstream row shrink a 1M window to 128k, or
/// turn thinking off for a model we know reasons.
#[derive(Clone, Debug, Default)]
pub struct CatalogMeta {
    pub context: Option<u32>,
    pub output: Option<u32>,
    pub pricing: Option<ModelPricing>,
    pub supports_thinking: Option<bool>,
    pub supports_vision: Option<bool>,
}

impl CatalogMeta {
    pub(crate) fn context_window(&self) -> u32 {
        self.context.unwrap_or(DEFAULT_CONTEXT)
    }

    pub(crate) fn max_output(&self) -> u32 {
        self.output.unwrap_or(DEFAULT_OUTPUT)
    }

    fn model_info(&self, model_id: &str) -> ModelInfo {
        ModelInfo {
            id: model_id.to_string(),
            context_window: Some(self.context_window()),
            max_output_tokens: Some(self.max_output()),
            pricing: Some(self.pricing.clone().unwrap_or_default()),
            supports_thinking: self.supports_thinking,
            supports_vision: self.supports_vision,
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
    /// Kept aside for providers that ship a built-in client, since listing them
    /// in `providers` would show them twice in the login pickers. The catalog is
    /// still the only source that keeps up with what they release, so a model
    /// missing from our static tables can read its rates and limits here.
    builtin_models: HashMap<String, HashMap<String, CatalogMeta>>,
    pub(crate) state_dir: StateDir,
}

impl CatalogData {
    fn empty(state_dir: StateDir) -> Self {
        Self {
            providers: HashMap::new(),
            builtin_models: HashMap::new(),
            state_dir,
        }
    }

    fn from_index(index: schema::CatalogIndex, state_dir: &StateDir) -> Self {
        let mut providers = HashMap::new();
        let mut builtin_models = HashMap::new();

        for (provider_id, provider) in index {
            let slug = builtin_slug(&provider_id);
            if builtin_provider(slug).is_some() && !CATALOG_BACKED_BUILTINS.contains(&slug) {
                let models = parse_models(&provider.models);
                debug!(
                    provider = %provider_id,
                    slug,
                    models = models.len(),
                    "built-in provider: keeping catalog metadata only"
                );
                builtin_models.insert(slug.to_string(), models);
                continue;
            }

            if !is_servable(&provider_id, &provider) {
                continue;
            }

            let models = parse_models(&provider.models);
            let model_count = models.len();
            let api_format = determine_catalog_format(&provider.npm);
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
            builtin_models,
            state_dir: state_dir.clone(),
        }
    }

    pub(crate) fn provider(&self, slug: &str) -> Option<&ProviderData> {
        self.providers.get(slug)
    }

    fn model_meta(&self, slug: &str, model_id: &str) -> Option<&CatalogMeta> {
        self.providers
            .get(slug)
            .and_then(|data| data.models.get(model_id))
            .or_else(|| self.builtin_models.get(slug)?.get(model_id))
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

/// Force-refetches the models.dev catalog; failures keep the stale catalog and cache.
/// Blocks, so only call it from startup paths, never from inside the executor.
pub fn refresh_catalog() -> Result<(), AgentError> {
    let state_dir = StateDir::resolve()
        .map_err(|e| config_error(format!("failed to resolve state dir: {e}")))?;
    let data = fetch_catalog_blocking(&state_dir)?;
    match SHARED_CATALOG.get() {
        Some(catalog) => *catalog.lock().unwrap() = data,
        // Set instead of `get_or_init` so a cold catalog takes the fetch we just
        // did rather than kicking off `init_catalog_blocking` and fetching twice.
        None => drop(SHARED_CATALOG.set(Mutex::new(data))),
    }
    Ok(())
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
        return Err(AgentError::api(
            status,
            format!("catalog fetch returned HTTP {status}"),
        ));
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

fn builtin_slug(catalog_id: &str) -> &str {
    BUILTIN_CATALOG_IDS
        .iter()
        .find(|(id, _)| *id == catalog_id)
        .map_or(catalog_id, |(_, slug)| slug)
}

/// Whether we could stream from this provider ourselves: one of the two
/// protocols we speak, at a base URL the catalog publishes. Built-ins are
/// checked first and never come through here, since they bring their own
/// client and only need the metadata.
fn is_servable(provider_id: &str, provider: &schema::CatalogProvider) -> bool {
    if !ALLOWED_NPM.contains(&provider.npm.as_str()) {
        debug!(provider = %provider_id, npm = %provider.npm, "skipping provider: unsupported npm package");
        return false;
    }
    if BLOCKED_PROVIDER_IN_CATALOG.contains(&provider_id) {
        debug!(provider = %provider_id, "skipping provider: blocked");
        return false;
    }
    if provider.api.is_none() {
        debug!(provider = %provider_id, "skipping provider: no API URL in catalog");
        return false;
    }
    true
}

fn parse_models(models: &HashMap<String, schema::CatalogModel>) -> HashMap<String, CatalogMeta> {
    models
        .iter()
        .map(|(model_id, model)| (model_id.clone(), parse_model(model)))
        .collect()
}

fn parse_model(model: &schema::CatalogModel) -> CatalogMeta {
    let limit = model.limit.as_ref();
    // A published `modalities` answers the vision question on its own; only a
    // row that lists none falls back to the coarser `attachment` flag.
    let supports_vision = match model.modalities.as_ref() {
        Some(modalities) => Some(modalities.input.iter().any(|input| input == IMAGE_MODALITY)),
        None => model.attachment,
    };
    CatalogMeta {
        context: limit.and_then(|l| l.context),
        output: limit.and_then(|l| l.output),
        pricing: model.cost.as_ref().map(|cost| ModelPricing {
            input: cost.input.unwrap_or(0.0),
            output: cost.output.unwrap_or(0.0),
            cache_write: cost.cache_write.unwrap_or(0.0),
            cache_read: cost.cache_read.unwrap_or(0.0),
            fast: None,
        }),
        supports_thinking: model.reasoning,
        supports_vision,
    }
}

fn catalog_client() -> HttpClient {
    isahc::HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .low_speed_timeout(1, Duration::from_secs(30))
        // curl carries http2 for OTLP.
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .expect("failed to build catalog HTTP client")
}

fn fetch_catalog_blocking(state_dir: &StateDir) -> Result<CatalogData, AgentError> {
    let index = smol::block_on(fetch_remote_catalog_async(&catalog_client()))?;
    smol::block_on(save_cached_catalog_async(&index));
    Ok(CatalogData::from_index(index, state_dir))
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

    match fetch_catalog_blocking(&state_dir) {
        Ok(data) => data,
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
                // The turn budget the agent set rides along in `..model`, and
                // [`Model::output_tokens`] clamps it to this cap on read, so
                // reporting what the endpoint accepts is all this has to do.
                max_output_tokens: Some(meta.max_output()),
                context_window: meta.context_window(),
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
/// catalog has already been downloaded. Never triggers a fetch, so callers
/// (e.g. `Model::from_spec`) must tolerate `None` and fall through, since
/// the catalog may still be warming in the background.
pub(crate) fn model_meta_if_available(slug: &str, model_id: &str) -> Option<CatalogMeta> {
    let guard = SHARED_CATALOG.get()?.lock().ok()?;
    guard.model_meta(slug, model_id).cloned()
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::schema::{CatalogCost, CatalogIndex, CatalogLimits, CatalogModel, CatalogProvider};
    use std::sync::Arc;

    use super::{
        Authentication, CatalogData, CatalogMeta, EndpointType, ProviderData, ProviderQuirks,
        SessionRef, StateDir, available_if_warm, determine_catalog_format, quirks_for,
    };
    use crate::manifest::ManifestRegistry;
    use crate::model::{Model, ModelInfo, ModelPricing};
    use crate::provider::Provider;
    use crate::providers::{ResolvedAuth, Timeouts, deepseek, opencode};
    use crate::{AgentError, ModelFamily, ModelTier, RequestOptions};
    use test_case::test_case;

    const SESSION_HEADER: &str = "x-opencode-session";
    const OPT_IN_HINT: &str = "providers.opencode.enable_free_models = true";
    /// A builtin with a static model table, so it is never a catalog provider.
    const BUILTIN_SLUG: &str = "deepseek";
    /// Stands in for a model released after our tables were written, so no
    /// curated row of any provider starts with it.
    const UNLISTED_MODEL: &str = "v9-turbo";
    /// A release that starts with a curated id without being it, the shape
    /// `lookup_entry` cannot tell apart from the row it belongs to.
    const SIBLING_SUFFIX: &str = "-preview";
    /// Same shape, but the catalog row publishes nothing beyond a price.
    const QUIET_SIBLING_SUFFIX: &str = "-quiet";
    /// A release the curated row does cover, only date-stamped.
    const SNAPSHOT_SUFFIX: &str = "-20260401";
    const UNLISTED_INPUT_PRICE: f64 = 1.5;
    const UNLISTED_OUTPUT_PRICE: f64 = 4.5;
    const UNLISTED_CACHE_READ: f64 = 0.15;
    const UNLISTED_CONTEXT: u32 = 512_000;
    const UNLISTED_OUTPUT: u32 = 96_000;
    /// Nothing like any real rate, so whichever source a model read is obvious.
    const STALE_CATALOG_PRICE: f64 = 99.0;
    const ZAI_PLAN_ID: &str = "zai-coding-plan";
    const PAID_INPUT_PRICE: f64 = 1.0;
    const PAID_CONTEXT: u32 = 128_000;
    const PAID_OUTPUT: u32 = 64_000;

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

    /// Limits are published so nothing under test reads a default; only the
    /// price separates the two rows.
    fn priced(input: f64) -> CatalogMeta {
        CatalogMeta {
            context: Some(PAID_CONTEXT),
            output: Some(PAID_OUTPUT),
            pricing: Some(ModelPricing {
                input,
                output: input * 2.0,
                ..ModelPricing::default()
            }),
            supports_thinking: Some(false),
            supports_vision: Some(false),
        }
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
                ("paid-model".into(), priced(PAID_INPUT_PRICE)),
                ("free-model".into(), priced(0.0)),
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
            supports_fast_override: None,
            pricing: ModelPricing::default(),
            discovered_free: false,
            max_output_tokens: None,
            turn_output_tokens: None,
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

    #[test_case("vision-model", true; "catalog_marks_model_as_vision")]
    #[test_case("text-model", false; "catalog_marks_model_as_text_only")]
    fn supports_vision_falls_back_to_catalog_for_builtins(model_id: &str, expected: bool) {
        let (_tmp, state_dir) = temp_state_dir();
        let models = HashMap::from([
            (
                "vision-model".into(),
                CatalogModel {
                    attachment: Some(true),
                    ..Default::default()
                },
            ),
            ("text-model".into(), CatalogModel::default()),
        ]);
        let index: CatalogIndex = HashMap::from([(
            "opencode-go".into(),
            CatalogProvider {
                name: "Opencode Go".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/go/v1".into()),
                models,
            },
        )]);
        super::seed_catalog_for_tests(index, state_dir);

        let model = super::Model::from_spec(&format!("opencode-go/{model_id}")).unwrap();
        assert_eq!(model.supports_vision(), expected);
    }

    /// models.dev lists every model a provider ships, so it can answer for the
    /// ones our table has never seen. Curated rows are checked against the
    /// provider's own pricing page, so they still beat a catalog that may be
    /// carrying a stale rate.
    #[test]
    fn catalog_answers_only_for_models_the_static_table_misses() {
        let (_tmp, state_dir) = temp_state_dir();
        super::seed_catalog_for_tests(builtin_catalog(), state_dir);

        let unlisted = Model::from_spec(&format!("{BUILTIN_SLUG}/{UNLISTED_MODEL}")).unwrap();
        assert_eq!(unlisted.pricing.input, UNLISTED_INPUT_PRICE);
        assert_eq!(unlisted.pricing.output, UNLISTED_OUTPUT_PRICE);
        assert_eq!(unlisted.pricing.cache_read, UNLISTED_CACHE_READ);
        assert_eq!(unlisted.context_window, UNLISTED_CONTEXT);
        assert_eq!(unlisted.max_output_tokens, Some(UNLISTED_OUTPUT));
        assert!(unlisted.supports_vision());
        assert!(
            !unlisted.supports_thinking(),
            "the manifest default is true, so only the catalog can say no"
        );

        let curated = &deepseek::models()[0];
        let listed = Model::from_spec(&format!("{BUILTIN_SLUG}/{}", curated_flash())).unwrap();
        assert_eq!(listed.pricing.input, curated.pricing.input);
        assert_eq!(listed.context_window, curated.context_window);
    }

    /// Curated rows match by prefix, so `deepseek-flash` answers for every id
    /// starting with it. That guess loses to models.dev naming the exact model,
    /// or the next release in an existing family bills at its predecessor's
    /// rates forever without ever looking stale. A date stamp is the exception:
    /// it names the same model, so the curated row keeps it.
    #[test]
    fn a_relative_matched_by_prefix_loses_to_the_catalog_naming_the_model() {
        let (_tmp, state_dir) = temp_state_dir();
        super::seed_catalog_for_tests(builtin_catalog(), state_dir);
        let curated = &deepseek::models()[0];

        let sibling = Model::from_spec(&format!("{BUILTIN_SLUG}/{}", sibling_model())).unwrap();
        assert_eq!(sibling.pricing.input, UNLISTED_INPUT_PRICE);
        assert_eq!(sibling.context_window, UNLISTED_CONTEXT);
        assert_eq!(sibling.max_output_tokens, Some(UNLISTED_OUTPUT));
        assert!(
            !sibling.supports_vision(),
            "the curated relative has vision"
        );
        assert_eq!(
            sibling.family, curated.family,
            "which dialect a model speaks is still the relative's answer to give"
        );

        let snapshot = Model::from_spec(&format!(
            "{BUILTIN_SLUG}/{}{SNAPSHOT_SUFFIX}",
            curated_flash()
        ))
        .unwrap();
        assert_eq!(snapshot.pricing.input, curated.pricing.input);
    }

    /// The catalog only outranks a relative where it has something to say.
    /// Every field it leaves out keeps falling through, first to the relative
    /// and then to the manifest, or reading models.dev would cost a model the
    /// metadata it already had.
    #[test]
    fn fields_the_catalog_omits_fall_through() {
        let (_tmp, state_dir) = temp_state_dir();
        super::seed_catalog_for_tests(builtin_catalog(), state_dir);
        let curated = &deepseek::models()[0];
        let manifest = ManifestRegistry::for_slug(BUILTIN_SLUG).unwrap();

        let quiet = Model::from_spec(&format!("{BUILTIN_SLUG}/{}", quiet_sibling_model())).unwrap();
        assert_eq!(
            quiet.pricing.input, UNLISTED_INPUT_PRICE,
            "the catalog priced it, so it has to be the one answering below"
        );
        assert_eq!(quiet.context_window, curated.context_window);
        assert_eq!(quiet.max_output_tokens, curated.max_output_tokens);
        assert_eq!(quiet.supports_vision(), curated.vision);
        assert_eq!(quiet.supports_thinking(), manifest.supports_thinking);
    }

    /// models.dev leaves `limit` off plenty of entries, and a builtin manifest
    /// carries a real number for its provider (DeepSeek serves 1M context)
    /// against the 128k/64k the catalog guesses for everyone else. An
    /// unpublished limit must not read as a published one, or every gap in the
    /// catalog silently shrinks the window we paid for.
    #[test]
    fn a_limit_the_catalog_omits_falls_through_to_the_manifest() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = single_provider_catalog(
            BUILTIN_SLUG,
            "@ai-sdk/openai-compatible",
            Some("https://api.deepseek.com"),
        );
        super::seed_catalog_for_tests(index, state_dir);

        let manifest = ManifestRegistry::for_slug(BUILTIN_SLUG).unwrap();
        let model = Model::from_spec(&format!("{BUILTIN_SLUG}/{UNLISTED_MODEL}")).unwrap();

        assert_eq!(
            model.pricing.input, UNLISTED_INPUT_PRICE,
            "the catalog entry has to be the one answering, or the limits below prove nothing"
        );
        assert_eq!(model.context_window, manifest.fallback_context_window);
        assert_eq!(model.max_output_tokens, manifest.fallback_max_output);
    }

    /// Every builtin the catalog covers reaches its metadata, whichever SDK it
    /// publishes, whether it lists a base URL at all, and whatever id it goes by
    /// there. None of them may reach the provider map: they have a client of
    /// their own, so a second entry would show up twice in the login pickers.
    #[test_case("anthropic", "anthropic", "@ai-sdk/anthropic", None; "sdk we speak but no base url")]
    #[test_case("deepseek", "deepseek", "@ai-sdk/openai-compatible", Some("https://api.deepseek.com"); "everything it takes to be servable")]
    #[test_case("openai", "openai", "@ai-sdk/openai", Some("https://api.openai.com/v1"); "sdk we do not speak")]
    #[test_case("google", "google", "@ai-sdk/google", None; "sdk we do not speak and no base url")]
    #[test_case("openrouter", "openrouter", "@openrouter/ai-sdk-provider", Some("https://openrouter.ai/api/v1"); "vendor sdk")]
    #[test_case("zai", "zai", "@ai-sdk/openai-compatible", Some("https://api.z.ai/api/paas/v4"); "pay as you go rates")]
    #[test_case("github-copilot", "copilot", "@ai-sdk/openai-compatible", Some("https://api.githubcopilot.com"); "renamed")]
    #[test_case("regolo-ai", "regolo", "@ai-sdk/openai-compatible", Some("https://api.regolo.ai/v1"); "renamed too")]
    fn builtins_keep_their_catalog_metadata(
        catalog_id: &str,
        slug: &str,
        npm: &str,
        api: Option<&str>,
    ) {
        let (_tmp, state_dir) = temp_state_dir();
        let data =
            CatalogData::from_index(single_provider_catalog(catalog_id, npm, api), &state_dir);

        assert!(data.model_meta(slug, UNLISTED_MODEL).is_some());
        assert!(data.provider(slug).is_none());
        assert!(data.provider(catalog_id).is_none());
    }

    /// Its models are covered by the pay as you go `zai` entry, and are priced
    /// at zero here because the plan already paid for them.
    #[test]
    fn plan_priced_provider_is_dropped_entirely() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = single_provider_catalog(
            ZAI_PLAN_ID,
            "@ai-sdk/openai-compatible",
            Some("https://api.z.ai/api/coding/paas/v4"),
        );
        let data = CatalogData::from_index(index, &state_dir);

        assert!(data.provider(ZAI_PLAN_ID).is_none());
        assert!(data.model_meta(ZAI_PLAN_ID, UNLISTED_MODEL).is_none());
        assert!(data.model_meta("zai", UNLISTED_MODEL).is_none());
    }

    /// A renamed builtin only reaches its metadata while both halves hold: the
    /// slug is one we ship, and the catalog id is not (or the alias is dead
    /// weight, since the plain path would already have matched).
    #[test]
    fn renamed_builtins_map_a_foreign_id_onto_a_slug_we_ship() {
        for (catalog_id, slug) in super::BUILTIN_CATALOG_IDS {
            assert!(super::builtin_provider(slug).is_some(), "{slug}");
            assert!(
                super::builtin_provider(catalog_id).is_none(),
                "{catalog_id}"
            );
        }
    }

    /// Rates, limits and both capability flags, the shape of a fully
    /// documented models.dev row.
    fn documented_row(vision: bool) -> CatalogModel {
        CatalogModel {
            limit: Some(CatalogLimits {
                context: Some(UNLISTED_CONTEXT),
                input: None,
                output: Some(UNLISTED_OUTPUT),
            }),
            cost: Some(CatalogCost {
                input: Some(UNLISTED_INPUT_PRICE),
                output: Some(UNLISTED_OUTPUT_PRICE),
                cache_read: Some(UNLISTED_CACHE_READ),
                cache_write: None,
            }),
            attachment: Some(vision),
            reasoning: Some(false),
            ..Default::default()
        }
    }

    /// A price and nothing else, the shape models.dev really ships for a chunk
    /// of its catalog, so a test can tell "the catalog answered" apart from
    /// "the catalog had nothing to say about this field".
    fn priced_row(input: f64, output: f64) -> CatalogModel {
        CatalogModel {
            cost: Some(CatalogCost {
                input: Some(input),
                output: Some(output),
                cache_read: None,
                cache_write: None,
            }),
            ..Default::default()
        }
    }

    fn catalog_index(
        catalog_id: &str,
        npm: &str,
        api: Option<&str>,
        models: HashMap<String, CatalogModel>,
    ) -> CatalogIndex {
        HashMap::from([(
            catalog_id.into(),
            CatalogProvider {
                name: catalog_id.into(),
                env: Vec::new(),
                npm: npm.into(),
                api: api.map(Into::into),
                models,
            },
        )])
    }

    fn single_provider_catalog(catalog_id: &str, npm: &str, api: Option<&str>) -> CatalogIndex {
        let models = HashMap::from([(
            UNLISTED_MODEL.into(),
            priced_row(UNLISTED_INPUT_PRICE, UNLISTED_OUTPUT_PRICE),
        )]);
        catalog_index(catalog_id, npm, api, models)
    }

    fn curated_flash() -> &'static str {
        deepseek::models()[0].prefixes[0]
    }

    fn sibling_model() -> String {
        format!("{}{SIBLING_SUFFIX}", curated_flash())
    }

    fn quiet_sibling_model() -> String {
        format!("{}{QUIET_SIBLING_SUFFIX}", curated_flash())
    }

    /// One row per rung of the order: a model the table misses, the curated id,
    /// its dated snapshot, a relative, and a relative the catalog only prices.
    fn builtin_catalog() -> CatalogIndex {
        let stale = || priced_row(STALE_CATALOG_PRICE, STALE_CATALOG_PRICE);
        let models = HashMap::from([
            (UNLISTED_MODEL.into(), documented_row(true)),
            (curated_flash().into(), stale()),
            (format!("{}{SNAPSHOT_SUFFIX}", curated_flash()), stale()),
            (sibling_model(), documented_row(false)),
            (
                quiet_sibling_model(),
                priced_row(UNLISTED_INPUT_PRICE, UNLISTED_OUTPUT_PRICE),
            ),
        ]);
        catalog_index(
            BUILTIN_SLUG,
            "@ai-sdk/openai-compatible",
            Some("https://api.deepseek.com"),
            models,
        )
    }

    #[test]
    fn catalog_miss_falls_back_to_family() {
        let (_tmp, state_dir) = temp_state_dir();
        super::warm_empty_catalog_for_tests(state_dir);

        let model = super::Model::from_spec("opencode-go/unlisted-model").unwrap();
        assert!(!model.supports_vision());
    }

    /// The catalog is the last word before the family guess, so anything more
    /// specific still wins. Only discovery can be exercised here: the builtins
    /// the catalog keeps (`opencode`, `opencode-go`) list no manifest models.
    #[test]
    fn discovery_beats_catalog_vision() {
        let (_tmp, state_dir) = temp_state_dir();
        let models = HashMap::from([(
            "omen-alpha".into(),
            CatalogModel {
                attachment: Some(true),
                ..Default::default()
            },
        )]);
        let index: CatalogIndex = HashMap::from([(
            "opencode-go".into(),
            CatalogProvider {
                name: "Opencode Go".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/go/v1".into()),
                models,
            },
        )]);
        super::seed_catalog_for_tests(index, state_dir);
        crate::model_registry::set_known_models(
            "opencode-go",
            vec![ModelInfo {
                supports_vision: Some(false),
                ..ModelInfo::id_only("omen-alpha".into())
            }],
        );

        let model = super::Model::from_spec("opencode-go/omen-alpha").unwrap();
        assert!(
            !model.supports_vision(),
            "discovery must win over catalog metadata"
        );
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
        assert_eq!(meta.context_window(), 128_000);
        assert_eq!(meta.max_output(), 16_384);
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
        /// `None` where the row omits the flag, so a builtin keeps its manifest
        /// default instead of reading an absent field as a published "no".
        pub attachment: Option<bool>,
        pub reasoning: Option<bool>,
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
