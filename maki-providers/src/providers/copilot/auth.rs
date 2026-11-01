use std::env;
use std::fs;
use std::iter;
use std::path::PathBuf;

use base64::Engine;
use indexmap::{IndexMap, IndexSet};
use keyring::Entry;
use maki_storage::StateDir;
use maki_storage::auth::{
    ProviderCredentials, delete_provider_credentials, load_provider_credentials,
    save_provider_credentials,
};
use serde::Deserialize;
use serde::de::IgnoredAny;
use tracing::{debug, warn};

use crate::AgentError;

pub(crate) const TOKEN_ENV_VARS: &[&str] = &["GH_COPILOT_TOKEN", "COPILOT_GITHUB_TOKEN"];
const DEFAULT_HOST: &str = "github.com";
const PROVIDER: &str = "copilot";
const KEYRING_SERVICE_PREFIX: &str = "gh:";
const GO_KEYRING_B64_PREFIX: &str = "go-keyring-base64:";
const KEYRING_LOCKED_MESSAGE: &str = "is unreadable; is the store locked?";

/// Hosts in gh's `hosts.yml` or the Copilot clients' `hosts.json` and `apps.json`, in file order.
type HostsConfig = IndexMap<String, HostConfig>;

#[derive(Default, Deserialize)]
#[serde(default)]
struct HostConfig {
    user: Option<String>,
    users: IndexMap<String, IgnoredAny>,
    oauth_token: Option<String>,
}

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

fn discover_token(hints: &mut Vec<String>) -> Result<ProviderCredentials, AgentError> {
    let configs = read_configs();
    let (token, host) = configs
        .iter()
        .find_map(github_oauth_token)
        .or_else(|| discover_keyring_token(&configs, hints))
        .ok_or_else(|| AgentError::Config {
            message: "Copilot token not found. Run `gh auth login --web`, sign in with the \
                Copilot client, or set GH_COPILOT_TOKEN."
                .into(),
        })?;
    Ok(ProviderCredentials {
        api_key: token,
        host: (host != DEFAULT_HOST).then_some(host),
    })
}

fn discover_keyring_token(
    configs: &[HostsConfig],
    hints: &mut Vec<String>,
) -> Option<(String, String)> {
    for host in keyring_hosts(configs) {
        for account in keyring_accounts(configs, host) {
            let Ok(entry) = Entry::new(&format!("{KEYRING_SERVICE_PREFIX}{host}"), account) else {
                debug!(host, account, "gh keyring account rejected");
                continue;
            };
            match entry.get_password() {
                Ok(raw) => return Some((decode_go_keyring(&raw), host.to_owned())),
                Err(keyring::Error::NoEntry) => {}
                Err(err) => {
                    warn!(
                        error = %err, host, account,
                        "gh keyring entry {}", KEYRING_LOCKED_MESSAGE
                    );
                    hints.push(locked_keyring_hint(host, account, err));
                }
            }
        }
    }
    None
}

fn locked_keyring_hint(host: &str, account: &str, error: impl std::fmt::Display) -> String {
    format!("gh keyring entry for {host}/{account} {KEYRING_LOCKED_MESSAGE} ({error})")
}

fn keyring_accounts<'a>(configs: &'a [HostsConfig], host: &str) -> IndexSet<&'a str> {
    configs
        .iter()
        .filter_map(|config| config.get(host))
        .flat_map(|config| config.user.iter().chain(config.users.keys()))
        .map(String::as_str)
        .chain([""])
        .collect()
}

fn keyring_hosts(configs: &[HostsConfig]) -> IndexSet<&str> {
    let config_hosts = configs.iter().flat_map(IndexMap::keys).map(String::as_str);
    iter::once(DEFAULT_HOST)
        .chain(config_hosts.filter(|host| is_github_host(host)))
        .collect()
}

fn read_configs() -> Vec<HostsConfig> {
    let copilot = copilot_config_paths()
        .into_iter()
        .filter_map(|path| serde_json::from_str(&fs::read_to_string(path).ok()?).ok());
    let gh = gh_config_paths()
        .into_iter()
        .filter_map(|path| serde_saphyr::from_str(&fs::read_to_string(path).ok()?).ok());
    copilot.chain(gh).collect()
}

/// Reverses the encoding written by the `go-keyring` library (used by the gh CLI,
/// which is written in Go): it base64-encodes the token bytes and prefixes them
/// with `go-keyring-base64:`. Plain (un-prefixed) tokens are returned as-is.
fn decode_go_keyring(raw: &str) -> String {
    raw.strip_prefix(GO_KEYRING_B64_PREFIX)
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| raw.to_owned())
}

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    if load_token().is_ok() {
        println!("Already authenticated with Copilot.");
        return Ok(());
    }

    let mut hints = Vec::new();
    let creds = match discover_token(&mut hints) {
        Ok(creds) => creds,
        Err(err) => {
            for hint in hints {
                eprintln!("{hint}");
            }
            return Err(err);
        }
    };
    let host = creds.host.as_deref().unwrap_or(DEFAULT_HOST);
    println!("Copilot token imported from gh CLI / Copilot client / system keyring ({host}).");
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

fn github_oauth_token(config: &HostsConfig) -> Option<(String, String)> {
    config
        .iter()
        .filter(|(host, _)| is_github_host(host))
        .find_map(|(host, config)| Some((config.oauth_token.clone()?, host.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn locked_keyring_hint_includes_host_account_and_error() {
        let hint = locked_keyring_hint("github.com", "janedoe", "access denied");
        assert!(hint.contains("github.com"));
        assert!(hint.contains("janedoe"));
        assert!(hint.contains(KEYRING_LOCKED_MESSAGE));
        assert!(hint.ends_with("(access denied)"));
    }

    fn json(contents: &str) -> HostsConfig {
        serde_json::from_str(contents).unwrap()
    }

    fn yaml(contents: &str) -> HostsConfig {
        serde_saphyr::from_str(contents).unwrap()
    }

    #[test_case(
        json, r#"{"github.com": {"oauth_token": "token-1"}}"# => Some(("token-1".to_string(), "github.com".to_string())); "json_matching_domain"
    )]
    #[test_case(
        json, r#"{"enterprise.example.com": {"oauth_token": "token-1"}}"# => None; "json_other_domain"
    )]
    #[test_case(
        json, r#"{"myco.ghe.com": {"oauth_token": "ghe-tok"}}"# => Some(("ghe-tok".to_string(), "myco.ghe.com".to_string())); "json_ghe_host"
    )]
    #[test_case(
        yaml, "github.com:\n  oauth_token: token-1\n  user: octocat\n" => Some(("token-1".to_string(), "github.com".to_string())); "yaml_matching_domain"
    )]
    #[test_case(
        yaml, "enterprise.example.com:\n  oauth_token: token-1\n" => None; "yaml_other_domain"
    )]
    #[test_case(
        yaml, "myco.ghe.com:\n  oauth_token: ghe-tok\n" => Some(("ghe-tok".to_string(), "myco.ghe.com".to_string())); "yaml_ghe_host"
    )]
    fn test_github_oauth_token(
        parse: fn(&str) -> HostsConfig,
        contents: &str,
    ) -> Option<(String, String)> {
        github_oauth_token(&parse(contents))
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

    #[test_case("go-keyring-base64:Z2hvX3Rva2Vu" => "gho_token".to_owned(); "go_keyring_prefixed")]
    #[test_case("gho_token" => "gho_token".to_owned(); "plain_token")]
    #[test_case("go-keyring-base64:!!!not-base64" => "go-keyring-base64:!!!not-base64".to_owned(); "invalid_base64_kept_raw")]
    fn test_decode_go_keyring(raw: &str) -> String {
        decode_go_keyring(raw)
    }

    #[test_case(
        json, r#"{"github.com": {"oauth_token": "t"}}"# => vec!["github.com"]; "json_hosts"
    )]
    #[test_case(
        yaml, "github.com:\n  oauth_token: t\nmyco.ghe.com:\n  oauth_token: t\n" =>
        vec!["github.com", "myco.ghe.com"]; "yaml_hosts"
    )]
    #[test_case(
        yaml, "b.ghe.com:\n  oauth_token: t\na.ghe.com:\n  oauth_token: t\ngitlab.com:\n  user: x\n" =>
        vec!["github.com", "b.ghe.com", "a.ghe.com"]; "default_first_then_github_hosts_in_file_order"
    )]
    fn test_keyring_hosts(parse: fn(&str) -> HostsConfig, contents: &str) -> Vec<String> {
        keyring_hosts(&[parse(contents)])
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test_case(
        json, r#"{"github.com": {"oauth_token": "t"}}"#, "github.com" =>
        vec![""]; "host_without_username_gets_empty_account"
    )]
    #[test_case(
        json, r#"{"github.com": {"user": "janedoe", "oauth_token": "t"}}"#, "github.com" =>
        vec!["janedoe", ""]; "json_user"
    )]
    #[test_case(
        yaml, "github.com:\n  user: janedoe\n  users:\n    janedoe:\n    janesmith_second:\n",
        "github.com" =>
        vec!["janedoe", "janesmith_second", ""]; "yaml_user_and_users"
    )]
    #[test_case(
        json, r#"{"github.com": {"user": "janedoe", "users": {"janedoe": {}, "janesmith": {}}}}"#,
        "github.com" =>
        vec!["janedoe", "janesmith", ""]; "json_user_and_users"
    )]
    #[test_case(
        yaml, "github.com:\n  user: janedoe\n", "myco.ghe.com" =>
        vec![""]; "missing_host"
    )]
    fn test_keyring_accounts(
        parse: fn(&str) -> HostsConfig,
        contents: &str,
        host: &str,
    ) -> Vec<String> {
        keyring_accounts(&[parse(contents)], host)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}
