use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use flume::Sender;
use maki_config::host_allowed;
use maki_config::providers::{Protocol, ProvidersConfig};
use maki_storage::StateDir;
use maki_storage::auth::lock_credentials;
use maki_storage::id::SessionRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};
use url::{Host, Url};

use crate::model::{
    Model, ModelInfo, ModelPricing, ModelTier, Prefixed, ThinkingSupport, longest_prefix_match,
};
use crate::provider::{BoxFuture, Provider};
use crate::spec::{ProviderRegistry, ProviderSpec};
use crate::types::ThinkingFields;
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

use super::codec::{self, BodyHook};
use super::{ResolvedAuth, Timeouts};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16384;
const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;
const BUILD_BODY_OPTION: &str = "build_body hook";
const SYSTEM_PREFIX_OPTION: &str = "system_prefix";
const HTTPS_SCHEME: &str = "https";
const HTTP_SCHEME: &str = "http";
const LOCALHOST: &str = "localhost";

/// One plugin-supplied callback. Generic in both directions so every hook on
/// [`ProviderHooks`] is the same shape, and `Option::is_some` is the only
/// presence question the registry ever asks.
pub trait Hook<In, Out>: Send + Sync {
    fn call(&self, input: In) -> BoxFuture<'_, Result<Out, AgentError>>;
}

#[derive(Default, Clone)]
pub struct ProviderHooks {
    pub auth: Option<Arc<dyn Hook<AuthPurpose, PluginAuth>>>,
    pub list_models: Option<Arc<dyn Hook<(), Vec<ModelInfo>>>>,
    pub build_body: Option<Arc<dyn Hook<BodyInput, Value>>>,
    pub map_error: Option<Arc<dyn Hook<ApiError, Option<ApiError>>>>,
    pub fetch_usage: Option<Arc<dyn Hook<(), Option<ProviderUsage>>>>,
    pub login: Option<Arc<dyn Hook<(), ()>>>,
    pub logout: Option<Arc<dyn Hook<(), ()>>>,
}

/// Why the auth hook is being asked for credentials. `Reload` only re-reads
/// what a login wrote, which is what lets it skip the cross-process lock.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AuthPurpose {
    Resolve,
    Refresh,
    Reload,
}

/// The request as it goes on the wire, plus the two things a plugin branches
/// on. `thinking` is rendered, not structured, because the hook is a wire-level
/// escape hatch and not a second place to model effort.
#[derive(Serialize)]
pub struct BodyInput {
    pub body: Value,
    pub model: String,
    pub thinking: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

#[derive(Deserialize)]
pub struct PluginAuth {
    pub base_url: Option<String>,
    pub headers: HashMap<String, String>,
}

impl PluginAuth {
    /// The one door plugin-supplied credentials come through, so a plugin
    /// cannot point maki's tokens at a host it never declared.
    pub fn into_resolved(self, slug: &str, hosts: &[String]) -> Result<ResolvedAuth, AgentError> {
        let base_url = declared_base_url(slug, self.base_url, hosts)
            .map_err(|message| AgentError::Config { message })?;
        Ok(ResolvedAuth::new(slug, self.headers.into_iter().collect())?.with_base_url(base_url))
    }
}

/// The only place an origin a plugin chose is admitted, whether it arrived with
/// the registration or from an auth hook. Both paths end up holding the token
/// maki sends, so both ask the same question of the same list.
///
/// Origin, not host: the scheme is half of what a declaration promises. A
/// declared host reached over plaintext puts the token on the wire in the
/// clear, so only `https` is admitted. `http` stays open for loopback, where a
/// self-hosted provider has no wire to listen on.
fn declared_base_url(
    slug: &str,
    base_url: Option<String>,
    hosts: &[String],
) -> Result<Option<String>, String> {
    let Some(url) = &base_url else {
        return Ok(None);
    };
    let parsed = Url::parse(url)
        .map_err(|e| format!("provider '{slug}': base_url '{url}' is not a url: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("provider '{slug}': base_url '{url}' has no host"))?;
    let scheme = parsed.scheme();
    if scheme != HTTPS_SCHEME && !(scheme == HTTP_SCHEME && is_loopback(&parsed)) {
        return Err(format!(
            "provider '{slug}': base_url '{url}' would send credentials over '{scheme}'; use \
             https, or http only for loopback"
        ));
    }
    if !host_allowed(host, hosts) {
        return Err(format!(
            "provider '{slug}': base_url host '{host}' is not in the declared net hosts"
        ));
    }
    Ok(base_url)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name == LOCALHOST,
        Some(Host::Ipv4(addr)) => addr.is_loopback(),
        Some(Host::Ipv6(addr)) => addr.is_loopback(),
        None => false,
    }
}

#[derive(Deserialize)]
pub struct PluginModel {
    /// Every id this row answers for. `prefixes[0]` is the canonical id,
    /// used wherever a concrete model has to be named.
    pub prefixes: Vec<String>,
    #[serde(default = "default_tier")]
    pub tier: ModelTier,
    #[serde(default)]
    pub supports_tool_examples: Option<bool>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub requires_thinking: bool,
    #[serde(default)]
    pub supports_vision: Option<bool>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
    #[serde(default)]
    pub thinking_fields: Option<ThinkingFields>,
}

impl Prefixed for PluginModel {
    fn prefixes(&self) -> impl Iterator<Item = &str> {
        self.prefixes.iter().map(String::as_str)
    }
}

impl PluginModel {
    fn canonical_id(&self) -> Option<&str> {
        self.prefixes.first().map(String::as_str)
    }

    fn to_model(
        &self,
        slug: &str,
        base: &'static ProviderSpec,
        id: String,
        tier: ModelTier,
    ) -> Model {
        Model {
            id,
            provider: Arc::from(slug),
            tier,
            family: base.family,
            supports_tool_examples_override: self.supports_tool_examples,
            thinking_override: ThinkingSupport::from_flags(
                self.supports_thinking,
                self.requires_thinking,
            ),
            supports_vision_override: self.supports_vision,
            supports_fast_override: None,
            pricing: self.pricing.clone().unwrap_or_default(),
            subsidised_by: None,
            discovered_free: false,
            max_output_tokens: Some(self.max_output_tokens),
            turn_output_tokens: None,
            context_window: self.context_window,
            thinking_fields: self.thinking_fields.clone().map(Box::new),
        }
    }

    /// This row as the catalogue reports it. Every field the declaration
    /// states is carried, `supports_*` included: they are `Option` on both
    /// sides, so an unstated one stays unstated rather than becoming a
    /// published negative. `provider_info` is a stash only the Rust provider
    /// that filled it can read back, so a declared row never has one.
    fn to_info(&self) -> ModelInfo {
        ModelInfo {
            id: self.canonical_id().unwrap_or_default().to_string(),
            context_window: Some(self.context_window),
            max_output_tokens: Some(self.max_output_tokens),
            pricing: self.pricing.clone(),
            supports_thinking: self.supports_thinking,
            supports_vision: self.supports_vision,
            tier: Some(self.tier),
            provider_info: None,
        }
    }
}

fn default_tier() -> ModelTier {
    ModelTier::Medium
}

fn default_max_output_tokens() -> u32 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

fn default_context_window() -> u32 {
    DEFAULT_CONTEXT_WINDOW
}

pub struct Registration {
    pub slug: String,
    pub display_name: String,
    pub codec: Option<Protocol>,
    pub base: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub system_prefix: Option<String>,
    pub models: Vec<PluginModel>,
    pub net_hosts: Vec<String>,
    pub hooks: ProviderHooks,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error(
        "invalid provider slug '{0}': must start with a letter or digit and hold only letters, digits, '_' and '-'"
    )]
    InvalidSlug(String),
    #[error("provider slug '{0}' is taken by a built-in provider")]
    BuiltinSlug(String),
    #[error("provider slug '{0}' is already defined in providers.toml")]
    ConfiguredSlug(String),
    #[error("provider '{0}' is already registered")]
    DuplicateSlug(String),
    #[error("provider '{0}' must set exactly one of `codec` or `base`")]
    CodecOrBase(String),
    #[error("provider '{slug}': base '{base}' is not a native provider")]
    UnknownBase { slug: String, base: String },
    #[error("provider '{0}' must declare at least one net host")]
    NoNetHosts(String),
    #[error("{0}")]
    UndeclaredBaseUrl(String),
    #[error("provider '{0}' cannot register outside a plugin load")]
    Closed(String),
    #[error("provider '{slug}': {option} is not supported by {target}")]
    Unsupported {
        slug: String,
        option: &'static str,
        target: String,
    },
}

/// What a registered slug builds its requests with. Exactly one of the two, so
/// the impossible "neither" is unrepresentable past registration.
#[derive(Clone, Copy)]
enum Target {
    Base(&'static ProviderSpec),
    Codec(Protocol),
}

impl Target {
    /// The native spec behind this target: model family, fallbacks and the
    /// model table a plugin that curates none borrows.
    fn spec(self) -> Option<&'static ProviderSpec> {
        match self {
            Self::Base(spec) => Some(spec),
            Self::Codec(protocol) => codec::protocol_spec(protocol),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Base(spec) => format!("base '{}'", spec.slug),
            Self::Codec(protocol) => format!("codec {protocol:?}"),
        }
    }
}

/// Only the openai codecs thread a body hook through [`codec::build`].
///
/// Written as an exhaustive match rather than a list of the ones that work, so
/// a new codec breaks this line and someone has to answer for it. An option a
/// codec cannot honour is a registration error, never a no-op.
fn honours_build_body(target: Target) -> bool {
    match target {
        Target::Codec(Protocol::Openai | Protocol::OpenaiResponses) => true,
        Target::Codec(Protocol::Anthropic | Protocol::Google) | Target::Base(_) => false,
    }
}

/// Google drops the system prefix and always has (see `super::google`), so a
/// plugin that sets one against it is told instead of ignored. Asked of the
/// spec behind the target rather than of the target, because `codec = "google"`
/// and `base = "google"` reach the same constructor and must answer alike.
fn honours_system_prefix(target: Target) -> bool {
    !target
        .spec()
        .is_some_and(|spec| spec.slug == super::google::SLUG)
}

fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

struct PluginEntry {
    slug: String,
    display_name: String,
    target: Target,
    system_prefix: Option<String>,
    models: Vec<PluginModel>,
    hooks: ProviderHooks,
    /// Shared with every other entry this slug ever had: an entry is replaced
    /// on reload, its credentials are not. Held here rather than looked up
    /// beside the entry so "registered" and "has credentials" cannot come
    /// apart at a call site.
    auth: Arc<AuthState>,
}

type Registry = HashMap<Box<str>, Arc<PluginEntry>>;

/// What every read answers. Only ever replaced whole, by [`commit_load`], so
/// there is no moment at which a reader can see a half-built registry: during
/// a load the previous generation is still the published one.
static PROVIDERS: LazyLock<RwLock<Arc<Registry>>> = LazyLock::new(RwLock::default);
/// The load in progress. `Some` between [`begin_load`] and [`commit_load`],
/// and open at process start so a host that loads plugins without driving the
/// phases (tests, embedders) still registers.
static STAGING: LazyLock<Mutex<Option<Registry>>> =
    LazyLock::new(|| Mutex::new(Some(Registry::new())));
/// Append-only for the life of the process: see the note in [`register`].
static AUTH: LazyLock<RwLock<HashMap<Box<str>, Arc<AuthState>>>> = LazyLock::new(RwLock::default);

/// Opens the registration window, at the top of every plugin load.
///
/// Nothing published goes away here. A `/reload` builds a new plugin host, and
/// an entry left behind by the old one holds hooks that answer on a channel
/// nobody serves any more. Dropping them before the replacements exist would be
/// worse: every reader in the process would answer "unknown provider" for as
/// long as the load takes. The staged map replaces the published one in one
/// step at [`commit_load`] instead.
pub fn begin_load() {
    *STAGING.lock().unwrap() = Some(Registry::new());
}

/// Publishes what this load registered. A load that registered nothing
/// publishes nothing, which is how a `/reload` drops a plugin that was removed.
pub fn commit_load() {
    let Some(staged) = STAGING.lock().unwrap().take() else {
        return;
    };
    *PROVIDERS.write().unwrap() = Arc::new(staged);
}

pub fn register(reg: Registration) -> Result<(), RegisterError> {
    let slug = reg.slug.clone();
    if !is_valid_slug(&slug) {
        return Err(RegisterError::InvalidSlug(slug));
    }
    if ProviderRegistry::native_slugs().any(|builtin| builtin == slug) {
        return Err(RegisterError::BuiltinSlug(slug));
    }
    if ProvidersConfig::load().get(&slug).is_some() {
        return Err(RegisterError::ConfiguredSlug(slug));
    }
    if reg.net_hosts.is_empty() {
        return Err(RegisterError::NoNetHosts(slug));
    }
    let base_url = declared_base_url(&slug, reg.base_url.clone(), &reg.net_hosts)
        .map_err(RegisterError::UndeclaredBaseUrl)?;

    let target = match (reg.codec, &reg.base) {
        (Some(protocol), None) => Target::Codec(protocol),
        (None, Some(base)) => Target::Base(
            ProviderRegistry::get(base)
                .filter(|spec| spec.is_native())
                .ok_or_else(|| RegisterError::UnknownBase {
                    slug: slug.clone(),
                    base: base.clone(),
                })?,
        ),
        _ => return Err(RegisterError::CodecOrBase(slug)),
    };
    let unsupported = |option| RegisterError::Unsupported {
        slug: slug.clone(),
        option,
        target: target.describe(),
    };
    if reg.hooks.build_body.is_some() && !honours_build_body(target) {
        return Err(unsupported(BUILD_BODY_OPTION));
    }
    if reg.system_prefix.is_some() && !honours_system_prefix(target) {
        return Err(unsupported(SYSTEM_PREFIX_OPTION));
    }

    let declared = initial_auth(&reg, base_url)?;
    let mut staging = STAGING.lock().unwrap();
    // Duplicates are asked of the load in progress, not of what is published:
    // a reload re-registers every slug it registered last time.
    let Some(staged) = staging.as_mut() else {
        return Err(RegisterError::Closed(slug));
    };
    if staged.contains_key(slug.as_str()) {
        return Err(RegisterError::DuplicateSlug(slug));
    }
    // Credentials survive a reload because of *which map they live in*: a
    // get-or-insert by slug, so a reload cannot mint a second `RefreshGate` for
    // a slug whose token is in flight. What the new registration *declares* is
    // handed to the state either way, which is what keeps an edited `base_url`
    // or `api_key_env` from being silently ignored until the next restart.
    let auth = Arc::clone(
        AUTH.write()
            .unwrap()
            .entry(slug.as_str().into())
            .or_insert_with(|| Arc::new(AuthState::new(declared.clone(), &reg.net_hosts))),
    );
    auth.redeclare(&slug, declared, &reg.net_hosts);
    staged.insert(
        slug.as_str().into(),
        Arc::new(PluginEntry {
            slug: reg.slug,
            display_name: reg.display_name,
            target,
            system_prefix: reg.system_prefix,
            models: reg.models,
            hooks: reg.hooks,
            auth,
        }),
    );
    Ok(())
}

/// The credentials a provider starts with, before any hook has run: whatever
/// the registration declared statically.
fn initial_auth(
    reg: &Registration,
    base_url: Option<String>,
) -> Result<ResolvedAuth, RegisterError> {
    let key = reg
        .api_key_env
        .as_ref()
        .and_then(|var| std::env::var(var).ok())
        .filter(|key| !key.is_empty());
    let auth = match key {
        Some(key) => ResolvedAuth::bearer(&reg.slug, &key),
        None => ResolvedAuth::new(&reg.slug, Vec::new()),
    };
    // `ResolvedAuth` only fails over `[<slug>.headers]` in providers.toml, and
    // a slug that appears there was already rejected above.
    auth.map(|auth| auth.with_base_url(base_url))
        .map_err(|_| RegisterError::ConfiguredSlug(reg.slug.clone()))
}

fn entries() -> Arc<Registry> {
    Arc::clone(&PROVIDERS.read().unwrap())
}

fn entry(slug: &str) -> Option<Arc<PluginEntry>> {
    entries().get(slug).cloned()
}

fn unknown(slug: &str) -> AgentError {
    AgentError::Config {
        message: format!("unknown plugin provider '{slug}'"),
    }
}

/// Auth for one slug, kept in its own map so a reload cannot drop a token or a
/// refresh in flight. One cell per slug for the life of the process: the codec
/// reads it per request, so whatever the hook last wrote is what goes on the
/// wire, without anything being rebuilt or copied back.
struct AuthState {
    current: Arc<Mutex<ResolvedAuth>>,
    /// The latest registration's egress list, not the one the entry a caller
    /// happens to hold was built with. A refresh that started before a reload
    /// still lands its answer here, so vetting it against anything older would
    /// admit an origin the current declaration no longer covers.
    hosts: Mutex<Arc<[String]>>,
    gate: RefreshGate,
}

impl AuthState {
    fn new(initial: ResolvedAuth, hosts: &[String]) -> Self {
        Self {
            current: Arc::new(Mutex::new(initial)),
            hosts: Mutex::new(hosts.into()),
            gate: RefreshGate::default(),
        }
    }

    fn hosts(&self) -> Arc<[String]> {
        Arc::clone(&self.hosts.lock().unwrap())
    }

    /// Re-points one slug's credentials at what the newest registration says.
    ///
    /// Two things change on a reload and neither may be ignored: the declared
    /// hosts, which are what every later hook answer is vetted against, and the
    /// static credentials, which are the only ones a plugin without an auth
    /// hook ever has. A token a hook already minted stays, unless the new
    /// declaration stopped covering the origin it is pointed at.
    fn redeclare(&self, slug: &str, declared: ResolvedAuth, hosts: &[String]) {
        *self.hosts.lock().unwrap() = hosts.into();
        let mut current = self.current.lock().unwrap();
        let undeclared = declared_base_url(slug, current.base_url.clone(), hosts).is_err();
        if !self.gate.ran() || undeclared {
            *current = declared;
        }
    }
}

impl PluginEntry {
    /// Resolve once, lazily, from async code. Every fallible provider method
    /// starts here, so no synchronous path ever has to reach a hook.
    async fn ensure_auth(&self) -> Result<(), AgentError> {
        if self.auth.gate.ran() {
            return Ok(());
        }
        self.run_auth(AuthPurpose::Resolve).await
    }

    /// The only place the auth hook is called. A plugin without one keeps the
    /// credentials its registration declared.
    ///
    /// Callable from the plugin host's own thread: the hook goes to the host's
    /// priority lane and the dispatch loop serves it while this future is
    /// parked, which is how a subagent driven from Lua refreshes a token. What
    /// may not reach here is a *blocking* caller, and that is held by
    /// construction instead of by a check: every path in is `async`, and
    /// `create` builds a provider without running a hook at all.
    async fn run_auth(&self, purpose: AuthPurpose) -> Result<(), AgentError> {
        let Some(hook) = &self.hooks.auth else {
            return Ok(());
        };
        // One question, asked once, because both answers follow from it: a
        // reload re-reads what a login wrote, so it spends no token. It waits
        // for neither the gate nor the cross-process lock, and it runs under
        // `block_on` on the ui thread, where either wait would freeze the ui.
        let spends_a_token = !matches!(purpose, AuthPurpose::Reload);
        let work = async {
            // Serialised against other maki processes on the same credentials,
            // and re-entrant in this one so the hook can store what it minted.
            let _lock = if spends_a_token {
                let slug = self.slug.clone();
                smol::unblock(move || {
                    StateDir::resolve()
                        .ok()
                        .map(|dir| lock_credentials(&dir, &slug))
                })
                .await
            } else {
                None
            };
            let mut fresh = hook
                .call(purpose)
                .await?
                .into_resolved(&self.slug, &self.auth.hosts())?;
            let mut guard = self.auth.current.lock().unwrap();
            // A hook that omits base_url keeps the resolved one; falling back
            // to the provider's default origin would silently repoint the token.
            if fresh.base_url.is_none() {
                fresh.base_url = guard.base_url.take();
            }
            *guard = fresh;
            Ok(())
        };
        if spends_a_token {
            self.auth.gate.single_flight(work).await
        } else {
            work.await
        }
    }
}

/// Single-flight around the plugin's auth hook. Every `create` mints a fresh
/// `PluginProvider`, so sub-agents running their own model would each spend the
/// plugin's rotating refresh token, and a spent one taken twice costs the whole
/// token family. They queue here instead, and the late arrival returns to find
/// the shared credentials already holding what the winner minted. That is why
/// the gate hangs off the per-slug auth state rather than the provider.
#[derive(Default)]
struct RefreshGate {
    lock: smol::lock::Mutex<()>,
    /// Counted, not compared: a refresh can hand back byte-identical
    /// credentials, so the count is the only thing that can tell a parked
    /// caller the work is already done. Written only under `lock`.
    runs: AtomicU64,
}

impl RefreshGate {
    /// Whether the hook has ever run to completion, which is also what makes
    /// the lazy first resolve happen once.
    fn ran(&self) -> bool {
        self.runs.load(Ordering::Acquire) > 0
    }

    async fn single_flight(
        &self,
        work: impl Future<Output = Result<(), AgentError>>,
    ) -> Result<(), AgentError> {
        let before = self.runs.load(Ordering::Acquire);
        let _guard = self.lock.lock().await;
        if self.runs.load(Ordering::Acquire) != before {
            debug!("peer refreshed while we waited, skipping auth hook");
            return Ok(());
        }
        work.await?;
        self.runs.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

struct BodyAdapter(Arc<dyn Hook<BodyInput, Value>>);

impl BodyHook for BodyAdapter {
    fn call<'a>(
        &'a self,
        body: Value,
        model: &'a Model,
        opts: RequestOptions,
    ) -> BoxFuture<'a, Result<Value, AgentError>> {
        self.0.call(BodyInput {
            body,
            model: model.id.clone(),
            thinking: opts.thinking.to_string(),
        })
    }
}

struct PluginProvider {
    entry: Arc<PluginEntry>,
    inner: Box<dyn Provider>,
}

impl PluginProvider {
    /// The single place `map_error` is applied, so it cannot cover streaming
    /// and miss the rest. The hook may restate the status and the message and
    /// nothing else: `retry_after` is what the server actually asked for, and
    /// retryability is derived from the status by `retry_kind`.
    fn mapped<'a, T: Send + 'a>(
        &'a self,
        result: Result<T, AgentError>,
    ) -> BoxFuture<'a, Result<T, AgentError>> {
        Box::pin(async move {
            let Some(hook) = &self.entry.hooks.map_error else {
                return result;
            };
            let Err(AgentError::Api {
                status,
                message,
                retry_after,
            }) = result
            else {
                return result;
            };
            let original = ApiError { status, message };
            let replacement = match hook.call(original.clone()).await {
                Ok(mapped) => mapped,
                Err(e) => {
                    warn!(error = %e, "map_error hook failed, keeping the original error");
                    None
                }
            };
            let ApiError { status, message } = replacement.unwrap_or(original);
            Err(AgentError::Api {
                status,
                message,
                retry_after,
            })
        })
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        self.entry.ensure_auth().await?;
        if let Some(hook) = &self.entry.hooks.list_models {
            return hook.call(()).await;
        }
        if self.entry.models.is_empty() {
            return self.inner.list_models().await;
        }
        Ok(self.entry.models.iter().map(PluginModel::to_info).collect())
    }

    async fn usage(&self) -> Result<Option<ProviderUsage>, AgentError> {
        self.entry.ensure_auth().await?;
        match &self.entry.hooks.fetch_usage {
            Some(hook) => hook.call(()).await,
            None => self.inner.fetch_usage().await,
        }
    }
}

impl Provider for PluginProvider {
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
            let result = async {
                self.entry.ensure_auth().await?;

                // First attempt streams through a counting relay: a 401 is only
                // retried when it preceded every event. Replaying deltas onto a
                // channel that already delivered part of the answer would
                // duplicate text in the UI and, after a cancel, in the history.
                let (tx, rx) = flume::unbounded();
                let attempt = async {
                    let result = self
                        .inner
                        .stream_message(model, messages, system, tools, &tx, opts, session_id)
                        .await;
                    drop(tx);
                    result
                };
                let forward = async move {
                    let mut forwarded = false;
                    while let Ok(ev) = rx.recv_async().await {
                        forwarded = true;
                        if event_tx.send_async(ev).await.is_err() {
                            break;
                        }
                    }
                    forwarded
                };
                let (result, forwarded) = futures_lite::future::zip(attempt, forward).await;
                match result {
                    // The plugin mints credentials without the user, so an
                    // expired token costs one silent refresh instead of a
                    // re-login prompt.
                    Err(e) if e.is_auth_error() && !forwarded => {
                        debug!(error = %e, "auth error, refreshing plugin-backed credentials");
                        match self.entry.run_auth(AuthPurpose::Refresh).await {
                            Ok(()) => {
                                self.inner
                                    .stream_message(
                                        model, messages, system, tools, event_tx, opts, session_id,
                                    )
                                    .await
                            }
                            Err(refresh_err) => {
                                warn!(error = %refresh_err, "silent refresh failed, falling back to re-login");
                                Err(e)
                            }
                        }
                    }
                    result => result,
                }
            }
            .await;
            self.mapped(result).await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let result = self.models().await;
            self.mapped(result).await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let result = self.usage().await;
            self.mapped(result).await
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let result = self.entry.run_auth(AuthPurpose::Refresh).await;
            self.mapped(result).await
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let result = self.entry.run_auth(AuthPurpose::Reload).await;
            self.mapped(result).await
        })
    }
}

/// Builds from registry data alone: no hook runs here, because this is called
/// from synchronous code and a hook means re-entering the plugin host.
pub fn create(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let entry = entry(slug).ok_or_else(|| unknown(slug))?;
    // The same handle the codec reads per request, so credentials resolved by a
    // later `ensure_auth` land without rebuilding anything.
    let shared = entry.auth.current.clone();
    let prefix = entry.system_prefix.clone();

    let inner = match entry.target {
        Target::Base(spec) => {
            let native = spec.native.ok_or_else(|| AgentError::Config {
                message: format!("base provider '{}' has no constructor", spec.slug),
            })?;
            (native.with_auth)(shared, timeouts, prefix)
        }
        Target::Codec(protocol) => codec::build(
            protocol,
            shared,
            timeouts,
            prefix,
            entry
                .hooks
                .build_body
                .clone()
                .map(|hook| Arc::new(BodyAdapter(hook)) as Arc<dyn BodyHook>),
        ),
    };

    Ok(Box::new(PluginProvider { entry, inner }))
}

/// Owned, not `&'static`: unlike a script's metadata, a plugin entry can be
/// replaced by a reload while a caller holds the name.
pub fn display_name(slug: &str) -> Option<String> {
    entry(slug).map(|entry| entry.display_name.clone())
}

pub fn base_for_slug(slug: &str) -> Option<&'static ProviderSpec> {
    entry(slug)?.target.spec()
}

pub fn lookup_model(slug: &str, model_id: &str) -> Option<Model> {
    let entry = entry(slug)?;
    let model = longest_prefix_match(&entry.models, model_id)?;
    Some(model.to_model(slug, entry.target.spec()?, model_id.to_string(), model.tier))
}

pub fn find_model_for_tier(slug: &str, tier: ModelTier) -> Option<Model> {
    let entry = entry(slug)?;
    let model = entry.models.iter().find(|model| model.tier == tier)?;
    Some(model.to_model(
        slug,
        entry.target.spec()?,
        model.canonical_id()?.to_string(),
        tier,
    ))
}

pub fn plugin_model_specs_for(slug: &str) -> Vec<String> {
    let Some(entry) = entry(slug) else {
        return Vec::new();
    };
    if entry.models.is_empty() {
        return entry
            .target
            .spec()
            .map(|spec| {
                spec.models()
                    .iter()
                    .flat_map(|entry| entry.prefixes.iter())
                    .map(|prefix| format!("{slug}/{prefix}"))
                    .collect()
            })
            .unwrap_or_default();
    }
    entry
        .models
        .iter()
        .filter_map(PluginModel::canonical_id)
        .map(|id| format!("{slug}/{id}"))
        .collect()
}

pub fn registered_slugs() -> Vec<String> {
    entries().keys().map(|slug| slug.to_string()).collect()
}

pub fn auth_providers() -> Vec<(String, String)> {
    entries()
        .values()
        .filter(|entry| entry.hooks.login.is_some())
        .map(|entry| (entry.slug.clone(), entry.display_name.clone()))
        .collect()
}

pub fn is_registered(slug: &str) -> bool {
    entries().contains_key(slug)
}

/// Blocks, so it belongs to the cli thread and never to the plugin host's: the
/// hook it drives runs on the host, and waiting for it from there deadlocks.
pub fn login(slug: &str) -> Result<(), AgentError> {
    interactive(slug, |hooks| hooks.login.clone(), "login")
}

pub fn logout(slug: &str) -> Result<(), AgentError> {
    interactive(slug, |hooks| hooks.logout.clone(), "logout")
}

/// The two hooks a person triggers rather than the model.
type InteractiveHook = Option<Arc<dyn Hook<(), ()>>>;

fn interactive(
    slug: &str,
    pick: fn(&ProviderHooks) -> InteractiveHook,
    what: &str,
) -> Result<(), AgentError> {
    let entry = entry(slug).ok_or_else(|| unknown(slug))?;
    let hook = pick(&entry.hooks).ok_or_else(|| AgentError::Config {
        message: format!("provider '{slug}' does not support {what} (uses API key)"),
    })?;
    smol::block_on(hook.call(()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use futures_lite::future::zip;
    use test_case::test_case;

    use super::*;
    use crate::retry::RetryKind;

    const AUTH_HEADER: &str = "authorization";
    const CONSTANT_TOKEN: &str = "Bearer constant";
    const FIRST_TOKEN: &str = "Bearer refreshed-1";
    const ROTATED_TOKEN: &str = "Bearer refreshed-2";
    const EXAMPLE_HOST: &str = "example.com";
    const EXAMPLE_BASE_URL: &str = "https://example.com";
    const OTHER_HOST: &str = "other.example";
    const OTHER_BASE_URL: &str = "https://other.example";
    const HOOK_CALLED: &str = "a hook ran on a synchronous path";
    const ONE_HOOK_CALL: &str = "concurrent callers share one auth hook run";
    const SINGLE_FLIGHT_RERUN: &str = "a refresh that overlaps nobody runs the hook again";
    const INNER_UNUSED: &str = "the inner provider is not exercised here";

    const SOME_SYSTEM_PREFIX: &str = "You are X.";
    const RELOAD_DROPS_STALE: &str = "a new load must not inherit the last load's entries";
    const RELOAD_KEEPS_SERVING: &str = "a load in progress must not unpublish what is serving";

    fn registration(slug: &str) -> Registration {
        Registration {
            slug: slug.to_string(),
            display_name: "Plugin".to_string(),
            codec: Some(Protocol::Openai),
            base: None,
            base_url: Some(EXAMPLE_BASE_URL.to_string()),
            api_key_env: None,
            system_prefix: None,
            models: vec![
                serde_json::from_value(serde_json::json!({
                    "prefixes": ["plug-1", "plug"],
                    "tier": "strong"
                }))
                .unwrap(),
            ],
            net_hosts: vec![EXAMPLE_HOST.to_string()],
            hooks: ProviderHooks::default(),
        }
    }

    /// A test registers the way a plugin load does: open the window, register,
    /// publish. Entries from the previous load go, exactly as on a `/reload`.
    fn register_loaded(reg: Registration) -> Result<(), RegisterError> {
        begin_load();
        let result = register(reg);
        commit_load();
        result
    }

    struct CountingAuth {
        calls: AtomicUsize,
        rotating: bool,
    }

    impl CountingAuth {
        fn new(rotating: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                rotating,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl Hook<AuthPurpose, PluginAuth> for CountingAuth {
        /// Yields before counting, so a caller that reaches here is observably
        /// in flight when its peer is next polled. Without that the fence would
        /// only hold for as long as `run_auth_hook` keeps an await ahead of the
        /// hook, which is an implementation detail of the caller, not of the
        /// gate under test.
        fn call(&self, _purpose: AuthPurpose) -> BoxFuture<'_, Result<PluginAuth, AgentError>> {
            Box::pin(async move {
                smol::future::yield_now().await;
                let run = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
                Ok(PluginAuth {
                    base_url: None,
                    headers: HashMap::from([(
                        AUTH_HEADER.to_string(),
                        if self.rotating {
                            format!("Bearer refreshed-{run}")
                        } else {
                            CONSTANT_TOKEN.to_string()
                        },
                    )]),
                })
            })
        }
    }

    struct PanicHook;

    impl<In, Out> Hook<In, Out> for PanicHook {
        fn call(&self, _input: In) -> BoxFuture<'_, Result<Out, AgentError>> {
            panic!("{HOOK_CALLED}");
        }
    }

    struct RemapHook(Option<ApiError>);

    impl Hook<ApiError, Option<ApiError>> for RemapHook {
        fn call(&self, _input: ApiError) -> BoxFuture<'_, Result<Option<ApiError>, AgentError>> {
            let mapped = self.0.clone();
            Box::pin(async move { Ok(mapped) })
        }
    }

    struct UnusedProvider;

    impl Provider for UnusedProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a Value,
            _event_tx: &'a Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                Err(AgentError::Config {
                    message: INNER_UNUSED.to_string(),
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async {
                Err(AgentError::Config {
                    message: INNER_UNUSED.to_string(),
                })
            })
        }
    }

    fn entry_with(hooks: ProviderHooks) -> Arc<PluginEntry> {
        let mut reg = registration("in-memory");
        reg.hooks = hooks;
        let auth = AuthState::new(
            initial_auth(&reg, reg.base_url.clone()).unwrap(),
            &reg.net_hosts,
        );
        Arc::new(PluginEntry {
            slug: reg.slug,
            display_name: reg.display_name,
            target: Target::Codec(Protocol::Openai),
            system_prefix: None,
            models: reg.models,
            hooks: reg.hooks,
            auth: Arc::new(auth),
        })
    }

    fn provider_with(hooks: ProviderHooks) -> PluginProvider {
        PluginProvider {
            entry: entry_with(hooks),
            inner: Box::new(UnusedProvider),
        }
    }

    fn token(entry: &PluginEntry) -> String {
        entry.auth.current.lock().unwrap().headers[0].1.clone()
    }

    #[test_case("myslug", true ; "valid_simple")]
    #[test_case("my-slug", true ; "valid_hyphen")]
    #[test_case("my_slug", true ; "valid_underscore")]
    #[test_case("A1", true ; "valid_upper")]
    #[test_case("", false ; "empty")]
    #[test_case("-bad", false ; "leading_hyphen")]
    #[test_case("has.dot", false ; "has_dot")]
    #[test_case("has/slash", false ; "has_slash")]
    #[test_case("has space", false ; "has_space")]
    fn slug_validation(input: &str, expected: bool) {
        assert_eq!(is_valid_slug(input), expected);
    }

    /// Two callers on one slug, because sub-agents each build their own
    /// provider over the same per-slug credentials. Only one may spend a
    /// rotating refresh token, and the parked caller finds the winner's token
    /// already in the shared cell instead of being handed a copy of it. A
    /// non-rotating hook repeats the same bytes, which is how we pin down that
    /// the gate trusts its counter and not a diff.
    #[test_case(true, FIRST_TOKEN, ROTATED_TOKEN ; "rotating_token")]
    #[test_case(false, CONSTANT_TOKEN, CONSTANT_TOKEN ; "unchanged_token")]
    fn refresh_gate_single_flights_concurrent_callers(
        rotating: bool,
        after_one: &str,
        after_two: &str,
    ) {
        let hook = Arc::new(CountingAuth::new(rotating));
        let entry = entry_with(ProviderHooks {
            auth: Some(hook.clone()),
            ..ProviderHooks::default()
        });

        smol::block_on(async {
            let (a, b) = zip(
                entry.run_auth(AuthPurpose::Refresh),
                entry.run_auth(AuthPurpose::Refresh),
            )
            .await;
            a.unwrap();
            b.unwrap();

            assert_eq!(hook.calls(), 1, "{ONE_HOOK_CALL}");
            assert_eq!(token(&entry), after_one);

            // The late caller snapshots the count before locking, so a refresh
            // that overlaps nobody still runs the hook.
            entry.run_auth(AuthPurpose::Refresh).await.unwrap();
        });

        assert_eq!(hook.calls(), 2, "{SINGLE_FLIGHT_RERUN}");
        assert_eq!(token(&entry), after_two);
    }

    #[test]
    fn reload_keeps_the_auth_state() {
        const SLUG: &str = "reload-plugin";
        let hook = Arc::new(CountingAuth::new(true));
        let with_hook = || {
            let mut reg = registration(SLUG);
            reg.hooks.auth = Some(hook.clone());
            reg
        };

        register_loaded(with_hook()).unwrap();
        let before = entry(SLUG).unwrap();
        register_loaded(with_hook()).unwrap();
        let after = entry(SLUG).unwrap();

        assert!(
            Arc::ptr_eq(&before.auth, &after.auth),
            "the new entry picks up the credentials the old one was using"
        );
        smol::block_on(async {
            let (a, b) = zip(
                before.run_auth(AuthPurpose::Refresh),
                after.run_auth(AuthPurpose::Refresh),
            )
            .await;
            a.unwrap();
            b.unwrap();
        });

        assert_eq!(hook.calls(), 1, "{ONE_HOOK_CALL}");
    }

    /// The defect [`begin_load`] exists to prevent: a `/reload` builds a new
    /// plugin host, and an entry the old one left behind answers on a channel
    /// nobody serves. A load that registers nothing must leave nothing.
    ///
    /// And the defect publishing-on-commit exists to prevent: the old entries
    /// only go once the replacements are ready, so a read landing while the
    /// load runs still gets the generation that is actually serving.
    #[test]
    fn a_new_load_drops_the_previous_load_s_entries() {
        const SLUG: &str = "stale-plugin";
        register_loaded(registration(SLUG)).unwrap();
        assert!(is_registered(SLUG));

        begin_load();
        assert!(is_registered(SLUG), "{RELOAD_KEEPS_SERVING}");
        commit_load();

        assert!(!is_registered(SLUG), "{RELOAD_DROPS_STALE}");
        assert!(
            AUTH.read().unwrap().contains_key(SLUG),
            "credentials outlive the load that registered them"
        );
    }

    /// The fence behind "no synchronous path calls a plugin": every hook here
    /// panics, and every synchronous entry point still answers.
    #[test]
    fn create_calls_no_hook() {
        const SLUG: &str = "sync-plugin";
        let mut reg = registration(SLUG);
        reg.hooks = ProviderHooks {
            auth: Some(Arc::new(PanicHook)),
            list_models: Some(Arc::new(PanicHook)),
            build_body: Some(Arc::new(PanicHook)),
            map_error: Some(Arc::new(PanicHook)),
            fetch_usage: Some(Arc::new(PanicHook)),
            login: Some(Arc::new(PanicHook)),
            logout: Some(Arc::new(PanicHook)),
        };
        register_loaded(reg).unwrap();

        create(SLUG, Timeouts::default()).unwrap();
        assert_eq!(display_name(SLUG).as_deref(), Some("Plugin"));
        assert!(base_for_slug(SLUG).is_some());
        assert_eq!(lookup_model(SLUG, "plug-1-mini").unwrap().id, "plug-1-mini");
        assert_eq!(
            find_model_for_tier(SLUG, ModelTier::Strong).unwrap().id,
            "plug-1"
        );
        assert_eq!(plugin_model_specs_for(SLUG), [format!("{SLUG}/plug-1")]);
        assert_eq!(auth_providers(), [(SLUG.to_string(), "Plugin".to_string())]);
    }

    fn bad_slug(reg: &mut Registration) {
        reg.slug = "has.dot".to_string();
    }

    fn builtin_slug(reg: &mut Registration) {
        reg.slug = "anthropic".to_string();
    }

    fn already_registered(reg: &mut Registration) {
        register(registration(&reg.slug)).unwrap();
    }

    fn codec_and_base(reg: &mut Registration) {
        reg.base = Some("openai".to_string());
    }

    fn missing_base(reg: &mut Registration) {
        reg.codec = None;
        reg.base = Some("not-a-provider".to_string());
    }

    fn no_net_hosts(reg: &mut Registration) {
        reg.net_hosts.clear();
    }

    fn outside_a_load(_reg: &mut Registration) {
        commit_load();
    }

    fn body_hook_on_anthropic(reg: &mut Registration) {
        reg.codec = Some(Protocol::Anthropic);
        reg.hooks.build_body = Some(Arc::new(PanicHook));
    }

    fn system_prefix_on_google(reg: &mut Registration) {
        reg.codec = Some(Protocol::Google);
        reg.system_prefix = Some(SOME_SYSTEM_PREFIX.to_string());
    }

    /// `base = "google"` reaches the same constructor as `codec = "google"`,
    /// which drops the prefix, so it has to be refused just as loudly.
    fn system_prefix_on_the_google_base(reg: &mut Registration) {
        reg.codec = None;
        reg.base = Some(super::super::google::SLUG.to_string());
        reg.system_prefix = Some(SOME_SYSTEM_PREFIX.to_string());
    }

    fn base_url_off_the_declared_hosts(reg: &mut Registration) {
        reg.base_url = Some("https://evil.test/v1".to_string());
    }

    /// A declared host reached over plaintext still puts the token on the wire
    /// in the clear, so the host list alone is not the whole question.
    fn base_url_over_plain_http(reg: &mut Registration) {
        reg.base_url = Some(format!("http://{EXAMPLE_HOST}/v1"));
    }

    /// `ConfiguredSlug` has no row: reaching it needs a `providers.toml` entry
    /// for the slug, and the process-wide config is not a test fixture.
    #[test_case(bad_slug, |e| matches!(e, RegisterError::InvalidSlug(_)) ; "invalid_slug")]
    #[test_case(builtin_slug, |e| matches!(e, RegisterError::BuiltinSlug(_)) ; "builtin_collision")]
    #[test_case(already_registered, |e| matches!(e, RegisterError::DuplicateSlug(_)) ; "duplicate")]
    #[test_case(codec_and_base, |e| matches!(e, RegisterError::CodecOrBase(_)) ; "codec_and_base_together")]
    #[test_case(missing_base, |e| matches!(e, RegisterError::UnknownBase { .. }) ; "unknown_base")]
    #[test_case(no_net_hosts, |e| matches!(e, RegisterError::NoNetHosts(_)) ; "net_hosts_empty")]
    #[test_case(outside_a_load, |e| matches!(e, RegisterError::Closed(_)) ; "registration_outside_a_load")]
    #[test_case(body_hook_on_anthropic, |e| matches!(e, RegisterError::Unsupported { .. }) ; "build_body_needs_an_openai_codec")]
    #[test_case(system_prefix_on_google, |e| matches!(e, RegisterError::Unsupported { .. }) ; "google_drops_the_system_prefix")]
    #[test_case(system_prefix_on_the_google_base, |e| matches!(e, RegisterError::Unsupported { .. }) ; "so_does_the_google_base")]
    #[test_case(base_url_off_the_declared_hosts, |e| matches!(e, RegisterError::UndeclaredBaseUrl(_)) ; "base_url_must_be_declared")]
    #[test_case(base_url_over_plain_http, |e| matches!(e, RegisterError::UndeclaredBaseUrl(_)) ; "base_url_must_be_https")]
    fn registration_rejects(mutate: fn(&mut Registration), expected: fn(&RegisterError) -> bool) {
        const SLUG: &str = "rejected-plugin";
        begin_load();
        let mut reg = registration(SLUG);
        mutate(&mut reg);
        let error = register(reg).unwrap_err();
        assert!(expected(&error), "{error}");
        commit_load();
    }

    fn api_error(status: u16, retry_after: Option<Duration>) -> AgentError {
        AgentError::Api {
            status,
            message: "upstream said no".to_string(),
            retry_after,
        }
    }

    #[test]
    fn map_error_absent_leaves_the_error_untouched() {
        let provider = provider_with(ProviderHooks::default());
        let error = smol::block_on(provider.mapped::<()>(Err(api_error(418, None)))).unwrap_err();
        assert!(matches!(error, AgentError::Api { status: 418, .. }));
        assert_eq!(error.retry_kind(), None);
    }

    /// The hook restates status and message; retryability and `Retry-After`
    /// stay maki's to decide.
    #[test]
    fn map_error_remaps_the_status_only() {
        const RETRY_AFTER: Duration = Duration::from_secs(7);
        const REMAPPED: &str = "slow down";
        let provider = provider_with(ProviderHooks {
            map_error: Some(Arc::new(RemapHook(Some(ApiError {
                status: 429,
                message: REMAPPED.to_string(),
            })))),
            ..ProviderHooks::default()
        });

        let error = smol::block_on(provider.mapped::<()>(Err(api_error(400, Some(RETRY_AFTER)))))
            .unwrap_err();

        assert!(
            matches!(&error, AgentError::Api { status: 429, message, .. } if message == REMAPPED)
        );
        assert_eq!(error.retry_kind(), Some(RetryKind::RateLimit));
        assert_eq!(error.retry_after(), Some(RETRY_AFTER));
    }

    #[test]
    fn plugin_auth_rejects_an_undeclared_base_url() {
        const SLUG: &str = "egress-plugin";
        let hosts = vec![EXAMPLE_HOST.to_string()];
        let auth = |base_url: &str| PluginAuth {
            base_url: Some(base_url.to_string()),
            headers: HashMap::new(),
        };

        assert!(
            auth("https://evil.test/v1")
                .into_resolved(SLUG, &hosts)
                .is_err()
        );
        assert_eq!(
            auth(&format!("https://{EXAMPLE_HOST}/v1"))
                .into_resolved(SLUG, &hosts)
                .unwrap()
                .base_url
                .as_deref(),
            Some("https://example.com/v1")
        );
    }

    /// `http` is admitted for loopback alone, so a provider served on the same
    /// machine still works without opening plaintext egress to the internet.
    #[test_case("https://example.com/v1", EXAMPLE_HOST, true ; "https_to_a_declared_host")]
    #[test_case("http://example.com/v1", EXAMPLE_HOST, false ; "plaintext_to_a_remote_host")]
    #[test_case("http://localhost:8080/v1", LOCALHOST, true ; "plaintext_to_localhost")]
    #[test_case("http://127.0.0.1:8080/v1", "127.0.0.1", true ; "plaintext_to_a_loopback_address")]
    #[test_case("ftp://example.com/v1", EXAMPLE_HOST, false ; "a_scheme_that_is_neither")]
    fn base_url_scheme(url: &str, host: &str, accepted: bool) {
        let hosts = vec![host.to_string()];
        assert_eq!(
            declared_base_url("scheme-plugin", Some(url.to_string()), &hosts).is_ok(),
            accepted,
            "{url}"
        );
    }

    /// What the append-only auth map must *not* cost: a reload re-reads the
    /// registration, so an edited `base_url` takes effect on a provider that
    /// has minted nothing, while a token a hook already minted is left alone.
    #[test_case(false, OTHER_BASE_URL ; "a_declaration_edit_lands_when_no_hook_has_run")]
    #[test_case(true, EXAMPLE_BASE_URL ; "a_minted_token_survives_the_reload")]
    fn a_reload_re_reads_the_declaration(minted: bool, expected: &str) {
        const SLUG: &str = "redeclare-plugin";
        let hook = Arc::new(CountingAuth::new(false));
        let mut first = registration(SLUG);
        if minted {
            first.hooks.auth = Some(hook.clone());
        }
        register_loaded(first).unwrap();
        if minted {
            smol::block_on(entry(SLUG).unwrap().ensure_auth()).unwrap();
        }

        let mut second = registration(SLUG);
        second.base_url = Some(OTHER_BASE_URL.to_string());
        second.net_hosts.push(OTHER_HOST.to_string());
        register_loaded(second).unwrap();

        let auth = entry(SLUG).unwrap().auth.current.lock().unwrap().clone();
        assert_eq!(auth.base_url.as_deref(), Some(expected));
    }

    /// Credentials that stopped satisfying the declaration are not kept: a
    /// reload that narrows the host list drops an origin it no longer covers,
    /// rather than carrying the token to a host nobody declared any more.
    #[test]
    fn narrowing_the_declared_hosts_drops_an_origin_it_no_longer_covers() {
        const SLUG: &str = "narrowed-plugin";
        let mut wide = registration(SLUG);
        wide.base_url = Some(OTHER_BASE_URL.to_string());
        wide.net_hosts.push(OTHER_HOST.to_string());
        wide.hooks.auth = Some(Arc::new(CountingAuth::new(false)));
        register_loaded(wide).unwrap();
        smol::block_on(entry(SLUG).unwrap().ensure_auth()).unwrap();

        register_loaded(registration(SLUG)).unwrap();

        let auth = entry(SLUG).unwrap().auth.current.lock().unwrap().clone();
        assert_eq!(auth.base_url.as_deref(), Some(EXAMPLE_BASE_URL));
    }
}
