//! The bundled `requesty` plugin against the exchanges the retired bespoke impl
//! recorded. The static wire facts are declaration data, so the stream cases
//! pin the codec applying them. The listing is the plugin's own hook, which
//! has to merge, sort and fail over exactly as the bespoke one did.

use maki_providers::Model;
use maki_providers::replay::{self, Fixture};
use maki_providers::requesty_fixtures as requesty;
use test_case::test_case;

use super::{REQUESTY, bundled};

#[test_case(&requesty::SUCCESS, requesty::thinking_model() ; "success")]
#[test_case(&requesty::SUCCESS_NON_THINKING, requesty::non_thinking_model() ; "success_non_thinking")]
#[test_case(&replay::UNAUTHORIZED, requesty::thinking_model() ; "unauthorized")]
#[test_case(&replay::SLOW_DOWN, requesty::thinking_model() ; "rate_limited_with_retry_after")]
#[test_case(&replay::RATE_LIMITED, requesty::thinking_model() ; "rate_limited")]
#[test_case(&replay::SERVER_ERROR, requesty::thinking_model() ; "server_error")]
#[test_case(&replay::MALFORMED_SSE, requesty::thinking_model() ; "malformed_sse")]
#[test_case(&replay::EMPTY_SSE_ERROR, requesty::thinking_model() ; "empty_sse_error_frame")]
#[test_case(&replay::TRUNCATED_STREAM, requesty::thinking_model() ; "truncated_stream")]
fn the_bundled_requesty_plugin_replays_the_recorded_exchange(fixture: &Fixture, model: Model) {
    replay::declared(bundled(REQUESTY), REQUESTY, fixture, &model);
}

/// A listing that fails is returned through `maki.provider.http_error`, so
/// with both down the managed one fails with the bespoke impl's error.
#[test_case(&requesty::MODELS ; "models")]
#[test_case(&requesty::MODELS_MANAGED_DOWN ; "models_managed_down")]
#[test_case(&requesty::MODELS_CATALOG_DOWN ; "models_catalog_down")]
#[test_case(&requesty::MODELS_BOTH_DOWN ; "models_both_down")]
fn the_bundled_requesty_plugin_lists_the_recorded_catalogs(fixture: &Fixture) {
    replay::declared_models(bundled(REQUESTY), REQUESTY, fixture);
}

#[test]
fn the_bundled_requesty_plugin_gates_the_effort_on_discovery() {
    replay::declared_discovered(
        bundled(REQUESTY),
        REQUESTY,
        &requesty::DISCOVERED_NON_THINKING,
        &requesty::model(requesty::NON_THINKING_SPEC),
    );
}
