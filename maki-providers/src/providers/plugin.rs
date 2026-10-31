use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::iter;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use flume::Sender;
use maki_config::host_allowed;
use maki_config::providers::{Protocol, ProvidersConfig, base_url_override, plan_base_url};
use maki_storage::StateDir;
use maki_storage::auth::{lock_credentials, plugin_auth_stamp};
use maki_storage::id::SessionRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};
use url::{Host, Url};

use crate::model::{Model, ModelEntry, ModelFamily, ModelInfo};
use crate::pricing::PricingSchedule;
use crate::provider::{BoxFuture, Provider};
use crate::spec::{BASES, ProviderRegistry, ProviderSpec};
use crate::types::ThinkingFallback;
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
    ThinkingConfig, model_registry,
};

use super::catalog;
use super::codec::{self, BodyHook, CodecOptions, RequestCtx};
pub use super::codec::{EffortField, OpenAiWire, SessionCarrier, ThinkingWire};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts};

mod spec;

const BUILD_BODY_OPTION: &str = "build_body hook";
const SYSTEM_PREFIX_OPTION: &str = "system_prefix";
const OPENAI_OPTION: &str = "openai";
const MAX_TOKENS_FIELD_OPTION: &str = "openai.max_tokens_field";
const STREAM_USAGE_OPTION: &str = "openai.include_stream_usage";
const BASE_URL_OPTION: &str = "base_url";
const HTTPS_SCHEME: &str = "https";
const HTTP_SCHEME: &str = "http";
const LOCALHOST: &str = "localhost";
/// The first recovery after a 401 often just picks up a token that a peer
/// request or another maki process minted, and that one can be dead too. So we
/// allow a second round, which then refreshes for real.
const MAX_AUTH_REPLAYS: usize = 2;

/// One plugin-supplied callback. Generic both ways so every hook on
/// [`ProviderHooks`] has the same shape and presence is one `Option` check.
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
/// what is stored, after a login or another maki process changed it. It spends
/// no token, and that is why it can skip the cross-process lock.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthPurpose {
    Resolve,
    Refresh,
    Reload,
}

/// The request as it goes on the wire, plus the things a plugin branches on.
/// `thinking` is rendered text and not structure, because the hook is a
/// wire-level escape hatch and not a second place to model effort. `None`
/// when thinking is off.
#[derive(Serialize)]
pub struct BodyInput {
    pub body: Value,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The [`ModelInfo::extra`] this slug's `list_models` attached to the
    /// model, absent when discovery said nothing about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_info: Option<Value>,
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

/// The only door an origin a plugin chose comes through, whether it arrived
/// with the registration or from an auth hook. Both end up holding the token
/// maki sends, so both answer to the same host list.
///
/// The scheme is half of the promise: a declared host reached over plaintext
/// still puts the token on the wire in the clear. Only `https` passes, plus
/// `http` on loopback, where a self-hosted provider has no wire to listen on.
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
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("provider '{slug}': base_url '{url}' has no port"))?;
    if !host_allowed(host, port, hosts) {
        return Err(format!(
            "provider '{slug}': base_url '{host}:{port}' is not in the declared net hosts"
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

/// One provider, as data: everything maki needs to build it except the
/// callbacks. Bundled and third-party plugins fill in the same fields, and
/// [`spec`] turns them into the row the rest of maki reads.
///
/// Decoded by serde straight off the authoring surface, and a key it does not
/// know is an error: a typo'd option would otherwise be an option silently
/// left out.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDecl {
    pub slug: String,
    pub display_name: String,
    pub codec: Option<Protocol>,
    pub base: Option<String>,
    /// The declaration's static origin, below the user's `<SLUG>_BASE_URL` and
    /// `providers.toml`. An origin a hook returns is a different thing and
    /// outranks both, see [`CodecOptions::base_url`].
    pub base_url: Option<String>,
    /// Setting it also lists the provider in `maki auth login`, which then
    /// asks for the key.
    pub api_key_env: Option<String>,
    pub system_prefix: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// Only for `codec = "openai"`: grouped under the codec that honours them,
    /// so one rule refuses every option a different target would ignore.
    pub openai: Option<OpenAiWire>,
    /// This and the next three describe a model no row or listing covers.
    /// Each falls back to the native provider behind the codec or base. The
    /// two limits also fill in any row that leaves its own out.
    pub family: Option<ModelFamily>,
    pub accepts_arbitrary_models: Option<bool>,
    /// `Some(None)` is a provider that publishes no output cap, stated as
    /// `false`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "stated_limit"
    )]
    pub max_output_tokens: Option<Option<u32>>,
    pub context_window: Option<u32>,
    /// For a provider whose rates move with the clock. The model rows quote
    /// the rates outside its windows.
    pub pricing_schedule: Option<PricingSchedule>,
    pub aperture: Option<ApertureDecl>,
    pub default_model: Option<String>,
    pub login_url: Option<String>,
    /// Offered by `maki auth login`, and the pick is saved as `plan` in
    /// `providers.toml`.
    #[serde(default)]
    pub plans: Vec<PlanDecl>,
    #[serde(default)]
    pub docs: DocsDecl,
    /// Never written by the author: a Lua plugin's come from its
    /// `plugin.toml`, where the permission to reach them is granted.
    #[serde(skip_deserializing)]
    pub net_hosts: Vec<String>,
}

/// A limit is a number, or `false` when the provider never published one.
/// Leaving the key out already means "borrow the native provider's", so
/// `false` is the only way left to say "there is none".
mod stated_limit {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    const NOT_A_LIMIT: &str = "a limit is a number, or false when none is published";

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Limit {
        Tokens(u32),
        Published(bool),
    }

    pub(super) fn serialize<S: Serializer>(
        limit: &Option<Option<u32>>,
        out: S,
    ) -> Result<S::Ok, S::Error> {
        match limit.flatten() {
            Some(tokens) => out.serialize_u32(tokens),
            None => out.serialize_bool(false),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Option<Option<u32>>, D::Error> {
        match Limit::deserialize(input).map_err(|_| D::Error::custom(NOT_A_LIMIT))? {
            Limit::Tokens(tokens) => Ok(Some(Some(tokens))),
            Limit::Published(false) => Ok(Some(None)),
            Limit::Published(true) => Err(D::Error::custom(NOT_A_LIMIT)),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApertureDecl {
    /// See [`crate::spec::ApertureRoute::path_prefix`].
    pub path_prefix: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDecl {
    pub key: String,
    pub display_name: String,
    /// Defaults to the declaration's own `base_url`.
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub login_url: Option<String>,
}

/// Read by the docs generator only.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocsDecl {
    pub features: Option<String>,
    /// Stands in for the model table of a provider that declares no rows.
    pub discovery_note: Option<String>,
}

/// A declaration plus the callbacks that go with it. The split is the point:
/// the data half is dumpable, the behaviour half is not. They still travel
/// together, so nothing builds a provider from data that was never checked
/// against its hooks.
pub struct Registration {
    pub decl: ProviderDecl,
    pub hooks: ProviderHooks,
}

/// Who wrote a declaration. Neither may take a slug maki compiles in. Only a
/// bundled plugin may take its own slug, and only its origins are trusted,
/// see [`vouched_origin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclAuthority {
    /// maki's own: a plugin compiled into the binary.
    Bundled,
    /// Anything that arrived after the build: a package, a local plugin
    /// directory, `init.lua`.
    ThirdParty,
    /// The user's own config file (`init.lua`). Runs with every permission
    /// the user's `plugin.toml` grants, so it may register any slug maki
    /// does not ship without a separate grant.
    Config,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error(
        "invalid provider slug '{0}': must start with a letter or digit and hold only letters, digits, '_' and '-'"
    )]
    InvalidSlug(String),
    #[error("provider slug '{0}' is already defined in providers.toml")]
    ConfiguredSlug(String),
    #[error("provider slug '{0}' belongs to a provider maki ships")]
    ReservedSlug(String),
    #[error(
        "provider slug '{0}' is served by models.dev and the package was not \
         granted it: list it under `providers` in plugin.toml and get it approved"
    )]
    UngrantedTakeover(String),
    #[error("provider '{slug}': {message}")]
    Credentials { slug: String, message: String },
    #[error("provider '{0}' is already registered")]
    DuplicateSlug(String),
    #[error("provider '{slug}': models: {message}")]
    InvalidModels { slug: String, message: String },
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
    /// The native spec behind this target: model family, thinking support,
    /// and for a base also its fallbacks and the model table a plugin that
    /// curates none borrows.
    fn spec(self) -> &'static ProviderSpec {
        match self {
            Self::Base(spec) => spec,
            Self::Codec(protocol) => codec::protocol_spec(protocol),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Base(spec) => format!("base '{}'", spec.slug),
            Self::Codec(protocol) => format!("codec {protocol:?}"),
        }
    }

    /// Where a declared `api_key_env` key goes: the header the native provider
    /// behind this target reads it from, so `codec = "google"` and
    /// `base = "google"` answer alike.
    fn key_header(self) -> KeyHeader {
        match self.spec().slug {
            super::anthropic::SLUG => KeyHeader::Raw(super::anthropic::API_KEY_HEADER),
            super::google::SLUG => KeyHeader::Raw(super::google::API_KEY_HEADER),
            _ => KeyHeader::Bearer,
        }
    }
}

/// Only the openai codecs thread a body hook through [`codec::build`].
///
/// An exhaustive match rather than a list of the ones that work, so a new
/// codec breaks this line and someone has to answer for it. An option a codec
/// cannot honour is a registration error, never a no-op.
fn honours_build_body(target: Target) -> bool {
    match target {
        Target::Codec(Protocol::Openai | Protocol::OpenaiResponses) => true,
        Target::Codec(Protocol::Anthropic | Protocol::Google) | Target::Base(_) => false,
    }
}

/// Both openai codecs read the `openai` table. Chat-only keys are refused on
/// the responses codec rather than dropped, see [`chat_only_option`].
fn honours_openai_wire(target: Target) -> bool {
    match target {
        Target::Codec(Protocol::Openai | Protocol::OpenaiResponses) => true,
        Target::Codec(Protocol::Anthropic | Protocol::Google) | Target::Base(_) => false,
    }
}

/// `max_tokens` and `stream_options` belong to chat completions. The responses
/// API spells the cap `max_output_tokens` and returns usage on
/// `response.completed`, so a declaration that sets either would be ignored.
fn chat_only_option(wire: &OpenAiWire) -> Option<&'static str> {
    if wire.max_tokens_field.is_some() {
        Some(MAX_TOKENS_FIELD_OPTION)
    } else if wire.include_stream_usage.is_some() {
        Some(STREAM_USAGE_OPTION)
    } else {
        None
    }
}

/// Google drops the system prefix and always has (see `super::google`), so a
/// plugin that sets one against it is told instead of ignored. Asked of the
/// spec behind the target, because `codec = "google"` and `base = "google"`
/// reach the same constructor and must answer alike.
fn honours_system_prefix(target: Target) -> bool {
    target.spec().slug != super::google::SLUG
}

/// A base is the native provider whole, and the native only moves for an
/// origin in its auth, which only an auth hook writes. A static one would be
/// dropped and the key sent to the vendor's own host.
fn honours_base_url(target: Target) -> bool {
    match target {
        Target::Codec(_) => true,
        Target::Base(_) => false,
    }
}

pub fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

struct PluginEntry {
    decl: ProviderDecl,
    /// `decl` as spec rows, so model lookup, pricing and login read a plugin
    /// provider the way they read a compiled-in one.
    built: spec::Built,
    authority: DeclAuthority,
    /// The plugin that registered this, `None` for a caller outside any
    /// plugin.
    owner: Option<Arc<str>>,
    target: Target,
    hooks: ProviderHooks,
    /// Shared across reloads of the same owner for this slug. A different
    /// owner starts with a new cell. Held here rather than
    /// looked up beside the entry, so "registered" and "has credentials"
    /// cannot come apart at a call site.
    auth: Arc<AuthState>,
}

/// Ordered by slug, so every listing comes out the same each run.
type Registry = BTreeMap<Box<str>, Arc<PluginEntry>>;

type AuthCell = (Option<Arc<str>>, Arc<AuthState>);

/// What every read answers. Only ever replaced whole, by [`commit_load`], so
/// there is no moment at which a reader can see a half-built registry: during
/// a load the previous generation is still the published one.
static PROVIDERS: LazyLock<RwLock<Arc<Registry>>> = LazyLock::new(RwLock::default);
/// The load in progress. `Some` between [`begin_load`] and [`commit_load`],
/// and open at process start so a host that loads plugins without driving the
/// phases (tests, embedders) still registers.
static STAGING: LazyLock<Mutex<Option<Registry>>> =
    LazyLock::new(|| Mutex::new(Some(Registry::new())));
/// One cell of credentials per slug, for the life of the process: see
/// [`shared_auth`].
/// The current owner and auth state of each slug: see [`shared_auth`].
static AUTH: LazyLock<RwLock<HashMap<Box<str>, AuthCell>>> = LazyLock::new(RwLock::default);
/// Slugs granted to enabled packages, kept even when their load fails so the
/// catalog client cannot take over the slug and receive the saved key. Maps
/// the package name to its granted slugs, so unloading the package releases
/// exactly its own. Cleared by [`begin_load`], so a package the new load
/// skips keeps nothing. See [`owns_slug`].
type Granted = HashMap<Box<str>, HashSet<Box<str>>>;
static GRANTED: LazyLock<RwLock<Granted>> = LazyLock::new(RwLock::default);

/// Opens the registration window, at the top of every plugin load.
///
/// Nothing published goes away here. Dropping the old entries before the
/// replacements exist would make every reader answer "unknown provider" for as
/// long as the load takes, so [`commit_load`] swaps the whole map in one step
/// instead.
///
/// Grants go with the cycle: they are cleared here and re-recorded as each
/// package load starts, so a package this load skips (disabled or removed)
/// keeps no slug owned past the `/reload`.
pub fn begin_load() {
    *STAGING.lock().unwrap() = Some(Registry::new());
    GRANTED.write().unwrap().clear();
}

/// Keeps the slugs of a legacy v0.6.4 `plugin =` selection in
/// `providers.toml` owned by the named package, so the catalog client cannot
/// take them over and receive the saved key. Returns a warning per entry
/// telling the user to migrate: list the slug under `providers` in the
/// package's `plugin.toml`. Call after [`begin_load`], which clears grants.
pub fn keep_legacy_selections() -> Vec<String> {
    let config = ProvidersConfig::load_or_default();
    let mut warnings = Vec::new();
    for (slug, def) in &config.providers {
        if let Some(package) = &def.plugin {
            keep_granted(package, std::slice::from_ref(slug));
            warnings.push(format!(
                "providers.toml selects package '{package}' for '{slug}'; \
                 add '{slug}' to its `providers` list in plugin.toml and remove \
                 the `plugin =` line"
            ));
        }
    }
    warnings
}

/// Publishes what this load registered. A load that registered nothing
/// publishes nothing, which is how a `/reload` drops a plugin that was removed.
///
/// Credentials are re-pointed here and not at registration, so they only ever
/// follow a declaration that is actually going to serve: one a failed plugin
/// staged and [`discard`] took back never touched them.
pub fn commit_load() {
    let Some(staged) = STAGING.lock().unwrap().take() else {
        return;
    };
    for entry in staged.values() {
        let slug = &entry.decl.slug;
        let api_key_env = entry.decl.api_key_env.clone().filter(|var| !var.is_empty());
        if let Err(error) = entry.auth.redeclare(
            slug,
            api_key_env,
            &entry.decl.net_hosts,
            entry.target.key_header(),
        ) {
            warn!(slug, %error, "provider credentials were not re-declared");
        }
    }
    let logins = staged.values().filter_map(|e| e.built.login).collect();
    maki_config::providers::set_registered_providers(logins);
    *PROVIDERS.write().unwrap() = Arc::new(staged);
}

/// Takes back everything `plugin` staged in the load in progress, for a plugin
/// whose load failed after it registered, so its slugs end up where they would
/// be had the plugin never loaded.
pub fn discard(plugin: &str) {
    let mut staging = STAGING.lock().unwrap();
    let Some(staged) = staging.as_mut() else {
        return;
    };
    let dropped: Vec<Box<str>> = staged
        .iter()
        .filter(|(_, entry)| entry.owner.as_deref() == Some(plugin))
        .map(|(slug, _)| slug.clone())
        .collect();
    for slug in &dropped {
        staged.remove(slug);
    }
    if !dropped.is_empty() {
        debug!(plugin, slugs = ?dropped, "discarded the declarations of a failed plugin load");
    }
}

/// Registers a declaration in the load in progress. A slug is registered once
/// per load: a second declaration of it is [`RegisterError::DuplicateSlug`].
///
/// `takeovers` are the slugs the trust layer granted this load to register:
/// every third-party slug must be among them; see [`register_owned`].
pub fn register(
    reg: Registration,
    authority: DeclAuthority,
    takeovers: &[String],
) -> Result<(), RegisterError> {
    register_owned(reg, authority, None, takeovers)
}

/// [`register`] for a Lua plugin, remembered as `plugin`'s so a load of it
/// that fails later can be taken back with [`discard`].
pub fn register_plugin(
    plugin: Arc<str>,
    reg: Registration,
    authority: DeclAuthority,
    takeovers: &[String],
) -> Result<(), RegisterError> {
    register_owned(reg, authority, Some(plugin), takeovers)
}

fn register_owned(
    reg: Registration,
    authority: DeclAuthority,
    owner: Option<Arc<str>>,
    takeovers: &[String],
) -> Result<(), RegisterError> {
    let Registration { decl, hooks } = reg;
    let slug = decl.slug.clone();
    if !is_valid_slug(&slug) {
        return Err(RegisterError::InvalidSlug(slug));
    }
    // A third party on a slug maki ships would be handed the key the user
    // saved for that provider, and would take over its models under a name
    // the picker still labels as maki's own. A slug maki only serves from
    // models.dev is a default the user never chose, so a plugin may replace
    // it with the trust layer's say so, and the takeover is logged below. A
    // bundled slug stays reserved even while its plugin is off, so turning a
    // plugin off never frees the name.
    let reserved = match authority {
        DeclAuthority::Bundled => ProviderRegistry::compiled(&slug).is_some(),
        DeclAuthority::ThirdParty | DeclAuthority::Config => ProviderRegistry::is_shipped(&slug),
    };
    if reserved {
        return Err(RegisterError::ReservedSlug(slug));
    }
    // A granted takeover of a served name is logged.
    if authority == DeclAuthority::ThirdParty && catalog::serves_slug(&slug) {
        if !takeovers.contains(&slug) {
            return Err(RegisterError::UngrantedTakeover(slug));
        }
        warn!(slug, "third-party plugin replaces the models.dev provider");
    }
    // A `providers.toml` entry only defines a provider when it sets a
    // `protocol`, and only a third party loses the slug to it. Without one the
    // entry just tweaks the declaration, like `[deepseek] base_url` pointing at
    // a gateway. A bundled plugin keeps its slug either way and only gets a
    // warning, because failing here would abort the whole bundled load and
    // maki would not start.
    if matches!(authority, DeclAuthority::ThirdParty | DeclAuthority::Config)
        && ProvidersConfig::load_or_default()
            .get(&slug)
            .is_some_and(|def| def.protocol.is_some())
    {
        return Err(RegisterError::ConfiguredSlug(slug));
    }
    if decl.net_hosts.is_empty() {
        return Err(RegisterError::NoNetHosts(slug));
    }
    // Checked here and then thrown away. A declared origin is only the codec's
    // last resort (see [`CodecOptions::base_url`]), not a credential, so
    // `create` can simply read it again. A plan's origin is where
    // `maki auth login` sends the key, so it has to stay on the same hosts.
    let origins = iter::once(&decl.base_url).chain(decl.plans.iter().map(|p| &p.base_url));
    for origin in origins {
        declared_base_url(&slug, origin.clone(), &decl.net_hosts)
            .map_err(RegisterError::UndeclaredBaseUrl)?;
    }
    let target = target_of(&decl, &hooks)?;
    let built = spec::build(&decl, target).map_err(|message| RegisterError::InvalidModels {
        slug: slug.clone(),
        message,
    })?;
    // Checked now and applied at [`commit_load`], so a bad `[<slug>.headers]`
    // fails the plugin that registered the slug. A bundled provider only
    // fails later, in its own [`create`], since failing the bundled load would
    // stop maki from starting over a provider the user may never pick.
    if matches!(authority, DeclAuthority::ThirdParty | DeclAuthority::Config) {
        ResolvedAuth::new(&slug, Vec::new()).map_err(|e| RegisterError::Credentials {
            slug: slug.clone(),
            message: e.to_string(),
        })?;
    }

    let mut staging = STAGING.lock().unwrap();
    let Some(staged) = staging.as_mut() else {
        return Err(RegisterError::Closed(slug));
    };
    let Entry::Vacant(vacant) = staged.entry(slug.as_str().into()) else {
        return Err(RegisterError::DuplicateSlug(slug));
    };
    debug!(
        slug,
        ?authority,
        codec = ?decl.codec,
        base = decl.base.as_deref(),
        models = decl.models.len(),
        hooked_credentials = hooks.auth.is_some(),
        "registered provider declaration"
    );
    vacant.insert(Arc::new(PluginEntry {
        auth: shared_auth(&slug, owner.clone()),
        built,
        decl,
        authority,
        owner,
        target,
        hooks,
    }));
    Ok(())
}

/// What this declaration builds its requests with, once every option it stated
/// has been checked against what that target can honour. An option the target
/// would quietly drop is a rejection, for the reason [`honours_build_body`]
/// gives.
fn target_of(decl: &ProviderDecl, hooks: &ProviderHooks) -> Result<Target, RegisterError> {
    let target = match (decl.codec, &decl.base) {
        (Some(protocol), None) => Target::Codec(protocol),
        (None, Some(base)) => Target::Base(
            ProviderRegistry::compiled(base)
                .filter(|spec| BASES.contains(&spec.slug))
                .ok_or_else(|| RegisterError::UnknownBase {
                    slug: decl.slug.clone(),
                    base: base.clone(),
                })?,
        ),
        _ => return Err(RegisterError::CodecOrBase(decl.slug.clone())),
    };
    let unsupported = |option| RegisterError::Unsupported {
        slug: decl.slug.clone(),
        option,
        target: target.describe(),
    };
    if hooks.build_body.is_some() && !honours_build_body(target) {
        return Err(unsupported(BUILD_BODY_OPTION));
    }
    if decl.system_prefix.is_some() && !honours_system_prefix(target) {
        return Err(unsupported(SYSTEM_PREFIX_OPTION));
    }
    if decl.openai.is_some() && !honours_openai_wire(target) {
        return Err(unsupported(OPENAI_OPTION));
    }
    if matches!(target, Target::Codec(Protocol::OpenaiResponses))
        && let Some(wire) = &decl.openai
        && let Some(option) = chat_only_option(wire)
    {
        return Err(unsupported(option));
    }
    let states_origin =
        decl.base_url.is_some() || decl.plans.iter().any(|plan| plan.base_url.is_some());
    if states_origin && !honours_base_url(target) {
        return Err(unsupported(BASE_URL_OPTION));
    }
    Ok(target)
}

/// One cell of credentials per slug, for the life of the process. Reusing it
/// is what carries a token across a reload, and what stops a reload from
/// minting a second [`RefreshGate`] for a slug whose token is in flight.
/// Reusing stops at the owner, though: a slug another package took over gets
/// a fresh cell, so a token one of them minted never serves the other.
fn shared_auth(slug: &str, owner: Option<Arc<str>>) -> Arc<AuthState> {
    let mut states = AUTH.write().unwrap();
    let (current_owner, state) = states
        .entry(slug.into())
        .or_insert_with(|| (owner.clone(), Arc::new(AuthState::new())));
    if *current_owner != owner {
        *current_owner = owner;
        *state = Arc::new(AuthState::new());
    }
    Arc::clone(state)
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

/// What a declaration's `api_key_env` resolved to. A decl that names one
/// answers for its key up front, the way every built-in does, so availability
/// follows from the declaration rather than from who wrote it.
#[derive(Clone)]
enum DeclaredKeys {
    /// No `api_key_env`: the credentials come from an auth hook, which cannot
    /// run on a synchronous path, so nothing is knowable until one does.
    Hooked,
    Resolved {
        env_var: String,
        pool: KeyPool,
    },
    /// [`KeyPool::resolve`] failed. The message is kept rather than the error
    /// because [`AgentError`] is not `Clone`, and `create` has to answer with
    /// it verbatim every time so the picker hides the provider instead of
    /// listing one that cannot serve a request.
    Missing {
        env_var: String,
        message: String,
    },
}

impl DeclaredKeys {
    /// The same call every built-in makes, so env / saved-credential /
    /// `providers.toml` precedence and the "run `maki auth login`" message are
    /// identical by construction rather than by copying.
    fn resolve(slug: &str, env_var: Option<String>) -> Self {
        let Some(env_var) = env_var else {
            return Self::Hooked;
        };
        match KeyPool::resolve(slug, &env_var) {
            Ok(pool) => Self::Resolved { env_var, pool },
            Err(e) => Self::Missing {
                env_var,
                message: e.to_string(),
            },
        }
    }

    fn env_var(&self) -> Option<&str> {
        match self {
            Self::Hooked => None,
            Self::Resolved { env_var, .. } | Self::Missing { env_var, .. } => Some(env_var),
        }
    }

    /// Resolves `env_var` afresh and adopts the answer only if it says
    /// something new: another variable, other keys, or a key appearing or
    /// going. The same keys keep the pool they had, so re-reading never walks a
    /// rotating pool back to its first key. Answers whether anything moved.
    fn refresh(&mut self, slug: &str, env_var: Option<String>) -> bool {
        let fresh = Self::resolve(slug, env_var);
        let unchanged = match (&*self, &fresh) {
            (Self::Hooked, Self::Hooked) => true,
            (
                Self::Resolved { env_var, pool },
                Self::Resolved {
                    env_var: read,
                    pool: read_pool,
                },
            ) => env_var == read && pool.same_keys(read_pool),
            (
                Self::Missing { env_var, message },
                Self::Missing {
                    env_var: read,
                    message: read_message,
                },
            ) => env_var == read && message == read_message,
            _ => false,
        };
        if !unchanged {
            *self = fresh;
        }
        !unchanged
    }

    /// The credentials a provider starts with, before any hook has run.
    fn initial_auth(&self, slug: &str, header: KeyHeader) -> Result<ResolvedAuth, RegisterError> {
        let auth = match self {
            Self::Resolved { pool, .. } => header.auth(slug, pool.current()),
            Self::Hooked | Self::Missing { .. } => ResolvedAuth::new(slug, Vec::new()),
        };
        // `ResolvedAuth` only fails over `[<slug>.headers]` in providers.toml,
        // which a declaration claiming a built-in slug is allowed to have.
        auth.map_err(|e| RegisterError::Credentials {
            slug: slug.to_string(),
            message: e.to_string(),
        })
    }
}

/// Auth for one slug, kept in its own map so a reload cannot drop a token or a
/// refresh in flight. The codec reads this cell per request, so whatever the
/// hook last wrote is what goes on the wire, with nothing rebuilt or copied
/// back.
struct AuthState {
    current: Arc<Mutex<ResolvedAuth>>,
    /// The latest registration's egress list, not the one the entry a caller
    /// happens to hold was built with. A refresh that started before a reload
    /// still lands its answer here, so vetting it against anything older would
    /// admit an origin the current declaration no longer covers.
    hosts: Mutex<Arc<[String]>>,
    keys: Mutex<DeclaredKeys>,
    gate: RefreshGate,
    /// How the store looked right after our last mint. If it looks different
    /// now, another maki process rotated the token behind our back.
    store_seen: Mutex<Option<u64>>,
}

impl AuthState {
    /// Starts with no credentials and no declared hosts. [`Self::redeclare`]
    /// runs before anything can read this, so starting empty fails closed if
    /// it ever did not, or if the declaration's `[<slug>.headers]` did not
    /// resolve (which [`create`] then reports).
    fn new() -> Self {
        Self {
            current: Arc::new(Mutex::new(ResolvedAuth::withheld())),
            hosts: Mutex::default(),
            keys: Mutex::new(DeclaredKeys::Hooked),
            gate: RefreshGate::default(),
            store_seen: Mutex::default(),
        }
    }

    fn hosts(&self) -> Arc<[String]> {
        Arc::clone(&self.hosts.lock().unwrap())
    }

    /// The pool `create` hands the provider for rotation, or the error the
    /// declared env var resolved to.
    ///
    /// Re-read on every call, the way a built-in rebuilt per `create` re-reads
    /// its key: one that appeared or was replaced since, say by a `maki auth
    /// login` after the old key was revoked, is what the next provider sends.
    fn declared_pool(&self, slug: &str, header: KeyHeader) -> Result<Option<KeyPool>, AgentError> {
        let mut keys = self.keys.lock().unwrap();
        let env_var = keys.env_var().map(str::to_owned);
        // Only over credentials nothing has minted: a token a hook produced is
        // newer than anything an env var can offer.
        if keys.refresh(slug, env_var)
            && !self.gate.ran()
            && let Ok(declared) = keys.initial_auth(slug, header)
        {
            *self.current.lock().unwrap() = declared;
        }
        match &*keys {
            DeclaredKeys::Hooked => Ok(None),
            DeclaredKeys::Resolved { pool, .. } => Ok(Some(pool.clone())),
            DeclaredKeys::Missing { env_var, message } => {
                debug!(slug, env_var, "declared api key did not resolve");
                Err(AgentError::Config {
                    message: message.clone(),
                })
            }
        }
    }

    /// Re-points one slug's credentials at what the newest registration says.
    ///
    /// Three things move on a reload and none may be ignored: the declared
    /// hosts, which every later hook answer is vetted against, the declared key
    /// env var, and the static credentials, which are the only ones a plugin
    /// without an auth hook ever has. A token a hook already minted stays,
    /// unless the new declaration stopped covering the origin it points at.
    fn redeclare(
        &self,
        slug: &str,
        api_key_env: Option<String>,
        hosts: &[String],
        header: KeyHeader,
    ) -> Result<(), RegisterError> {
        *self.hosts.lock().unwrap() = hosts.into();
        let mut keys = self.keys.lock().unwrap();
        keys.refresh(slug, api_key_env);
        let declared = keys.initial_auth(slug, header)?;
        let mut current = self.current.lock().unwrap();
        let undeclared = declared_base_url(slug, current.base_url.clone(), hosts).is_err();
        if !self.gate.ran() || undeclared {
            *current = declared;
        }
        Ok(())
    }

    /// [`Self::mint`] under the cross-process lock: serialised against other
    /// maki processes on the same credentials, and re-entrant in this one so
    /// the hook can store what it minted.
    ///
    /// If another process rotated the store since our last mint, a refresh
    /// turns into a reload. The server revokes the old token on every rotation,
    /// so refreshing again would kill the token that process just got. It
    /// would then refresh on its next 401 and kill ours, and the two would go
    /// on knocking each other out.
    async fn locked_mint(
        &self,
        slug: &str,
        hook: &dyn Hook<AuthPurpose, PluginAuth>,
        purpose: AuthPurpose,
    ) -> Result<Recovery, AgentError> {
        let owned = slug.to_owned();
        let (_lock, stored) = smol::unblock(move || match StateDir::resolve() {
            Ok(dir) => (
                Some(lock_credentials(&dir, &owned)),
                plugin_auth_stamp(&dir, &owned),
            ),
            Err(_) => (None, None),
        })
        .await;
        let purpose = match purpose {
            AuthPurpose::Refresh
                if self.gate.ran() && *self.store_seen.lock().unwrap() != stored =>
            {
                debug!(slug, "stored credentials rotated elsewhere, adopting them");
                AuthPurpose::Reload
            }
            purpose => purpose,
        };
        self.mint(slug, hook, purpose).await?;
        Ok(match purpose {
            AuthPurpose::Reload => Recovery::Adopted,
            _ => Recovery::Minted,
        })
    }

    /// Runs the auth hook and makes what it answered the credentials every
    /// request to the slug carries.
    async fn mint(
        &self,
        slug: &str,
        hook: &dyn Hook<AuthPurpose, PluginAuth>,
        purpose: AuthPurpose,
    ) -> Result<(), AgentError> {
        let mut fresh = hook
            .call(purpose)
            .await?
            .into_resolved(slug, &self.hosts())?;
        let owned = slug.to_owned();
        let stamp =
            smol::unblock(move || plugin_auth_stamp(&StateDir::resolve().ok()?, &owned)).await;
        let mut current = self.current.lock().unwrap();
        // A hook that omits base_url keeps the resolved one. Falling back to
        // the provider's default origin would silently repoint the token.
        if fresh.base_url.is_none() {
            fresh.base_url = current.base_url.take();
        }
        *current = fresh;
        *self.store_seen.lock().unwrap() = stamp;
        Ok(())
    }
}

impl PluginEntry {
    /// Resolve once, lazily, from async code. Every fallible provider method
    /// starts here, so no synchronous path ever has to reach a hook.
    async fn ensure_auth(&self) -> Result<(), AgentError> {
        if self.auth.gate.ran() {
            return Ok(());
        }
        self.run_auth(AuthPurpose::Resolve).await.map(drop)
    }

    /// The only place the auth hook is called. A plugin without one keeps the
    /// credentials its registration declared.
    ///
    /// Answers where the credentials came from, which is a different question
    /// from whether the call failed: with no auth hook this succeeds having
    /// changed nothing, and a caller that replayed a request on that would only
    /// re-send the credentials it already had.
    ///
    /// Callable from the plugin host's own thread, because the hook goes to the
    /// host's priority lane and the dispatch loop serves it while this future
    /// is parked. What may not reach here is a *blocking* caller, and that is
    /// held by construction rather than by a check: every path in is `async`,
    /// and `create` builds a provider without running a hook at all.
    async fn run_auth(&self, purpose: AuthPurpose) -> Result<Recovery, AgentError> {
        self.run_auth_since(purpose, self.auth.gate.generation())
            .await
    }

    /// [`Self::run_auth`] for a caller whose credentials came from generation
    /// `seen`. If something minted since, the caller just uses the newer
    /// credentials and the hook stays quiet. Refreshing would revoke the
    /// token everyone else already moved to.
    async fn run_auth_since(
        &self,
        purpose: AuthPurpose,
        seen: u64,
    ) -> Result<Recovery, AgentError> {
        let Some(hook) = self.hooks.auth.clone() else {
            return Ok(Recovery::Unhooked);
        };
        let slug = self.decl.slug.clone();
        // A reload re-reads what is stored, so it spends no token. It waits
        // for neither the gate nor the cross-process lock, and it runs under
        // `block_on` on the ui thread, where either wait would freeze the ui.
        if matches!(purpose, AuthPurpose::Reload) {
            self.auth.mint(&slug, hook.as_ref(), purpose).await?;
            return Ok(Recovery::Adopted);
        }
        // Everything else spends a single-use token, so it runs to completion
        // on its own task. A caller that goes away mid-flight, an Esc or a turn
        // that timed out, must neither release the gate and the lock while the
        // hook still holds the token nor drop what the hook minted with it.
        let auth = Arc::clone(&self.auth);
        let (done, outcome) = flume::bounded(1);
        smol::spawn(async move {
            let minted = auth
                .gate
                .single_flight(seen, auth.locked_mint(&slug, hook.as_ref(), purpose))
                .await;
            let _ = done.send(minted);
        })
        .detach();
        outcome
            .recv_async()
            .await
            .map_err(|_| AgentError::Channel)?
    }
}

/// Single-flight around the plugin's auth hook. Every `create` mints a fresh
/// `PluginProvider`, so sub-agents running their own model would each spend the
/// plugin's rotating refresh token, and a spent one taken twice costs the whole
/// token family. They queue here instead, and the late arrival returns to find
/// the shared credentials already holding what the winner minted. That is why
/// the gate hangs off the per-slug auth state rather than the provider.
///
/// "Late" is judged by the credentials a caller was rejected with, not by when
/// it reached the gate. A request sent before a peer's refresh can get its 401
/// long after that refresh finished, and it must not refresh again.
#[derive(Default)]
struct RefreshGate {
    lock: smol::lock::Mutex<()>,
    /// Bumped by every hook run that went through the gate, so not by a
    /// reload. We count instead of comparing tokens because a refresh can hand
    /// back the very same bytes.
    generation: AtomicU64,
}

/// What [`PluginEntry::run_auth`] did about the credentials in hand.
#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    /// No auth hook, so nothing new to send.
    Unhooked,
    /// Took credentials that a login, a peer or another process minted.
    Adopted,
    /// Ran the hook for credentials of our own.
    Minted,
}

impl RefreshGate {
    /// Whether a resolve or refresh has ever minted, which is also what makes
    /// the lazy first resolve happen once. A reload alone does not count, as it
    /// may skip what a plugin only does on resolve.
    fn ran(&self) -> bool {
        self.generation() > 0
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    async fn single_flight(
        &self,
        seen: u64,
        work: impl Future<Output = Result<Recovery, AgentError>>,
    ) -> Result<Recovery, AgentError> {
        let _guard = self.lock.lock().await;
        if self.generation() != seen {
            debug!(
                seen,
                "credentials replaced since the caller got them, skipping auth hook"
            );
            return Ok(Recovery::Adopted);
        }
        let recovery = work.await?;
        self.generation.fetch_add(1, Ordering::Release);
        Ok(recovery)
    }
}

struct BodyAdapter(Arc<dyn Hook<BodyInput, Value>>);

impl BodyHook for BodyAdapter {
    fn call<'a>(
        &'a self,
        body: Value,
        ctx: &RequestCtx<'_>,
    ) -> BoxFuture<'a, Result<Value, AgentError>> {
        self.0.call(BodyInput {
            body,
            model: ctx.model.id.clone(),
            thinking: ctx
                .opts
                .thinking
                .is_enabled()
                .then(|| ctx.opts.thinking.to_string()),
            model_info: ctx.discovered.as_ref().and_then(|info| info.extra.clone()),
        })
    }
}

struct PluginProvider {
    entry: Arc<PluginEntry>,
    /// Cloned out of the auth state at `create`, so rotation shares the index
    /// with every other provider built for this slug. `None` for a declaration
    /// whose credentials come from a hook: there is no pool to walk.
    pool: Option<KeyPool>,
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
        // Rows describe models, they do not bound the catalogue, so without a
        // hook the listing is whatever the codec or base serves. Listing the
        // rows too, and falling back to them alone, is `fetch_all_models`'s job.
        match &self.entry.hooks.list_models {
            Some(hook) => hook.call(()).await,
            None => self.inner.list_models().await,
        }
    }

    async fn usage(&self) -> Result<Option<ProviderUsage>, AgentError> {
        self.entry.ensure_auth().await?;
        match &self.entry.hooks.fetch_usage {
            Some(hook) => hook.call(()).await,
            None => self.inner.fetch_usage().await,
        }
    }
}

#[warn(clippy::missing_trait_methods)]
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
                let mut replays = 0;
                let mut refreshed = false;
                loop {
                    let seen = self.entry.auth.gate.generation();
                    // Streams through a counting relay, so a 401 is only
                    // replayed when no event got out yet. Replaying onto a
                    // channel that already delivered part of the answer would
                    // duplicate text in the UI and, after a cancel, in the
                    // history.
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
                        // The plugin mints credentials without the user, so a
                        // dead token costs a silent recovery instead of a
                        // re-login prompt. Only a hook earns the replay. A
                        // declaration keyed off an `api_key_env` has none, and
                        // would just re-send the key the server rejected. And a
                        // 401 on a token we just minted is not about the token,
                        // so refreshing again would only revoke our peers'.
                        Err(e)
                            if e.is_auth_error()
                                && !forwarded
                                && !refreshed
                                && replays < MAX_AUTH_REPLAYS =>
                        {
                            debug!(error = %e, replays, "auth error, recovering plugin-backed credentials");
                            match self.entry.run_auth_since(AuthPurpose::Refresh, seen).await {
                                Ok(Recovery::Adopted) => replays += 1,
                                Ok(Recovery::Minted) => {
                                    refreshed = true;
                                    replays += 1;
                                }
                                Ok(Recovery::Unhooked) => return Err(e),
                                Err(refresh_err) => {
                                    warn!(error = %refresh_err, "silent refresh failed, falling back to re-login");
                                    return Err(e);
                                }
                            }
                        }
                        result => return result,
                    }
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
            let result = self.entry.run_auth(AuthPurpose::Refresh).await.map(drop);
            self.mapped(result).await
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let result = self.entry.run_auth(AuthPurpose::Reload).await.map(drop);
            self.mapped(result).await
        })
    }

    /// Answering `None` here is not free: the retry loop rotates keys without
    /// spending its budget, so a provider that hides its pool turns every
    /// rotation into a retry.
    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.pool.as_ref()?,
            &self.entry.auth.current,
            self.entry.target.key_header(),
        ))
    }

    /// Whatever the target knows about its models, a `base`'s own rules
    /// included, still applies under a declaration that wraps it.
    fn adjust_model(&self, model: &mut Model) {
        self.inner.adjust_model(model);
    }

    fn served_context_window<'a>(&'a self, model: &'a Model) -> BoxFuture<'a, Option<u32>> {
        self.inner.served_context_window(model)
    }
}

/// Builds from registry data alone: no hook runs here, because this is called
/// from synchronous code and a hook means re-entering the plugin host.
///
/// A declaration that names an `api_key_env` and resolved no key fails here
/// rather than at the first request, which is what makes `provider_available`
/// tell the truth about it and the picker hide it. A declaration without one
/// gets its credentials from a hook and cannot know yet, so it stays lazy. The
/// rule keys off the declared field, never off who wrote the declaration.
pub fn create(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let entry = entry(slug).ok_or_else(|| unknown(slug))?;
    // Re-resolved per build, as a native constructor did, so a bad
    // `[<slug>.headers]` a built-in's registration let through fails here.
    ResolvedAuth::new(slug, Vec::new())?;
    let pool = entry.auth.declared_pool(slug, entry.target.key_header())?;
    // The same handle the codec reads per request, so credentials resolved by a
    // later `ensure_auth` land without rebuilding anything.
    let shared = entry.auth.current.clone();
    let prefix = entry.decl.system_prefix.clone();

    let inner = match entry.target {
        Target::Base(spec) => {
            let native = spec.native().ok_or_else(|| AgentError::Config {
                message: format!("base provider '{}' has no constructor", spec.slug),
            })?;
            (native.with_auth)(shared, timeouts, prefix)
        }
        Target::Codec(protocol) => codec::build(codec_options(&entry, protocol), shared, timeouts),
    };

    debug!(
        slug,
        rotating_keys = pool.is_some(),
        "built plugin provider"
    );
    Ok(Box::new(PluginProvider { entry, pool, inner }))
}

/// The declaration, as the codec takes it.
///
/// The declared origin lands in `base_url` and not in the auth cell on
/// purpose: `auth.base_url` outranks the user's `<SLUG>_BASE_URL`, which a
/// static declaration must not. `base_url` is read back against env and
/// `providers.toml` inside the compat layer, where nothing here can write it,
/// and `auth.base_url` is only ever written through
/// [`PluginAuth::into_resolved`], the one door that vets an origin against the
/// declared `net_hosts`.
fn codec_options(entry: &PluginEntry, protocol: Protocol) -> CodecOptions {
    let decl = &entry.decl;
    CodecOptions {
        api_key_env: decl.api_key_env.clone().unwrap_or_default().into(),
        base_url: decl.base_url.clone().unwrap_or_default().into(),
        provider_name: decl.display_name.clone().into(),
        system_prefix: decl.system_prefix.clone(),
        openai: decl.openai.clone().unwrap_or_default(),
        build_body: entry
            .hooks
            .build_body
            .clone()
            .map(|hook| Arc::new(BodyAdapter(hook)) as Arc<dyn BodyHook>),
        ..CodecOptions::new(protocol, decl.slug.clone())
    }
}

/// The credentials a registered slug currently holds, for a hook that has to
/// reach an endpoint the codec knows nothing about. A snapshot, like the one
/// every codec takes per request, so a refresh landing mid-call cannot swap the
/// headers a request is already building.
pub fn resolved_auth(slug: &str) -> Option<ResolvedAuth> {
    Some(entry(slug)?.auth.current.lock().unwrap().clone())
}

/// The origin a request to `slug` would reach right now, resolved the way a
/// codec resolves it per request: an auth-supplied origin, then the user's
/// `<SLUG>_BASE_URL` or `providers.toml`, then the declared default.
///
/// A hook reaching an endpoint off the codec's request path has to resolve the
/// same origin the codec would, or a user who points the slug at a gateway has
/// that one call go somewhere else.
pub fn effective_base_url(slug: &str) -> Option<String> {
    resolve_base_url(entry(slug)?.as_ref()).map(|(base_url, _)| base_url)
}

/// The origin of [`effective_base_url`] when the user or maki picked it, never
/// one a third-party plugin authored. A bundled plugin's origin is maki's.
///
/// The codec reaches this origin over the Rust client, which knows no
/// private-address guard, so a provider's own side calls have to reach it on
/// the same terms: a user pointing a slug at a LAN gateway, or sitting behind
/// a fake-IP proxy, gets a listing and not only a chat.
pub fn vouched_origin(slug: &str) -> Option<Url> {
    let entry = entry(slug)?;
    let (base_url, chosen_by) = resolve_base_url(&entry)?;
    if chosen_by == OriginChooser::Plugin
        && matches!(
            entry.authority,
            DeclAuthority::ThirdParty | DeclAuthority::Config
        )
    {
        return None;
    }
    Url::parse(&base_url).ok()
}

#[derive(PartialEq, Eq)]
enum OriginChooser {
    User,
    Plugin,
}

fn resolve_base_url(entry: &PluginEntry) -> Option<(String, OriginChooser)> {
    if let Some(explicit) = entry.auth.current.lock().unwrap().base_url.clone() {
        return Some((explicit, OriginChooser::Plugin));
    }
    let slug = entry.decl.slug.as_str();
    let config = ProvidersConfig::load_or_default();
    let def = config.get(slug);
    if let Some(stated) = base_url_override(slug).or_else(|| def?.base_url.clone()) {
        return Some((stated, OriginChooser::User));
    }
    // The user picked the plan, but the plugin wrote the URL behind it.
    let planned = plan_base_url(slug, def).or_else(|| entry.decl.base_url.clone())?;
    Some((planned, OriginChooser::Plugin))
}

/// The `host:port` of [`effective_base_url`], for a caller deciding whether an
/// outbound request is one this provider is already making. Every origin that
/// can win there was either vetted at registration (the declaration's own, a
/// hook's) or stated by the user (`<SLUG>_BASE_URL`, `providers.toml`).
///
/// The port is always written out, even the default one. This string is
/// matched as a `net_hosts` pattern, and a bare host there means every port.
pub fn effective_authority(slug: &str) -> Option<String> {
    let base_url = Url::parse(&effective_base_url(slug)?).ok()?;
    Some(format!(
        "{}:{}",
        base_url.host_str()?,
        base_url.port_or_known_default()?
    ))
}

/// What a codec declaration sends for `choice`. One that names no
/// `openai.thinking` dialect leaves the mode to the model's own fields, which
/// send nothing for a mode they do not spell.
fn declared_sends_thinking(model: &Model, choice: ThinkingConfig) -> ThinkingConfig {
    let sent = || {
        let entry = entry(&model.provider)?;
        let wire = entry.decl.openai.as_ref()?.thinking.as_ref()?;
        let discovered = model_registry::discovered(&model.provider, &model.id);
        Some(wire.sends(discovered.as_ref(), choice))
    };
    sent().unwrap_or(choice)
}

pub fn spec(slug: &str) -> Option<&'static ProviderSpec> {
    Some(entry(slug)?.built.spec)
}

/// The native provider whose wire `slug` speaks, through its codec or base.
pub fn native_spec(slug: &str) -> Option<&'static ProviderSpec> {
    Some(entry(slug)?.target.spec())
}

/// The provider a `base` lends whole, models.dev entries included. A codec
/// lends only its wire, so it has none.
pub fn base_spec(slug: &str) -> Option<&'static ProviderSpec> {
    match entry(slug)?.target {
        Target::Base(spec) => Some(spec),
        Target::Codec(_) => None,
    }
}

fn declared_thinking_fields_fallback(model: &Model) -> Option<ThinkingFallback> {
    let entry = entry(&model.provider)?;
    match entry.target {
        Target::Codec(Protocol::Openai) => entry
            .decl
            .openai
            .as_ref()
            .is_none_or(|wire| wire.thinking.is_none())
            .then_some(ThinkingFallback::None),
        Target::Codec(Protocol::OpenaiResponses | Protocol::Anthropic | Protocol::Google)
        | Target::Base(_) => None,
    }
}

/// A provider for `slug` built against auth the caller resolved, for a caller
/// that routes a request onto another provider's wire rather than owning the
/// credentials. `None` when no declaration drives a codec for the slug, which
/// leaves the caller its own fallback.
///
/// `system_prefix` is the routing caller's, and it outranks the declared one
/// for the same reason the native path takes it as an argument: it belongs to
/// the session, not to the provider.
pub fn build_with_auth(
    slug: &str,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Option<Box<dyn Provider>> {
    let entry = entry(slug)?;
    let Target::Codec(protocol) = entry.target else {
        return None;
    };
    let mut options = codec_options(&entry, protocol);
    options.system_prefix = system_prefix.or(options.system_prefix);
    Some(codec::build(options, auth, timeouts))
}

pub fn specs() -> Vec<&'static ProviderSpec> {
    entries().values().map(|entry| entry.built.spec).collect()
}

/// The providers whose `maki auth login` runs a `login` hook.
pub fn auth_providers() -> Vec<(String, String)> {
    entries()
        .values()
        .filter(|entry| entry.hooks.login.is_some())
        .map(|entry| (entry.decl.slug.clone(), entry.decl.display_name.clone()))
        .collect()
}

pub fn is_registered(slug: &str) -> bool {
    entries().contains_key(slug)
}

/// Records the slugs granted to an enabled package, so they stay owned even
/// when the package's load fails: the catalog client must not take over the
/// slug and receive the key the user saved for it. Called before the load
/// attempt, so a failure does not lose the grant.
pub fn keep_granted(package: &str, slugs: &[String]) {
    let mut granted = GRANTED.write().unwrap();
    let set = granted.entry(package.into()).or_default();
    for slug in slugs {
        set.insert(slug.as_str().into());
    }
}

/// Drops the slugs of a package being unloaded, so a later catalog fetch may
/// serve them again.
pub fn release_granted(package: &str) {
    GRANTED.write().unwrap().remove(package);
}

/// Whether a slug is owned by a plugin: registered, or granted to an enabled
/// package whose load failed. The catalog client must not serve a slug a
/// package owns, because it would receive the key saved for that name.
pub fn owns_slug(slug: &str) -> bool {
    if entries().contains_key(slug) {
        return true;
    }
    GRANTED
        .read()
        .unwrap()
        .values()
        .any(|slugs| slugs.contains(slug))
}

/// Blocks, so it belongs to the cli thread and never to the plugin host's: the
/// hook it drives runs on the host, and waiting for it from there deadlocks.
pub fn login(slug: &str) -> Result<(), AgentError> {
    let entry = entry(slug).ok_or_else(|| unknown(slug))?;
    let hook = entry
        .hooks
        .login
        .clone()
        .ok_or_else(|| AgentError::Config {
            message: format!("provider '{slug}' does not support login (uses API key)"),
        })?;
    smol::block_on(hook.call(()))
}

/// Runs the `logout` hook, `false` when no registered provider defines one.
/// Forgetting what maki stored never needs one, so the hook is only for what
/// maki cannot do itself, like revoking a token upstream.
pub fn logout(slug: &str) -> Result<bool, AgentError> {
    let Some(hook) = entry(slug).and_then(|entry| entry.hooks.logout.clone()) else {
        return Ok(false);
    };
    smol::block_on(hook.call(()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use futures_lite::future::zip;
    use maki_config::PROVIDER_BUILTINS;
    use maki_config::providers::base_url_env_var;
    use maki_storage::auth::{PluginAuthData, save_plugin_auth};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::spec::{UNKNOWN_MODEL_CONTEXT_WINDOW, UNKNOWN_MODEL_MAX_OUTPUT};
    use super::*;
    use crate::model::ModelTier;
    use crate::model_registry;
    use crate::retry::RetryKind;
    use crate::test_support::{Canned, serve};
    use crate::types::dialect;
    use crate::{Effort, ThinkingConfig};

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
    const STALE_401_REFRESHED: &str = "a 401 on already replaced credentials spent a token";
    const FOREIGN_ROTATION_REFRESHED: &str =
        "a refresh must adopt another process's rotation before spending a token";
    const RESOLVE_SKIPPED: &str = "a reload before the first request stood in for resolve";
    const INNER_UNUSED: &str = "the inner provider is not exercised here";

    const DISPLAY_NAME: &str = "Plugin";
    const THINKING_ROUTE: &str = "thinking-fields-route";
    const THINKING_GATEWAY: &str = "thinking-fields-gateway";
    const TYPED_THINKING_BUDGET: u32 = 8192;
    const SOME_SYSTEM_PREFIX: &str = "You are X.";
    const MODEL_ID: &str = "plug-1";
    const MODEL_ALIAS: &str = "plug";
    /// Matches [`MODEL_ALIAS`] by prefix and [`MODEL_ID`] more tightly, which
    /// is what makes a lookup prove it took the longest match.
    const MODEL_VARIANT: &str = "plug-1-mini";
    const MODEL_TIER: ModelTier = ModelTier::Strong;
    const PROVIDER_MAX_OUTPUT: u32 = 32_000;
    const PROVIDER_CONTEXT: u32 = 96_000;
    const ROW_MAX_OUTPUT: u32 = 8_000;
    const ROW_CONTEXT: u32 = 48_000;
    const CAP_SLUG: &str = "cap-plugin";
    const UNDECLARED_BASE_URL: &str = "https://evil.test/v1";
    /// A native built-in, which no declaration may take.
    const BUILTIN_SLUG: &str = super::super::anthropic::SLUG;
    /// A slug a bundled plugin ships, which only that plugin may declare.
    const BUNDLED_SLUG: &str = PROVIDER_BUILTINS[0];
    const RELOAD_DROPS_STALE: &str = "a new load must not inherit the last load's entries";
    const RELOAD_KEEPS_SERVING: &str = "a load in progress must not unpublish what is serving";
    const LEASE_LOST: &str = "the origin the auth hook leased must reach the auth cell";
    const RESERVED_SLUG_TAKEN: &str = "a refused declaration must not be serving the slug";

    fn decl(slug: &str) -> ProviderDecl {
        ProviderDecl {
            slug: slug.to_string(),
            display_name: DISPLAY_NAME.to_string(),
            codec: Some(Protocol::Openai),
            base_url: Some(EXAMPLE_BASE_URL.to_string()),
            models: vec![
                serde_json::from_value(serde_json::json!({
                    "prefixes": [MODEL_ID, MODEL_ALIAS],
                    "tier": MODEL_TIER,
                }))
                .unwrap(),
            ],
            net_hosts: vec![EXAMPLE_HOST.to_string()],
            ..ProviderDecl::default()
        }
    }

    fn registration(slug: &str) -> Registration {
        Registration {
            decl: decl(slug),
            hooks: ProviderHooks::default(),
        }
    }

    /// Borrows `base` whole, so it states no origin of its own.
    fn based(slug: &str, base: &str) -> Registration {
        let mut reg = registration(slug);
        reg.decl.codec = None;
        reg.decl.base = Some(base.to_string());
        reg.decl.base_url = None;
        reg
    }

    /// A test registers the way a plugin load does: open the window, register,
    /// publish. Entries from the previous load go, exactly as on a `/reload`.
    /// The slug is granted, which a takeover test overrides to cover.
    fn register_loaded(reg: Registration) -> Result<(), RegisterError> {
        let slug = reg.decl.slug.clone();
        register_loaded_as(reg, DeclAuthority::ThirdParty, &[slug])
    }

    fn register_loaded_as(
        reg: Registration,
        authority: DeclAuthority,
        takeovers: &[String],
    ) -> Result<(), RegisterError> {
        begin_load();
        let result = register(reg, authority, takeovers);
        commit_load();
        result
    }

    struct CountingAuth {
        purposes: Mutex<Vec<AuthPurpose>>,
        rotating: bool,
        /// The origin this hook leases. A hook is the only way an origin ever
        /// reaches the auth cell, a declared one being the codec's last resort.
        base_url: Option<String>,
    }

    impl CountingAuth {
        fn new(rotating: bool) -> Self {
            Self {
                purposes: Mutex::default(),
                rotating,
                base_url: None,
            }
        }

        fn leasing(base_url: &str) -> Self {
            Self {
                base_url: Some(base_url.to_string()),
                ..Self::new(false)
            }
        }

        fn calls(&self) -> usize {
            self.purposes.lock().unwrap().len()
        }

        fn purposes(&self) -> Vec<AuthPurpose> {
            self.purposes.lock().unwrap().clone()
        }
    }

    impl Hook<AuthPurpose, PluginAuth> for CountingAuth {
        /// Yields before counting, so a caller that reaches here is observably
        /// in flight when its peer is next polled. Without that the fence would
        /// only hold for as long as [`PluginEntry::run_auth`] keeps an await
        /// ahead of the hook, which is the caller's business and not the gate's.
        fn call(&self, purpose: AuthPurpose) -> BoxFuture<'_, Result<PluginAuth, AgentError>> {
            Box::pin(async move {
                smol::future::yield_now().await;
                let run = {
                    let mut purposes = self.purposes.lock().unwrap();
                    purposes.push(purpose);
                    purposes.len()
                };
                Ok(PluginAuth {
                    base_url: self.base_url.clone(),
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

    /// Says when it has started, then holds on until released, so a test can
    /// drop the caller while the hook is known to be mid-flight.
    struct ParkedAuth {
        entered: flume::Sender<()>,
        released: flume::Receiver<()>,
        calls: AtomicUsize,
    }

    impl Hook<AuthPurpose, PluginAuth> for ParkedAuth {
        fn call(&self, _purpose: AuthPurpose) -> BoxFuture<'_, Result<PluginAuth, AgentError>> {
            Box::pin(async move {
                let _ = self.entered.send(());
                let _ = self.released.recv_async().await;
                self.calls.fetch_add(1, Ordering::AcqRel);
                Ok(PluginAuth {
                    base_url: None,
                    headers: HashMap::from([(AUTH_HEADER.to_string(), FIRST_TOKEN.to_string())]),
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
        const SLUG: &str = "in-memory";
        let decl = decl(SLUG);
        let auth = AuthState::new();
        auth.redeclare(SLUG, None, &decl.net_hosts, KeyHeader::Bearer)
            .unwrap();
        let target = Target::Codec(Protocol::Openai);
        Arc::new(PluginEntry {
            built: spec::build(&decl, target).unwrap(),
            decl,
            authority: DeclAuthority::ThirdParty,
            owner: None,
            target,
            hooks,
            auth: Arc::new(auth),
        })
    }

    fn provider_with(hooks: ProviderHooks) -> PluginProvider {
        PluginProvider {
            entry: entry_with(hooks),
            pool: None,
            inner: Box::new(UnusedProvider),
        }
    }

    /// Points every per-user directory at a fresh temp dir, so the credential
    /// store a test writes is its own.
    fn isolated_home() -> TempDir {
        let home = TempDir::new().unwrap();
        for var in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            unsafe { std::env::set_var(var, home.path()) };
        }
        home
    }

    fn token(entry: &PluginEntry) -> String {
        entry.auth.current.lock().unwrap().headers[0].1.clone()
    }

    fn base_url(slug: &str) -> Option<String> {
        let entry = entry(slug).unwrap();
        let auth = entry.auth.current.lock().unwrap();
        auth.base_url.clone()
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

            // A 401 on the credentials from before that refresh shows up late.
            entry.run_auth_since(AuthPurpose::Refresh, 0).await.unwrap();
            assert_eq!(hook.calls(), 1, "{STALE_401_REFRESHED}");

            entry.run_auth(AuthPurpose::Refresh).await.unwrap();
        });

        assert_eq!(hook.calls(), 2, "{SINGLE_FLIGHT_RERUN}");
        assert_eq!(token(&entry), after_two);
    }

    /// Another maki process rotated the store and so killed our token. The
    /// first refresh only reloads, to pick up the new one. Once we have seen
    /// the store, the next 401 is ours to refresh.
    #[test]
    fn a_refresh_after_a_foreign_rotation_reloads() {
        let _home = isolated_home();
        let hook = Arc::new(CountingAuth::new(true));
        let entry = entry_with(ProviderHooks {
            auth: Some(hook.clone()),
            ..ProviderHooks::default()
        });

        smol::block_on(async {
            entry.run_auth(AuthPurpose::Resolve).await.unwrap();
            let dir = StateDir::resolve().unwrap();
            save_plugin_auth(&dir, &entry.decl.slug, &PluginAuthData::default()).unwrap();
            entry.run_auth(AuthPurpose::Refresh).await.unwrap();
            entry.run_auth(AuthPurpose::Refresh).await.unwrap();
        });

        assert_eq!(
            hook.purposes(),
            [
                AuthPurpose::Resolve,
                AuthPurpose::Reload,
                AuthPurpose::Refresh
            ],
            "{FOREIGN_ROTATION_REFRESHED}"
        );
    }

    /// A respawn reloads before any request, and the plugin may only do some
    /// of its work on resolve.
    #[test]
    fn a_reload_before_the_first_request_still_resolves() {
        let hook = Arc::new(CountingAuth::new(true));
        let entry = entry_with(ProviderHooks {
            auth: Some(hook.clone()),
            ..ProviderHooks::default()
        });

        smol::block_on(async {
            entry.run_auth(AuthPurpose::Reload).await.unwrap();
            entry.ensure_auth().await.unwrap();
        });

        assert_eq!(
            hook.purposes(),
            [AuthPurpose::Reload, AuthPurpose::Resolve],
            "{RESOLVE_SKIPPED}"
        );
    }

    /// One package's token must not survive being replaced by another. The
    /// same owner keeps its gate across the reload, a different one starts a
    /// fresh cell, and returning to the first does not resurrect its token.
    #[test]
    fn owner_changes_do_not_reuse_another_packages_auth_gate() {
        const SLUG: &str = "owner-auth-test";
        const FIRST_OWNER: &str = "first-owner";
        const NEXT_OWNER: &str = "next-owner";
        const HOST: &str = "127.0.0.1";
        const MODELS: &[Canned] = &[
            Canned::json(200, r#"{"data":[]}"#),
            Canned::json(200, r#"{"data":[]}"#),
            Canned::json(200, r#"{"data":[]}"#),
            Canned::json(200, r#"{"data":[]}"#),
        ];
        let _home = isolated_home();
        let (base, requests) = serve(MODELS);
        let first = Arc::new(CountingAuth::new(true));
        let next = Arc::new(CountingAuth::new(false));
        let returned = Arc::new(CountingAuth::new(true));
        for (owner, hook) in [
            (FIRST_OWNER, &first),
            (FIRST_OWNER, &first),
            (NEXT_OWNER, &next),
            (FIRST_OWNER, &returned),
        ] {
            let mut reg = registration(SLUG);
            reg.decl.base_url = Some(base.clone());
            reg.decl.net_hosts = vec![HOST.into()];
            reg.hooks.auth = Some(hook.clone());
            begin_load();
            register_plugin(
                Arc::from(owner),
                reg,
                DeclAuthority::ThirdParty,
                &[SLUG.to_owned()],
            )
            .unwrap();
            commit_load();
            let provider = create(SLUG, Timeouts::default()).unwrap();
            smol::block_on(provider.list_models()).unwrap();
        }
        assert_eq!(first.calls(), 1);
        assert_eq!(next.calls(), 1);
        assert_eq!(returned.calls(), 1);
        let requests = requests.lock().unwrap();
        let tokens = requests
            .iter()
            .map(|request| request.headers[AUTH_HEADER].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            tokens,
            [FIRST_TOKEN, FIRST_TOKEN, CONSTANT_TOKEN, FIRST_TOKEN]
        );
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
    fn a_new_load_drops_the_entries_of_the_previous_one() {
        const SLUG: &str = "stale-plugin";
        const CREDENTIALS_DROPPED: &str = "credentials must outlive the load that registered them";
        register_loaded(registration(SLUG)).unwrap();

        begin_load();
        assert!(is_registered(SLUG), "{RELOAD_KEEPS_SERVING}");
        commit_load();

        assert!(!is_registered(SLUG), "{RELOAD_DROPS_STALE}");
        assert!(
            AUTH.read().unwrap().contains_key(SLUG),
            "{CREDENTIALS_DROPPED}"
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
        assert_eq!(spec(SLUG).map(|s| s.display_name), Some(DISPLAY_NAME));
        let variant = Model::from_spec(&format!("{SLUG}/{MODEL_VARIANT}")).unwrap();
        assert_eq!(variant.id, MODEL_VARIANT);
        assert_eq!(
            Model::from_tier_dynamic(SLUG, MODEL_TIER).unwrap().id,
            MODEL_ID
        );
        assert_eq!(
            auth_providers(),
            [(SLUG.to_string(), DISPLAY_NAME.to_string())]
        );
    }

    /// Availability follows the declaration and not who wrote it: a decl that
    /// names an `api_key_env` answers for its key, so `create` fails while
    /// there is none and the picker hides the provider rather than listing one
    /// that cannot serve a request. A key that appears later needs no reload,
    /// and neither does one that replaces a key the provider already had,
    /// which is what a user does after the old one was revoked.
    #[test]
    fn a_declared_key_env_decides_availability() {
        const SLUG: &str = "keyed-plugin";
        const ENV_VAR: &str = "MAKI_TEST_KEYED_PLUGIN_KEY";
        const KEY: &str = "sk-keyed";
        const REPLACED_KEY: &str = "sk-replaced";
        const LISTED_BUT_KEYLESS: &str = "a declared key env with no key must not build";
        const STALE_KEY: &str = "a replaced key must reach the next provider built";
        let mut reg = registration(SLUG);
        reg.decl.api_key_env = Some(ENV_VAR.to_string());
        register_loaded(reg).unwrap();

        let error = create(SLUG, Timeouts::default())
            .err()
            .expect(LISTED_BUT_KEYLESS);
        assert!(error.to_string().contains(ENV_VAR), "{error}");

        unsafe { std::env::set_var(ENV_VAR, KEY) };
        create(SLUG, Timeouts::default()).unwrap();
        assert_eq!(token(&entry(SLUG).unwrap()), format!("Bearer {KEY}"));

        unsafe { std::env::set_var(ENV_VAR, REPLACED_KEY) };
        create(SLUG, Timeouts::default()).unwrap();
        assert_eq!(
            token(&entry(SLUG).unwrap()),
            format!("Bearer {REPLACED_KEY}"),
            "{STALE_KEY}"
        );
    }

    /// A declared key goes in the header the target reads it from: a Bearer
    /// token is no key to Gemini, and to Anthropic it reads as an OAuth token.
    #[test_case(Some(Protocol::Openai), None, "authorization", true ; "openai_codec_bearer")]
    #[test_case(Some(Protocol::Anthropic), None, "x-api-key", false ; "anthropic_codec_raw")]
    #[test_case(Some(Protocol::Google), None, "x-goog-api-key", false ; "google_codec_raw")]
    #[test_case(None, Some(crate::providers::google::SLUG), "x-goog-api-key", false ; "google_base_raw")]
    fn a_declared_key_lands_in_the_targets_header(
        codec: Option<Protocol>,
        base: Option<&str>,
        header: &str,
        bearer: bool,
    ) {
        const SLUG: &str = "header-plugin";
        const ENV_VAR: &str = "MAKI_TEST_HEADER_PLUGIN_KEY";
        const KEY: &str = "sk-header";
        let mut reg = match base {
            Some(base) => based(SLUG, base),
            None => registration(SLUG),
        };
        reg.decl.codec = codec;
        reg.decl.api_key_env = Some(ENV_VAR.to_string());
        unsafe { std::env::set_var(ENV_VAR, KEY) };
        register_loaded(reg).unwrap();

        create(SLUG, Timeouts::default()).unwrap();

        let value = if bearer {
            format!("Bearer {KEY}")
        } else {
            KEY.to_string()
        };
        let entry = entry(SLUG).unwrap();
        let auth = entry.auth.current.lock().unwrap();
        assert_eq!(auth.headers, [(header.to_string(), value)]);
    }

    /// Tier defaults are looked up through `default`, like in a curated table.
    /// So a declaration that marks no row still needs one picked per tier, and
    /// a row the author did mark has to keep it.
    #[test_case(None, 0 ; "unmarked_tier_defaults_to_its_first_row")]
    #[test_case(Some(1), 1 ; "a_marked_row_stays_the_default")]
    fn every_declared_tier_has_one_default(marked: Option<usize>, expected: usize) {
        const SLUG: &str = "tiered-plugin";
        const SECOND_MODEL: &str = "plug-2";
        let mut reg = registration(SLUG);
        let mut second = reg.decl.models[0].clone();
        second.prefixes = vec![SECOND_MODEL.to_string()];
        reg.decl.models.push(second);
        if let Some(index) = marked {
            reg.decl.models[index].default = true;
        }
        register_loaded(reg).unwrap();

        let defaults: Vec<usize> = spec(SLUG)
            .unwrap()
            .models()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.default.then_some(index))
            .collect();
        assert_eq!(defaults, [expected]);
    }

    /// Fast mode is part of the Anthropic wire, so a plugin speaking it
    /// through a `base` keeps it for the fast-priced rows it borrows.
    #[test]
    fn the_anthropic_base_lends_fast_mode() {
        const SLUG: &str = "fast-plugin";
        register_loaded(based(SLUG, super::super::anthropic::SLUG)).unwrap();

        let fast_id = ProviderRegistry::compiled(super::super::anthropic::SLUG)
            .unwrap()
            .models()
            .iter()
            .find(|row| row.pricing.as_ref().is_some_and(|p| p.fast.is_some()))
            .and_then(ModelEntry::canonical_id)
            .unwrap();
        let model = Model::from_spec(&format!("{SLUG}/{fast_id}")).unwrap();
        assert!(model.supports_fast());
    }

    /// Listing one model under a `base` overrides that model alone. Every
    /// other model keeps the base's row, and so does every tier the
    /// declaration leaves uncovered.
    #[test]
    fn a_base_lends_the_rows_a_declaration_leaves_out() {
        const SLUG: &str = "lending-plugin";
        register_loaded(based(SLUG, super::super::anthropic::SLUG)).unwrap();

        let base = ProviderRegistry::compiled(super::super::anthropic::SLUG).unwrap();
        let lent = base
            .models()
            .iter()
            .find(|row| row.default && row.tier != MODEL_TIER && row.pricing.is_some())
            .unwrap();
        let lent_id = lent.canonical_id().unwrap();
        let model = Model::from_spec(&format!("{SLUG}/{lent_id}")).unwrap();
        let tier_default = |tier| {
            ProviderRegistry::find_default_for_tier(SLUG, tier).and_then(ModelEntry::canonical_id)
        };

        assert_eq!(Some(model.pricing), lent.pricing);
        assert_eq!(tier_default(lent.tier), Some(lent_id));
        assert_eq!(tier_default(MODEL_TIER), Some(MODEL_ID));
    }

    /// A plugin whose load fails after registering must leave no provider
    /// behind, exactly as if it had never loaded. Another plugin's
    /// declarations in the same load are none of its business.
    #[test]
    fn a_discarded_plugin_leaves_nothing_staged() {
        const PLUGIN: &str = "failed-plugin";
        const SLUG: &str = "failed-plugin-slug";
        const BYSTANDER: &str = "bystander-plugin";
        const DISCARDED_SERVES: &str = "a failed plugin's provider was published";
        const BYSTANDER_LOST: &str = "discarding one plugin took another's provider";
        begin_load();
        register_plugin(
            Arc::from(PLUGIN),
            registration(SLUG),
            DeclAuthority::ThirdParty,
            &[SLUG.to_owned()],
        )
        .unwrap();
        register_plugin(
            Arc::from(BYSTANDER),
            registration(BYSTANDER),
            DeclAuthority::ThirdParty,
            &[BYSTANDER.to_owned()],
        )
        .unwrap();
        discard(PLUGIN);
        commit_load();

        assert!(!is_registered(SLUG), "{DISCARDED_SERVES}");
        assert!(is_registered(BYSTANDER), "{BYSTANDER_LOST}");
    }

    /// A granted slug stays owned when the package's load fails, so the
    /// catalog client cannot take it over and receive the saved key.
    /// Unloading the package releases the slug again.
    #[test]
    fn a_failed_load_keeps_its_granted_slugs_owned() {
        const PACKAGE: &str = "failed-package";
        const SLUG: &str = "granted-but-failed";
        const OTHER: &str = "other-package-slug";
        const OWNED_WHILE_FAILED: &str = "a granted slug must stay owned when the load fails";
        const RELEASED_ON_UNLOAD: &str = "unloading the package must release its slugs";

        keep_granted(PACKAGE, &[SLUG.to_owned()]);
        keep_granted("other-package", &[OTHER.to_owned()]);

        assert!(owns_slug(SLUG), "{OWNED_WHILE_FAILED}");
        assert!(owns_slug(OTHER), "another package's grant is independent");

        release_granted(PACKAGE);
        assert!(!owns_slug(SLUG), "{RELEASED_ON_UNLOAD}");
        assert!(
            owns_slug(OTHER),
            "unloading one package leaves another's grant"
        );

        release_granted("other-package");
    }

    /// Grants are tied to the load cycle: [`begin_load`] drops them, so a
    /// package a `/reload` skips (disabled or removed) keeps no slug owned.
    #[test]
    fn a_new_load_cycle_drops_stale_grants() {
        const STALE: &str = "stale-grant-slug";
        const OWNED: &str = "a granted slug is owned";
        const RESET: &str = "a new load cycle must drop the previous grants";

        keep_granted("gone-package", &[STALE.to_owned()]);
        assert!(owns_slug(STALE), "{OWNED}");

        begin_load();
        commit_load();
        assert!(!owns_slug(STALE), "{RESET}");
    }

    /// A refresh spends a single-use token, so the caller going away (an Esc,
    /// a turn that timed out) must not abandon it halfway: the hook runs to
    /// the end, what it minted lands, and the gate stays shut until it has.
    #[test]
    fn a_refresh_outlives_the_caller_that_started_it() {
        const MINT_DROPPED: &str = "what the abandoned refresh minted never landed";
        let (entered_tx, entered) = flume::bounded(1);
        let (release, released) = flume::bounded(1);
        let hook = Arc::new(ParkedAuth {
            entered: entered_tx,
            released,
            calls: AtomicUsize::new(0),
        });
        let entry = entry_with(ProviderHooks {
            auth: Some(hook.clone()),
            ..ProviderHooks::default()
        });

        smol::block_on(async {
            let abandoned =
                futures_lite::future::poll_once(entry.run_auth(AuthPurpose::Refresh)).await;
            assert!(abandoned.is_none());
            entered.recv_async().await.unwrap();
            release.send(()).unwrap();
            let _gate = entry.auth.gate.lock.lock().await;
        });

        assert_eq!(hook.calls.load(Ordering::Acquire), 1, "{ONE_HOOK_CALL}");
        assert_eq!(token(&entry), FIRST_TOKEN, "{MINT_DROPPED}");
    }

    /// The 401 replay is a credential refresh, not a retry. A declaration whose
    /// key comes from an `api_key_env` has no hook to mint a new one, so a
    /// replay would only re-send the key the server just rejected. With a hook,
    /// a 401 on the token we just minted is not about the token, so it ends
    /// there instead of spending another.
    #[test_case("hookless-401-plugin", false, 1 ; "hookless_does_not_replay")]
    #[test_case("hooked-401-plugin", true, 2 ; "hooked_refreshes_once")]
    fn a_401_is_replayed_only_on_fresh_credentials(slug: &str, hooked: bool, expected: usize) {
        const KEY_ENV: &str = "MAKI_TEST_401_KEY";
        const KEY: &str = "sk-rejected";
        const PROMPT: &str = "read a.txt";
        const UNAUTHORIZED: u16 = 401;
        const REJECTED: Canned = Canned::json(UNAUTHORIZED, r#"{"error":{"message":"revoked"}}"#);
        /// One answer more than a correct provider asks for, so an extra replay
        /// is recorded rather than parked on an `accept` that never returns.
        const SCRIPT: &[Canned] = &[REJECTED; 3];
        const WRONG_REPLAYS: &str = "a 401 replayed stale credentials or past the bound";
        const STILL_UNAUTHORIZED: &str = "the auth error must reach the caller";

        let _home = isolated_home();
        let (base_url, requests) = serve(SCRIPT);
        unsafe { std::env::set_var(base_url_env_var(slug), &base_url) };
        let mut reg = registration(slug);
        if hooked {
            reg.hooks.auth = Some(Arc::new(CountingAuth::new(true)));
        } else {
            unsafe { std::env::set_var(KEY_ENV, KEY) };
            reg.decl.api_key_env = Some(KEY_ENV.to_string());
        }
        register_loaded(reg).unwrap();

        let provider = create(slug, Timeouts::default()).unwrap();
        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        let messages = [Message::user(PROMPT.to_owned())];
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(provider.stream_message(
            &model,
            &messages,
            "",
            &serde_json::json!([]),
            &tx,
            RequestOptions::default(),
            None,
        ));

        assert!(
            result.as_ref().err().is_some_and(AgentError::is_auth_error),
            "{STILL_UNAUTHORIZED}"
        );
        assert_eq!(requests.lock().unwrap().len(), expected, "{WRONG_REPLAYS}");
    }

    /// Anthropic and Google know their vendor's host, so before they took the
    /// declared origin a key meant for a gateway went to the vendor instead.
    #[test_case(Protocol::Anthropic, "anthropic-origin-plugin" ; "anthropic")]
    #[test_case(Protocol::Google, "google-origin-plugin" ; "google")]
    fn every_codec_posts_to_the_declared_origin(protocol: Protocol, slug: &str) {
        const ENV_VAR: &str = "MAKI_TEST_CODEC_ORIGIN_KEY";
        const KEY: &str = "sk-gateway";
        const PROMPT: &str = "hi";
        const SCRIPT: &[Canned] = &[Canned::json(401, r#"{"error":{"message":"no"}}"#)];
        const MISSED_ORIGIN: &str = "the request did not reach the declared origin";

        let (base_url, requests) = serve(SCRIPT);
        let host = Url::parse(&base_url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_owned();
        unsafe { std::env::set_var(ENV_VAR, KEY) };
        let mut reg = registration(slug);
        reg.decl.codec = Some(protocol);
        reg.decl.base_url = Some(base_url);
        reg.decl.net_hosts = vec![host];
        reg.decl.api_key_env = Some(ENV_VAR.to_string());
        register_loaded(reg).unwrap();

        let provider = create(slug, Timeouts::default()).unwrap();
        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        let (tx, _rx) = flume::unbounded();
        let _ = smol::block_on(provider.stream_message(
            &model,
            &[Message::user(PROMPT.to_owned())],
            "",
            &serde_json::json!([]),
            &tx,
            RequestOptions::default(),
            None,
        ));

        assert_eq!(requests.lock().unwrap().len(), 1, "{MISSED_ORIGIN}");
    }

    /// Only an origin the user or maki chose skips the side-call guard, never
    /// the one a third-party decl wrote.
    #[test_case(DeclAuthority::ThirdParty, None, None ; "third_party_declared_origin")]
    #[test_case(DeclAuthority::ThirdParty, Some(OTHER_BASE_URL), Some(OTHER_BASE_URL) ; "third_party_user_origin")]
    #[test_case(DeclAuthority::Bundled, None, Some(EXAMPLE_BASE_URL) ; "bundled_declared_origin")]
    fn vouched_origin_is_the_users_or_makis(
        authority: DeclAuthority,
        configured: Option<&str>,
        expected: Option<&str>,
    ) {
        const SLUG: &str = "vouch-plugin";
        if let Some(configured) = configured {
            unsafe { std::env::set_var(base_url_env_var(SLUG), configured) };
        }
        register_loaded_as(registration(SLUG), authority, &[SLUG.to_owned()]).unwrap();

        let expected = expected.map(|url| Url::parse(url).unwrap());
        assert_eq!(vouched_origin(SLUG), expected);
    }

    #[test_case(None, "example.com:443" ; "the_default_port_spelled_out")]
    #[test_case(Some("http://[::1]:8080/v1"), "[::1]:8080" ; "an_ipv6_origin")]
    fn effective_authority_names_the_port(configured: Option<&str>, expected: &str) {
        const SLUG: &str = "authority-plugin";
        if let Some(configured) = configured {
            unsafe { std::env::set_var(base_url_env_var(SLUG), configured) };
        }
        register_loaded_as(
            registration(SLUG),
            DeclAuthority::ThirdParty,
            &[SLUG.to_owned()],
        )
        .unwrap();

        assert_eq!(effective_authority(SLUG).as_deref(), Some(expected));
    }

    #[test]
    fn a_declaration_builds_its_own_spec_row() {
        const SLUG: &str = "row-plugin";
        const ENV_VAR: &str = "MAKI_TEST_ROW_PLUGIN_KEY";
        const CONTEXT_WINDOW: u32 = 64_000;
        let mut reg = registration(SLUG);
        reg.decl.api_key_env = Some(ENV_VAR.to_string());
        reg.decl.family = Some(ModelFamily::Synthetic);
        reg.decl.context_window = Some(CONTEXT_WINDOW);
        reg.decl.default_model = Some(MODEL_ID.to_string());
        register_loaded(reg).unwrap();

        let row = spec(SLUG).unwrap();
        assert_eq!(row.api_key_env, ENV_VAR);
        assert_eq!(row.family, ModelFamily::Synthetic);
        assert_eq!(row.fallback_context_window, CONTEXT_WINDOW);
        assert_eq!(ProviderRegistry::get(SLUG).map(|s| s.slug), Some(SLUG));

        let login = maki_config::providers::builtin_provider(SLUG).unwrap();
        assert_eq!(login.default_api_key_env, ENV_VAR);
        assert_eq!(
            login.default_model,
            Some(format!("{SLUG}/{MODEL_ID}").as_str())
        );
    }

    #[test_case(None ; "codec_lends_only_its_wire")]
    #[test_case(Some(BUILTIN_SLUG) ; "base_lends_its_provider")]
    fn undeclared_curation_and_limits(base: Option<&str>) {
        const SLUG: &str = "defaults-plugin";
        let reg = match base {
            Some(base) => based(SLUG, base),
            None => registration(SLUG),
        };
        register_loaded(reg).unwrap();

        let row = spec(SLUG).unwrap();
        let expected = match base.and_then(ProviderRegistry::compiled) {
            Some(base) => (
                base.accepts_arbitrary_models,
                base.fallback_max_output,
                base.fallback_context_window,
            ),
            None => (
                true,
                Some(UNKNOWN_MODEL_MAX_OUTPUT),
                UNKNOWN_MODEL_CONTEXT_WINDOW,
            ),
        };
        assert_eq!(
            (
                row.accepts_arbitrary_models,
                row.fallback_max_output,
                row.fallback_context_window,
            ),
            expected
        );
    }

    /// A codec sends `adaptive` the way its declared dialect spells it, and
    /// without a dialect it leaves the mode to fields the model does not have.
    #[test_case(Some("high-only"), Some(Effort::High) ; "dialect_with_a_fixed_level")]
    #[test_case(Some("deepseek"), None ; "dialect_that_leaves_it_to_the_api")]
    #[test_case(None, None ; "no_dialect")]
    fn a_codec_declaration_sends_adaptive_as_its_dialect(
        dialect: Option<&str>,
        expected: Option<Effort>,
    ) {
        let slug = format!("adaptive-codec-{}", dialect.unwrap_or("plain"));
        let mut reg = registration(&slug);
        reg.decl.openai = dialect.map(|name| {
            serde_json::from_value(serde_json::json!({ "thinking": { "dialect": name } })).unwrap()
        });
        register_loaded(reg).unwrap();

        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        assert_eq!(model.adaptive_effort(), expected);
    }

    #[test_case(false, false, false ; "native_fields_without_a_dialect")]
    #[test_case(false, true, false ; "toggle_fields_without_a_dialect")]
    #[test_case(true, false, false ; "dialect_overrides_native_levels")]
    #[test_case(true, true, false ; "dialect_overrides_toggle_fields")]
    #[test_case(true, true, true ; "required_dialect_keeps_enabled_choices")]
    fn codec_dialect_owns_the_effective_thinking_choices(
        dialect: bool,
        toggle_only: bool,
        required: bool,
    ) {
        let slug = format!("effective-thinking-{dialect}-{toggle_only}-{required}");
        let mut reg = registration(&slug);
        let fields = if toggle_only {
            serde_json::json!({"adaptive": {"enable_thinking": true}})
        } else {
            serde_json::json!({"max": {"reasoning_effort": "xhigh"}})
        };
        reg.decl.models[0].supports_thinking = Some(true);
        reg.decl.models[0].requires_thinking = required;
        reg.decl.models[0].thinking_fields = Some(serde_json::from_value(fields).unwrap());
        if dialect {
            reg.decl.openai = Some(
                serde_json::from_value(serde_json::json!({"thinking": {"dialect": "standard"}}))
                    .unwrap(),
            );
        }
        let wire = reg.decl.openai.clone().unwrap_or_default();
        register_loaded(reg).unwrap();

        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        assert_eq!(model.effective_thinking_fields().is_none(), dialect);
        let requested = ThinkingConfig::Effort(Effort::Low);
        let expected = if dialect {
            ThinkingConfig::Effort(Effort::Low)
        } else if toggle_only {
            ThinkingConfig::Adaptive
        } else {
            ThinkingConfig::Effort(Effort::Max)
        };
        assert_eq!(requested.clamped(&model), requested);
        assert_eq!(requested.shown(&model), expected);
        assert!(!model.thinking_options().is_empty());

        let ctx = RequestCtx {
            model: &model,
            opts: RequestOptions {
                thinking: requested,
                fast: false,
            }
            .clamped(&model),
            session: None,
            discovered: None,
        };
        let mut body = serde_json::json!({});
        wire.apply_body(&mut body, &ctx);
        let expected_body = if dialect {
            serde_json::json!({"reasoning_effort": "low"})
        } else if toggle_only {
            serde_json::json!({"enable_thinking": true})
        } else {
            serde_json::json!({"reasoning_effort": "xhigh"})
        };
        assert_eq!(body, expected_body);
    }

    #[test_case(false, ThinkingConfig::Effort(Effort::Max), serde_json::json!({"reasoning_effort": "xhigh"}) ; "named_fields_send_an_effort")]
    #[test_case(true, ThinkingConfig::Adaptive, serde_json::json!({"enable_thinking": true}) ; "toggle_fields_discard_the_count")]
    fn openai_codec_budget_shows_the_fragment_it_sends(
        toggle_only: bool,
        expected: ThinkingConfig,
        expected_body: Value,
    ) {
        let slug = format!("typed-thinking-budget-{toggle_only}");
        let mut reg = registration(&slug);
        reg.decl.models[0].supports_thinking = Some(true);
        reg.decl.models[0].thinking_fields = Some(
            serde_json::from_value(if toggle_only {
                serde_json::json!({"adaptive": {"enable_thinking": true}})
            } else {
                serde_json::json!({"max": {"reasoning_effort": "xhigh"}})
            })
            .unwrap(),
        );
        register_loaded(reg).unwrap();

        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        let thinking = ThinkingConfig::Budget(TYPED_THINKING_BUDGET);
        assert_eq!(thinking.clamped(&model), thinking);
        assert_eq!(thinking.shown(&model), expected);
        let ctx = RequestCtx {
            model: &model,
            opts: RequestOptions {
                thinking,
                fast: false,
            },
            session: None,
            discovered: None,
        };
        let mut body = serde_json::json!({});
        OpenAiWire::default().apply_body(&mut body, &ctx);
        assert_eq!(body, expected_body);
    }

    #[test_case(Protocol::Anthropic ; "anthropic_codec_ignores_fields")]
    #[test_case(Protocol::Google ; "google_codec_ignores_fields")]
    #[test_case(Protocol::OpenaiResponses ; "responses_codec_ignores_fields")]
    fn inactive_codec_fields_do_not_change_thinking(protocol: Protocol) {
        let slug = format!("inactive-thinking-{protocol:?}").to_lowercase();
        let mut reg = registration(&slug);
        reg.decl.codec = Some(protocol);
        reg.decl.models[0].supports_thinking = Some(true);
        reg.decl.models[0].thinking_fields = Some(
            serde_json::from_value(serde_json::json!({
                "max": {"reasoning_effort": "xhigh"}
            }))
            .unwrap(),
        );
        register_loaded(reg).unwrap();

        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        let requested = ThinkingConfig::Effort(Effort::Low);
        assert!(model.effective_thinking_fields().is_none());
        assert_eq!(requested.clamped(&model), requested);
        assert!(
            model
                .thinking_options()
                .iter()
                .any(|option| option.name == Effort::Low.as_str())
        );
    }

    #[test_case("anthropic", MODEL_ID, false ; "anthropic_ignores_fields")]
    #[test_case("google", MODEL_ID, false ; "google_ignores_fields")]
    #[test_case("copilot", MODEL_ID, false ; "copilot_ignores_fields")]
    #[test_case("zai", MODEL_ID, false ; "zai_ignores_fields")]
    #[test_case("xai", MODEL_ID, false ; "xai_ignores_fields")]
    #[test_case("opencode", MODEL_ID, false ; "opencode_ignores_fields")]
    #[test_case("ollama", MODEL_ID, true ; "ollama_honours_fields")]
    #[test_case("llama-cpp", MODEL_ID, true ; "llama_cpp_honours_fields")]
    #[test_case("openai", MODEL_ID, true ; "openai_chat_honours_fields")]
    #[test_case("openai", "gpt-6.1-sol", false ; "openai_plan_ignores_fields")]
    #[test_case("openai", "gpt-5.3-codex", false ; "openai_codex_ignores_fields")]
    #[test_case("aperture", "zai/glm-5.2", false ; "aperture_native_zai_ignores_fields")]
    #[test_case("aperture", "llama-cpp/example-model", true ; "aperture_local_honours_fields")]
    #[test_case("aperture", "ollama/qwen3", true ; "aperture_ollama_honours_fields")]
    #[test_case("aperture", "unrouted/example-model", false ; "aperture_fallback_ignores_fields")]
    fn native_base_picker_uses_only_effective_fields(
        base: &str,
        model_id: &str,
        uses_fields: bool,
    ) {
        for required in [false, true] {
            for toggle_only in [false, true] {
                let slug = format!("native-fields-{base}-{required}-{toggle_only}");
                let mut reg = based(&slug, base);
                let row = &mut reg.decl.models[0];
                row.prefixes = vec![model_id.into()];
                row.supports_thinking = Some(true);
                row.requires_thinking = required;
                row.thinking_fields = Some(
                    serde_json::from_value(if toggle_only {
                        serde_json::json!({"adaptive": {"enable_thinking": true}})
                    } else {
                        serde_json::json!({"max": {"reasoning_effort": "xhigh"}})
                    })
                    .unwrap(),
                );
                register_loaded(reg).unwrap();

                let model = Model::from_spec(&format!("{slug}/{model_id}")).unwrap();
                assert_eq!(model.effective_thinking_fields().is_some(), uses_fields);
                let options = model.thinking_options();
                assert!(!options.is_empty());
                assert_eq!(options.iter().any(|o| o.name == "off"), !required);
                let requested = ThinkingConfig::Effort(Effort::Low);
                assert_eq!(requested.clamped(&model), requested);
                if uses_fields {
                    let names: Vec<_> = options.iter().map(|o| o.name).collect();
                    let expected: Vec<_> = [
                        (!required).then_some("off"),
                        Some("adaptive"),
                        (!toggle_only).then_some("max"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    assert_eq!(names, expected);
                    assert!(options.iter().all(|o| o.tokens.is_none()));
                    assert_eq!(
                        requested.shown(&model),
                        if toggle_only {
                            ThinkingConfig::Adaptive
                        } else {
                            ThinkingConfig::Effort(Effort::Max)
                        }
                    );
                } else {
                    let mut without_fields = model.clone();
                    without_fields.thinking_fields = None;
                    assert_eq!(options, without_fields.thinking_options());
                    for thinking in [requested, ThinkingConfig::Adaptive] {
                        assert_eq!(thinking.shown(&model), thinking.shown(&without_fields));
                    }
                }
            }
        }
    }

    #[test_case(None, false, true ; "openai_codec_honours_fields")]
    #[test_case(None, true, false ; "explicit_dialect_ignores_fields")]
    #[test_case(Some("google"), false, false ; "native_google_ignores_fields")]
    #[test_case(Some("llama-cpp"), false, false ; "declared_base_has_no_aperture_constructor")]
    #[test_case(Some("aperture"), false, false ; "declared_aperture_base_falls_back_without_recursing")]
    fn aperture_picker_follows_its_routed_adapter(
        base: Option<&str>,
        dialect: bool,
        uses_fields: bool,
    ) {
        let mut route = base.map_or_else(
            || registration(THINKING_ROUTE),
            |base| based(THINKING_ROUTE, base),
        );
        route.decl.aperture = Some(ApertureDecl {
            path_prefix: super::super::aperture::DEFAULT_PATH_PREFIX.into(),
        });
        if dialect {
            route.decl.openai = Some(
                serde_json::from_value(serde_json::json!({"thinking": {"dialect": "standard"}}))
                    .unwrap(),
            );
        }
        let mut gateway = based(THINKING_GATEWAY, super::super::aperture::SLUG);
        let model_id = format!("{THINKING_ROUTE}/{MODEL_ID}");
        gateway.decl.models[0].prefixes = vec![model_id.clone()];
        gateway.decl.models[0].supports_thinking = Some(true);
        gateway.decl.models[0].requires_thinking = true;
        gateway.decl.models[0].thinking_fields = Some(
            serde_json::from_value(serde_json::json!({"adaptive": {"enable_thinking": true}}))
                .unwrap(),
        );
        begin_load();
        let takeovers = [THINKING_ROUTE.to_owned(), THINKING_GATEWAY.to_owned()];
        register(route, DeclAuthority::ThirdParty, &takeovers).unwrap();
        register(gateway, DeclAuthority::ThirdParty, &takeovers).unwrap();
        commit_load();

        let model = Model::from_spec(&format!("{THINKING_GATEWAY}/{model_id}")).unwrap();
        assert_eq!(model.effective_thinking_fields().is_some(), uses_fields);
        assert!(!model.thinking_options().is_empty());
        if uses_fields {
            assert_eq!(
                model
                    .thinking_options()
                    .iter()
                    .map(|option| option.name)
                    .collect::<Vec<_>>(),
                ["adaptive"]
            );
        }
    }

    #[test_case("zai", Some(Effort::High) ; "base_with_a_fixed_level")]
    #[test_case(BUILTIN_SLUG, None ; "base_with_an_adaptive_mode")]
    fn a_based_declaration_sends_adaptive_as_its_base(base: &str, expected: Option<Effort>) {
        let slug = format!("adaptive-based-on-{base}");
        register_loaded(based(&slug, base)).unwrap();

        let model = Model::from_spec(&format!("{slug}/{MODEL_ID}")).unwrap();
        assert_eq!(model.adaptive_effort(), expected);
    }

    /// A plugin's `list_models` knows its own catalogue, so the tier it
    /// reports for a model it alone serves is the one startup and the picker
    /// use.
    #[test]
    fn a_codec_provider_takes_its_discovered_tiers() {
        const SLUG: &str = "discovered-tiers-plugin";
        const DISCOVERED_TIER: ModelTier = ModelTier::Weak;
        let mut reg = registration(SLUG);
        reg.decl.models.clear();
        register_loaded(reg).unwrap();
        model_registry::set_known_models(
            SLUG,
            vec![ModelInfo {
                tier: Some(DISCOVERED_TIER),
                ..ModelInfo::id_only(MODEL_VARIANT.to_string())
            }],
        );

        let model = Model::from_tier_dynamic(SLUG, DISCOVERED_TIER).unwrap();
        assert_eq!(model.spec(), format!("{SLUG}/{MODEL_VARIANT}"));
    }

    #[test_case(None, None, Some(None), None ; "unpublished_provider_cap_reaches_the_row")]
    #[test_case(None, None, Some(Some(PROVIDER_MAX_OUTPUT)), Some(PROVIDER_MAX_OUTPUT) ; "provider_cap_fills_the_row")]
    #[test_case(Some(ROW_MAX_OUTPUT), Some(ROW_CONTEXT), Some(Some(PROVIDER_MAX_OUTPUT)), Some(ROW_MAX_OUTPUT) ; "row_states_its_own")]
    fn rows_inherit_the_provider_limits(
        row_max_output: Option<u32>,
        row_context: Option<u32>,
        provider_max_output: Option<Option<u32>>,
        expected_max_output: Option<u32>,
    ) {
        const SLUG: &str = "limits-plugin";
        let mut reg = registration(SLUG);
        reg.decl.max_output_tokens = provider_max_output;
        reg.decl.context_window = Some(PROVIDER_CONTEXT);
        reg.decl.models[0].max_output_tokens = row_max_output;
        reg.decl.models[0].context_window = row_context;
        register_loaded(reg).unwrap();

        let row = &spec(SLUG).unwrap().models()[0];
        assert_eq!(row.max_output_tokens, expected_max_output);
        assert_eq!(
            row.context_window,
            Some(row_context.unwrap_or(PROVIDER_CONTEXT))
        );
    }

    #[test]
    fn listing_a_model_does_not_move_its_limits() {
        const SLUG: &str = "unstated-limits-plugin";
        const UNLISTED_MODEL: &str = "other-model";
        register_loaded(registration(SLUG)).unwrap();

        let listed = Model::from_spec(&format!("{SLUG}/{MODEL_ID}")).unwrap();
        let unlisted = Model::from_spec(&format!("{SLUG}/{UNLISTED_MODEL}")).unwrap();
        assert!(spec(SLUG).unwrap().models()[0].context_window.is_none());
        assert_eq!(listed.max_output_tokens, unlisted.max_output_tokens);
        assert_eq!(listed.context_window, unlisted.context_window);
    }

    fn authored_cap(stated: Option<Value>) -> Value {
        let mut authored = serde_json::json!({
            "slug": CAP_SLUG,
            "display_name": DISPLAY_NAME,
            "codec": "openai",
        });
        if let Some(stated) = stated {
            authored["max_output_tokens"] = stated;
        }
        authored
    }

    /// Built rows are cached by the dumped JSON. If `false` came back as a
    /// missing key, which means "take the default", a provider with no cap
    /// would share a cached row with one that has a default.
    #[test_case(Some(serde_json::json!(false)), Some(None) ; "unpublished")]
    #[test_case(Some(serde_json::json!(4096)), Some(Some(4096)) ; "published")]
    #[test_case(None, None ; "left_out")]
    fn a_declared_output_cap_round_trips(stated: Option<Value>, expected: Option<Option<u32>>) {
        let decl: ProviderDecl = serde_json::from_value(authored_cap(stated.clone())).unwrap();
        assert_eq!(decl.max_output_tokens, expected);

        let dumped = serde_json::to_value(&decl).unwrap();
        assert_eq!(dumped.get("max_output_tokens"), stated.as_ref());
    }

    #[test]
    fn a_true_output_cap_is_refused() {
        let authored = authored_cap(Some(serde_json::json!(true)));
        assert!(serde_json::from_value::<ProviderDecl>(authored).is_err());
    }

    /// A dialect is named, not spelled out: it resolves by name, an unknown
    /// name says which names there are, and a decl dumps the name it came
    /// from rather than storing it twice.
    #[test]
    fn a_dialect_is_resolved_by_name() {
        const KNOWN: &str = "deepseek";
        const UNKNOWN: &str = "not-a-dialect";
        let authored = |name: &str| {
            serde_json::json!({
                "slug": "dialect-plugin",
                "display_name": DISPLAY_NAME,
                "codec": "openai",
                "openai": { "thinking": { "dialect": name } },
            })
        };

        let decl: ProviderDecl = serde_json::from_value(authored(KNOWN)).unwrap();
        let thinking = decl.openai.as_ref().and_then(|wire| wire.thinking.as_ref());
        assert_eq!(
            thinking.map(|thinking| thinking.dialect),
            Some(&dialect::DEEPSEEK)
        );

        let error = serde_json::from_value::<ProviderDecl>(authored(UNKNOWN))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains(UNKNOWN) && error.contains(KNOWN), "{error}");

        let dumped = serde_json::to_value(&decl).unwrap();
        assert_eq!(
            dumped["openai"]["thinking"]["dialect"],
            serde_json::json!(KNOWN)
        );
    }

    fn bad_slug(reg: &mut Registration) {
        reg.decl.slug = "has.dot".to_string();
    }

    fn two_defaults_for_a_tier(reg: &mut Registration) {
        let mut twin = reg.decl.models[0].clone();
        twin.prefixes = vec![MODEL_VARIANT.to_string()];
        twin.default = true;
        reg.decl.models[0].default = true;
        reg.decl.models.push(twin);
    }

    fn plan_off_the_declared_hosts(reg: &mut Registration) {
        reg.decl.plans = vec![PlanDecl {
            key: MODEL_ID.to_string(),
            display_name: DISPLAY_NAME.to_string(),
            base_url: Some(UNDECLARED_BASE_URL.to_string()),
            default_model: None,
            login_url: None,
        }];
    }

    fn already_registered(reg: &mut Registration) {
        register(registration(&reg.decl.slug), DeclAuthority::Bundled, &[]).unwrap();
    }

    fn codec_and_base(reg: &mut Registration) {
        reg.decl.base = Some("openai".to_string());
    }

    fn missing_base(reg: &mut Registration) {
        reg.decl.codec = None;
        reg.decl.base = Some("not-a-provider".to_string());
    }

    fn no_net_hosts(reg: &mut Registration) {
        reg.decl.net_hosts.clear();
    }

    fn outside_a_load(_reg: &mut Registration) {
        commit_load();
    }

    fn body_hook_on_anthropic(reg: &mut Registration) {
        reg.decl.codec = Some(Protocol::Anthropic);
        reg.hooks.build_body = Some(Arc::new(PanicHook));
    }

    /// The table the chat codec reads, on a codec that would read none of it.
    fn openai_wire_on_anthropic(reg: &mut Registration) {
        reg.decl.codec = Some(Protocol::Anthropic);
        reg.decl.openai = Some(OpenAiWire::default());
    }

    /// A chat field on the responses codec would be dropped on the floor.
    fn max_tokens_field_on_responses(reg: &mut Registration) {
        reg.decl.codec = Some(Protocol::OpenaiResponses);
        reg.decl.openai = Some(
            serde_json::from_value(serde_json::json!({ "max_tokens_field": "max_tokens" }))
                .unwrap(),
        );
    }

    fn system_prefix_on_google(reg: &mut Registration) {
        reg.decl.codec = Some(Protocol::Google);
        reg.decl.system_prefix = Some(SOME_SYSTEM_PREFIX.to_string());
    }

    /// `base = "google"` reaches the same constructor as `codec = "google"`,
    /// which drops the prefix, so it has to be refused just as loudly.
    fn system_prefix_on_the_google_base(reg: &mut Registration) {
        reg.decl.codec = None;
        reg.decl.base = Some(super::super::google::SLUG.to_string());
        reg.decl.system_prefix = Some(SOME_SYSTEM_PREFIX.to_string());
    }

    /// A base only moves for an origin its auth hook returns.
    fn base_url_on_a_base(reg: &mut Registration) {
        reg.decl.codec = None;
        reg.decl.base = Some(super::super::anthropic::SLUG.to_string());
    }

    fn base_url_off_the_declared_hosts(reg: &mut Registration) {
        reg.decl.base_url = Some(UNDECLARED_BASE_URL.to_string());
    }

    /// A declared host reached over plaintext still puts the token on the wire
    /// in the clear, so the host list alone is not the whole question.
    fn base_url_over_plain_http(reg: &mut Registration) {
        reg.decl.base_url = Some(format!("http://{EXAMPLE_HOST}/v1"));
    }

    /// `ConfiguredSlug` and `Credentials` have no row: both need a
    /// `providers.toml` entry for the slug, and the process-wide config is not
    /// a test fixture. They are covered out of line, in
    /// `tests/configured_overlay.rs`.
    #[test_case(bad_slug, |e| matches!(e, RegisterError::InvalidSlug(_)) ; "invalid_slug")]
    #[test_case(already_registered, |e| matches!(e, RegisterError::DuplicateSlug(_)) ; "duplicate")]
    #[test_case(codec_and_base, |e| matches!(e, RegisterError::CodecOrBase(_)) ; "codec_and_base_together")]
    #[test_case(missing_base, |e| matches!(e, RegisterError::UnknownBase { .. }) ; "unknown_base")]
    #[test_case(no_net_hosts, |e| matches!(e, RegisterError::NoNetHosts(_)) ; "net_hosts_empty")]
    #[test_case(outside_a_load, |e| matches!(e, RegisterError::Closed(_)) ; "registration_outside_a_load")]
    #[test_case(body_hook_on_anthropic, |e| matches!(e, RegisterError::Unsupported { .. }) ; "build_body_needs_an_openai_codec")]
    #[test_case(openai_wire_on_anthropic, |e| matches!(e, RegisterError::Unsupported { option, .. } if *option == OPENAI_OPTION) ; "openai_table_needs_the_openai_codec")]
    #[test_case(max_tokens_field_on_responses, |e| matches!(e, RegisterError::Unsupported { option, .. } if *option == MAX_TOKENS_FIELD_OPTION) ; "responses_refuses_the_chat_cap_field")]
    #[test_case(system_prefix_on_google, |e| matches!(e, RegisterError::Unsupported { .. }) ; "google_drops_the_system_prefix")]
    #[test_case(system_prefix_on_the_google_base, |e| matches!(e, RegisterError::Unsupported { .. }) ; "so_does_the_google_base")]
    #[test_case(base_url_on_a_base, |e| matches!(e, RegisterError::Unsupported { option, .. } if *option == BASE_URL_OPTION) ; "base_url_needs_a_codec")]
    #[test_case(base_url_off_the_declared_hosts, |e| matches!(e, RegisterError::UndeclaredBaseUrl(_)) ; "base_url_must_be_declared")]
    #[test_case(base_url_over_plain_http, |e| matches!(e, RegisterError::UndeclaredBaseUrl(_)) ; "base_url_must_be_https")]
    #[test_case(plan_off_the_declared_hosts, |e| matches!(e, RegisterError::UndeclaredBaseUrl(_)) ; "plan_base_url_must_be_declared")]
    #[test_case(two_defaults_for_a_tier, |e| matches!(e, RegisterError::InvalidModels { .. }) ; "one_default_per_tier")]
    fn registration_rejects(mutate: fn(&mut Registration), expected: fn(&RegisterError) -> bool) {
        const SLUG: &str = "rejected-plugin";
        begin_load();
        let mut reg = registration(SLUG);
        mutate(&mut reg);
        let error = register(reg, DeclAuthority::Bundled, &[]).unwrap_err();
        assert!(expected(&error), "{error}");
        commit_load();
    }

    /// A compiled-in slug is nobody's to declare, maki's own plugins
    /// included: a package that took `anthropic` would have the user's
    /// Anthropic key resolved into its own credentials and sent to the origin
    /// it declared, under a name the picker still labels "Anthropic".
    #[test_case(BUILTIN_SLUG, DeclAuthority::ThirdParty ; "third_party")]
    #[test_case(BUILTIN_SLUG, DeclAuthority::Config ; "config")]
    #[test_case(BUILTIN_SLUG, DeclAuthority::Bundled ; "bundled")]
    #[test_case(BUNDLED_SLUG, DeclAuthority::ThirdParty ; "third_party_on_a_bundled_slug")]
    #[test_case(BUNDLED_SLUG, DeclAuthority::Config ; "config_on_a_bundled_slug")]
    fn a_shipped_slug_is_reserved(slug: &str, authority: DeclAuthority) {
        let error = register_loaded_as(registration(slug), authority, &[]).unwrap_err();

        assert!(matches!(error, RegisterError::ReservedSlug(_)), "{error}");
        assert!(!is_registered(slug), "{RESERVED_SLUG_TAKEN}");
    }

    /// The user's own config file may register any slug maki does not ship
    /// without a grant: it runs with every permission the user's `plugin.toml`
    /// grants, so a separate approval would add no protection. A slug maki
    /// ships stays reserved.
    #[test]
    fn a_config_load_registers_any_slug_maki_does_not_ship() {
        const SLUG: &str = "config-registered";
        const REGISTERED: &str = "the config load must register the slug";
        register_loaded_as(registration(SLUG), DeclAuthority::Config, &[]).expect(REGISTERED);
        assert!(is_registered(SLUG), "{REGISTERED}");
    }

    /// Installing a plugin is an explicit act the user reviews, so a slug maki
    /// only serves from models.dev is a third party's to replace once granted:
    /// the plugin takes over the slug, and the catalog listing drops it. A
    /// slug maki ships stays reserved (`a_shipped_slug_is_reserved`), and a
    /// slug without its grant is refused
    /// (`an_ungranted_slug_is_refused`).
    #[test]
    fn a_third_party_plugin_replaces_a_served_catalog_slug() {
        const CATALOG_SLUG: &str = "served-catalog";
        const TAKEOVER_FAILED: &str = "the registration must have taken the slug";
        const REPLACED: &str = "the catalog must stop listing the replaced slug";
        let provider = catalog::schema::CatalogProvider {
            name: DISPLAY_NAME.into(),
            env: Vec::new(),
            npm: catalog::ALLOWED_NPM[0].into(),
            api: Some(EXAMPLE_BASE_URL.into()),
            models: HashMap::new(),
        };
        catalog::seed_catalog_for_tests(
            HashMap::from([(CATALOG_SLUG.into(), provider)]),
            StateDir::from_path(Default::default()),
        );

        let granted = [CATALOG_SLUG.to_owned()];
        register_loaded_as(
            registration(CATALOG_SLUG),
            DeclAuthority::ThirdParty,
            &granted,
        )
        .expect("a granted catalog slug is a third party's to replace");

        assert!(is_registered(CATALOG_SLUG), "{TAKEOVER_FAILED}");
        assert!(
            !catalog::catalog_providers()
                .iter()
                .any(|provider| provider.slug == CATALOG_SLUG),
            "{REPLACED}"
        );
    }

    /// A slug the catalog does not serve registers without a grant, and a
    /// granted slug stays owned when the load fails. A bundled plugin ships
    /// inside the binary: that is maki's own say so, no grant needed.
    #[test]
    fn an_ungranted_slug_is_refused() {
        const NOT_TAKEN: &str = "a refused slug must not be serving it";
        const STILL_LISTED: &str = "the catalog must keep listing the refused slug";
        const BUNDLED_TAKEOVER_FAILED: &str = "a bundled plugin registers without a grant";

        register_loaded_as(
            registration("plain-ungranted"),
            DeclAuthority::ThirdParty,
            &[],
        )
        .expect("a slug the catalog does not serve registers without a grant");

        // A slug the catalog serves: refused the same way, and the catalog
        // keeps listing it.
        const CATALOG_SLUG: &str = "ungranted-catalog";
        let provider = catalog::schema::CatalogProvider {
            name: DISPLAY_NAME.into(),
            env: Vec::new(),
            npm: catalog::ALLOWED_NPM[0].into(),
            api: Some(EXAMPLE_BASE_URL.into()),
            models: HashMap::new(),
        };
        catalog::seed_catalog_for_tests(
            HashMap::from([(CATALOG_SLUG.into(), provider)]),
            StateDir::from_path(Default::default()),
        );

        let error = register_loaded_as(registration(CATALOG_SLUG), DeclAuthority::ThirdParty, &[])
            .unwrap_err();
        assert!(
            matches!(error, RegisterError::UngrantedTakeover(_)),
            "{error}"
        );
        assert!(!is_registered(CATALOG_SLUG), "{NOT_TAKEN}");
        assert!(
            catalog::catalog_providers()
                .iter()
                .any(|provider| provider.slug == CATALOG_SLUG),
            "{STILL_LISTED}"
        );

        register_loaded_as(registration(CATALOG_SLUG), DeclAuthority::Bundled, &[])
            .expect(BUNDLED_TAKEOVER_FAILED);
        assert!(is_registered(CATALOG_SLUG), "{BUNDLED_TAKEOVER_FAILED}");
    }

    const UPSTREAM_SAID_NO: &str = "upstream said no";
    /// A status maki never retries, so a mapping that changes nothing is
    /// visible in `retry_kind` as well as in the status.
    const NOT_RETRYABLE: u16 = 418;
    const RATE_LIMITED: u16 = 429;

    fn api_error(status: u16, retry_after: Option<Duration>) -> AgentError {
        AgentError::Api {
            status,
            message: UPSTREAM_SAID_NO.to_string(),
            retry_after,
        }
    }

    #[test]
    fn map_error_absent_leaves_the_error_untouched() {
        let provider = provider_with(ProviderHooks::default());
        let error =
            smol::block_on(provider.mapped::<()>(Err(api_error(NOT_RETRYABLE, None)))).unwrap_err();
        assert!(matches!(
            error,
            AgentError::Api {
                status: NOT_RETRYABLE,
                ..
            }
        ));
        assert_eq!(error.retry_kind(), None);
    }

    /// The hook restates status and message. Retryability and `Retry-After`
    /// stay maki's to decide.
    #[test]
    fn map_error_remaps_the_status_only() {
        const RETRY_AFTER: Duration = Duration::from_secs(7);
        const REMAPPED: &str = "slow down";
        let provider = provider_with(ProviderHooks {
            map_error: Some(Arc::new(RemapHook(Some(ApiError {
                status: RATE_LIMITED,
                message: REMAPPED.to_string(),
            })))),
            ..ProviderHooks::default()
        });

        let error =
            smol::block_on(provider.mapped::<()>(Err(api_error(NOT_RETRYABLE, Some(RETRY_AFTER)))))
                .unwrap_err();

        assert!(
            matches!(&error, AgentError::Api { status: RATE_LIMITED, message, .. } if message == REMAPPED)
        );
        assert_eq!(error.retry_kind(), Some(RetryKind::RateLimit));
        assert_eq!(error.retry_after(), Some(RETRY_AFTER));
    }

    #[test]
    fn plugin_auth_rejects_an_undeclared_base_url() {
        const SLUG: &str = "egress-plugin";
        let hosts = vec![EXAMPLE_HOST.to_string()];
        let declared = format!("{EXAMPLE_BASE_URL}/v1");
        let auth = |base_url: &str| PluginAuth {
            base_url: Some(base_url.to_string()),
            headers: HashMap::new(),
        };

        assert!(
            auth(UNDECLARED_BASE_URL)
                .into_resolved(SLUG, &hosts)
                .is_err()
        );
        assert_eq!(
            auth(&declared)
                .into_resolved(SLUG, &hosts)
                .unwrap()
                .base_url
                .as_deref(),
            Some(declared.as_str())
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
        const SLUG: &str = "scheme-plugin";
        let hosts = vec![host.to_string()];
        assert_eq!(
            declared_base_url(SLUG, Some(url.to_string()), &hosts).is_ok(),
            accepted,
            "{url}"
        );
    }

    #[test_case("https://example.com/v1", "example.com:443", true ; "the_default_port_is_the_declared_one")]
    #[test_case("https://example.com:9443/v1", "example.com:443", false ; "another_port")]
    #[test_case("http://[::1]:8080/v1", "[::1]:8080", true ; "a_loopback_ipv6_on_its_port")]
    fn base_url_port(url: &str, host: &str, accepted: bool) {
        const SLUG: &str = "port-plugin";
        let hosts = vec![host.to_string()];
        assert_eq!(
            declared_base_url(SLUG, Some(url.to_string()), &hosts).is_ok(),
            accepted,
            "{url}"
        );
    }

    /// What the per-slug auth cell must and must not carry across a reload: a
    /// slug that has minted nothing takes the fresh declaration, an origin a
    /// hook leased survives as long as the new declaration still covers it,
    /// and it goes the moment a narrowed host list stops covering it.
    ///
    /// The origin is always the hook's. A declared `base_url` is the codec's
    /// last resort and never reaches the auth cell, which is what keeps a
    /// plugin from outranking the user's `<SLUG>_BASE_URL`.
    #[test_case(false, true, None ; "an_unminted_slug_takes_the_fresh_declaration")]
    #[test_case(true, true, Some(OTHER_BASE_URL) ; "a_leased_origin_survives_the_reload")]
    #[test_case(true, false, None ; "narrowing_the_hosts_drops_an_origin_they_no_longer_cover")]
    fn a_reload_re_reads_the_declaration(
        minted: bool,
        still_covered: bool,
        expected: Option<&str>,
    ) {
        const SLUG: &str = "redeclare-plugin";
        let declaration = |covers_the_lease: bool| {
            let mut reg = registration(SLUG);
            if covers_the_lease {
                reg.decl.net_hosts.push(OTHER_HOST.to_string());
            }
            reg
        };

        let mut first = declaration(true);
        if minted {
            first.hooks.auth = Some(Arc::new(CountingAuth::leasing(OTHER_BASE_URL)));
        }
        register_loaded(first).unwrap();
        if minted {
            smol::block_on(entry(SLUG).unwrap().ensure_auth()).unwrap();
            assert_eq!(
                base_url(SLUG).as_deref(),
                Some(OTHER_BASE_URL),
                "{LEASE_LOST}"
            );
        }

        register_loaded(declaration(still_covered)).unwrap();

        assert_eq!(base_url(SLUG).as_deref(), expected);
    }
}
