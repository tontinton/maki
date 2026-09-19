//! The bundled provider plugins: which built-in rows they claim, and the
//! recorded exchanges each one replays.
//!
//! The goldens live over in `maki-providers` next to the fixtures. Each was
//! recorded once, against the bespoke impl the plugin replaced, and is never
//! regenerated after it: the files are the spec the plugin answers to.
//!
//! Recorded on loopback, which is what `<SLUG>_BASE_URL` points at here. The
//! plugins declare their real hosts and nothing else, so reaching the recorded
//! server is the same widening a user with a gateway relies on.
//!
//! The registry, the environment and `providers.toml` are all process wide, so
//! every test boots its own host and counts on `cargo nextest` giving each test
//! a process of its own.

use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::{PROVIDER_BUILTINS, PluginsConfig};
use maki_lua::PluginHost;
use maki_providers::plugin;
use maki_providers::replay::{self, Fixture};
use maki_providers::spec::{Build, ProviderRegistry};
use maki_providers::{
    ThinkingConfig, deepseek_fixtures as deepseek, synthetic_fixtures as synthetic,
};
use serde_json::Value;
use tempfile::TempDir;
use test_case::test_case;

mod mistral;
mod openrouter;
mod regolo;
mod requesty;
mod tensorx;

const SYNTHETIC: &str = "synthetic";
const DEEPSEEK: &str = "deepseek";
const MISTRAL: &str = "mistral";
const TENSORX: &str = "tensorx";
const REGOLO: &str = "regolo";
const REQUESTY: &str = "requesty";
const OPENROUTER: &str = "openrouter";

const HOST_FAILED: &str = "the plugin host did not start";
const LOAD_FAILED: &str = "the bundled provider plugin did not load";
const TEMPDIR_FAILED: &str = "no temporary state directory";
const NOT_A_PROVIDER_BUILTIN: &str = "a bundled provider plugin is missing from PROVIDER_BUILTINS";
const UNCLAIMED_ROW: &str = "no bundled plugin claims a declared row, so nothing builds it";

/// Points every base directory at a throwaway tree, so neither the real
/// `providers.toml` nor this machine's credentials reach the registration.
fn isolated_state() -> TempDir {
    let dir = TempDir::new().expect(TEMPDIR_FAILED);
    for var in [
        "HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        unsafe { std::env::set_var(var, dir.path()) };
    }
    dir
}

fn plugin_host() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED)
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
    let mut host = plugin_host();
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec![slug.to_owned()],
        packages: Vec::new(),
        opts: HashMap::new(),
    })
    .expect(LOAD_FAILED);
    host
}

/// The same load, handed to the replay harness so it runs inside the harness's
/// own load window.
fn bundled(slug: &'static str) -> impl FnOnce() -> PluginHost {
    move || load_bundled(slug)
}

/// A declared row has no constructor of its own: the bundled plugin of the
/// same name claiming it is the only thing that builds it. Each plugin loads
/// alone, so the slug is provably its own and not a neighbour's.
#[test]
fn every_declared_row_is_claimed_by_its_bundled_plugin() {
    let _state = isolated_state();
    let declared = ProviderRegistry::builtins()
        .iter()
        .filter(|spec| matches!(spec.build, Build::Declared))
        .map(|spec| spec.slug);
    for slug in declared {
        assert!(
            PROVIDER_BUILTINS.contains(&slug),
            "{NOT_A_PROVIDER_BUILTIN}: {slug}"
        );
        plugin::begin_load();
        let _host = load_bundled(slug);
        plugin::commit_load();
        assert!(plugin::is_registered(slug), "{UNCLAIMED_ROW}: {slug}");
    }
}

#[test_case(&synthetic::SUCCESS ; "success")]
#[test_case(&replay::UNAUTHORIZED ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR ; "server_error")]
#[test_case(&replay::MALFORMED_SSE ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM ; "truncated_stream")]
fn the_bundled_synthetic_plugin_replays_the_recorded_exchange(fixture: &Fixture) {
    replay::declared(bundled(SYNTHETIC), SYNTHETIC, fixture, &synthetic::model());
}

#[test_case(&deepseek::SUCCESS ; "success")]
#[test_case(&replay::UNAUTHORIZED ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR ; "server_error")]
#[test_case(&replay::MALFORMED_SSE ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM ; "truncated_stream")]
#[test_case(&deepseek::THINKING_OFF ; "thinking_off")]
#[test_case(&deepseek::THINKING_ADAPTIVE ; "thinking_adaptive")]
fn the_bundled_deepseek_plugin_replays_the_recorded_exchange(fixture: &Fixture) {
    replay::declared(
        bundled(DEEPSEEK),
        DEEPSEEK,
        fixture,
        &deepseek::model(deepseek::FLASH_SPEC),
    );
}

/// The `reasoning_content` back-fill, which needs a history to act on and a
/// tool list to be allowed to.
#[test_case("padding_with_tools", deepseek::FLASH_SPEC, true ; "a turn with no reasoning gets some")]
#[test_case("padding_without_tools", deepseek::FLASH_SPEC, false ; "no tools, nothing added")]
#[test_case("reasoner_with_tools", deepseek::REASONER_SPEC, true ; "the model that refuses the field")]
fn the_bundled_deepseek_plugin_pads_the_same_turns(
    name: &'static str,
    spec: &str,
    with_tools: bool,
) {
    let fixture = Fixture {
        name,
        script: deepseek::SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(deepseek::EFFORT),
        session: None,
    };
    let tools = if with_tools {
        replay::tools()
    } else {
        Value::Array(Vec::new())
    };
    replay::declared_with(
        bundled(DEEPSEEK),
        DEEPSEEK,
        &fixture,
        &deepseek::model(spec),
        &deepseek::history(),
        &tools,
    );
}

/// The balance endpoint, which is the one hook that leaves the codec's request
/// path: the plugin reaches it through `maki.net`, under its own declared
/// hosts, and the golden says it asked the same server the same question as
/// the bespoke impl did. A refused request is returned through
/// `maki.provider.http_error`, so it fails with that impl's error too.
#[test_case(&deepseek::BALANCE ; "balance")]
#[test_case(&deepseek::USER_BALANCE_UNAUTHORIZED ; "unauthorized")]
fn the_bundled_deepseek_plugin_reads_the_balance_endpoint(fixture: &Fixture) {
    replay::declared_usage(bundled(DEEPSEEK), DEEPSEEK, fixture);
}
