//! The bundled `mistral` plugin against the exchanges the retired bespoke impl
//! recorded.

use std::sync::{Arc, Mutex};

use maki_providers::mistral_fixtures as mistral;
use maki_providers::plugin;
use maki_providers::replay::{self, Fixture};
use maki_providers::{Model, ResolvedAuth, ThinkingSupport, Timeouts};
use test_case::test_case;

use super::{MISTRAL, bundled, isolated_state, load_bundled};

const ENV_VAR: &str = "MISTRAL_API_KEY";
const API_KEY: &str = "sk-test";
const MINISTRAL: &str = "ministral-14b-latest";
const MEDIUM: &str = "mistral-medium-latest";
const UNKNOWN_MODEL: &str = "the model spec did not resolve";
const NOT_BUILT: &str = "mistral did not build from its declaration";

#[test_case(&mistral::SUCCESS ; "success")]
#[test_case(&mistral::THINKING_OFF ; "thinking_off")]
#[test_case(&mistral::IN_SESSION ; "in_session")]
#[test_case(&replay::UNAUTHORIZED ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR ; "server_error")]
#[test_case(&replay::MALFORMED_SSE ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM ; "truncated_stream")]
fn the_bundled_mistral_plugin_replays_the_recorded_exchange(fixture: &Fixture) {
    replay::declared(bundled(MISTRAL), MISTRAL, fixture, &mistral::model());
}

/// The assistant-turn rewrite, which needs a history to act on.
#[test]
fn the_bundled_mistral_plugin_rewrites_the_same_turns() {
    replay::declared_with(
        bundled(MISTRAL),
        MISTRAL,
        &mistral::HISTORY,
        &mistral::model(),
        &mistral::history(),
        &replay::tools(),
    );
}

/// The catalogue goes through `maki.provider_parse`, and a refused listing
/// fails with the bespoke impl's error.
#[test_case(&mistral::MODELS ; "models")]
#[test_case(&mistral::MODELS_UNAUTHORIZED ; "models_unauthorized")]
fn the_bundled_mistral_plugin_lists_the_recorded_models(fixture: &Fixture) {
    replay::declared_models(bundled(MISTRAL), MISTRAL, fixture);
}

fn model(model_id: &str) -> Model {
    Model::from_spec(&format!("{MISTRAL}/{model_id}")).expect(UNKNOWN_MODEL)
}

fn declared(model_id: &str) -> Model {
    let mut model = model(model_id);
    plugin::create(MISTRAL, Timeouts::default())
        .expect(NOT_BUILT)
        .adjust_model(&mut model);
    model
}

/// What Aperture builds for a route onto the slug.
fn routed(model_id: &str) -> Model {
    let mut model = model(model_id);
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

/// The small models' refusal to reason is declared data the codec applies,
/// so Aperture's route onto the slug, which builds that same codec, has to
/// honour it too.
#[test_case(declared, MINISTRAL, false ; "declared_ministral")]
#[test_case(routed, MINISTRAL, false ; "routed_ministral")]
#[test_case(declared, MEDIUM, true ; "declared_medium")]
#[test_case(routed, MEDIUM, true ; "routed_medium")]
fn only_ministral_is_denied_thinking(adjusted: fn(&str) -> Model, model_id: &str, thinks: bool) {
    let _state = isolated_state();
    unsafe { std::env::set_var(ENV_VAR, API_KEY) };
    plugin::begin_load();
    let _host = load_bundled(MISTRAL);
    plugin::commit_load();

    let model = adjusted(model_id);
    assert_eq!(
        model.thinking_override == Some(ThinkingSupport::No),
        !thinks
    );
    assert_eq!(model.supports_thinking(), thinks);
}
