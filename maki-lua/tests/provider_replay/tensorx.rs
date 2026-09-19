//! The bundled `tensorx` plugin against the exchanges the retired bespoke impl
//! recorded.

use maki_providers::replay::{self, Fixture};
use maki_providers::tensorx_fixtures as tensorx;
use test_case::test_case;

use super::{TENSORX, bundled};

#[test_case(&replay::UNAUTHORIZED ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR ; "server_error")]
#[test_case(&replay::MALFORMED_SSE ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM ; "truncated_stream")]
fn the_bundled_tensorx_plugin_replays_the_recorded_exchange(fixture: &Fixture) {
    replay::declared(
        bundled(TENSORX),
        TENSORX,
        fixture,
        &tensorx::model(tensorx::UNLISTED_SPEC),
    );
}

/// Every numeric edge in `/model/info` goes through `maki.provider_parse`, so
/// the rows come out as serde_json reads them.
#[test_case(&tensorx::MODELS ; "models")]
#[test_case(&tensorx::MODELS_WITHOUT_DATA ; "models_without_data")]
#[test_case(&tensorx::MODELS_UNAUTHORIZED ; "models_unauthorized")]
fn the_bundled_tensorx_plugin_lists_the_recorded_catalogue(fixture: &Fixture) {
    replay::declared_models(bundled(TENSORX), TENSORX, fixture);
}

/// The knobs a row's `extra` carries come back to `build_body` as
/// `opts.model_info`, so these bodies pin the whole round trip.
#[test_case(&tensorx::THINKING_PARAM, tensorx::THINKING_PARAM_SPEC ; "thinking_param")]
#[test_case(&tensorx::THINKING_PARAM_OFF, tensorx::THINKING_PARAM_SPEC ; "thinking_param_off")]
#[test_case(&tensorx::REASONING_EFFORT, tensorx::REASONING_EFFORT_SPEC ; "reasoning_effort")]
#[test_case(&tensorx::REASONING_EFFORT_OFF, tensorx::REASONING_EFFORT_SPEC ; "reasoning_effort_off")]
#[test_case(&tensorx::BOTH_KNOBS, tensorx::BOTH_KNOBS_SPEC ; "both_knobs")]
#[test_case(&tensorx::DEEPSEEK_V4, tensorx::DEEPSEEK_V4_SPEC ; "deepseek_v4")]
#[test_case(&tensorx::DEEPSEEK_V4_OFF, tensorx::DEEPSEEK_V4_SPEC ; "deepseek_v4_off")]
#[test_case(&tensorx::DEEPSEEK_REASONER, tensorx::DEEPSEEK_REASONER_SPEC ; "deepseek_reasoner")]
#[test_case(&tensorx::UNDISCOVERED, tensorx::UNLISTED_SPEC ; "undiscovered")]
#[test_case(&tensorx::UNDISCOVERED_DEEPSEEK_V4, tensorx::UNLISTED_DEEPSEEK_V4_SPEC ; "undiscovered_deepseek_v4")]
fn the_bundled_tensorx_plugin_shapes_the_turn_by_what_discovery_found(
    fixture: &Fixture,
    spec: &str,
) {
    replay::declared_discovered(bundled(TENSORX), TENSORX, fixture, &tensorx::model(spec));
}
