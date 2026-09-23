use std::sync::Mutex;

use async_tungstenite::tungstenite::http::HeaderValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::auth::CODING_PLAN_BASE_URL;
use crate::AgentError;
use crate::providers::ResolvedAuth;

pub(super) const TURN_STATE_HEADER: &str = "x-codex-turn-state";
pub(super) const ROUTING_HINT_HEADER: &str = "x-codex-routing-hint";

#[derive(Default)]
pub(crate) struct RoutingState(Mutex<TurnRouting>);

#[derive(Default)]
struct TurnRouting {
    identity: Option<[u8; 32]>,
    value: Option<String>,
    source: Option<&'static str>,
}

pub(super) fn is_coding_plan(base: &str) -> bool {
    base.trim_end_matches('/') == CODING_PLAN_BASE_URL
}

pub(super) fn routing_hint(body: &Value) -> String {
    let mut hint = format!("model={}", body["model"].as_str().unwrap_or_default());
    if let Some(tier) = body["service_tier"].as_str() {
        hint.push_str(";tier=");
        hint.push_str(tier);
    }
    hint
}

impl RoutingState {
    pub(crate) fn clear(&self) {
        let mut state = self.0.lock().unwrap();
        state.value = None;
        state.source = None;
    }

    pub(super) fn prepare(&self, auth: &ResolvedAuth) -> Result<(), AgentError> {
        let headers: Vec<_> = auth
            .headers
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), TURN_STATE_HEADER | ROUTING_HINT_HEADER))
            .collect();
        let identity = Sha256::digest(serde_json::to_vec(&(
            auth.base_url
                .as_deref()
                .map(|base| base.trim_end_matches('/')),
            headers,
        ))?)
        .into();
        let mut state = self.0.lock().unwrap();
        if state.identity != Some(identity) {
            *state = TurnRouting {
                identity: Some(identity),
                ..Default::default()
            };
        }
        Ok(())
    }

    pub(super) fn retain(&self, value: Option<&str>, source: &'static str) {
        let Some(value) =
            value.filter(|value| !value.trim().is_empty() && HeaderValue::from_str(value).is_ok())
        else {
            return;
        };
        let mut state = self.0.lock().unwrap();
        if state.value.is_none() {
            state.value = Some(value.into());
            state.source = Some(source);
        }
    }

    pub(super) fn observe(&self, event: &str, parsed: &Value) {
        let source = match event {
            "response.metadata" => "response.metadata",
            "codex.response.metadata" => "codex.response.metadata",
            _ => return,
        };
        if let Some(headers) = parsed["headers"].as_object() {
            for (name, value) in headers {
                if name.eq_ignore_ascii_case(TURN_STATE_HEADER) {
                    self.retain(value.as_str(), source);
                }
            }
        }
    }

    pub(super) fn value(&self) -> Option<String> {
        self.0.lock().unwrap().value.clone()
    }

    pub(super) fn diagnostics(&self) -> Value {
        let state = self.0.lock().unwrap();
        json!({"present": state.value.is_some(), "source": state.source,
            "fingerprint": state.value.as_ref().map(|value| format!("{:x}", Sha256::digest(value.as_bytes())))})
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::{RoutingState, TURN_STATE_HEADER, is_coding_plan, routing_hint};
    use crate::providers::ResolvedAuth;

    const STATE: &str = "opaque-turn-state";
    const MODEL: &str = "gpt-test";

    #[test_case(None, "model=gpt-test" ; "absent")]
    #[test_case(Some("default"), "model=gpt-test;tier=default" ; "default")]
    #[test_case(Some("priority"), "model=gpt-test;tier=priority" ; "priority")]
    fn effective_route(tier: Option<&str>, expected: &str) {
        assert_eq!(
            routing_hint(&json!({"model":MODEL,"service_tier":tier})),
            expected
        );
    }

    #[test_case("https://chatgpt.com/backend-api/codex", true ; "coding_plan")]
    #[test_case("https://chatgpt.com/backend-api/codex/", true ; "trailing_slash")]
    #[test_case("https://api.openai.com/v1", false ; "public_api")]
    #[test_case("https://example.com/backend-api/codex", false ; "other_provider")]
    fn endpoint_gate(base: &str, expected: bool) {
        assert_eq!(is_coding_plan(base), expected);
    }

    #[test_case("response.metadata")]
    #[test_case("codex.response.metadata")]
    fn first_valid_metadata_is_retained(event: &str) {
        let routing = RoutingState::default();
        for value in ["", " ", "bad\nheader", STATE, "later"] {
            routing.observe(event, &json!({"headers":{TURN_STATE_HEADER:value}}));
        }
        assert_eq!(routing.value().as_deref(), Some(STATE));
        assert_eq!(routing.diagnostics()["source"], event);
        assert!(!routing.diagnostics().to_string().contains(STATE));
        routing.clear();
        assert!(routing.value().is_none());
    }

    #[test_case("authorization", true ; "auth_reset")]
    #[test_case("x-codex-routing-hint", false ; "tier_keeps_state")]
    #[test_case("x-codex-turn-state", false ; "echo_keeps_state")]
    fn identity_changes(header: &str, reset: bool) {
        let routing = RoutingState::default();
        let mut auth = ResolvedAuth::for_test(Some("http://localhost".into()), vec![]);
        routing.prepare(&auth).unwrap();
        routing.retain(Some(STATE), "http_header");
        auth.set_header(header, "changed".into());
        routing.prepare(&auth).unwrap();
        assert_eq!(routing.value().is_none(), reset);
        auth.base_url = Some("http://other".into());
        routing.prepare(&auth).unwrap();
        assert!(routing.value().is_none());
    }
}
