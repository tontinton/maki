use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;

use maki_storage::paths;

const PROVIDERS_FILE: &str = "providers.toml";
const BAD_CONFIG_EXIT_CODE: i32 = 2;

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
    /// Opencode-only: when `Some(false)`, free catalog models are hidden
    /// entirely. Defaults to `false` when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_free_models: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderDef>,
}

#[derive(Debug, Error)]
pub enum ProvidersConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path}: {message}")]
    Parse {
        path: PathBuf,
        message: String,
        #[source]
        source: Box<toml::de::Error>,
    },
}

impl ProvidersConfig {
    /// Read and parse `providers.toml`. Hard-exits on any error so a typo
    /// in tier or pricing (or an unreadable file) surfaces immediately
    /// instead of silently dropping every provider and starting maki with an
    /// empty registry. Missing files are treated as an empty registry (see
    /// `load_from`) and never reach this path.
    pub fn load() -> Self {
        Self::try_load().unwrap_or_else(|error| {
            eprintln!("error: {error}");
            process::exit(BAD_CONFIG_EXIT_CODE);
        })
    }

    /// Read and parse `providers.toml`, returning a typed error instead of
    /// hard-exiting. Use for runtime callers that must not kill the process
    /// (e.g. refreshing the key pool on demand).
    pub fn try_load() -> Result<Self, ProvidersConfigError> {
        Self::load_from(&providers_file_path())
    }

    fn load_from(path: &Path) -> Result<Self, ProvidersConfigError> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ProvidersConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        // `toml::de::Error`'s `Display` leaks the raw offending line (which
        // may contain a secret api_key embedded in malformed TOML); `message`
        // surfaces only the parse error and position.
        let config = toml::from_str(&content).map_err(|source: toml::de::Error| {
            ProvidersConfigError::Parse {
                path: path.to_path_buf(),
                message: source.message().to_owned(),
                source: Box::new(source),
            }
        })?;
        debug!(path = %path.display(), "loaded providers config");
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

fn providers_file_path() -> PathBuf {
    paths::config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(PROVIDERS_FILE)
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

pub fn resolve_base_url(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    if let Some(d) = def {
        if let Some(url) = &d.base_url {
            return Some(url.clone());
        }
        if let Some(plan_name) = &d.plan
            && let Some(builtin) = builtin_provider(slug)
            && let Some(plans) = builtin.plans
        {
            for (key, plan) in plans {
                if key == plan_name {
                    return Some(plan.base_url.to_string());
                }
            }
        }
    }
    builtin_provider(slug).map(|b| b.default_base_url.to_string())
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
    use tempfile::NamedTempFile;
    use test_case::test_case;

    #[test]
    fn load_from_returns_parse_error_for_invalid_toml() {
        // Malformed TOML whose error message would otherwise leak the literal
        // api_key line (containing a secret) — assert the typed error message
        // suppresses it via `toml::de::Error::message()`.
        const SECRET: &str = "secret-api-key";
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), format!("[deepseek]\napi_key = \"{SECRET}\n")).unwrap();

        let error = ProvidersConfig::load_from(file.path()).unwrap_err();

        assert!(matches!(error, ProvidersConfigError::Parse { .. }));
        assert!(
            !error.to_string().contains(SECRET),
            "parse error message leaked secret: {error}"
        );
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
