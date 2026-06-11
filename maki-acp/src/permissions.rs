use agent_client_protocol_schema::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
};
use maki_agent::permissions::PermissionAnswer;

const ALLOW_ONCE_ID: &str = "allow_once";
const ALLOW_ALWAYS_ID: &str = "allow_always";
const REJECT_ONCE_ID: &str = "reject_once";
const REJECT_ALWAYS_ID: &str = "reject_always";

pub fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ONCE_ID),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ALWAYS_ID),
            "Allow always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ONCE_ID),
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ALWAYS_ID),
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

pub fn outcome_to_answer(outcome: &RequestPermissionOutcome) -> PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny,
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_ONCE_ID => PermissionAnswer::AllowOnce,
            ALLOW_ALWAYS_ID => PermissionAnswer::AllowSession,
            REJECT_ONCE_ID => PermissionAnswer::Deny,
            REJECT_ALWAYS_ID => PermissionAnswer::DenyAlwaysLocal,
            _ => PermissionAnswer::Deny,
        },
        _ => PermissionAnswer::Deny,
    }
}
