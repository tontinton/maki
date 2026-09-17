use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::debug;

use maki_storage::paths;
use maki_storage::sessions::Effort;
use serde_json::{Map as JsonMap, Value as JsonValue};

const PROVIDERS_FILE: &str = "providers.toml";
const BAD_CONFIG_EXIT_CODE: i32 = 2;
/// The only built-in that reads `enable_free_models`.
const OPENCODE_SLUG: &str = "opencode";
const LOCAL_OVERLAY_SLUGS: [&str; 2] = ["ollama", "llama-cpp"];

/// Where the last parse came from and what the file looked like then. The
/// config path follows the project directory, so one process can read more
/// than one of these.
type FileStamp = (PathBuf, Option<SystemTime>, u64);

/// The parse of `providers.toml`, kept until the file changes.
/// [`ProvidersConfig::load`] runs on nearly every model resolution, and once
/// per row while the model picker builds its list, so without this each of
/// those costs a read plus a full TOML parse.
static PARSED: Mutex<Option<(FileStamp, ProvidersConfig)>> = Mutex::new(None);

/// Coarse capability classification used by maki-providers to dispatch tiered
/// requests. Mirrors `maki_providers::ModelTier` shape but lives here so the
/// config layer can validate inputs without depending on maki-providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Weak,
    #[default]
    Medium,
    Strong,
    Compaction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub id: String,
    #[serde(default)]
    pub tier: Tier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tool_examples: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_fields: Option<ThinkingFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_fast_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_fast_output: Option<f64>,
}

impl ModelDef {
    /// Any pricing field set means the user provided pricing (other fields default to 0).
    pub fn has_pricing(&self) -> bool {
        self.pricing_input.is_some()
            || self.pricing_output.is_some()
            || self.pricing_cache_write.is_some()
            || self.pricing_cache_read.is_some()
    }

    pub fn has_fast_pricing(&self) -> bool {
        self.pricing_fast_input.is_some() || self.pricing_fast_output.is_some()
    }
}

/// How one model spells thinking on the wire: each mode carries the JSON
/// fragment that maki-providers merges into the request body, so any shape a
/// chat template needs works without a schema per provider. Typed rather than
/// a free-form `Value` so a typo'd level key fails the parse and says so,
/// instead of quietly leaving the model without thinking.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off: Option<JsonMap<String, JsonValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<JsonMap<String, JsonValue>>,
    /// Keyed by [`Effort`]; the declared keys are the levels the model accepts.
    #[serde(flatten)]
    pub levels: BTreeMap<Effort, JsonMap<String, JsonValue>>,
}

/// Normalize a provider name into a lowercase, hyphen-separated slug.
/// "My Cool Provider" -> "my-cool-provider"
pub fn slugify(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Openai,
    OpenaiResponses,
    Anthropic,
    Google,
}

impl FromStr for Protocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openai" => Ok(Self::Openai),
            "openai-responses" => Ok(Self::OpenaiResponses),
            "anthropic" => Ok(Self::Anthropic),
            "google" => Ok(Self::Google),
            _ => Err(format!("unknown protocol: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProviderPlan {
    pub display_name: &'static str,
    pub base_url: &'static str,
    pub default_model: Option<&'static str>,
    pub login_url: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuiltInProvider {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub protocol: Protocol,
    pub default_base_url: &'static str,
    pub default_api_key_env: &'static str,
    pub default_model: &'static str,
    pub plans: Option<&'static [(&'static str, ProviderPlan)]>,
    pub login_url: Option<&'static str>,
    /// Whether the login flow should prompt for a base URL (e.g. local inference servers).
    pub needs_url: bool,
}

inventory::collect!(BuiltInProvider);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverrideFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Path prefix sent to the gateway, replacing the default (`/v1`, or
    /// `/v1beta` for Gemini routes). Set it to `""` when the upstream's base
    /// url already carries its own path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

/// Overrides for a single gateway provider (Aperture), keyed by its id (e.g.
/// `zai`, `ollama`, `ikora-openai`). Provider-level fields apply to every model
/// from that provider; `models` refine individual models.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderOverride {
    #[serde(flatten)]
    pub default: OverrideFields,
    #[serde(default)]
    pub models: HashMap<String, OverrideFields>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderDef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discover_models: bool,
    /// Extra HTTP headers sent with every request to this provider. Values
    /// expand `${VAR}` from the environment, so gateway credentials (e.g.
    /// Cloudflare Access service tokens in front of a private endpoint) never
    /// land in the config file:
    ///
    /// ```toml
    /// [anthropic]
    /// base_url = "https://gw.internal/anthropic"
    /// [anthropic.headers]
    /// CF-Access-Client-Id = "${CF_ACCESS_CLIENT_ID}"
    /// CF-Access-Client-Secret = "${CF_ACCESS_CLIENT_SECRET}"
    /// ```
    ///
    /// A same-name header (case-insensitive) replaces the built-in auth
    /// header instead of appending, and keeps winning across key rotations.
    /// An unset or empty variable fails the whole provider so callers see the
    /// missing name instead of a silent 401.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Opencode-only: when `Some(false)`, free catalog models are hidden
    /// entirely. Defaults to `false` when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_free_models: Option<bool>,
    /// Aperture-only: per-gateway-provider overrides for the routed native
    /// providers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overrides: HashMap<String, ProviderOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderDef>,
}

impl ProvidersConfig {
    /// Read and parse `providers.toml`. Hard-exits on parse errors so a typo
    /// in tier or pricing surfaces immediately instead of silently dropping
    /// every provider and starting maki with an empty registry.
    pub fn load() -> Self {
        Self::read().unwrap_or_else(|e| {
            eprintln!("error: {e}");
            process::exit(BAD_CONFIG_EXIT_CODE);
        })
    }

    /// Same read, but a typo only costs the answer. For callers past startup,
    /// where taking the process down mid-session is never the right trade.
    pub fn load_or_default() -> Self {
        Self::read().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "ignoring providers.toml");
            Self::default()
        })
    }

    fn read() -> Result<Self, String> {
        let path = providers_file_path();
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read providers.toml");
                return Ok(Self::default());
            }
        };
        let stamp = (path, meta.modified().ok(), meta.len());
        let mut parsed = PARSED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, config)) = parsed.as_ref()
            && *cached == stamp
        {
            return Ok(config.clone());
        }
        let content = match fs::read_to_string(&stamp.0) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %stamp.0.display(), error = %e, "cannot read providers.toml");
                return Ok(Self::default());
            }
        };
        let config = toml::from_str::<ProvidersConfig>(&content)
            .map_err(|e| format!("invalid {}: {e}", stamp.0.display()))?;
        debug!(path = %stamp.0.display(), "loaded providers config");
        *parsed = Some((stamp, config.clone()));
        Ok(config)
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = providers_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&path, content)?;
        // A write the cache cannot distinguish from what it holds (same length
        // inside one mtime tick) would otherwise serve the old parse.
        *PARSED.lock().unwrap_or_else(|e| e.into_inner()) = None;
        debug!(path = %path.display(), "saved providers config");
        Ok(())
    }

    pub fn get(&self, slug: &str) -> Option<&ProviderDef> {
        self.providers.get(slug)
    }

    pub fn upsert(&mut self, slug: String, def: ProviderDef) {
        self.providers.insert(slug, def);
    }

    pub fn remove(&mut self, slug: &str) -> bool {
        self.providers.remove(slug).is_some()
    }
}

/// The `providers.toml` we already read, or where a fresh one goes. Both share
/// this path so `save` cannot leave a second copy behind in the other dir.
fn providers_file_path() -> PathBuf {
    paths::find_config_path(PROVIDERS_FILE).unwrap_or_else(|| {
        paths::config_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(PROVIDERS_FILE)
    })
}

pub fn builtin_provider(slug: &str) -> Option<&'static BuiltInProvider> {
    inventory::iter::<BuiltInProvider>()
        .into_iter()
        .find(|p| p.slug == slug)
}

pub fn all_builtins() -> Vec<&'static BuiltInProvider> {
    inventory::iter::<BuiltInProvider>().collect()
}

pub fn resolve_api_key_env(slug: &str, def: Option<&ProviderDef>) -> String {
    if let Some(d) = def
        && let Some(env) = &d.api_key_env
    {
        return env.clone();
    }
    if let Some(builtin) = builtin_provider(slug) {
        return builtin.default_api_key_env.to_string();
    }
    format!("{}_API_KEY", slug.to_uppercase().replace('-', "_"))
}

/// The `<SLUG>_BASE_URL` env var name (e.g. `anthropic` -> `ANTHROPIC_BASE_URL`,
/// `llama-cpp` -> `LLAMA_CPP_BASE_URL`).
pub fn base_url_env_var(slug: &str) -> String {
    format!("{}_BASE_URL", slug.to_uppercase().replace('-', "_"))
}

/// The `<SLUG>_BASE_URL` override, or `None` when unset or empty.
pub fn base_url_override(slug: &str) -> Option<String> {
    std::env::var(base_url_env_var(slug))
        .ok()
        .filter(|url| !url.is_empty())
}

/// Env override then `providers.toml`, without the built-in default. Callers
/// that already carry a default (the openai-compat layer, whose static default
/// can be more specific than the inventory one) use this.
pub fn configured_base_url(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    if let Some(url) = base_url_override(slug) {
        return Some(url);
    }
    let def = def?;
    if let Some(url) = &def.base_url {
        return Some(url.clone());
    }
    let plan_name = def.plan.as_ref()?;
    builtin_provider(slug)?
        .plans?
        .iter()
        .find(|(key, _)| key == plan_name)
        .map(|(_, plan)| plan.base_url.to_string())
}

pub fn resolve_base_url(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    configured_base_url(slug, def)
        .or_else(|| builtin_provider(slug).map(|b| b.default_base_url.to_string()))
}

/// Whether a built-in slug reads the thinking keys (`supports_thinking`,
/// `requires_thinking`, `thinking_fields`) of its `providers.toml` models.
/// Only the local endpoints do, and only those keys: everything else about
/// them stays compiled in, so an `OLLAMA_HOST` setup can describe how its
/// models think without a second slug and without touching auth wiring.
///
/// The list is the slugs whose request path merges those keys into the body,
/// not every slug that serves models we cannot know (`google`, `copilot` and
/// `mistral` also accept arbitrary models, but their paths would drop the
/// keys on the floor). It grows when a path learns to read them.
pub fn overlays_local_thinking(slug: &str) -> bool {
    LOCAL_OVERLAY_SLUGS.contains(&slug)
}

/// The rest of a local `[[slug.models]]` entry, which still loses to the
/// compiled-in catalog. Named key by key so exempting `models` from the
/// built-in warning does not quietly swallow the other half of it. A new
/// [`ModelDef`] field has to be classified here or in the overlay;
/// `every_model_def_field_is_read_or_reported` stops compiling until it is.
fn ignored_local_model_fields(models: &[ModelDef]) -> Vec<&'static str> {
    let any = |is_set: fn(&ModelDef) -> bool| models.iter().any(is_set);
    let mut ignored = Vec::new();
    if any(|m| m.tier != Tier::default()) {
        ignored.push("models.tier");
    }
    if any(|m| m.context_window.is_some()) {
        ignored.push("models.context_window");
    }
    if any(|m| m.max_output_tokens.is_some()) {
        ignored.push("models.max_output_tokens");
    }
    if any(|m| m.supports_tool_examples.is_some()) {
        ignored.push("models.supports_tool_examples");
    }
    if any(|m| m.supports_vision.is_some()) {
        ignored.push("models.supports_vision");
    }
    if any(|m| m.has_pricing() || m.has_fast_pricing()) {
        ignored.push("models.pricing_*");
    }
    ignored
}

/// Fields a `providers.toml` entry sets that a built-in slug ignores, because
/// built-ins keep their compiled protocol, model catalog and auth wiring.
/// Callers decide what counts as built-in (the inventory misses the `opencode`
/// slugs) and when to report it.
///
/// [`overlays_local_thinking`] slugs are the exception: their `models` table is
/// half read, so only the keys nobody reads are named.
pub fn ignored_builtin_fields(slug: &str, def: &ProviderDef) -> Vec<&'static str> {
    let mut ignored = Vec::new();
    if def.protocol.is_some() {
        ignored.push("protocol");
    }
    if def.api_key_env.is_some() {
        ignored.push("api_key_env");
    }
    if def.discover_models {
        ignored.push("discover_models");
    }
    if !def.models.is_empty() {
        if overlays_local_thinking(slug) {
            ignored.extend(ignored_local_model_fields(&def.models));
        } else {
            ignored.push("models");
        }
    }
    if def.enable_free_models.is_some() && slug != OPENCODE_SLUG {
        ignored.push("enable_free_models");
    }
    ignored
}

pub fn resolve_protocol(slug: &str, def: Option<&ProviderDef>) -> Option<Protocol> {
    if let Some(d) = def
        && let Some(p) = &d.protocol
    {
        return Some(*p);
    }
    builtin_provider(slug).map(|b| b.protocol)
}

pub fn resolve_display_name(slug: &str, def: Option<&ProviderDef>) -> String {
    if let Some(d) = def
        && let Some(name) = &d.display_name
    {
        return name.clone();
    }
    builtin_provider(slug)
        .map(|b| b.display_name.to_string())
        .unwrap_or_else(|| slug.to_string())
}

pub fn resolve_default_model(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    if let Some(d) = def {
        if let Some(m) = &d.default_model {
            return Some(m.clone());
        }
        if let Some(plan_name) = &d.plan
            && let Some(builtin) = builtin_provider(slug)
            && let Some(plans) = builtin.plans
        {
            for (key, plan) in plans {
                if key == plan_name
                    && let Some(m) = &plan.default_model
                {
                    return Some(m.to_string());
                }
            }
        }
    }
    builtin_provider(slug).map(|b| b.default_model.to_string())
}

pub fn resolve_login_url(slug: &str, plan: Option<&str>) -> Option<String> {
    if let Some(plan_name) = plan
        && let Some(builtin) = builtin_provider(slug)
        && let Some(plans) = builtin.plans
    {
        for (key, plan) in plans {
            if *key == plan_name
                && let Some(url) = plan.login_url
            {
                return Some(url.to_string());
            }
        }
    }
    builtin_provider(slug).and_then(|b| b.login_url.map(|u| u.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn provider_def_parses_custom_headers() {
        let def: ProviderDef = toml::from_str(
            "base_url = \"https://gw.example.com/v1\"\n[headers]\n\"CF-Access-Client-Id\" = \"${CF_ID}\"\n\"CF-Access-Client-Secret\" = \"${CF_SECRET}\"\n",
        )
        .unwrap();
        assert_eq!(def.headers.len(), 2);
        assert_eq!(def.headers["CF-Access-Client-Id"], "${CF_ID}");
    }

    #[test]
    fn provider_def_without_headers_is_empty() {
        let def: ProviderDef = toml::from_str("base_url = \"https://x\"\n").unwrap();
        assert!(def.headers.is_empty());
    }

    #[test]
    fn provider_def_roundtrip() {
        let mut config = ProvidersConfig::default();
        config.upsert(
            "my-provider".into(),
            ProviderDef {
                protocol: Some(Protocol::Openai),
                base_url: Some("https://api.example.com/v1".into()),
                api_key_env: Some("MY_API_KEY".into()),
                discover_models: true,
                enable_free_models: Some(false),
                ..Default::default()
            },
        );
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: ProvidersConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.get("my-provider").unwrap().protocol,
            Some(Protocol::Openai)
        );
        assert_eq!(
            parsed.get("my-provider").unwrap().base_url,
            Some("https://api.example.com/v1".into())
        );
        assert_eq!(
            parsed.get("my-provider").unwrap().enable_free_models,
            Some(false)
        );
    }

    const EMPTY_PROVIDER_DEF_TOML: &str = "";

    #[test]
    fn provider_def_enable_free_models_defaults_none() {
        let def: ProviderDef = toml::from_str(EMPTY_PROVIDER_DEF_TOML).unwrap();
        assert_eq!(def.enable_free_models, None);
    }

    const UNKNOWN_TIER_TOML: &str = r#"id = "x"
tier = "mediums"
"#;

    #[test]
    fn model_def_rejects_unknown_tier() {
        let err = toml::from_str::<ModelDef>(UNKNOWN_TIER_TOML).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("medium"), "expected enum hint, got: {msg}");
    }

    #[test]
    fn model_def_tier_defaults_to_medium() {
        let m: ModelDef = toml::from_str(r#"id = "x""#).unwrap();
        assert_eq!(m.tier, Tier::Medium);
    }

    #[test_case("weak", Tier::Weak ; "weak")]
    #[test_case("medium", Tier::Medium ; "medium")]
    #[test_case("strong", Tier::Strong ; "strong")]
    #[test_case("compaction", Tier::Compaction ; "compaction")]
    fn model_def_tier_roundtrip(input: &str, expected: Tier) {
        let toml = format!(
            r#"id = "x"
tier = "{input}"
"#
        );
        let m: ModelDef = toml::from_str(&toml).unwrap();
        assert_eq!(m.tier, expected);
    }

    #[test_case("anthropic", None => "ANTHROPIC_API_KEY".to_string(); "builtin_default")]
    #[test_case("my-custom", None => "MY_CUSTOM_API_KEY".to_string(); "custom_default")]
    fn resolve_api_key_env_tests(slug: &str, def: Option<&ProviderDef>) -> String {
        resolve_api_key_env(slug, def)
    }

    #[test]
    fn resolve_base_url_prefers_def_over_none() {
        // Unique slug: `openai` would pick up a real OPENAI_BASE_URL from the shell.
        let slug = "maki-test-def-over-none-slug";
        let def = ProviderDef {
            base_url: Some("http://proxy.local/v1".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_base_url(slug, Some(&def)).as_deref(),
            Some("http://proxy.local/v1")
        );
        assert_ne!(
            resolve_base_url(slug, Some(&def)),
            resolve_base_url(slug, None)
        );
    }

    #[test]
    fn resolve_base_url_empty_def_matches_none() {
        let slug = "maki-test-empty-def-slug";
        let def = ProviderDef::default();
        assert_eq!(
            resolve_base_url(slug, Some(&def)),
            resolve_base_url(slug, None)
        );
    }

    #[test]
    fn resolve_base_url_custom_slug_uses_def() {
        let slug = "maki-test-custom-base-url-slug";
        let def = ProviderDef {
            base_url: Some("http://xxxx:1234/v1".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_base_url(slug, Some(&def)).as_deref(),
            Some("http://xxxx:1234/v1")
        );
        assert_eq!(resolve_base_url(slug, None), None);
    }

    #[test]
    fn resolve_base_url_env_beats_def() {
        let slug = "maki-test-env-base-url-slug";
        let env_var = base_url_env_var(slug);
        // SAFETY: setting a variable is only sound while no other thread reads
        // the environment, and the runner is what holds that up: `just test`
        // runs `cargo nextest`, which gives every test its own process.
        unsafe {
            std::env::set_var(&env_var, "http://env.local/v1");
        }
        let def = ProviderDef {
            base_url: Some("http://toml.local/v1".into()),
            ..Default::default()
        };
        let got = resolve_base_url(slug, Some(&def));
        // SAFETY: same one process per test rule as above.
        unsafe {
            std::env::remove_var(&env_var);
        }
        assert_eq!(got.as_deref(), Some("http://env.local/v1"));
    }

    #[test]
    fn ignored_builtin_fields_lists_custom_only_fields() {
        let def = ProviderDef {
            base_url: Some("http://proxy.local/v1".into()),
            protocol: Some(Protocol::Openai),
            api_key_env: Some("MY_KEY".into()),
            discover_models: true,
            ..Default::default()
        };
        assert_eq!(
            ignored_builtin_fields("anthropic", &def),
            ["protocol", "api_key_env", "discover_models"]
        );
    }

    #[test]
    fn ignored_builtin_fields_keeps_opencode_free_models() {
        let def = ProviderDef {
            enable_free_models: Some(false),
            ..Default::default()
        };
        assert!(ignored_builtin_fields(OPENCODE_SLUG, &def).is_empty());
        assert_eq!(
            ignored_builtin_fields("openrouter", &def),
            ["enable_free_models"]
        );
    }

    const THINKING_MODEL: &str = r#"models = [{ id = "qwen", thinking_fields = { high = { reasoning_effort = "xhigh" } } }]"#;
    const RICH_MODEL: &str = r#"models = [{ id = "qwen", tier = "strong", context_window = 131072, supports_vision = true, pricing_input = 1.0 }]"#;

    /// A local slug reads the thinking keys and nothing else, so the half it
    /// drops still has to say so: exempting the whole table would leave a user
    /// wondering why their `tier` never took.
    #[test_case("ollama", THINKING_MODEL, Vec::new() ; "local_slug_reads_thinking_keys")]
    #[test_case("anthropic", THINKING_MODEL, vec!["models"] ; "other_builtins_keep_their_catalog")]
    #[test_case("ollama", RICH_MODEL, vec!["models.tier", "models.context_window", "models.supports_vision", "models.pricing_*"] ; "local_slug_names_what_it_dropped")]
    fn ignored_builtin_fields_on_declared_models(slug: &str, entry: &str, expected: Vec<&str>) {
        let def: ProviderDef = toml::from_str(entry).unwrap();
        assert_eq!(ignored_builtin_fields(slug, &def), expected);
    }

    /// The literal below stops compiling when [`ModelDef`] grows a field, which
    /// is the one reminder to decide what a local slug does with it: read it in
    /// the overlay, or name it here as dropped. Otherwise the warning slowly
    /// stops covering the table it describes.
    #[test]
    fn every_model_def_field_is_read_or_reported() {
        let every_field_set = ModelDef {
            id: "qwen".to_string(),
            tier: Tier::Strong,
            context_window: Some(131_072),
            max_output_tokens: Some(8192),
            supports_tool_examples: Some(true),
            supports_thinking: Some(true),
            requires_thinking: Some(true),
            thinking_fields: Some(ThinkingFields::default()),
            supports_vision: Some(true),
            pricing_input: Some(1.0),
            pricing_output: Some(1.0),
            pricing_cache_write: Some(1.0),
            pricing_cache_read: Some(1.0),
            pricing_fast_input: Some(1.0),
            pricing_fast_output: Some(1.0),
        };
        assert_eq!(
            ignored_local_model_fields(&[every_field_set]),
            vec![
                "models.tier",
                "models.context_window",
                "models.max_output_tokens",
                "models.supports_tool_examples",
                "models.supports_vision",
                "models.pricing_*",
            ]
        );
    }

    /// A level key nobody reads would leave the model thinking-less with no
    /// hint why, so a typo takes the startup down instead.
    #[test_case(r#"{ high = { reasoning_effort = "xhigh" } }"#, true ; "known_level")]
    #[test_case(r#"{ hight = { reasoning_effort = "xhigh" } }"#, false ; "typo")]
    fn thinking_level_keys_are_checked_at_parse(fields: &str, parses: bool) {
        let entry = format!(r#"models = [{{ id = "m", thinking_fields = {fields} }}]"#);
        assert_eq!(toml::from_str::<ProviderDef>(&entry).is_ok(), parses);
    }

    #[test_case("MyProvider", "myprovider"; "mixed_case")]
    #[test_case("My Cool Provider", "my-cool-provider"; "spaces")]
    #[test_case("  my-provider  ", "my-provider"; "trimmed")]
    #[test_case("My--Provider", "my-provider"; "double_dash")]
    #[test_case("-my-provider-", "my-provider"; "leading_trailing_dash")]
    #[test_case("My_Provider", "my-provider"; "underscores")]
    #[test_case("My.Cool@Provider!", "my-cool-provider"; "special_chars")]
    fn slugify_tests(input: &str, expected: &str) {
        assert_eq!(slugify(input), expected);
    }
}
