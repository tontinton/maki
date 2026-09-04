use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::process;

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::debug;

use maki_storage::paths;

const PROVIDERS_FILE: &str = "providers.toml";
const BAD_CONFIG_EXIT_CODE: i32 = 2;
/// The only built-in that reads `enable_free_models`.
const OPENCODE_SLUG: &str = "opencode";

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
    /// Set when this provider's cost is prepaid via a flat subscription
    /// (e.g. `"Max"` for Claude Max through cliproxy). Every model resolved
    /// through it bills `$0` while still tracking the published list
    /// price as a reference; see [`maki_providers::ModelPricing::subsidised_by`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsidised_by: Option<String>,
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
        let path = providers_file_path();
        if !path.exists() {
            return Self::default();
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read providers.toml");
                return Self::default();
            }
        };
        match toml::from_str::<ProvidersConfig>(&content) {
            Ok(config) => {
                debug!(path = %path.display(), "loaded providers config");
                config
            }
            Err(e) => {
                eprintln!("error: invalid {}: {e}", path.display());
                process::exit(BAD_CONFIG_EXIT_CODE);
            }
        }
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = providers_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&path, content)?;
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

/// Fields a `providers.toml` entry sets that a built-in slug ignores, because
/// built-ins keep their compiled protocol, model catalog and auth wiring.
/// Callers decide what counts as built-in (the inventory misses the `opencode`
/// slugs) and when to report it.
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
        ignored.push("models");
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
        // SAFETY: unique test-only var; removed before the test returns.
        unsafe {
            std::env::set_var(&env_var, "http://env.local/v1");
        }
        let def = ProviderDef {
            base_url: Some("http://toml.local/v1".into()),
            ..Default::default()
        };
        let got = resolve_base_url(slug, Some(&def));
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
