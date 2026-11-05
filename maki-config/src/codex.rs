//! Custom providers from Codex's `config.toml`, spelled as `providers.toml`
//! entries.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use maki_storage::paths;
use serde::Deserialize;
use toml::Value;
use tracing::warn;

use crate::providers::{Protocol, ProviderDef, slugify};

const HOME_ENV: &str = "CODEX_HOME";
const HOME_DIR: &str = ".codex";
const CONFIG_FILE: &str = "config.toml";
/// Chat completions, which Codex has retired and older files still name.
const CHAT_WIRE_API: &str = "chat";

/// Only the keys the import reads. The rest of the file is Codex's own, so a
/// key maki does not know never fails the read.
#[derive(Deserialize)]
struct CodexConfig {
    model: Option<String>,
    model_provider: Option<String>,
    #[serde(default)]
    model_providers: BTreeMap<String, CodexProvider>,
}

/// The keys with a `providers.toml` spelling. Anything else lands in `rest`
/// and keeps the entry out: a header, a query parameter or an auth scheme maki
/// cannot carry would otherwise import as configured and fail on its first
/// request, and reading only these keys leaves no list of Codex's other keys
/// to keep current.
#[derive(Deserialize)]
struct CodexProvider {
    name: Option<String>,
    base_url: Option<String>,
    env_key: Option<String>,
    wire_api: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
pub struct CodexImport {
    /// By maki slug. The provider Codex defaults to carries its model as
    /// `default_model`.
    pub providers: BTreeMap<String, ProviderDef>,
    /// Slugs left out, each with the first key that has no spelling here.
    pub unsupported: Vec<(String, String)>,
}

/// `$CODEX_HOME/config.toml`, where Codex defaults the home to `~/.codex`.
pub fn config_path() -> Option<PathBuf> {
    config_path_in(env::var_os(HOME_ENV), paths::home())
}

fn config_path_in(codex_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    codex_home
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(HOME_DIR)))
        .map(|dir| dir.join(CONFIG_FILE))
}

/// `None` when there is no file or it does not parse. Only the parser's
/// message is logged: its full error quotes the line, and this file can hold
/// tokens.
pub fn load(path: &Path) -> Option<CodexImport> {
    let content = fs::read_to_string(path).ok()?;
    parse(&content)
        .inspect_err(|e| {
            warn!(path = %path.display(), error = e.message(), "cannot read Codex model providers");
        })
        .ok()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn parse(content: &str) -> Result<CodexImport, toml::de::Error> {
    let CodexConfig {
        model,
        model_provider,
        model_providers,
    } = toml::from_str(content)?;
    let default_slug = model_provider.as_deref().map(slugify);

    let mut import = CodexImport::default();
    for (id, provider) in model_providers {
        let slug = slugify(&id);
        // Without a `base_url` Codex talks to OpenAI or one of its own
        // built-ins, and maki has its own providers for those.
        let Some(base_url) = non_empty(provider.base_url) else {
            continue;
        };
        if slug.is_empty() {
            continue;
        }
        if let Some(key) = provider.rest.into_keys().next() {
            import.unsupported.push((slug, key));
            continue;
        }
        let protocol = match provider.wire_api.as_deref() {
            Some(CHAT_WIRE_API) => Protocol::Openai,
            _ => Protocol::OpenaiResponses,
        };
        let default_model = model
            .as_ref()
            .filter(|_| default_slug.as_deref() == Some(slug.as_str()))
            .map(|model| format!("{slug}/{model}"));
        let api_key_env = non_empty(provider.env_key);
        // Codex sends no credentials without an `env_key`, the usual shape of
        // a local server. A custom provider always resolves a key, so the
        // entry carries an empty one.
        let api_key = api_key_env.is_none().then(String::new);
        let def = ProviderDef {
            display_name: non_empty(provider.name),
            protocol: Some(protocol),
            base_url: Some(base_url),
            api_key_env,
            api_key,
            default_model,
            discover_models: true,
            ..Default::default()
        };
        import.providers.insert(slug, def);
    }
    Ok(import)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const SLUG: &str = "team-proxy";

    const FULL_ENTRY: &str = r#"
model = "gpt-5.5"
model_provider = "team_proxy"
approval_policy = "on-request"

[model_providers.team_proxy]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"
env_key = "TEAM_PROXY_KEY"
wire_api = "responses"
"#;
    const FULL_ENTRY_IMPORTED: &str = r#"display_name = "Team proxy"
protocol = "openai-responses"
base_url = "https://proxy.example.com/v1"
api_key_env = "TEAM_PROXY_KEY"
default_model = "team-proxy/gpt-5.5"
discover_models = true
"#;

    const BARE_ENTRY: &str = r#"
model = "gpt-5.5"

[model_providers.team-proxy]
name = "Team proxy"
base_url = "http://localhost:8080/v1"
env_key = ""
"#;
    const BARE_ENTRY_IMPORTED: &str = r#"display_name = "Team proxy"
protocol = "openai-responses"
base_url = "http://localhost:8080/v1"
api_key = ""
discover_models = true
"#;

    const CHAT_ENTRY: &str = r#"
[model_providers.team-proxy]
name = "Team proxy"
base_url = "http://localhost:8080/v1"
wire_api = "chat"
"#;
    const CHAT_ENTRY_IMPORTED: &str = r#"display_name = "Team proxy"
protocol = "openai"
base_url = "http://localhost:8080/v1"
api_key = ""
discover_models = true
"#;

    #[test_case(FULL_ENTRY, FULL_ENTRY_IMPORTED ; "every_mapped_key")]
    #[test_case(BARE_ENTRY, BARE_ENTRY_IMPORTED ; "keyless_and_not_the_default")]
    #[test_case(CHAT_ENTRY, CHAT_ENTRY_IMPORTED ; "retired_chat_wire_api")]
    fn codex_provider_becomes_a_providers_toml_entry(codex: &str, expected: &str) {
        let import = parse(codex).unwrap();
        assert!(import.unsupported.is_empty());
        assert_eq!(import.providers.len(), 1);
        assert_eq!(
            toml::to_string_pretty(&import.providers[SLUG]).unwrap(),
            expected
        );
    }

    const CHATGPT_LOGIN: &str = r#"
[model_providers.team-proxy]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"
requires_openai_auth = true
"#;
    const HEADERS: &str = r#"
[model_providers.team-proxy]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"
env_key = "TEAM_PROXY_KEY"
env_http_headers = { "X-Gateway-Token" = "GATEWAY_TOKEN" }
"#;
    const TIMEOUT: &str = r#"
[model_providers.team-proxy]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"
stream_idle_timeout_ms = 300000
"#;
    const QUERY_PARAMS: &str = r#"
[model_providers.team-proxy]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"
query_params = { api-version = "2026-04-01" }
"#;
    const SIGV4: &str = r#"
[model_providers.team-proxy]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"

[model_providers.team-proxy.aws]
region = "eu-west-1"
"#;
    const BUILT_IN_OVERRIDE: &str = r#"
[model_providers.amazon-bedrock.aws]
region = "eu-west-1"
"#;
    const ID_WITHOUT_A_SLUG: &str = r#"
[model_providers."__"]
name = "Team proxy"
base_url = "https://proxy.example.com/v1"
"#;
    const NO_PROVIDERS: &str = r#"model = "gpt-5.5""#;

    #[test_case(CHATGPT_LOGIN, &[(SLUG, "requires_openai_auth")] ; "chatgpt_login")]
    #[test_case(HEADERS, &[(SLUG, "env_http_headers")] ; "headers")]
    #[test_case(TIMEOUT, &[(SLUG, "stream_idle_timeout_ms")] ; "any_other_key")]
    #[test_case(QUERY_PARAMS, &[(SLUG, "query_params")] ; "query_params")]
    #[test_case(SIGV4, &[(SLUG, "aws")] ; "sigv4")]
    #[test_case(BUILT_IN_OVERRIDE, &[] ; "built_in_override_without_url")]
    #[test_case(ID_WITHOUT_A_SLUG, &[] ; "id_without_a_slug")]
    #[test_case(NO_PROVIDERS, &[] ; "no_providers_table")]
    fn entries_maki_cannot_serve_are_left_out(codex: &str, unsupported: &[(&str, &str)]) {
        let import = parse(codex).unwrap();
        assert!(import.providers.is_empty());
        let reported: Vec<(&str, &str)> = import
            .unsupported
            .iter()
            .map(|(slug, key)| (slug.as_str(), key.as_str()))
            .collect();
        assert_eq!(reported, unsupported);
    }

    const WRONG_TYPE: &str = r#"
[model_providers.team-proxy]
base_url = 1
"#;

    #[test_case(None ; "missing_file")]
    #[test_case(Some(WRONG_TYPE) ; "unparseable_file")]
    fn unreadable_config_is_no_import(content: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        if let Some(content) = content {
            fs::write(&path, content).unwrap();
        }
        assert!(load(&path).is_none());
    }

    const CODEX_HOME: &str = "codex-home";
    const USER_HOME: &str = "user-home";

    #[test_case(Some(CODEX_HOME), Some(USER_HOME), Some(&[CODEX_HOME, CONFIG_FILE]) ; "codex_home_wins")]
    #[test_case(Some(""), Some(USER_HOME), Some(&[USER_HOME, HOME_DIR, CONFIG_FILE]) ; "empty_codex_home_is_unset")]
    #[test_case(None, Some(USER_HOME), Some(&[USER_HOME, HOME_DIR, CONFIG_FILE]) ; "default_under_home")]
    #[test_case(None, None, None ; "no_home")]
    fn config_path_follows_codex_home(
        codex_home: Option<&str>,
        home: Option<&str>,
        expected: Option<&[&str]>,
    ) {
        let path = config_path_in(codex_home.map(OsString::from), home.map(PathBuf::from));
        assert_eq!(path, expected.map(|parts| parts.iter().collect()));
    }
}
