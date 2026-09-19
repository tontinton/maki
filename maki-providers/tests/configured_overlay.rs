//! What a `providers.toml` entry means for a slug a declaration serves.
//!
//! Out of line rather than beside [`maki_providers::plugin`]'s own tests
//! because the config is read from the process-wide home directory: each case
//! needs its own, and `cargo nextest` is what gives a test its own process.

use maki_config::providers::Protocol;
use maki_providers::Timeouts;
use maki_providers::plugin::{self, DeclAuthority, ProviderDecl, RegisterError, Registration};
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

/// A built-in slug that has moved onto a declaration, the host of the origin
/// it inherits, and the gateway a user points it at.
const CLAIMED_SLUG: &str = "deepseek";
const CLAIMED_HOST: &str = "api.deepseek.com";
const GATEWAY_URL: &str = "https://gateway.example/v1";

const GATEWAY_HEADER: &str = "X-Gateway-Key";
const UNSET_VAR: &str = "MAKI_TEST_UNSET_GATEWAY_KEY";

const OWN_SLUG: &str = "configured-provider";
const OWN_DEFINITION: &str = r#"protocol = "openai"
base_url = "https://configured.example/v1""#;
const OWN_LONE_KEY: &str = "discover_models = true";
const OWN_HOST: &str = "declared.example";
const OWN_DISPLAY_NAME: &str = "Configured";

const TEMPDIR_FAILED: &str = "no temporary state directory";
const CONFIG_DIR_FAILED: &str = "the isolated config directory did not resolve";
const WRITE_FAILED: &str = "providers.toml could not be written";
const PROVIDER_LOST: &str = "a providers.toml overlay took the slug off its declaration";
const BUILT_DESPITE_BAD_HEADER: &str = "a bad [<slug>.headers] must fail the build";

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

fn declaration(slug: &str, display_name: Option<&str>, host: &str) -> Registration {
    Registration {
        decl: ProviderDecl {
            slug: slug.to_owned(),
            display_name: display_name.map(str::to_owned),
            codec: Some(Protocol::Openai),
            base: None,
            base_url: None,
            api_key_env: None,
            system_prefix: None,
            models: Vec::new(),
            openai: None,
            net_hosts: vec![host.to_owned()],
        },
        hooks: plugin::ProviderHooks::default(),
    }
}

/// The bundled plugin's claim on [`CLAIMED_SLUG`], as a startup load stages it.
fn load_claim() {
    plugin::begin_load();
    plugin::register(
        declaration(CLAIMED_SLUG, None, CLAIMED_HOST),
        DeclAuthority::Bundled,
    )
    .expect(PROVIDER_LOST);
    plugin::commit_load();
}

/// The documented overlay: `[<builtin>] base_url` points the shipped provider
/// at a gateway. The built-in owns the slug, so the entry is not a second
/// provider competing for it and must not cost the declaration its slug, which
/// would leave the user with a models.dev stub in place of the provider the
/// overlay was written for.
#[test]
fn an_overlay_on_a_built_in_slug_keeps_the_declaration() {
    let _home = isolated(&format!("[{CLAIMED_SLUG}]\nbase_url = \"{GATEWAY_URL}\"\n"));
    load_claim();

    assert!(
        matches!(Owner::of(CLAIMED_SLUG), Owner::Plugin),
        "{PROVIDER_LOST}"
    );
    assert_eq!(
        plugin::effective_base_url(CLAIMED_SLUG).as_deref(),
        Some(GATEWAY_URL)
    );
}

/// A bad `[<builtin>.headers]` fails that provider when it is built, the way a
/// native constructor did, and not the bundled load: that would keep maki from
/// starting over a provider the user may never pick.
#[test]
fn a_bad_header_on_a_built_in_slug_fails_only_its_build() {
    let _home = isolated(&format!(
        "[{CLAIMED_SLUG}.headers]\n\"{GATEWAY_HEADER}\" = \"${{{UNSET_VAR}}}\"\n"
    ));
    unsafe { std::env::remove_var(UNSET_VAR) };
    load_claim();

    assert!(
        matches!(Owner::of(CLAIMED_SLUG), Owner::Plugin),
        "{PROVIDER_LOST}"
    );
    let error = plugin::create(CLAIMED_SLUG, Timeouts::default())
        .err()
        .expect(BUILT_DESPITE_BAD_HEADER)
        .to_string();
    assert!(
        error.contains(GATEWAY_HEADER) && error.contains(UNSET_VAR),
        "{error}"
    );
}

/// A slug maki has no row for is a different question: nothing is inherited,
/// the entry is the whole provider, and a declaration claiming it would be a
/// second definition of the same name. However little the entry says.
#[test_case(OWN_DEFINITION ; "a_whole_definition")]
#[test_case(OWN_LONE_KEY ; "a_lone_key")]
fn a_slug_providers_toml_defines_is_still_refused(entry: &str) {
    let _home = isolated(&format!("[{OWN_SLUG}]\n{entry}\n"));
    plugin::begin_load();
    let error = plugin::register(
        declaration(OWN_SLUG, Some(OWN_DISPLAY_NAME), OWN_HOST),
        DeclAuthority::ThirdParty,
    )
    .unwrap_err();
    plugin::commit_load();

    assert!(matches!(error, RegisterError::ConfiguredSlug(_)), "{error}");
}
