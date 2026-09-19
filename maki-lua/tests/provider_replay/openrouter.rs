//! The bundled `openrouter` plugin against the exchanges the retired bespoke
//! impl recorded. The static wire facts are declaration data, so the stream
//! cases pin the codec applying them. The listing is the plugin's own hook,
//! and the discovered turns pin the effort each row carries.

use maki_providers::openrouter_fixtures as openrouter;
use maki_providers::replay::{self, Fixture};
use test_case::test_case;

use super::{OPENROUTER, bundled};

#[test_case(&replay::UNAUTHORIZED ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR ; "server_error")]
#[test_case(&replay::MALFORMED_SSE ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM ; "truncated_stream")]
fn the_bundled_openrouter_plugin_replays_the_recorded_exchange(fixture: &Fixture) {
    replay::declared(
        bundled(OPENROUTER),
        OPENROUTER,
        fixture,
        &openrouter::model(openrouter::UNLISTED_SPEC),
    );
}

#[test_case(&openrouter::MODELS ; "models")]
#[test_case(&openrouter::MODELS_UNAUTHORIZED ; "models_unauthorized")]
fn the_bundled_openrouter_plugin_lists_the_recorded_catalog(fixture: &Fixture) {
    replay::declared_models(bundled(OPENROUTER), OPENROUTER, fixture);
}

/// A row's `effort` hands its raw names to Rust, which drops the unknown ones
/// and narrows the declared dialect, so these bodies pin the whole round trip.
#[test_case(&openrouter::MAX_SNAPS_TO_XHIGH, openrouter::XHIGH_SPEC ; "max_snaps_to_xhigh")]
#[test_case(&openrouter::MAX_SNAPS_TO_MEDIUM, openrouter::MEDIUM_SPEC ; "max_snaps_to_medium")]
#[test_case(&openrouter::MAX_WITH_UNKNOWN_EFFORTS_ONLY, openrouter::UNKNOWN_EFFORTS_SPEC ; "max_with_unknown_efforts_only")]
#[test_case(&openrouter::MAX_WITH_PARAMETER_ONLY_REASONING, openrouter::PARAMETER_ONLY_SPEC ; "max_with_parameter_only_reasoning")]
#[test_case(&openrouter::UNDISCOVERED, openrouter::UNLISTED_SPEC ; "undiscovered")]
#[test_case(&openrouter::OFF_DEFAULT_ENABLED, openrouter::DEFAULT_ENABLED_SPEC ; "off_default_enabled")]
#[test_case(&openrouter::OFF_MANDATORY, openrouter::MANDATORY_SPEC ; "off_mandatory")]
#[test_case(&openrouter::BUDGET, openrouter::XHIGH_SPEC ; "budget")]
#[test_case(&openrouter::NOT_A_REASONING_MODEL, openrouter::NO_REASONING_SPEC ; "not_a_reasoning_model")]
#[test_case(&openrouter::IN_SESSION, openrouter::XHIGH_SPEC ; "in_session")]
fn the_bundled_openrouter_plugin_shapes_the_turn_by_what_discovery_found(
    fixture: &Fixture,
    spec: &str,
) {
    replay::declared_discovered(
        bundled(OPENROUTER),
        OPENROUTER,
        fixture,
        &openrouter::model(spec),
    );
}
