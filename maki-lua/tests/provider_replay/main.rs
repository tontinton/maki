//! The bundled provider plugins: the slugs they declare, the recorded
//! exchanges each one replays, and the declared data a route onto the slug
//! has to honour too.
//!
//! The registry, the environment and `providers.toml` are all process wide, so
//! every test boots its own host and counts on `cargo nextest` giving each test
//! a process of its own. The golden walker gets the same guarantee by
//! replaying each file in a child process, see [`golden`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use maki_agent::tools::ToolRegistry;
use maki_config::providers::builtin_provider;
use maki_config::{PROVIDER_BUILTINS, PluginsConfig};
use maki_lua::PluginHost;
use maki_providers::plugin;
use maki_providers::{Model, ResolvedAuth, ThinkingSupport, Timeouts};
use tempfile::TempDir;
use test_case::test_case;

mod golden;

const MISTRAL: &str = "mistral";
const DEEPSEEK: &str = "deepseek";
const PUBLISHED_PEAK_HOURS: &str = "2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri";
const MISTRAL_KEY_ENV: &str = "MISTRAL_API_KEY";
const MISTRAL_KEY: &str = "sk-test";
const MINISTRAL: &str = "ministral-14b-latest";
const MEDIUM: &str = "mistral-medium-latest";

const HOME_VARS: &[&str] = &[
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

const HOST_FAILED: &str = "the plugin host did not start";
const LOAD_FAILED: &str = "the bundled provider plugin did not load";
const TEMPDIR_FAILED: &str = "no temporary state directory";
const UNDECLARED_SLUG: &str =
    "a bundled provider plugin did not declare the slug it is named after";
const NOT_LISTED_FOR_LOGIN: &str = "a bundled provider plugin is missing from maki auth login";
const NO_SCHEDULE: &str = "deepseek bills by the clock";
const UNKNOWN_MODEL: &str = "the model spec did not resolve";
const NOT_BUILT: &str = "mistral did not build from its declaration";

/// Points every base directory at a throwaway tree, so neither the real
/// `providers.toml` nor this machine's credentials reach the registration.
fn isolated_state() -> TempDir {
    let dir = TempDir::new().expect(TEMPDIR_FAILED);
    for var in HOME_VARS {
        unsafe { std::env::set_var(var, dir.path()) };
    }
    dir
}

/// One bundled plugin and no others, so what registers the slug is never
/// ambiguous, loaded with the permissions its own `plugin.toml` declares, so
/// what is under test is the plugin exactly as it ships.
///
/// Nothing waves loopback past the SSRF guard: the recorded server listens on
/// the origin `<SLUG>_BASE_URL` names, which is the one a user points at a
/// local gateway. The host comes back instead of being dropped, since a hook
/// whose host has died answers nothing.
fn load_bundled(slug: &str) -> PluginHost {
    let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED);
    plugin::begin_load();
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec![slug.to_owned()],
        packages: Vec::new(),
        opts: HashMap::new(),
    })
    .expect(LOAD_FAILED);
    plugin::commit_load();
    host
}

/// [`PROVIDER_BUILTINS`] is what keeps third parties off a bundled slug, so it
/// only protects a provider if the plugin of that name really declares it.
/// Each one loads alone, so a neighbour can never answer for it.
#[test]
fn every_provider_builtin_declares_its_own_slug() {
    let _state = isolated_state();
    for &slug in PROVIDER_BUILTINS {
        let _host = load_bundled(slug);
        assert!(plugin::is_registered(slug), "{UNDECLARED_SLUG}: {slug}");
        assert!(
            builtin_provider(slug).is_some(),
            "{NOT_LISTED_FOR_LOGIN}: {slug}"
        );
    }
}

/// The hours, days and surcharge exactly as DeepSeek's pricing page lists
/// them. A lost schedule would not fail anything else. Every turn would just
/// quietly bill at the off-peak rate.
#[test]
fn deepseek_bills_the_published_peak_hours() {
    let _state = isolated_state();
    let _host = load_bundled(DEEPSEEK);

    let schedule = plugin::spec(DEEPSEEK)
        .and_then(|spec| spec.pricing_schedule)
        .expect(NO_SCHEDULE);
    assert_eq!(schedule.to_string(), PUBLISHED_PEAK_HOURS);
}

fn mistral_model(model_id: &str) -> Model {
    Model::from_spec(&format!("{MISTRAL}/{model_id}")).expect(UNKNOWN_MODEL)
}

fn declared(model_id: &str) -> Model {
    let mut model = mistral_model(model_id);
    plugin::create(MISTRAL, Timeouts::default())
        .expect(NOT_BUILT)
        .adjust_model(&mut model);
    model
}

/// What Aperture builds for a route onto the slug.
fn routed(model_id: &str) -> Model {
    let mut model = mistral_model(model_id);
    let auth = ResolvedAuth::new(MISTRAL, Vec::new()).expect(NOT_BUILT);
    plugin::build_with_auth(
        MISTRAL,
        Arc::new(Mutex::new(auth)),
        Timeouts::default(),
        None,
    )
    .expect(NOT_BUILT)
    .adjust_model(&mut model);
    model
}

/// Mistral's small models' refusal to reason is declared data the codec
/// applies, so Aperture's route onto the slug, which builds that same codec,
/// has to honour it too.
#[test_case(declared, MINISTRAL, false ; "declared_ministral")]
#[test_case(routed, MINISTRAL, false ; "routed_ministral")]
#[test_case(declared, MEDIUM, true ; "declared_medium")]
#[test_case(routed, MEDIUM, true ; "routed_medium")]
fn only_ministral_is_denied_thinking(adjusted: fn(&str) -> Model, model_id: &str, thinks: bool) {
    let _state = isolated_state();
    unsafe { std::env::set_var(MISTRAL_KEY_ENV, MISTRAL_KEY) };
    let _host = load_bundled(MISTRAL);

    let model = adjusted(model_id);
    assert_eq!(
        model.thinking_override == Some(ThinkingSupport::No),
        !thinks
    );
    assert_eq!(model.supports_thinking(), thinks);
}
