use std::process::Command;

use maki_config::providers::{ProvidersConfig, resolve_api_key_env};
use maki_config::{PROVIDER_BUILTINS, env_var_refs};

use crate::providers::anthropic::bedrock;
use crate::providers::catalog;
use crate::providers::copilot::auth as copilot_auth;
use crate::spec::ProviderRegistry;

/// Credentials other CLIs read too (gh, huggingface-cli, doctl, databricks,
/// wrangler, snowsql, wandb, vultr-cli). A models.dev entry or a header can
/// name one, and stripping it would break those tools in the bash tool with no
/// way for the user to put it back.
const SHARED_CREDENTIAL_VARS: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "HF_TOKEN",
    "DIGITALOCEAN_ACCESS_TOKEN",
    "DATABRICKS_HOST",
    "DATABRICKS_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_API_KEY",
    "SNOWFLAKE_ACCOUNT",
    "WANDB_API_KEY",
    "VULTR_API_KEY",
];

/// A bash command could print a provider key into the model's context, and an
/// MCP server is someone else's code, so neither gets the keys maki reads. Env
/// set on `cmd` after this still goes through, which is how an MCP server's
/// `environment` hands over a key on purpose.
///
/// The list is built on every call, so an edit to `providers.toml` or a
/// catalog that warmed up late counts from the next spawn.
pub fn strip_provider_keys(cmd: &mut Command) -> &mut Command {
    let config = ProvidersConfig::load_or_default();
    for var in provider_key_vars(&config, catalog::key_vars_if_available()) {
        cmd.env_remove(var);
    }
    cmd
}

/// `providers.toml` cannot change the `api_key_env` of a known slug, so that
/// var is never a key maki reads. Its `headers` are still sent though, so
/// their `${VAR}`s count.
fn provider_key_vars(config: &ProvidersConfig, catalog_vars: Vec<String>) -> Vec<String> {
    let known = ProviderRegistry::all()
        .into_iter()
        .map(|spec| spec.api_key_env)
        .chain(copilot_auth::TOKEN_ENV_VARS.iter().copied())
        .chain([bedrock::BEARER_TOKEN_ENV])
        .filter(|var| !var.is_empty())
        .map(str::to_owned);
    // The key stays the user's secret even while its plugin is off or still
    // loading.
    let bundled = PROVIDER_BUILTINS
        .iter()
        .map(|slug| resolve_api_key_env(slug, None));
    let custom = config
        .providers
        .iter()
        .filter(|(slug, _)| ProviderRegistry::get(slug).is_none())
        .map(|(slug, def)| resolve_api_key_env(slug, Some(def)));
    let header_refs = config
        .providers
        .values()
        .flat_map(|def| def.headers.values())
        .flat_map(|value| env_var_refs(value))
        .map(str::to_owned);
    known
        .chain(bundled)
        .chain(custom)
        .chain(header_refs)
        .chain(catalog_vars)
        .filter(|var| !SHARED_CREDENTIAL_VARS.contains(&var.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use maki_config::providers::ProviderDef;
    use test_case::test_case;

    use super::*;
    use crate::providers::anthropic;

    const CUSTOM_SLUG: &str = "my-proxy";
    const CUSTOM_KEY_ENV: &str = "MY_PROXY_SECRET";
    const DEFAULT_SLUG: &str = "other-proxy";
    const DEFAULT_KEY_ENV: &str = "OTHER_PROXY_API_KEY";
    const IGNORED_KEY_ENV: &str = "MAKI_TEST_IGNORED_KEY";
    const GATEWAY_HEADER: &str = "CF-Access-Client-Secret";
    const GATEWAY_SECRET_ENV: &str = "CF_ACCESS_CLIENT_SECRET";
    const SHARED_HEADER: &str = "X-GitHub-Token";
    const SHARED_TOKEN: &str = "GITHUB_TOKEN";
    const CATALOG_KEY_ENV: &str = "FIREWORKS_API_KEY";
    const SHARED_CATALOG_KEY_ENV: &str = "HF_TOKEN";
    const UNRELATED_VAR: &str = "MAKI_TEST_UNRELATED";
    /// Its plugin never loads in this crate's tests.
    const BUNDLED_KEY_ENV: &str = "MISTRAL_API_KEY";
    #[cfg(unix)]
    const SECRET: &str = "sk-secret";

    fn config() -> ProvidersConfig {
        let with_env = |env: &str| ProviderDef {
            api_key_env: Some(env.into()),
            ..Default::default()
        };
        let builtin_with_headers = ProviderDef {
            headers: BTreeMap::from([
                (GATEWAY_HEADER.into(), format!("${{{GATEWAY_SECRET_ENV}}}")),
                (SHARED_HEADER.into(), format!("${{{SHARED_TOKEN}}}")),
            ]),
            ..with_env(IGNORED_KEY_ENV)
        };
        ProvidersConfig {
            providers: HashMap::from([
                (CUSTOM_SLUG.into(), with_env(CUSTOM_KEY_ENV)),
                (DEFAULT_SLUG.into(), ProviderDef::default()),
                (anthropic::SPEC.slug.into(), builtin_with_headers),
            ]),
        }
    }

    #[test_case(anthropic::SPEC.api_key_env, true ; "builtin_key")]
    #[test_case(copilot_auth::TOKEN_ENV_VARS[1], true ; "copilot_fallback_token")]
    #[test_case(bedrock::BEARER_TOKEN_ENV, true ; "bedrock_bearer_token")]
    #[test_case(CUSTOM_KEY_ENV, true ; "custom_api_key_env")]
    #[test_case(DEFAULT_KEY_ENV, true ; "custom_default_slug_key")]
    #[test_case(IGNORED_KEY_ENV, false ; "builtin_api_key_env_override_ignored")]
    #[test_case(GATEWAY_SECRET_ENV, true ; "header_ref")]
    #[test_case(SHARED_TOKEN, false ; "shared_credential_in_header_kept")]
    #[test_case(CATALOG_KEY_ENV, true ; "catalog_key")]
    #[test_case(SHARED_CATALOG_KEY_ENV, false ; "shared_credential_in_catalog_kept")]
    #[test_case(UNRELATED_VAR, false ; "unrelated_var_kept")]
    #[test_case(BUNDLED_KEY_ENV, true ; "unloaded_bundled_plugin_key")]
    fn provider_key_vars_membership(var: &str, stripped: bool) {
        let catalog_vars = vec![CATALOG_KEY_ENV.into(), SHARED_CATALOG_KEY_ENV.into()];
        assert_eq!(
            provider_key_vars(&config(), catalog_vars)
                .iter()
                .any(|v| v == var),
            stripped
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_sees_only_keys_set_after_strip() {
        let inherited = anthropic::SPEC.api_key_env;
        let explicit = bedrock::BEARER_TOKEN_ENV;
        let mut cmd = Command::new("env");
        cmd.env(inherited, SECRET);
        let output = strip_provider_keys(&mut cmd)
            .env(explicit, SECRET)
            .output()
            .unwrap();
        let env = String::from_utf8(output.stdout).unwrap();
        assert!(!env.contains(&format!("{inherited}=")));
        assert!(env.contains(&format!("{explicit}={SECRET}")));
    }
}
