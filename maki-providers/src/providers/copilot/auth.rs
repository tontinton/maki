use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

use crate::AgentError;

const TOKEN_ENV_VARS: &[&str] = &["GH_COPILOT_TOKEN", "COPILOT_GITHUB_TOKEN"];
const COPILOT_DOMAIN: &str = "github.com";

pub(crate) fn load_token() -> Result<String, AgentError> {
    for key in TOKEN_ENV_VARS {
        if let Ok(token) = env::var(key)
            && !token.trim().is_empty()
        {
            return Ok(token);
        }
    }

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
        message: "Copilot token not found. Run `gh auth login --web`, sign in with GitHub Copilot, or set GH_COPILOT_TOKEN.".into(),
    })
}

pub fn login() -> Result<(), AgentError> {
    let _ = load_token()?;
    println!("Copilot token found.");
    Ok(())
}

pub fn logout() -> Result<(), AgentError> {
    Err(AgentError::Config {
        message: "Copilot auth is managed by GitHub Copilot. Sign out from your Copilot client or remove GH_COPILOT_TOKEN.".into(),
    })
}

fn copilot_config_paths() -> Vec<PathBuf> {
    let base = config_dir().map(|config| config.join("github-copilot"));
    base.map(|base| vec![base.join("hosts.json"), base.join("apps.json")])
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
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
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

    #[test]
    fn extracts_matching_oauth_token() {
        let contents = r#"{
            "github.com": {
                "oauth_token": "token-1"
            }
        }"#;
        assert_eq!(
            extract_json_oauth_token(contents, "github.com").as_deref(),
            Some("token-1")
        );
    }

    #[test]
    fn ignores_other_domains() {
        let contents = r#"{
            "enterprise.example.com": {
                "oauth_token": "token-1"
            }
        }"#;
        assert_eq!(extract_json_oauth_token(contents, "github.com"), None);
    }

    #[test]
    fn extracts_matching_gh_oauth_token() {
        let contents = r#"
github.com:
  oauth_token: token-1
  user: octocat
"#;
        assert_eq!(
            extract_yaml_oauth_token(contents, "github.com").as_deref(),
            Some("token-1")
        );
    }

    #[test]
    fn ignores_other_gh_domains() {
        let contents = r#"
enterprise.example.com:
  oauth_token: token-1
"#;
        assert_eq!(extract_yaml_oauth_token(contents, "github.com"), None);
    }
}
