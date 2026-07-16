use std::env;
use std::fs;
use std::path::PathBuf;

use maki_storage::StateDir;
use maki_storage::auth::{
    ProviderCredentials, delete_provider_credentials, load_provider_credentials,
    save_provider_credentials,
};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use tracing::debug;

use crate::AgentError;

const TOKEN_ENV_VARS: &[&str] = &["GH_COPILOT_TOKEN", "COPILOT_GITHUB_TOKEN"];
const DEFAULT_HOST: &str = "github.com";
const PROVIDER: &str = "copilot";

pub(crate) fn graphql_url(host: &str) -> String {
    if host == DEFAULT_HOST {
        "https://api.github.com/graphql".to_owned()
    } else {
        format!("https://{host}/api/graphql")
    }
}

pub(crate) fn load_token() -> Result<ProviderCredentials, AgentError> {
    for key in TOKEN_ENV_VARS {
        if let Ok(token) = env::var(key)
            && !token.trim().is_empty()
        {
            return Ok(ProviderCredentials {
                api_key: token,
                host: None,
            });
        }
    }

    if let Ok(dir) = StateDir::resolve()
        && let Some(creds) = load_provider_credentials(&dir, PROVIDER)
    {
        debug!("using saved Copilot credentials");
        return Ok(creds);
    }

    Err(AgentError::Config {
        message: "not authenticated, run `maki auth login copilot` or set GH_COPILOT_TOKEN".into(),
    })
}

fn discover_token() -> Result<ProviderCredentials, AgentError> {
    for path in copilot_config_paths() {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some((token, host)) = extract_oauth_token_json(&contents)
        {
            return Ok(ProviderCredentials {
                api_key: token,
                host: (host != DEFAULT_HOST).then_some(host),
            });
        }
    }

    for path in gh_config_paths() {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some((token, host)) = extract_oauth_token_yaml(&contents)
        {
            return Ok(ProviderCredentials {
                api_key: token,
                host: (host != DEFAULT_HOST).then_some(host),
            });
        }
    }

    Err(AgentError::Config {
        message: "Copilot token not found. Run `gh auth login --web`, sign in with the Copilot \
            client, or set GH_COPILOT_TOKEN."
            .into(),
    })
}

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    if load_token().is_ok() {
        println!("Already authenticated with Copilot.");
        return Ok(());
    }

    let creds = discover_token()?;
    let host = creds.host.as_deref().unwrap_or(DEFAULT_HOST);
    println!("Copilot token imported from gh CLI / Copilot client config ({host}).");
    save_provider_credentials(dir, PROVIDER, &creds)?;
    Ok(())
}

pub fn logout(dir: &StateDir) -> Result<(), AgentError> {
    if delete_provider_credentials(dir, PROVIDER)? {
        println!("Logged out of Copilot.");
    } else {
        println!("Not currently logged in to Copilot.");
    }
    Ok(())
}

fn is_github_host(host: &str) -> bool {
    host == DEFAULT_HOST || host.ends_with(".ghe.com") || host.ends_with(".github.com")
}

fn copilot_config_paths() -> Vec<PathBuf> {
    config_dir()
        .map(|config| config.join("github-copilot"))
        .map(|base| vec![base.join("hosts.json"), base.join("apps.json")])
        .unwrap_or_default()
}

fn gh_config_paths() -> Vec<PathBuf> {
    config_dir()
        .map(|config| vec![config.join("gh").join("hosts.yml")])
        .unwrap_or_default()
}

fn config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| maki_storage::paths::home().map(|home| home.join(".config")))
}

fn extract_oauth_token_json(contents: &str) -> Option<(String, String)> {
    let value: JsonValue = serde_json::from_str(contents).ok()?;
    value.as_object()?.iter().find_map(|(key, value)| {
        if is_github_host(key) {
            value["oauth_token"]
                .as_str()
                .map(|tok| (tok.to_owned(), key.clone()))
        } else {
            None
        }
    })
}

fn extract_oauth_token_yaml(contents: &str) -> Option<(String, String)> {
    let value: YamlValue = serde_yaml::from_str(contents).ok()?;
    value.as_mapping()?.iter().find_map(|(key, value)| {
        let host = key.as_str()?;
        if is_github_host(host) {
            value["oauth_token"]
                .as_str()
                .map(|tok| (tok.to_owned(), host.to_owned()))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(
        r#"{"github.com": {"oauth_token": "token-1"}}"# => Some(("token-1".to_string(), "github.com".to_string())); "json_matching_domain"
    )]
    #[test_case(
        r#"{"enterprise.example.com": {"oauth_token": "token-1"}}"# => None; "json_other_domain"
    )]
    #[test_case(
        r#"{"myco.ghe.com": {"oauth_token": "ghe-tok"}}"# => Some(("ghe-tok".to_string(), "myco.ghe.com".to_string())); "json_ghe_host"
    )]
    fn extract_json_token(contents: &str) -> Option<(String, String)> {
        extract_oauth_token_json(contents)
    }

    #[test_case(
        "github.com:\n  oauth_token: token-1\n  user: octocat\n" => Some(("token-1".to_string(), "github.com".to_string())); "yaml_matching_domain"
    )]
    #[test_case(
        "enterprise.example.com:\n  oauth_token: token-1\n" => None; "yaml_other_domain"
    )]
    #[test_case(
        "myco.ghe.com:\n  oauth_token: ghe-tok\n" => Some(("ghe-tok".to_string(), "myco.ghe.com".to_string())); "yaml_ghe_host"
    )]
    fn extract_yaml_token(contents: &str) -> Option<(String, String)> {
        extract_oauth_token_yaml(contents)
    }

    #[test_case("github.com" => true; "github_com")]
    #[test_case("myco.ghe.com" => true; "ghe_com")]
    #[test_case("gitlab.com" => false; "gitlab")]
    #[test_case("evil-ghe.com" => false; "fake_ghe")]
    fn test_is_github_host(host: &str) -> bool {
        is_github_host(host)
    }

    #[test_case("github.com" => "https://api.github.com/graphql"; "graphql_default")]
    #[test_case("myco.ghe.com" => "https://myco.ghe.com/api/graphql"; "graphql_ghe")]
    fn test_graphql_url(host: &str) -> String {
        graphql_url(host)
    }
}
