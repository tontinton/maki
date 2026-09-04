use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_config::providers::{OverrideFields, ProviderOverride};
use serde_json::Value;
use tracing::warn;

use crate::manifest::{ManifestRegistry, ProviderManifest};
use crate::model::{Model, ModelEntry, ModelInfo, ModelPricing, ThinkingSupport, lookup_entry};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};
use maki_storage::id::SessionRef;

use super::anthropic::Anthropic;
use super::deepseek::DeepSeek;
use super::google::Google;
use super::local::{LLAMACPP, LocalEndpoint, OLLAMA};
use super::mistral::Mistral;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::openrouter::OpenRouter;
use super::regolo::Regolo;
use super::synthetic::Synthetic;
use super::tensorx::TensorX;
use super::zai::Zai;
use super::{ResolvedAuth, Timeouts};

const HOST_ENV: &str = "APERTURE_HOST";
const PER_MILLION: f64 = 1_000_000.0;
const ALL_MODELS: &str = "*";
const DEFAULT_PATH_PREFIX: &str = "/v1";
const GEMINI_PATH_PREFIX: &str = "/v1beta";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "aperture",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Aperture",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "aperture",
    display_name: "Aperture",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "",
    default_api_key_env: "",
    default_model: "",
    plans: None,
    login_url: None,
    needs_url: true,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

type Overrides = HashMap<String, ProviderOverride>;

/// Load per-gateway-provider overrides from the `aperture` entry's `overrides`
/// table in `providers.toml`.
fn load_overrides() -> Overrides {
    let overrides = maki_config::providers::ProvidersConfig::load()
        .get("aperture")
        .map(|d| d.overrides.clone())
        .unwrap_or_default();
    validate_overrides(&overrides);
    overrides
}

/// Warn about `base` values that don't name an OpenAI-compatible provider, so a
/// typo doesn't silently fall through to the generic path.
fn validate_overrides(overrides: &Overrides) {
    for (provider, po) in overrides {
        let fields = std::iter::once((ALL_MODELS, &po.default))
            .chain(po.models.iter().map(|(id, f)| (id.as_str(), f)));
        for (model, f) in fields {
            if let Some(base) = &f.base
                && parse_compat_base(base).is_none()
            {
                warn!(
                    provider,
                    model, base, "base is not an OpenAI-compatible provider, ignoring"
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
    }
}

/// Native providers Aperture's gateway can proxy onto. The gateway speaks
/// OpenAI chat/responses, Anthropic messages, and Gemini generateContent, so
/// each routes to its native provider kind and lets it build the right path.
/// OpenAI itself is excluded: routing to the native `OpenAi` provider would
/// hit its codex responses-API path for `gpt-*-codex` models and bypass the
/// gateway. Copilot is excluded: its auth is GraphQL-discovered, not a clean
/// proxy target.
fn compat_kind(kind: ProviderKind) -> Option<ProviderKind> {
    match kind {
        ProviderKind::Anthropic
        | ProviderKind::Google
        | ProviderKind::Ollama
        | ProviderKind::LlamaCpp
        | ProviderKind::Mistral
        | ProviderKind::Zai
        | ProviderKind::DeepSeek
        | ProviderKind::OpenRouter
        | ProviderKind::Synthetic
        | ProviderKind::Regolo
        | ProviderKind::TensorX => Some(kind),
        _ => None,
    }
}

fn parse_compat_base(s: &str) -> Option<ProviderKind> {
    ProviderKind::from_str(s).ok().and_then(compat_kind)
}

/// Resolve the native provider an Aperture model should stream through. The
/// override `base` wins, then the provider segment of the id if it itself names
/// a known OpenAI-compatible provider. Lets a user remap an opaque gateway
/// vendor (e.g. `ikora-openai`) to a real native provider (e.g. `llama-cpp`).
/// `None` falls through to the generic OpenAI-compat path.
fn routed_kind(provider_id: &str, merged: &OverrideFields) -> Option<ProviderKind> {
    [merged.base.as_deref(), Some(provider_id)]
        .into_iter()
        .flatten()
        .find_map(parse_compat_base)
}

fn manifest_for_kind(kind: ProviderKind) -> Option<&'static ProviderManifest> {
    ManifestRegistry::for_slug(&kind.to_string())
}

fn kind_supports_thinking(kind: ProviderKind) -> bool {
    manifest_for_kind(kind).is_some_and(|m| m.supports_thinking)
}

/// Path prefix maki sends to the gateway, which appends the whole incoming
/// request path to the upstream's configured base url. The prefix therefore
/// follows the API format the request uses: `/v1` for OpenAI chat (and for the
/// generic gateway path), `/v1beta` for Gemini. Anthropic gets none because its
/// provider rebuilds `/v1/messages` from the bare origin. Zai gets none because
/// its API has no `/v1` segment at all (`/api/paas/v4/chat/completions`), so
/// the upstream base url must carry the full path and any prefix would double
/// up. `path_prefix` in the overrides replaces the default per gateway
/// provider.
/// Exhaustive on purpose: a new `ProviderKind` must state its prefix here
/// instead of silently inheriting `/v1` (which broke Zai once already).
fn default_path_prefix(kind: Option<ProviderKind>) -> &'static str {
    match kind {
        Some(ProviderKind::Anthropic | ProviderKind::Zai) => "",
        Some(ProviderKind::Google) => GEMINI_PATH_PREFIX,
        Some(
            ProviderKind::Ollama
            | ProviderKind::LlamaCpp
            | ProviderKind::Mistral
            | ProviderKind::DeepSeek
            | ProviderKind::OpenRouter
            | ProviderKind::Synthetic
            | ProviderKind::Regolo
            | ProviderKind::TensorX
            | ProviderKind::OpenAi
            | ProviderKind::Copilot
            | ProviderKind::Opencode
            | ProviderKind::Xai
            | ProviderKind::Aperture,
        )
        | None => DEFAULT_PATH_PREFIX,
    }
}

fn path_prefix(kind: Option<ProviderKind>, merged: &OverrideFields) -> String {
    let Some(configured) = merged.path_prefix.as_deref() else {
        return default_path_prefix(kind).to_string();
    };
    let trimmed = configured.trim().trim_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    format!("/{trimmed}")
}

/// Clone the shared auth, appending the path prefix to the host. The gateway
/// forwards the resulting path to the upstream.
fn routed_auth(auth: &Arc<Mutex<ResolvedAuth>>, prefix: &str) -> Arc<Mutex<ResolvedAuth>> {
    let mut cloned = auth.lock().unwrap().clone();
    if !prefix.is_empty()
        && let Some(base) = cloned.base_url.as_deref()
    {
        cloned.base_url = Some(format!("{}{prefix}", base.trim_end_matches('/')));
    }
    Arc::new(Mutex::new(cloned))
}

fn build_routed_provider(
    kind: ProviderKind,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    match kind {
        ProviderKind::Ollama => Box::new(
            LocalEndpoint::with_auth(&OLLAMA, auth, timeouts).with_system_prefix(system_prefix),
        ),
        ProviderKind::LlamaCpp => Box::new(
            LocalEndpoint::with_auth(&LLAMACPP, auth, timeouts).with_system_prefix(system_prefix),
        ),
        ProviderKind::Mistral => {
            Box::new(Mistral::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::Zai => {
            Box::new(Zai::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::DeepSeek => {
            Box::new(DeepSeek::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::OpenRouter => {
            Box::new(OpenRouter::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::Synthetic => {
            Box::new(Synthetic::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::TensorX => {
            Box::new(TensorX::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::Regolo => {
            Box::new(Regolo::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::Anthropic => {
            Box::new(Anthropic::with_auth(auth, timeouts).with_system_prefix(system_prefix))
        }
        ProviderKind::Google => Box::new(Google::with_auth(auth, timeouts)),
        // Excluded by `compat_kind`: routing to native OpenAI would bypass the
        // gateway for codex models, and the rest have no clean proxy story. A
        // new kind must pick a side here.
        ProviderKind::OpenAi
        | ProviderKind::Copilot
        | ProviderKind::Opencode
        | ProviderKind::Xai
        | ProviderKind::Aperture => unreachable!("excluded by compat_kind"),
    }
}

/// The model the routed provider should stream against. The gateway strips the
/// `<vendor>/` prefix itself and uses it to pick the upstream, so every route
/// keeps the full id. Gemini is the exception: its model id goes into the url
/// path, where an encoded slash would not survive, so it gets the bare id.
fn native_route_model(model: &Model, kind: ProviderKind, bare_model_id: &str) -> Model {
    let mut m = model.clone();
    if kind == ProviderKind::Google {
        m.id = bare_model_id.to_string();
    }
    m
}

pub struct Aperture {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
    overrides: Overrides,
}

impl Aperture {
    pub fn new(timeouts: Timeouts) -> Result<Self, AgentError> {
        let base_url = resolve_base_url()?;
        let auth = Arc::new(Mutex::new(
            ResolvedAuth::new(CONFIG.slug, Vec::new())?.with_base_url(Some(base_url)),
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
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix.filter(|s| !s.is_empty());
        self
    }
}

fn resolve_base_url() -> Result<String, AgentError> {
    if let Ok(url) = std::env::var(HOST_ENV)
        && !url.trim().is_empty()
    {
        return Ok(trim_slash(url));
    }
    if let Some(url) = maki_config::providers::ProvidersConfig::load()
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
                context_window: ov.context_window,
                max_output_tokens: ov.max_output_tokens,
                pricing: m["pricing"].as_object().map(|p| {
                    ModelPricing::per_token(
                        price_per_m(p.get("input")),
                        price_per_m(p.get("output")),
                        0.0,
                        price_per_m(p.get("input_cache_read")),
                    )
                }),
                supports_thinking: ov
                    .supports_thinking
                    .or_else(|| routed_kind(provider_id, &ov).map(kind_supports_thinking)),
                supports_vision: ov.supports_vision,
                tier: None,
                provider_info: None,
            })
        })
        .collect()
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
    if let Some(kind) = routed_kind(provider_id, &ov) {
        model.family = kind.family();
        if let Some(manifest) = manifest_for_kind(kind) {
            model.thinking_override = model
                .thinking_override
                .or_else(|| ThinkingSupport::from_flags(Some(manifest.supports_thinking), false));
            if let Ok(entry) = lookup_entry(manifest.models, model_id) {
                model.context_window = entry.context_window;
                model.max_output_tokens = entry.max_output_tokens;
                model.supports_vision_override =
                    model.supports_vision_override.or(Some(entry.vision));
            }
        }
    }
    if let Some(cw) = ov.context_window {
        model.context_window = cw;
    }
    model.max_output_tokens = ov.max_output_tokens.or(model.max_output_tokens);
    if let Some(thinking) = ov.supports_thinking {
        model.thinking_override = ThinkingSupport::from_flags(Some(thinking), false);
    }
    model.supports_vision_override = ov.supports_vision.or(model.supports_vision_override);
}

impl Provider for Aperture {
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
            let (provider_id, model_id) = model.id.split_once('/').unwrap_or(("", &model.id));
            let ov = merged_override(&self.overrides, provider_id, model_id);
            let kind = routed_kind(provider_id, &ov);
            let auth = routed_auth(&self.auth, &path_prefix(kind, &ov));
            if let Some(kind) = kind {
                let provider =
                    build_routed_provider(kind, auth, self.timeouts, self.system_prefix.clone());
                let request_model = native_route_model(model, kind, model_id);
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
            let auth = auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let body = self.compat.build_body(model, messages, system, tools);
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
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
            if let Some(kind) = routed_kind(provider_id, &ov) {
                let routed = build_routed_provider(
                    kind,
                    routed_auth(&self.auth, &path_prefix(Some(kind), &ov)),
                    self.timeouts,
                    self.system_prefix.clone(),
                );
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
    use crate::model::ModelFamily;
    use serde_json::json;
    use test_case::test_case;

    #[test_case("zai", Some(ProviderKind::Zai) ; "known_zai")]
    #[test_case("synthetic", Some(ProviderKind::Synthetic) ; "known_synthetic")]
    #[test_case("openai", None ; "openai_excluded")]
    #[test_case("llama-cpp", Some(ProviderKind::LlamaCpp) ; "known_llama_cpp")]
    #[test_case("ikora-openai", None ; "unknown_vendor_no_override")]
    #[test_case("anthropic", Some(ProviderKind::Anthropic) ; "known_anthropic")]
    #[test_case("google", Some(ProviderKind::Google) ; "known_google")]
    #[test_case("copilot", None ; "copilot_excluded")]
    #[test_case("aperture", None ; "aperture_no_recurse")]
    #[test_case("gemini", None ; "gemini_vendor_unparsable_without_override")]
    fn routed_kind_without_overrides(provider_id: &str, expected: Option<ProviderKind>) {
        assert_eq!(
            routed_kind(provider_id, &OverrideFields::default()),
            expected
        );
    }

    fn base_override(base: &str) -> OverrideFields {
        OverrideFields {
            base: Some(base.into()),
            ..Default::default()
        }
    }

    #[test_case("gemini", "google", Some(ProviderKind::Google) ; "remaps_vendor_the_gateway_renamed")]
    #[test_case("ikora-openai", "llama-cpp", Some(ProviderKind::LlamaCpp) ; "remaps_unknown_vendor")]
    #[test_case("zai", "not-a-real-provider", Some(ProviderKind::Zai) ; "invalid_base_falls_back_to_provider_id")]
    fn routed_kind_with_base_override(
        provider_id: &str,
        base: &str,
        expected: Option<ProviderKind>,
    ) {
        assert_eq!(routed_kind(provider_id, &base_override(base)), expected);
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
            routed_kind(provider_id, &base_override("google")),
            Some(ProviderKind::Google)
        );
    }

    #[test_case(ProviderKind::Google, "aperture/gemini/gemini-pro-latest", "gemini-pro-latest" ; "native_google_strips_vendor_prefix")]
    #[test_case(ProviderKind::Anthropic, "aperture/anthropic/claude-test", "anthropic/claude-test" ; "anthropic_keeps_vendor_prefix")]
    #[test_case(ProviderKind::Zai, "aperture/zai/glm-5.2", "zai/glm-5.2" ; "compat_zai_keeps_vendor_prefix")]
    #[test_case(ProviderKind::Ollama, "aperture/ollama/glm-5.2", "ollama/glm-5.2" ; "compat_ollama_keeps_vendor_prefix")]
    fn native_route_model_vendor_prefix_policy(kind: ProviderKind, spec: &str, expected_id: &str) {
        let model = Model::from_spec(spec).unwrap();
        let bare = model.id.split_once('/').unwrap().1;
        assert_eq!(native_route_model(&model, kind, bare).id, expected_id);
    }

    #[test]
    fn routed_kind_model_base_wins_over_provider_base() {
        let vendor = "ikora-openai";
        let overrides = Overrides::from([(
            vendor.into(),
            ProviderOverride {
                default: base_override("mistral"),
                models: HashMap::from([("special".into(), base_override("llama-cpp"))]),
            },
        )]);
        let kind = |model| routed_kind(vendor, &merged_override(&overrides, vendor, model));
        assert_eq!(kind("special"), Some(ProviderKind::LlamaCpp));
        assert_eq!(kind("other"), Some(ProviderKind::Mistral));
    }

    fn test_auth() -> Arc<Mutex<ResolvedAuth>> {
        Arc::new(Mutex::new(ResolvedAuth::for_test(
            Some("https://aperture.example.com".into()),
            Vec::new(),
        )))
    }

    #[test_case(Some(ProviderKind::Ollama), Some("https://aperture.example.com/v1") ; "ollama_appends_v1")]
    #[test_case(Some(ProviderKind::Zai), Some("https://aperture.example.com") ; "zai_keeps_bare_host")]
    #[test_case(Some(ProviderKind::DeepSeek), Some("https://aperture.example.com/v1") ; "deepseek_appends_v1")]
    #[test_case(None, Some("https://aperture.example.com/v1") ; "unrouted_appends_v1")]
    #[test_case(Some(ProviderKind::Google), Some("https://aperture.example.com/v1beta") ; "google_appends_v1beta")]
    #[test_case(Some(ProviderKind::Anthropic), Some("https://aperture.example.com") ; "anthropic_keeps_bare_host")]
    fn routed_auth_prefix_per_route(kind: Option<ProviderKind>, expected: Option<&str>) {
        let prefix = path_prefix(kind, &OverrideFields::default());
        let auth = routed_auth(&test_auth(), &prefix);
        assert_eq!(auth.lock().unwrap().base_url.as_deref(), expected);
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
        let auth = routed_auth(&test_auth(), &path_prefix(Some(ProviderKind::Zai), &merged));
        assert_eq!(auth.lock().unwrap().base_url.as_deref(), expected);
    }

    #[test]
    fn routed_auth_handles_host_with_trailing_slash() {
        let auth = Arc::new(Mutex::new(ResolvedAuth::for_test(
            Some("https://aperture.example.com/".into()),
            Vec::new(),
        )));
        let routed = routed_auth(&auth, DEFAULT_PATH_PREFIX);
        assert_eq!(
            routed.lock().unwrap().base_url.as_deref(),
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
                {"id": "qwen3.6", "metadata": {"provider": {"id": "ollama"}}, "pricing": {"input": "0.000001", "output": 0.000002, "input_cache_read": "0.0000001"}},
                {"id": "gemma4", "metadata": {"provider": {"id": "ikora"}}},
                {"id": "raw-model"}
            ]
        });
        let models = parse_models(&body, &overrides);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "ollama/qwen3.6");
        assert_eq!(models[0].context_window, Some(65_536));
        assert_eq!(models[0].supports_vision, Some(true));
        assert!(models[0].max_output_tokens.is_none());
        let p = models[0].pricing.as_ref().unwrap();
        assert!((p.input - 1.0).abs() < 1e-9);
        assert!((p.output - 2.0).abs() < 1e-9);
        assert!((p.cache_read - 0.1).abs() < 1e-9);
        assert_eq!(models[1].id, "ikora/gemma4");
        assert!(models[1].pricing.is_none());
        assert!(models[1].supports_vision.is_none());
        assert_eq!(models[2].id, "raw-model");
    }

    #[test]
    fn parse_models_handles_missing_data() {
        assert!(parse_models(&json!({}), &Overrides::new()).is_empty());
        assert!(parse_models(&json!({"data": []}), &Overrides::new()).is_empty());
    }

    #[test]
    fn apply_adjustments_uses_routed_provider_static_table() {
        let mut model = Model::from_spec("aperture/zai/glm-5.2").unwrap();
        assert_eq!(
            model.context_window,
            ProviderKind::Aperture.fallback_context_window()
        );
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_output_tokens, Some(131_072));
        assert_eq!(model.family, ModelFamily::Glm);
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
        let mut model = Model::from_spec("aperture/mistral/mistral-medium-latest").unwrap();
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
        let mut overrides = Overrides::new();
        overrides.insert(
            "mistral".into(),
            ProviderOverride {
                default: OverrideFields {
                    supports_vision: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut model = Model::from_spec("aperture/mistral/mistral-medium-latest").unwrap();
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

    #[test_case("aperture/deepseek/deepseek-chat", ProviderKind::DeepSeek ; "routed_thinking_capable")]
    #[test_case("aperture/ollama/qwen3", ProviderKind::Ollama ; "routed_non_thinking")]
    #[test_case("aperture/zai/glm-5.2", ProviderKind::Zai ; "routed_zai")]
    fn apply_adjustments_thinking_follows_routed_kind(spec: &str, kind: ProviderKind) {
        let mut model = Model::from_spec(spec).unwrap();
        assert!(model.thinking_override.is_none());
        apply_adjustments(&mut model, &Overrides::new());
        assert_eq!(
            model.thinking_override,
            ThinkingSupport::from_flags(Some(kind_supports_thinking(kind)), false)
        );
        assert_eq!(model.supports_thinking(), kind_supports_thinking(kind));
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
        let mut overrides = Overrides::new();
        overrides.insert(
            "deepseek".into(),
            ProviderOverride {
                default: OverrideFields {
                    supports_thinking: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut model = Model::from_spec("aperture/deepseek/deepseek-chat").unwrap();
        apply_adjustments(&mut model, &overrides);
        assert_eq!(
            model.thinking_override,
            ThinkingSupport::from_flags(Some(false), false)
        );
        assert!(!model.supports_thinking());
    }

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
            ThinkingSupport::from_flags(
                Some(kind_supports_thinking(ProviderKind::LlamaCpp)),
                false
            )
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
            routed_kind(
                "ikora-openai",
                &merged_override(&overrides, "ikora-openai", "gemma4")
            ),
            Some(ProviderKind::LlamaCpp)
        );
        let ov = merged_override(&overrides, "zai", "glm-5.2");
        assert_eq!(ov.context_window, Some(200_000));
        assert_eq!(ov.max_output_tokens, Some(8_192));
        assert_eq!(ov.supports_thinking, Some(true));
        assert_eq!(ov.supports_vision, Some(true));
        assert_eq!(path_prefix(Some(ProviderKind::Zai), &ov), "");
        let ov2 = merged_override(&overrides, "zai", "other");
        assert_eq!(ov2.context_window, Some(128_000));
    }
}
