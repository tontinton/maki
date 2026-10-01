//! What a `providers.toml` entry means for a slug a declaration serves.
//!
//! Out of line rather than beside [`maki_providers::plugin`]'s own tests
//! because the config is read from the process-wide home directory: each case
//! needs its own, and `cargo nextest` is what gives a test its own process.

use maki_config::providers::Protocol;
use maki_providers::Timeouts;
use maki_providers::plugin::{
    self, DeclAuthority, PlanDecl, ProviderDecl, RegisterError, Registration,
};
use maki_providers::spec::Owner;
use tempfile::TempDir;
use test_case::test_case;

const HOME_VARS: &[&str] = &[
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

const PROVIDERS_FILE: &str = "providers.toml";

const DECLARED_SLUG: &str = "overlaid-provider";
const DECLARED_HOST: &str = "api.overlaid.example";
const DECLARED_URL: &str = "https://api.overlaid.example/v1";
const DECLARED_DISPLAY_NAME: &str = "Overlaid";
const GATEWAY_URL: &str = "https://gateway.example/v1";
const LONE_KEY: &str = "discover_models = true";

const GATEWAY_HEADER: &str = "X-Gateway-Key";
const UNSET_VAR: &str = "MAKI_TEST_UNSET_GATEWAY_KEY";

const OWN_SLUG: &str = "configured-provider";
const CONFIGURED_URL: &str = "https://configured.example/v1";
const OWN_HOST: &str = "declared.example";
const OWN_DISPLAY_NAME: &str = "Configured";

const TEMPDIR_FAILED: &str = "no temporary state directory";
const CONFIG_DIR_FAILED: &str = "the isolated config directory did not resolve";
const WRITE_FAILED: &str = "providers.toml could not be written";
const PROVIDER_LOST: &str = "a providers.toml overlay took the slug off its declaration";
const BUILT_DESPITE_BAD_HEADER: &str = "a bad [<slug>.headers] must fail the build";

const PLAN_KEY: &str = "coding";
const PLAN_HOST: &str = "plan.overlaid.example";
const PLAN_KEY_ENV: &str = "MAKI_TEST_PLANNED_PROVIDER_KEY";

/// Points every base directory at a throwaway tree holding `providers.toml`,
/// so no case reads this machine's config. The file goes wherever maki's own
/// path resolution says it lives, so a test can never quietly write somewhere
/// nothing reads.
fn isolated(providers_toml: &str) -> TempDir {
    let dir = TempDir::new().expect(TEMPDIR_FAILED);
    for var in HOME_VARS {
        unsafe { std::env::set_var(var, dir.path()) };
    }
    let config = maki_storage::paths::config_dir().expect(CONFIG_DIR_FAILED);
    std::fs::write(config.join(PROVIDERS_FILE), providers_toml).expect(WRITE_FAILED);
    dir
}

fn own_definition() -> String {
    format!("protocol = \"openai\"\nbase_url = \"{CONFIGURED_URL}\"")
}

fn declaration(slug: &str, display_name: &str, host: &str) -> Registration {
    Registration {
        decl: ProviderDecl {
            slug: slug.to_owned(),
            display_name: display_name.to_owned(),
            codec: Some(Protocol::Openai),
            base_url: Some(format!("https://{host}/v1")),
            net_hosts: vec![host.to_owned()],
            ..ProviderDecl::default()
        },
        hooks: plugin::ProviderHooks::default(),
    }
}

fn load_bundled() {
    plugin::begin_load();
    plugin::register(
        declaration(DECLARED_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST),
        DeclAuthority::Bundled,
    )
    .expect(PROVIDER_LOST);
    plugin::commit_load();
}

/// Users write `[deepseek] base_url = ...` to send a bundled provider through
/// their gateway. If that entry cost the plugin its slug, they would get a
/// bare models.dev stub instead of the provider they meant to tweak. So the
/// declaration stays, whatever the entry says. Even a `protocol` only earns a
/// warning, because refusing the bundled load would stop maki from starting.
#[test_case(&format!("base_url = \"{GATEWAY_URL}\""), GATEWAY_URL ; "a_gateway")]
#[test_case(LONE_KEY, DECLARED_URL ; "a_lone_key")]
#[test_case(&own_definition(), CONFIGURED_URL ; "a_whole_definition")]
fn an_entry_keeps_the_bundled_declaration(entry: &str, origin: &str) {
    let _home = isolated(&format!("[{DECLARED_SLUG}]\n{entry}\n"));
    load_bundled();

    assert!(
        matches!(Owner::of(DECLARED_SLUG), Owner::Plugin),
        "{PROVIDER_LOST}"
    );
    assert_eq!(
        plugin::effective_base_url(DECLARED_SLUG).as_deref(),
        Some(origin)
    );
}

/// A bad `[<slug>.headers]` only fails the provider when it is built. Failing
/// the bundled load instead would stop maki from starting, over a provider the
/// user may never pick.
#[test]
fn a_bad_header_on_a_bundled_slug_fails_only_its_build() {
    let _home = isolated(&format!(
        "[{DECLARED_SLUG}.headers]\n\"{GATEWAY_HEADER}\" = \"${{{UNSET_VAR}}}\"\n"
    ));
    unsafe { std::env::remove_var(UNSET_VAR) };
    load_bundled();

    assert!(
        matches!(Owner::of(DECLARED_SLUG), Owner::Plugin),
        "{PROVIDER_LOST}"
    );
    let error = plugin::create(DECLARED_SLUG, Timeouts::default())
        .err()
        .expect(BUILT_DESPITE_BAD_HEADER)
        .to_string();
    assert!(
        error.contains(GATEWAY_HEADER) && error.contains(UNSET_VAR),
        "{error}"
    );
}

/// With `plan = "..."` the user picks, but the plugin wrote every origin on
/// the menu. So the origin is only trusted when maki itself wrote it.
#[test_case(DeclAuthority::ThirdParty, false ; "third_party")]
#[test_case(DeclAuthority::Bundled, true ; "bundled")]
fn a_planned_origin_is_vouched_only_for_a_bundled_declaration(
    authority: DeclAuthority,
    vouched: bool,
) {
    let _home = isolated(&format!("[{DECLARED_SLUG}]\nplan = \"{PLAN_KEY}\"\n"));
    let plan_url = format!("https://{PLAN_HOST}/v1");
    let mut reg = declaration(DECLARED_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST);
    reg.decl.api_key_env = Some(PLAN_KEY_ENV.to_owned());
    reg.decl.net_hosts.push(PLAN_HOST.to_owned());
    reg.decl.plans = vec![PlanDecl {
        key: PLAN_KEY.to_owned(),
        display_name: DECLARED_DISPLAY_NAME.to_owned(),
        base_url: Some(plan_url.clone()),
        default_model: None,
        login_url: None,
    }];
    plugin::begin_load();
    plugin::register(reg, authority).expect(PROVIDER_LOST);
    plugin::commit_load();

    assert_eq!(
        plugin::effective_base_url(DECLARED_SLUG).as_deref(),
        Some(plan_url.as_str())
    );
    assert_eq!(plugin::vouched_origin(DECLARED_SLUG).is_some(), vouched);
}

/// An entry with a `protocol` is a whole provider already. A third-party
/// plugin taking the same slug would be a second provider with the same name.
#[test]
fn a_slug_providers_toml_defines_is_still_refused() {
    let _home = isolated(&format!("[{OWN_SLUG}]\n{}\n", own_definition()));
    plugin::begin_load();
    let error = plugin::register(
        declaration(OWN_SLUG, OWN_DISPLAY_NAME, OWN_HOST),
        DeclAuthority::ThirdParty,
    )
    .unwrap_err();
    plugin::commit_load();

    assert!(matches!(error, RegisterError::ConfiguredSlug(_)), "{error}");
}
