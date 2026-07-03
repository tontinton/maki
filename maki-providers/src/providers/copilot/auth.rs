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
const COPILOT_DOMAIN: &str = "github.com";
const PROVIDER: &str = "copilot";

pub(crate) fn load_token() -> Result<String, AgentError> {
    for key in TOKEN_ENV_VARS {
        if let Ok(token) = env::var(key)
            && !token.trim().is_empty()
        {
            return Ok(token);
        }
    }

    if let Ok(dir) = StateDir::resolve()
        && let Some(creds) = load_provider_credentials(&dir, PROVIDER)
    {
        debug!("using saved Copilot credentials");
        return Ok(creds.api_key);
    }

    Err(AgentError::Config {
        message: "not authenticated, run `maki auth login copilot` or set GH_COPILOT_TOKEN".into(),
    })
}

fn discover_token() -> Result<String, AgentError> {
    for path in copilot_config_paths() {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some(token) = extract_json_oauth_token(&contents, COPILOT_DOMAIN)
        {
            return Ok(token);
        }
    }

    for path in gh_config_paths() {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some(token) = extract_yaml_oauth_token(&contents, COPILOT_DOMAIN)
        {
            return Ok(token);
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

    let token = discover_token()?;
    save_provider_credentials(dir, PROVIDER, &ProviderCredentials { api_key: token })?;
    println!("Copilot token imported from gh CLI / Copilot client config.");
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

fn extract_json_oauth_token(contents: &str, domain: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(contents).ok()?;
    value.as_object()?.iter().find_map(|(key, value)| {
        if key.starts_with(domain) {
            value["oauth_token"].as_str().map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

fn extract_yaml_oauth_token(contents: &str, domain: &str) -> Option<String> {
    let value: YamlValue = serde_yaml::from_str(contents).ok()?;
    value.as_mapping()?.iter().find_map(|(key, value)| {
        if key.as_str().is_some_and(|key| key.starts_with(domain)) {
            value["oauth_token"].as_str().map(ToOwned::to_owned)
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
        r#"{"github.com": {"oauth_token": "token-1"}}"#, "github.com" => Some("token-1".to_string()); "json_matching_domain"
    )]
    #[test_case(
        r#"{"enterprise.example.com": {"oauth_token": "token-1"}}"#, "github.com" => None; "json_other_domain"
    )]
    fn extract_json_oauth_token_by_domain(contents: &str, domain: &str) -> Option<String> {
        extract_json_oauth_token(contents, domain)
    }

    #[test_case(
        "github.com:\n  oauth_token: token-1\n  user: octocat\n", "github.com" => Some("token-1".to_string()); "yaml_matching_domain"
    )]
    #[test_case(
        "enterprise.example.com:\n  oauth_token: token-1\n", "github.com" => None; "yaml_other_domain"
    )]
    fn extract_yaml_oauth_token_by_domain(contents: &str, domain: &str) -> Option<String> {
        extract_yaml_oauth_token(contents, domain)
    }
}
