//! The bundled `regolo` plugin against the exchanges the retired bespoke impl
//! recorded.

use maki_providers::regolo_fixtures as regolo;
use maki_providers::replay::{self, Fixture};
use test_case::test_case;

use super::{REGOLO, bundled};

#[test_case(&regolo::SUCCESS ; "success")]
#[test_case(&regolo::THINKING_OFF ; "thinking_off")]
#[test_case(&replay::UNAUTHORIZED ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR ; "server_error")]
#[test_case(&replay::MALFORMED_SSE ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM ; "truncated_stream")]
fn the_bundled_regolo_plugin_replays_the_recorded_exchange(fixture: &Fixture) {
    replay::declared(bundled(REGOLO), REGOLO, fixture, &regolo::model());
}

/// The golden pins which listings were asked as well as the rows.
#[test_case(&regolo::MODELS ; "models")]
#[test_case(&regolo::MODELS_WITHOUT_GROUPS ; "models_without_groups")]
#[test_case(&regolo::MODELS_MALFORMED_GROUPS ; "models_malformed_groups")]
#[test_case(&regolo::MODELS_UNAUTHORIZED ; "models_unauthorized")]
fn the_bundled_regolo_plugin_lists_the_recorded_catalogue(fixture: &Fixture) {
    replay::declared_models(bundled(REGOLO), REGOLO, fixture);
}

/// The side calls go out through `maki.async.gather`, and the dates in their
/// paths and the daily reset come off the plugin's own clock.
#[test_case(&regolo::USAGE ; "usage")]
#[test_case(&regolo::USAGE_NO_BUDGET ; "usage_no_budget")]
#[test_case(&regolo::USAGE_OVER_BUDGET ; "usage_over_budget")]
#[test_case(&regolo::USAGE_SIDE_CALLS_FAIL ; "usage_side_calls_fail")]
#[test_case(&regolo::USAGE_SIDE_CALLS_MALFORMED ; "usage_side_calls_malformed")]
#[test_case(&regolo::USAGE_UNAUTHORIZED ; "usage_unauthorized")]
fn the_bundled_regolo_plugin_reads_the_recorded_usage(fixture: &Fixture) {
    replay::declared_usage(bundled(REGOLO), REGOLO, fixture);
}
