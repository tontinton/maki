use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use maki_config::providers::{OverrideFields, Protocol, ProviderOverride, ProvidersConfig};
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::model::{
    Capabilities, FactSource, Model, ModelFamily, ModelInfo, ModelPricing, ThinkingSupport,
    lookup_entry,
};
use crate::model_registry;
use crate::provider::{BoxFuture, Provider};
use crate::spec::{
    AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS, Native,
    ProviderRegistry, ProviderSpec,
};
use crate::types::{EffortDialect, ThinkingFallback, dialect};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};
use maki_storage::id::SessionRef;

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{ResolvedAuth, Timeouts, google, plugin};

const HOST_ENV: &str = "APERTURE_HOST";
const PER_MILLION: f64 = 1_000_000.0;
const ALL_MODELS: &str = "*";
pub(crate) const DEFAULT_PATH_PREFIX: &str = "/v1";
pub(crate) const GEMINI_PATH_PREFIX: &str = "/v1beta";
/// For upstreams whose base url already carries its own path.
pub(crate) const NO_PATH_PREFIX: &str = "";

pub(super) const SLUG: &str = "aperture";
const DISPLAY_NAME: &str = "Aperture";
/// Tailscale handles auth, so there is no key and no login URL.
const NO_ENV_VAR: &str = "";
const MAX_TOKENS_FIELD: &str = "max_tokens";
const DOC_API_URL: &str = "Aperture gateway (set APERTURE_HOST)";
const FEATURES: &str =
    "Tailscale Aperture LLM gateway; set APERTURE_HOST or configure in providers.toml";
const AUTH_DOC: &str = "`APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`)";
/// The generic route speaks chat completions, so a model pinned to think
/// spells it as chat's `reasoning_effort`, whatever row it borrows from.
const GENERIC_DIALECT: &EffortDialect<'static> = &dialect::STANDARD;
const DISCOVERY_NOTE: &str = "Aperture discovers models from your gateway. Set `APERTURE_HOST` to your Tailscale Aperture \
     endpoint (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth.";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: Cow::Borrowed(SLUG),
    api_key_env: Cow::Borrowed(NO_ENV_VAR),
    base_url: Cow::Borrowed(""),
    max_tokens_field: Cow::Borrowed(MAX_TOKENS_FIELD),
    include_stream_usage: true,
    usage_cost: false,
    provider_name: Cow::Borrowed(DISPLAY_NAME),
};

/// Aperture routes onto other providers; nothing routes onto Aperture.
pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: NO_ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: false,
    supports_deferred_tools: false,
    sends_thinking,
    thinking_fields_fallback,
    accepts_arbitrary_models: true,
    catalog_is_authoritative: false,
    fallback_max_output: Some(16_384),
    fallback_context_window: 128_000,
    models_toml: NO_CURATED_MODELS,
    pricing_schedule: None,
    build: Build::Native(Native {
        new: create,
        with_auth: create_with_auth,
    }),
    aperture: None,
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: "",
        default_model: "",
        plans: None,
        login_url: None,
        needs_url: true,
    }),
    docs: GeneratedDocs {
        api_urls: &[DOC_API_URL],
        features: Some(FEATURES),
        auth: AuthDoc::Custom(AUTH_DOC),
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Aperture::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Aperture::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());

type Overrides = HashMap<String, ProviderOverride>;

/// Load per-gateway-provider overrides from the `aperture` entry's `overrides`
/// table in `providers.toml`.
fn load_overrides() -> Overrides {
    let overrides = ProvidersConfig::load()
        .get("aperture")
        .map(|d| d.overrides.clone())
        .unwrap_or_default();
    validate_overrides(&overrides);
    overrides
}

/// Runs on every frame of the status bar, so a broken `providers.toml` reads
/// as no overrides instead of exiting.
fn sends_thinking(model: &Model, choice: ThinkingConfig) -> ThinkingConfig {
    let config = ProvidersConfig::load_or_default();
    let overrides = config.get(SLUG).map(|def| &def.overrides);
    routed_sends_thinking(overrides.unwrap_or(&Overrides::new()), model, choice)
}

/// The routed provider builds the request, so it answers, about the model as
/// its own: a plugin codec finds its declaration and discovery by its slug.
fn routed_sends_thinking(
    overrides: &Overrides,
    model: &Model,
    choice: ThinkingConfig,
) -> ThinkingConfig {
    match routed_model(overrides, model) {
        Some((spec, routed)) => (spec.sends_thinking)(&routed, choice),
        None if model.supports_thinking() => {
            ThinkingFallback::Dialect(GENERIC_DIALECT).sends(model, choice)
        }
        None => choice,
    }
}

/// The generic route takes no `thinking_fields`, only its own spelling.
fn thinking_fields_fallback(model: &Model) -> Option<ThinkingFallback> {
    let config = ProvidersConfig::load_or_default();
    let overrides = config.get(SLUG).map(|def| &def.overrides);
    let (spec, routed) = routed_model(overrides.unwrap_or(&Overrides::new()), model)?;
    (spec.thinking_fields_fallback)(&routed)
}

/// The fields the generic route adds for `choice`, kept apart from the body so
/// a refusal strips exactly these.
fn generic_thinking(model: &Model, choice: ThinkingConfig) -> Map<String, Value> {
    let mut fields = Value::Object(Map::new());
    choice.apply_dialect(&mut fields, model, GENERIC_DIALECT);
    match fields {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

fn routed_model(overrides: &Overrides, model: &Model) -> Option<(&'static ProviderSpec, Model)> {
    let (provider_id, model_id) = model.id.split_once('/')?;
    let spec = routed_spec(
        provider_id,
        &merged_override(overrides, provider_id, model_id),
    )?;
    if spec.native().is_none() && plugin::base_spec(spec.slug).is_some() {
        return None;
    }
    let routed = Model {
        provider: Arc::from(spec.slug),
        ..native_route_model(model, spec, model_id)
    };
    Some((spec, routed))
}

/// Warn about `base` values that don't name an OpenAI-compatible provider, so a
/// typo doesn't silently fall through to the generic path.
fn validate_overrides(overrides: &Overrides) {
    for (provider, po) in overrides {
        let fields = std::iter::once((ALL_MODELS, &po.default))
            .chain(po.models.iter().map(|(id, f)| (id.as_str(), f)));
        for (model, f) in fields {
            if let Some(base) = &f.base
                && routed_spec(base, &OverrideFields::default()).is_none()
            {
                warn!(
                    provider,
                    model, base, "base is not an OpenAI-compatible provider, ignoring"
                );
            }
            if let Some(like) = &f.like
                && Capabilities::like(like).is_none()
            {
                warn!(
                    provider,
                    model, like, "like names no model Maki knows, ignoring"
                );
            }
        }
    }
}

/// Model-level fields win over provider-level defaults, field by field.
fn merged_override(overrides: &Overrides, provider_id: &str, model_id: &str) -> OverrideFields {
    let Some(po) = overrides.get(provider_id) else {
        return OverrideFields::default();
    };
    let Some(m) = po.models.get(model_id) else {
        return po.default.clone();
    };
    OverrideFields {
        context_window: m.context_window.or(po.default.context_window),
        max_output_tokens: m.max_output_tokens.or(po.default.max_output_tokens),
        supports_thinking: m.supports_thinking.or(po.default.supports_thinking),
        supports_vision: m.supports_vision.or(po.default.supports_vision),
        base: m.base.clone().or_else(|| po.default.base.clone()),
        path_prefix: m
            .path_prefix
            .clone()
            .or_else(|| po.default.path_prefix.clone()),
        like: m.like.clone().or_else(|| po.default.like.clone()),
    }
}

/// Resolve the native provider an Aperture model should stream through. The
/// override `base` wins, then the provider segment of the id if it itself names
/// a routable provider. Lets a user remap an opaque gateway vendor (e.g.
/// `ikora-openai`) to a real native provider (e.g. `llama-cpp`). `None` falls
/// through to the generic OpenAI-compat path.
fn routed_spec(provider_id: &str, merged: &OverrideFields) -> Option<&'static ProviderSpec> {
    [merged.base.as_deref(), Some(provider_id)]
        .into_iter()
        .flatten()
        .find_map(|s| ProviderRegistry::get(s).filter(|spec| spec.aperture.is_some()))
}

/// A model that routes nowhere still has to reach the gateway, so it falls back
/// to the generic path. A configured prefix replaces whatever the route asks
/// for, empty string included.
fn path_prefix(spec: Option<&'static ProviderSpec>, merged: &OverrideFields) -> String {
    let Some(configured) = merged.path_prefix.as_deref() else {
        return spec
            .and_then(|s| s.aperture)
            .map_or(DEFAULT_PATH_PREFIX, |r| r.path_prefix)
            .to_string();
    };
    let trimmed = configured.trim().trim_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    format!("/{trimmed}")
}

/// Clone the shared auth, appending the path prefix to the host. The gateway
/// forwards the resulting path to the upstream.
fn routed_auth(auth: &Mutex<ResolvedAuth>, prefix: &str) -> ResolvedAuth {
    let mut cloned = auth.lock().unwrap().clone();
    if !prefix.is_empty()
        && let Some(base) = cloned.base_url.as_deref()
    {
        cloned.base_url = Some(format!("{}{prefix}", base.trim_end_matches('/')));
    }
    cloned
}

/// The model the routed provider should stream against. The gateway strips the
/// `<vendor>/` prefix itself and uses it to pick the upstream, so every route
/// keeps the full id. Gemini is the exception: its model id goes into the url
/// path, where an encoded slash would not survive, so it gets the bare id.
fn native_route_model(model: &Model, spec: &'static ProviderSpec, bare_model_id: &str) -> Model {
    let mut m = model.clone();
    if spec.slug == google::SLUG {
        m.id = bare_model_id.to_string();
    }
    m
}

/// A routed provider and the auth cell it reads, which every call rewrites
/// from the gateway auth so a rotated token still reaches the route.
struct Route {
    auth: Arc<Mutex<ResolvedAuth>>,
    provider: Arc<dyn Provider>,
}

impl Route {
    fn serve(&self, auth: ResolvedAuth) -> Arc<dyn Provider> {
        *self.auth.lock().unwrap() = auth;
        Arc::clone(&self.provider)
    }
}

pub struct Aperture {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
    overrides: Overrides,
    /// Keyed by the routed slug and the path prefix its auth carries.
    routes: Mutex<HashMap<(&'static str, String), Route>>,
}

impl Aperture {
    pub fn new(timeouts: Timeouts) -> Result<Self, AgentError> {
        let base_url = resolve_base_url()?;
        let auth = Arc::new(Mutex::new(
            ResolvedAuth::new(&CONFIG.slug, Vec::new())?.with_base_url(Some(base_url)),
        ));
        Ok(Self::with_auth_and_overrides(
            auth,
            timeouts,
            load_overrides(),
        ))
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self::with_auth_and_overrides(auth, timeouts, load_overrides())
    }

    /// Single constructor: the overrides are injected so tests can pass an
    /// explicit map instead of reading the real `providers.toml`.
    fn with_auth_and_overrides(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: Timeouts,
        overrides: Overrides,
    ) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            timeouts,
            system_prefix: None,
            overrides,
            routes: Mutex::default(),
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix.filter(|s| !s.is_empty());
        self
    }

    /// Builds outside the map lock, so a slow build holds up no other route.
    /// Two first calls racing each build one, and the first insert wins.
    fn routed_provider(
        &self,
        spec: &'static ProviderSpec,
        prefix: &str,
    ) -> Option<Arc<dyn Provider>> {
        let auth = routed_auth(&self.auth, prefix);
        let key = (spec.slug, prefix.to_owned());
        if let Some(route) = self.routes.lock().unwrap().get(&key) {
            return Some(route.serve(auth));
        }
        let cell = Arc::new(Mutex::new(auth.clone()));
        let provider = self.build_route(spec, Arc::clone(&cell))?;
        let mut routes = self.routes.lock().unwrap();
        let route = routes.entry(key).or_insert(Route {
            auth: cell,
            provider: Arc::from(provider),
        });
        Some(route.serve(auth))
    }

    fn build_route(
        &self,
        spec: &'static ProviderSpec,
        auth: Arc<Mutex<ResolvedAuth>>,
    ) -> Option<Box<dyn Provider>> {
        plugin::build_with_auth(
            spec.slug,
            Arc::clone(&auth),
            self.timeouts,
            self.system_prefix.clone(),
        )
        .or_else(|| {
            spec.native()
                .map(|n| (n.with_auth)(auth, self.timeouts, self.system_prefix.clone()))
        })
    }
}

fn resolve_base_url() -> Result<String, AgentError> {
    if let Ok(url) = std::env::var(HOST_ENV)
        && !url.trim().is_empty()
    {
        return Ok(trim_slash(url));
    }
    if let Some(url) = ProvidersConfig::load()
        .get("aperture")
        .and_then(|d| d.base_url.clone())
        .filter(|u| !u.trim().is_empty())
    {
        return Ok(trim_slash(url));
    }
    Err(AgentError::Config {
        message: format!("{HOST_ENV} not set"),
    })
}

fn trim_slash(url: String) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn price_per_m(field: Option<&Value>) -> f64 {
    field
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .map(|n| n * PER_MILLION)
        .unwrap_or(0.0)
}

fn parse_models(body: &Value, overrides: &Overrides) -> Vec<ModelInfo> {
    let Some(models) = body["data"].as_array() else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let provider_id = m["metadata"]["provider"]["id"].as_str().unwrap_or("");
            let ov = merged_override(overrides, provider_id, id);
            Some(ModelInfo {
                id: if provider_id.is_empty() {
                    id.to_string()
                } else {
                    format!("{provider_id}/{id}")
                },
                context_window: ov
                    .context_window
                    .or_else(|| payload_u32(m, "context_window_tokens")),
                max_output_tokens: ov
                    .max_output_tokens
                    .or_else(|| payload_u32(m, "max_output_tokens")),
                pricing: m["pricing"].as_object().map(|p| {
                    ModelPricing::per_million(
                        price_per_m(p.get("input")),
                        price_per_m(p.get("output")),
                        0.0,
                        price_per_m(p.get("input_cache_read")),
                    )
                }),
                supports_thinking: ov
                    .supports_thinking
                    .or_else(|| routed_spec(provider_id, &ov).map(|spec| spec.supports_thinking)),
                supports_vision: ov.supports_vision,
                tier: None,
                provider_info: None,
                extra: None,
                effort: None,
            })
        })
        .collect()
}

fn payload_u32(model: &Value, field: &str) -> Option<u32> {
    u32::try_from(model[field].as_u64()?).ok()
}

/// Reconcile a model's metadata against its routed native provider's static
/// table, then layer user overrides on top. The static table is the baseline
/// (so `aperture/zai/glm-5.2` picks up Zai's real `context_window` without
/// being hand-duplicated in the overrides config); explicit overrides still win.
fn apply_adjustments(model: &mut Model, overrides: &Overrides) {
    let Some((provider_id, model_id)) = model.id.split_once('/') else {
        return;
    };
    let ov = merged_override(overrides, provider_id, model_id);
    let payload = model_registry::discovered(SLUG, &model.id);
    let payload_context = payload.as_ref().and_then(|info| info.context_window);
    let payload_output = payload.as_ref().and_then(|info| info.max_output_tokens);
    let routed = routed_spec(provider_id, &ov);
    if let Some(spec) = routed {
        model.family = spec.family;
        if model.thinking_override.is_none() {
            model.thinking_override =
                ThinkingSupport::from_flags(Some(spec.supports_thinking), false);
            model.sources.thinking = FactSource::Default;
        }
        if let Some(entry) = lookup_entry(spec.models(), model_id) {
            if let Some((tokens, source)) = sourced(payload_context, entry.context_window) {
                model.context_window = tokens;
                model.sources.context_window = source;
            }
            if let Some((tokens, source)) = sourced(payload_output, entry.max_output_tokens) {
                model.max_output_tokens = Some(tokens);
                model.sources.max_output_tokens = source;
            }
        }
    }
    if let Some((borrowed, source)) = borrowed_capabilities(&ov, routed, provider_id, model_id) {
        model.family = borrowed.family;
        model.supports_tool_examples_override = model
            .supports_tool_examples_override
            .or(borrowed.supports_tool_examples);
        if model.supports_vision_override.is_none() && borrowed.supports_vision.is_some() {
            model.supports_vision_override = borrowed.supports_vision;
            model.sources.vision = source;
        }
    }
    if let Some(cw) = ov.context_window {
        model.context_window = cw;
        model.sources.context_window = FactSource::Config;
    }
    if let Some(tokens) = ov.max_output_tokens {
        model.max_output_tokens = Some(tokens);
        model.sources.max_output_tokens = FactSource::Config;
    }
    if let Some(thinking) = ov.supports_thinking {
        model.thinking_override = ThinkingSupport::from_flags(Some(thinking), false);
        model.sources.thinking = FactSource::Config;
    }
    if let Some(vision) = ov.supports_vision {
        model.supports_vision_override = Some(vision);
        model.sources.vision = FactSource::Config;
    }
}

/// The gateway's own limit describes this deployment, so it outranks the
/// routed table.
fn sourced(payload: Option<u32>, table: Option<u32>) -> Option<(u32, FactSource)> {
    payload
        .map(|tokens| (tokens, FactSource::Discovery))
        .or_else(|| Some((table?, FactSource::Table)))
}

/// An explicit `like` wins. Without one the gateway already names the
/// upstream, so `aperture/openai/gpt-5.4` reads like `openai/gpt-5.4`.
fn borrowed_capabilities(
    ov: &OverrideFields,
    routed: Option<&'static ProviderSpec>,
    provider_id: &str,
    model_id: &str,
) -> Option<(Capabilities, FactSource)> {
    if let Some(borrowed) = ov.like.as_deref().and_then(Capabilities::like) {
        return Some((borrowed, FactSource::Like));
    }
    let upstream = routed.or_else(|| ProviderRegistry::get(provider_id))?;
    Some((Capabilities::of(upstream, model_id)?, FactSource::Table))
}

impl Provider for Aperture {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a UnboundedSender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let (provider_id, model_id) = model.id.split_once('/').unwrap_or(("", &model.id));
            let ov = merged_override(&self.overrides, provider_id, model_id);
            let spec = routed_spec(provider_id, &ov);
            let prefix = path_prefix(spec, &ov);
            if let Some(spec) = spec
                && let Some(provider) = self.routed_provider(spec, &prefix)
            {
                let request_model = native_route_model(model, spec, model_id);
                return provider
                    .stream_message(
                        &request_model,
                        messages,
                        system,
                        tools,
                        event_tx,
                        opts,
                        session_id,
                    )
                    .await;
            }
            let auth = routed_auth(&self.auth, &prefix);
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body =
                self.compat
                    .build_body(model, messages, system, tools, opts.thinking, auth.top_p);
            let thinking = generic_thinking(model, opts.thinking);
            let thinking_keys: Vec<String> = thinking.keys().cloned().collect();
            if let Some(object) = body.as_object_mut() {
                object.extend(thinking);
            }
            self.compat
                .stream_with_thinking_retry(
                    model,
                    &HeaderMap::new(),
                    body,
                    &thinking_keys,
                    event_tx,
                    &auth,
                )
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let base = auth.base_url.as_deref().unwrap_or("");
            let text = self
                .compat
                .get_text(&auth, &format!("{base}{DEFAULT_PATH_PREFIX}/models"))
                .await?;
            let body: Value = serde_json::from_str(&text)?;
            Ok(parse_models(&body, &self.overrides))
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        if let Some((provider_id, model_id)) = model.id.split_once('/') {
            let model_id = model_id.to_string();
            let ov = merged_override(&self.overrides, provider_id, &model_id);
            if let Some(spec) = routed_spec(provider_id, &ov)
                && let Some(routed) = self.routed_provider(spec, &path_prefix(Some(spec), &ov))
            {
                let full_id = std::mem::replace(&mut model.id, model_id);
                routed.adjust_model(model);
                model.id = full_id;
            }
        }
        apply_adjustments(model, &self.overrides);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Effort;
    use crate::model::ModelFamily;
    use crate::test_support::{Canned, isolated_home, register_bundled, serve};
    use reqwest::header::AUTHORIZATION;
    use serde_json::json;
    use test_case::test_case;
    use tokio::sync::mpsc::unbounded_channel;

    const ROUTED_SLUG: &str = "routed-plugin";
    const ROUTED_HOST: &str = "routed.example";
    const ROUTED_VISION_MODEL: &str = "routed-vision";
    const ROUTED_LIMITS_MODEL: &str = "routed-limits";
    /// No row describes it, so it takes the openai codec's thinking support.
    const ROUTED_CHAT_SPEC: &str = "aperture/routed-plugin/routed-chat";
    const NATIVE_CHAT_SPEC: &str = "aperture/ollama/qwen3";
    const PROMPT: &str = "hi";
    const FIRST_TOKEN: &str = "Bearer first";
    const ROTATED_TOKEN: &str = "Bearer rotated";
    const ANSWER: Canned = Canned::sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n",
    );
    const TWO_TURNS: &[Canned] = &[ANSWER, ANSWER];
    const REBUILT: &str = "the route was built again instead of reused";
    const STALE_TOKEN: &str = "a routed request missed the gateway's current token";

    fn register_routed() {
        register_bundled(
            json!({
                "slug": ROUTED_SLUG,
                "display_name": "Routed",
                "codec": "openai",
                "openai": { "thinking": { "dialect": "high-only" } },
                "aperture": { "path_prefix": DEFAULT_PATH_PREFIX },
                "models": [
                    { "prefixes": [ROUTED_VISION_MODEL], "supports_vision": true },
                    { "prefixes": [ROUTED_LIMITS_MODEL], "context_window": 64000 }
                ],
            }),
            ROUTED_HOST,
        );
    }

    fn spec_slug(provider_id: &str, merged: &OverrideFields) -> Option<&'static str> {
        routed_spec(provider_id, merged).map(|spec| spec.slug)
    }

    #[test_case("zai", Some("zai") ; "known_zai")]
    #[test_case(ROUTED_SLUG, Some(ROUTED_SLUG) ; "declared_route")]
    #[test_case("openai", None ; "openai_excluded")]
    #[test_case("llama-cpp", Some("llama-cpp") ; "known_llama_cpp")]
    #[test_case("ikora-openai", None ; "unknown_vendor_no_override")]
    #[test_case("anthropic", Some("anthropic") ; "known_anthropic")]
    #[test_case("google", Some("google") ; "known_google")]
    #[test_case("copilot", None ; "copilot_excluded")]
    #[test_case("aperture", None ; "aperture_no_recurse")]
    #[test_case("gemini", None ; "gemini_vendor_unparsable_without_override")]
    fn routed_spec_without_overrides(provider_id: &str, expected: Option<&str>) {
        register_routed();
        assert_eq!(spec_slug(provider_id, &OverrideFields::default()), expected);
    }

    fn base_override(base: &str) -> OverrideFields {
        OverrideFields {
            base: Some(base.into()),
            ..Default::default()
        }
    }

    #[test_case("gemini", "google", Some("google") ; "remaps_vendor_the_gateway_renamed")]
    #[test_case("ikora-openai", "llama-cpp", Some("llama-cpp") ; "remaps_unknown_vendor")]
    #[test_case("zai", "not-a-real-provider", Some("zai") ; "invalid_base_falls_back_to_provider_id")]
    fn routed_spec_with_base_override(provider_id: &str, base: &str, expected: Option<&str>) {
        assert_eq!(spec_slug(provider_id, &base_override(base)), expected);
    }

    /// Mirrors `stream_message`'s routing decision: `Model::from_spec` strips the
    /// builtin slug, so the id `stream_message` splits is `<vendor>/<model>`,
    /// not `aperture/<vendor>/<model>`.
    #[test]
    fn stream_message_routes_aperture_gemini_spec_via_override() {
        let model = Model::from_spec("aperture/gemini/gemini-pro-latest").unwrap();
        let (provider_id, model_id) = model.id.split_once('/').unwrap();
        assert_eq!(provider_id, "gemini");
        assert_eq!(model_id, "gemini-pro-latest");
        assert_eq!(
            spec_slug(provider_id, &base_override("google")),
            Some("google")
        );
    }

    fn route(slug: &str) -> &'static ProviderSpec {
        ProviderRegistry::get(slug).expect("routable provider")
    }

    #[test_case("google", "aperture/gemini/gemini-pro-latest", "gemini-pro-latest" ; "native_google_strips_vendor_prefix")]
    #[test_case("anthropic", "aperture/anthropic/claude-test", "anthropic/claude-test" ; "anthropic_keeps_vendor_prefix")]
    #[test_case("zai", "aperture/zai/glm-5.2", "zai/glm-5.2" ; "compat_zai_keeps_vendor_prefix")]
    #[test_case("ollama", "aperture/ollama/glm-5.2", "ollama/glm-5.2" ; "compat_ollama_keeps_vendor_prefix")]
    fn native_route_model_vendor_prefix_policy(slug: &str, spec: &str, expected_id: &str) {
        let model = Model::from_spec(spec).unwrap();
        let bare = model.id.split_once('/').unwrap().1;
        assert_eq!(
            native_route_model(&model, route(slug), bare).id,
            expected_id
        );
    }

    /// The route answers as it sends: a plugin codec by its own declaration,
    /// which it only finds under its own slug, not under `aperture`.
    #[test_case("aperture/zai/glm-5.3", None, Some(Effort::High) ; "zai_route_sends_high")]
    #[test_case("aperture/ollama/qwen3", None, Some(Effort::Medium) ; "ollama_route_sends_medium")]
    #[test_case("aperture/anthropic/claude-sonnet-4-5", None, None ; "anthropic_route_keeps_the_mode")]
    #[test_case(ROUTED_CHAT_SPEC, None, Some(Effort::High) ; "codec_route_sends_its_dialect_level")]
    #[test_case("aperture/ikora-openai/gemma4", None, None ; "unrouted_sends_no_effort")]
    #[test_case("aperture/ikora-openai/gemma4", Some("zai"), Some(Effort::High) ; "base_override_answers_for_its_route")]
    #[test_case("aperture/zai/glm-5.3", Some("anthropic"), None ; "base_override_wins_over_the_vendor")]
    fn adaptive_effort_is_the_routed_providers(
        spec: &str,
        base: Option<&str>,
        expected: Option<Effort>,
    ) {
        register_routed();
        let model = Model::from_spec(spec).unwrap();
        let vendor = model.id.split_once('/').unwrap().0;
        let overrides = Overrides::from_iter(base.map(|base| {
            (
                vendor.to_string(),
                ProviderOverride {
                    default: base_override(base),
                    models: HashMap::new(),
                },
            )
        }));
        assert_eq!(
            routed_sends_thinking(&overrides, &model, ThinkingConfig::Adaptive).level(),
            expected
        );
    }

    #[test]
    fn routed_spec_model_base_wins_over_provider_base() {
        let vendor = "ikora-openai";
        let overrides = Overrides::from([(
            vendor.into(),
            ProviderOverride {
                default: base_override("zai"),
                models: HashMap::from([("special".into(), base_override("llama-cpp"))]),
            },
        )]);
        let slug = |model| spec_slug(vendor, &merged_override(&overrides, vendor, model));
        assert_eq!(slug("special"), Some("llama-cpp"));
        assert_eq!(slug("other"), Some("zai"));
    }

    fn test_auth() -> Arc<Mutex<ResolvedAuth>> {
        Arc::new(Mutex::new(ResolvedAuth::for_test(
            Some("https://aperture.example.com".into()),
            Vec::new(),
        )))
    }

    #[test_case(Some("ollama"), Some("https://aperture.example.com/v1") ; "ollama_appends_v1")]
    #[test_case(Some("zai"), Some("https://aperture.example.com") ; "zai_keeps_bare_host")]
    #[test_case(Some(ROUTED_SLUG), Some("https://aperture.example.com/v1") ; "declared_route_appends_v1")]
    #[test_case(None, Some("https://aperture.example.com/v1") ; "unrouted_appends_v1")]
    #[test_case(Some("google"), Some("https://aperture.example.com/v1beta") ; "google_appends_v1beta")]
    #[test_case(Some("anthropic"), Some("https://aperture.example.com") ; "anthropic_keeps_bare_host")]
    fn routed_auth_prefix_per_route(slug: Option<&str>, expected: Option<&str>) {
        register_routed();
        let prefix = path_prefix(slug.map(route), &OverrideFields::default());
        let auth = routed_auth(&test_auth(), &prefix);
        assert_eq!(auth.base_url.as_deref(), expected);
    }

    #[test_case(Some("") , Some("https://aperture.example.com") ; "empty_prefix_keeps_bare_host")]
    #[test_case(Some("v1"), Some("https://aperture.example.com/v1") ; "configured_prefix_beats_zai_default")]
    #[test_case(Some("api/paas/v4"), Some("https://aperture.example.com/api/paas/v4") ; "prefix_gets_leading_slash")]
    #[test_case(Some("/v1/"), Some("https://aperture.example.com/v1") ; "prefix_trailing_slash_trimmed")]
    fn routed_auth_path_prefix_override_wins(configured: Option<&str>, expected: Option<&str>) {
        let merged = OverrideFields {
            path_prefix: configured.map(String::from),
            ..Default::default()
        };
        let auth = routed_auth(&test_auth(), &path_prefix(Some(route("zai")), &merged));
        assert_eq!(auth.base_url.as_deref(), expected);
    }

    #[test]
    fn routed_auth_handles_host_with_trailing_slash() {
        let auth = Arc::new(Mutex::new(ResolvedAuth::for_test(
            Some("https://aperture.example.com/".into()),
            Vec::new(),
        )));
        let routed = routed_auth(&auth, DEFAULT_PATH_PREFIX);
        assert_eq!(
            routed.base_url.as_deref(),
            Some("https://aperture.example.com/v1")
        );
    }

    #[test]
    fn merged_override_model_wins_over_provider_default() {
        let mut overrides = Overrides::new();
        overrides.insert(
            "zai".into(),
            ProviderOverride {
                default: OverrideFields {
                    context_window: Some(128_000),
                    max_output_tokens: Some(8_192),
                    ..Default::default()
                },
                models: HashMap::from([(
                    "glm-5.2".into(),
                    OverrideFields {
                        context_window: Some(200_000),
                        ..Default::default()
                    },
                )]),
            },
        );
        let ov = merged_override(&overrides, "zai", "glm-5.2");
        assert_eq!(ov.context_window, Some(200_000));
        assert_eq!(ov.max_output_tokens, Some(8_192));
    }

    #[test]
    fn merged_override_unknown_provider_is_empty() {
        let overrides = Overrides::new();
        let ov = merged_override(&overrides, "nobody", "x");
        assert!(ov.context_window.is_none());
        assert!(ov.max_output_tokens.is_none());
        assert!(ov.base.is_none());
        assert!(ov.supports_thinking.is_none());
        assert!(ov.supports_vision.is_none());
    }

    #[test]
    fn parse_models_prefixes_provider_pricing_and_overrides() {
        let mut overrides = Overrides::new();
        overrides.insert(
            "ollama".into(),
            ProviderOverride {
                default: OverrideFields::default(),
                models: HashMap::from([(
                    "qwen3.6".into(),
                    OverrideFields {
                        context_window: Some(65_536),
                        supports_vision: Some(true),
                        ..Default::default()
                    },
                )]),
            },
        );
        let body = json!({
            "object": "list",
            "data": [
                {"id": "qwen3.6", "metadata": {"provider": {"id": "ollama"}}, "context_window_tokens": 1048576, "max_output_tokens": 262144, "pricing": {"input": "0.000001", "output": 0.000002, "input_cache_read": "0.0000001"}},
                {"id": "gemma4", "metadata": {"provider": {"id": "ikora"}}, "context_window_tokens": 128000, "max_output_tokens": 8192},
                {"id": "raw-model"}
            ]
        });
        let models = parse_models(&body, &overrides);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "ollama/qwen3.6");
        assert_eq!(
            models[0].context_window,
            Some(65_536),
            "override beats the payload"
        );
        assert_eq!(models[0].supports_vision, Some(true));
        assert_eq!(models[0].max_output_tokens, Some(262_144));
        let p = models[0].pricing.as_ref().unwrap();
        assert!((p.input - 1.0).abs() < 1e-9);
        assert!((p.output - 2.0).abs() < 1e-9);
        assert!((p.cache_read - 0.1).abs() < 1e-9);
        assert_eq!(models[1].id, "ikora/gemma4");
        assert_eq!(models[1].context_window, Some(128_000));
        assert_eq!(models[1].max_output_tokens, Some(8_192));
        assert!(models[1].pricing.is_none());
        assert!(models[1].supports_vision.is_none());
        assert_eq!(models[2].id, "raw-model");
        assert!(models[2].context_window.is_none());
    }

    #[test]
    fn parse_models_handles_missing_data() {
        assert!(parse_models(&json!({}), &Overrides::new()).is_empty());
        assert!(parse_models(&json!({"data": []}), &Overrides::new()).is_empty());
    }

    #[test]
    fn apply_adjustments_uses_routed_provider_static_table() {
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        assert_eq!(model.context_window, SPEC.fallback_context_window);
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_output_tokens, Some(131_072));
        assert_eq!(model.family, ModelFamily::Glm);
    }

    #[test]
    fn gateway_limits_beat_a_routed_static_table() {
        crate::model_registry::set_known_models(
            "aperture",
            vec![ModelInfo {
                id: "zai/glm-5.2".into(),
                context_window: Some(123_456),
                max_output_tokens: Some(7_890),
                ..ModelInfo::default()
            }],
        );
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.context_window, 123_456);
        assert_eq!(model.max_output_tokens, Some(7_890));
        assert_eq!(model.sources.context_window, FactSource::Discovery);
        assert_eq!(model.sources.max_output_tokens, FactSource::Discovery);
        crate::model_registry::set_known_models("aperture", Vec::new());
    }

    #[test]
    fn a_table_row_without_max_output_does_not_wipe_the_payload() {
        register_routed();
        let id = format!("{ROUTED_SLUG}/{ROUTED_LIMITS_MODEL}");
        crate::model_registry::set_known_models(
            "aperture",
            vec![ModelInfo {
                id: id.clone(),
                context_window: Some(123_456),
                max_output_tokens: Some(7_890),
                ..ModelInfo::default()
            }],
        );
        let mut model = Model::from_spec(&format!("aperture/{id}")).unwrap();
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.context_window, 123_456);
        assert_eq!(model.max_output_tokens, Some(7_890));
        crate::model_registry::set_known_models("aperture", Vec::new());
    }

    #[test]
    fn an_override_beats_both_the_payload_and_the_table() {
        crate::model_registry::set_known_models(
            "aperture",
            vec![ModelInfo {
                id: "zai/glm-5.2".into(),
                context_window: Some(123_456),
                max_output_tokens: Some(7_890),
                ..ModelInfo::default()
            }],
        );
        let overrides = Overrides::from([(
            "zai".to_string(),
            ProviderOverride {
                default: OverrideFields {
                    context_window: Some(1_000),
                    max_output_tokens: Some(500),
                    ..Default::default()
                },
                ..Default::default()
            },
        )]);
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(model.context_window, 1_000);
        assert_eq!(model.max_output_tokens, Some(500));
        crate::model_registry::set_known_models("aperture", Vec::new());
    }

    #[test]
    fn apply_adjustments_override_wins_over_static_table() {
        let mut overrides = Overrides::new();
        overrides.insert(
            "zai".into(),
            ProviderOverride {
                default: OverrideFields {
                    context_window: Some(200_000),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(model.context_window, 200_000);
        assert_eq!(model.max_output_tokens, Some(131_072));
    }

    #[test]
    fn apply_adjustments_vision_uses_routed_static_table() {
        register_routed();
        let spec = format!("aperture/{ROUTED_SLUG}/{ROUTED_VISION_MODEL}");
        let mut model = Model::from_spec(&spec).unwrap();
        assert!(model.supports_vision_override.is_none());
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.supports_vision_override, Some(true));
        assert!(model.supports_vision());
    }

    #[test]
    fn apply_adjustments_vision_override_enables_gateway_model() {
        let mut overrides = Overrides::new();
        overrides.insert(
            "synthetic".into(),
            ProviderOverride {
                default: OverrideFields::default(),
                models: HashMap::from([(
                    "hf:moonshotai/Kimi-K3".into(),
                    OverrideFields {
                        supports_vision: Some(true),
                        ..Default::default()
                    },
                )]),
            },
        );
        let mut model = Model::from_spec("aperture/synthetic/hf:moonshotai/Kimi-K3").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(model.supports_vision_override, Some(true));
        assert!(model.supports_vision());
    }

    #[test]
    fn apply_adjustments_vision_override_disables_static_vision_model() {
        register_routed();
        let mut overrides = Overrides::new();
        overrides.insert(
            ROUTED_SLUG.into(),
            ProviderOverride {
                default: OverrideFields {
                    supports_vision: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let spec = format!("aperture/{ROUTED_SLUG}/{ROUTED_VISION_MODEL}");
        let mut model = Model::from_spec(&spec).unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(model.supports_vision_override, Some(false));
        assert!(!model.supports_vision());
    }

    #[test]
    fn apply_adjustments_no_route_leaves_model_untouched() {
        let mut model = Model::from_spec("aperture/ikora-openai/gemma4").unwrap();
        let before = model.clone();
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.context_window, before.context_window);
        assert_eq!(model.max_output_tokens, before.max_output_tokens);
        assert!(model.thinking_override.is_none());
        assert!(model.supports_vision_override.is_none());
        assert!(!model.supports_thinking());
    }

    const OPAQUE_UPSTREAM: &str = "azureai";
    const OPAQUE_MODEL: &str = "opaque-model";
    const CLAUDE_TARGET: &str = "anthropic/claude-opus-5";

    fn opaque_override(fields: OverrideFields) -> Overrides {
        Overrides::from([(
            OPAQUE_UPSTREAM.to_string(),
            ProviderOverride {
                default: OverrideFields::default(),
                models: HashMap::from([(OPAQUE_MODEL.to_string(), fields)]),
            },
        )])
    }

    fn opaque_model(overrides: &Overrides) -> Model {
        let mut model =
            Model::from_spec(&format!("aperture/{OPAQUE_UPSTREAM}/{OPAQUE_MODEL}")).unwrap();
        apply_adjustments(&mut model, overrides);
        model
    }

    #[test]
    fn like_lends_capabilities_but_not_limits_or_price() {
        let model = opaque_model(&opaque_override(OverrideFields {
            like: Some(CLAUDE_TARGET.into()),
            ..Default::default()
        }));
        assert_eq!(model.family, ModelFamily::Claude);
        assert_eq!(model.vision_with_source(), (true, FactSource::Like));
        assert!(model.supports_tool_examples());
        assert_eq!(model.context_window, SPEC.fallback_context_window);
        assert_eq!(model.sources.context_window, FactSource::Default);
        assert!(model.pricing.is_zero());
        assert_eq!(model.thinking_with_source(), (false, FactSource::Default));
    }

    #[test]
    fn a_stated_field_wins_over_like() {
        let model = opaque_model(&opaque_override(OverrideFields {
            like: Some(CLAUDE_TARGET.into()),
            supports_vision: Some(false),
            context_window: Some(1_000),
            ..Default::default()
        }));
        assert_eq!(model.family, ModelFamily::Claude);
        assert_eq!(model.vision_with_source(), (false, FactSource::Config));
        assert_eq!(model.context_window, 1_000);
        assert_eq!(model.sources.context_window, FactSource::Config);
    }

    #[test]
    fn like_does_not_change_the_route() {
        let overrides = opaque_override(OverrideFields {
            like: Some("zai/glm-5.3".into()),
            ..Default::default()
        });
        let model = opaque_model(&overrides);
        assert_eq!(model.family, ModelFamily::Glm);
        assert!(routed_model(&overrides, &model).is_none());
    }

    #[test]
    fn the_gateway_provider_id_names_the_row() {
        let mut model = Model::from_spec("aperture/openai/gpt-6-sol").unwrap();
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.family, ModelFamily::Gpt);
        assert_eq!(model.vision_with_source(), (true, FactSource::Table));
    }

    #[test]
    fn an_unknown_like_falls_back_to_the_gateway_provider_id() {
        let overrides = Overrides::from([(
            "zai".to_string(),
            ProviderOverride {
                default: OverrideFields {
                    like: Some("nobody/nothing".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )]);
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(model.family, ModelFamily::Glm);
    }

    /// The routed provider answers, not Aperture, which declares no thinking of
    /// its own. Expectations are spelled out rather than read back out of the
    /// spec table, or the test would agree with whatever the code found.
    #[test_case(ROUTED_CHAT_SPEC, true ; "routed_thinking_capable")]
    #[test_case("aperture/ollama/qwen3", false ; "routed_non_thinking")]
    fn apply_adjustments_thinking_follows_routed_spec(spec: &str, expected: bool) {
        register_routed();
        let mut model = Model::from_spec(spec).unwrap();
        assert!(model.thinking_override.is_none());
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(
            model.thinking_override,
            ThinkingSupport::from_flags(Some(expected), false)
        );
        assert_eq!(model.supports_thinking(), expected);
    }

    #[test]
    fn adjust_model_inherits_routed_provider_thinking_support() {
        let auth = Arc::new(Mutex::new(ResolvedAuth::for_test(
            Some("https://example.com".into()),
            Vec::new(),
        )));
        let aperture =
            Aperture::with_auth_and_overrides(auth, Timeouts::default(), Overrides::new());
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        assert!(!model.supports_thinking());
        aperture.adjust_model(&mut model);
        assert!(model.supports_thinking());
    }

    fn gateway_auth(host: &str, token: &str) -> ResolvedAuth {
        ResolvedAuth::for_test(
            Some(host.to_owned()),
            vec![(AUTHORIZATION.as_str().to_owned(), token.to_owned())],
        )
    }

    async fn ask(aperture: &Aperture, model: &Model) {
        ask_with(aperture, model, RequestOptions::default()).await;
    }

    async fn ask_with(aperture: &Aperture, model: &Model, opts: RequestOptions) {
        let (tx, _rx) = unbounded_channel();
        aperture
            .stream_message(
                model,
                &[Message::user(PROMPT.to_owned())],
                "",
                &json!([]),
                &tx,
                opts,
                None,
            )
            .await
            .unwrap();
    }

    fn only_route(aperture: &Aperture) -> Arc<dyn Provider> {
        let routes = aperture.routes.lock().unwrap();
        assert_eq!(routes.len(), 1, "{REBUILT}");
        Arc::clone(&routes.values().next().unwrap().provider)
    }

    #[test_case(NATIVE_CHAT_SPEC ; "native_route")]
    #[test_case(ROUTED_CHAT_SPEC ; "codec_plugin_route")]
    #[tokio::test]
    async fn route_is_built_once_and_follows_the_gateway_auth(spec: &str) {
        let _home = isolated_home();
        register_routed();
        let (base_url, requests) = serve(TWO_TURNS);
        let host = base_url.trim_end_matches(DEFAULT_PATH_PREFIX);
        let auth = Arc::new(Mutex::new(gateway_auth(host, FIRST_TOKEN)));
        let aperture = Aperture::with_auth_and_overrides(
            Arc::clone(&auth),
            Timeouts::default(),
            Overrides::new(),
        );
        let mut model = Model::from_spec(spec).unwrap();

        ask(&aperture, &model).await;
        let built = only_route(&aperture);
        *auth.lock().unwrap() = gateway_auth(host, ROTATED_TOKEN);
        ask(&aperture, &model).await;
        aperture.adjust_model(&mut model);

        assert!(Arc::ptr_eq(&built, &only_route(&aperture)), "{REBUILT}");
        let tokens: Vec<String> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.authorization().to_owned())
            .collect();
        assert_eq!(tokens, [FIRST_TOKEN, ROTATED_TOKEN], "{STALE_TOKEN}");
    }

    #[test]
    fn apply_adjustments_thinking_override_wins_over_route() {
        let mut overrides = Overrides::new();
        overrides.insert(
            "zai".into(),
            ProviderOverride {
                default: OverrideFields::default(),
                models: HashMap::from([(
                    "glm-5.2".into(),
                    OverrideFields {
                        supports_thinking: Some(true),
                        ..Default::default()
                    },
                )]),
            },
        );
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(
            model.thinking_override,
            ThinkingSupport::from_flags(Some(true), false)
        );
        assert!(model.supports_thinking());
    }

    #[test]
    fn apply_adjustments_thinking_override_disables_routed_capable_model() {
        register_routed();
        let mut overrides = Overrides::new();
        overrides.insert(
            ROUTED_SLUG.into(),
            ProviderOverride {
                default: OverrideFields {
                    supports_thinking: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut model = Model::from_spec(ROUTED_CHAT_SPEC).unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(
            model.thinking_override,
            ThinkingSupport::from_flags(Some(false), false)
        );
        assert!(!model.supports_thinking());
    }

    /// `ikora-openai` routes nowhere on its own, so the `true` below can only
    /// have come from llama-cpp.
    #[test]
    fn apply_adjustments_thinking_via_base_override() {
        let mut overrides = Overrides::new();
        overrides.insert(
            "ikora-openai".into(),
            ProviderOverride {
                default: OverrideFields {
                    base: Some("llama-cpp".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut model = Model::from_spec("aperture/ikora-openai/gemma4").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(
            model.thinking_override,
            ThinkingSupport::from_flags(Some(true), false)
        );
    }

    #[test]
    fn overrides_roundtrip_from_providers_toml() {
        // Model ids containing dots must be quoted: TOML treats `glm-5.2` as a
        // dotted key unless wrapped in quotes.
        let toml = r#"
[aperture.overrides.ikora-openai]
base = "llama-cpp"

[aperture.overrides.zai]
context_window = 128000
max_output_tokens = 8192
path_prefix = ""

[aperture.overrides.zai.models."glm-5.2"]
context_window = 200000
supports_thinking = true
supports_vision = true
"#;
        let config: maki_config::providers::ProvidersConfig = toml::from_str(toml).unwrap();
        let overrides = config.get("aperture").unwrap().overrides.clone();
        assert_eq!(
            spec_slug(
                "ikora-openai",
                &merged_override(&overrides, "ikora-openai", "gemma4")
            ),
            Some("llama-cpp")
        );
        let ov = merged_override(&overrides, "zai", "glm-5.2");
        assert_eq!(ov.context_window, Some(200_000));
        assert_eq!(ov.max_output_tokens, Some(8_192));
        assert_eq!(ov.supports_thinking, Some(true));
        assert_eq!(ov.supports_vision, Some(true));
        assert_eq!(path_prefix(Some(route("zai")), &ov), "");
        let ov2 = merged_override(&overrides, "zai", "other");
        assert_eq!(ov2.context_window, Some(128_000));
    }

    const REASONING_EFFORT: &str = "reasoning_effort";
    const REFUSED_REASONING: Canned = Canned::json(
        400,
        r#"{"error":{"message":"unsupported parameter: reasoning_effort","type":"invalid_request_error"}}"#,
    );
    const REFUSAL_THEN_ANSWERS: &[Canned] = &[REFUSED_REASONING, ANSWER, ANSWER];
    const ZAI_TARGET: &str = "zai/glm-5.3";

    fn pinned_thinking() -> Overrides {
        opaque_override(OverrideFields {
            supports_thinking: Some(true),
            like: Some(ZAI_TARGET.into()),
            ..Default::default()
        })
    }

    #[test_case(pinned_thinking(), Some("high") ; "a_pin_sends_chat_effort_whatever_like_names")]
    #[test_case(Overrides::new(), None ; "no_pin_sends_nothing")]
    fn the_generic_route_spells_thinking_the_chat_way(
        overrides: Overrides,
        expected: Option<&str>,
    ) {
        let model = opaque_model(&overrides);
        let fields = generic_thinking(&model, ThinkingConfig::Effort(Effort::High));
        assert_eq!(
            fields.get(REASONING_EFFORT).and_then(Value::as_str),
            expected
        );
    }

    #[test]
    fn a_generic_pin_reports_what_the_chat_spelling_sends() {
        let overrides = pinned_thinking();
        let model = opaque_model(&overrides);
        assert_eq!(model.thinking_with_source(), (true, FactSource::Config));
        assert_eq!(
            routed_sends_thinking(&overrides, &model, ThinkingConfig::Effort(Effort::Max)),
            ThinkingConfig::Effort(Effort::High)
        );
    }

    #[tokio::test]
    async fn a_refused_reasoning_effort_is_replayed_and_remembered() {
        let _home = isolated_home();
        let (base_url, requests) = serve(REFUSAL_THEN_ANSWERS);
        let host = base_url.trim_end_matches(DEFAULT_PATH_PREFIX);
        let auth = Arc::new(Mutex::new(gateway_auth(host, FIRST_TOKEN)));
        let overrides = pinned_thinking();
        let model = opaque_model(&overrides);
        let aperture = Aperture::with_auth_and_overrides(auth, Timeouts::default(), overrides);
        let opts = RequestOptions {
            thinking: ThinkingConfig::Adaptive,
            ..Default::default()
        };

        ask_with(&aperture, &model, opts).await;
        ask_with(&aperture, &model, opts).await;

        let sent: Vec<bool> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.json().get(REASONING_EFFORT).is_some())
            .collect();
        assert_eq!(sent, [true, false, false]);
    }
}
