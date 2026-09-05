use agent_client_protocol_schema::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
};
use maki_agent::permissions::PermissionAnswer;

const ALLOW_ONCE_ID: &str = "allow_once";
const ALLOW_ALWAYS_ID: &str = "allow_always";
const REJECT_ONCE_ID: &str = "reject_once";
const REJECT_ALWAYS_ID: &str = "reject_always";
const REJECT_ALWAYS_LABEL: &str = "Reject always";
const REJECT_SESSION_LABEL: &str = "Reject for this session";

/// The reject label depends on folder trust, because that is what decides how
/// long the answer lives. In an untrusted folder Maki keeps the rule in memory
/// rather than writing `.maki/permissions.toml` into a checkout nobody trusted,
/// and the option label is the only place an ACP client can be told.
pub fn permission_options(project_trusted: bool) -> Vec<PermissionOption> {
    let reject_always = if project_trusted {
        REJECT_ALWAYS_LABEL
    } else {
        REJECT_SESSION_LABEL
    };
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
            reject_always,
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// ACP clients show fixed option kinds, so these four ids are everything an ACP
/// session can answer. "Allow always" stays in the session, and "Reject always"
/// is a project answer that the permission manager only writes to
/// `.maki/permissions.toml` once the folder is trusted, the same as the TUI.
/// An ACP session never asks about trust, so an untrusted folder keeps both of
/// them in memory, collects nothing, and says so in the reject label. A deny
/// that has to outlive the session needs `maki trust add` on the folder or a
/// rule in the global permissions file.
pub fn outcome_to_answer(outcome: &RequestPermissionOutcome) -> PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny,
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_ONCE_ID => PermissionAnswer::AllowOnce,
            ALLOW_ALWAYS_ID => PermissionAnswer::AllowSession,
            REJECT_ONCE_ID => PermissionAnswer::Deny,
            REJECT_ALWAYS_ID => PermissionAnswer::DenyAlwaysProject,
            _ => PermissionAnswer::Deny,
        },
        _ => PermissionAnswer::Deny,
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::SelectedPermissionOutcome;
    use test_case::test_case;

    use super::{
        ALLOW_ALWAYS_ID, ALLOW_ONCE_ID, PermissionAnswer, REJECT_ALWAYS_ID, REJECT_ALWAYS_LABEL,
        REJECT_ONCE_ID, REJECT_SESSION_LABEL, RequestPermissionOutcome, outcome_to_answer,
        permission_options,
    };

    /// An ACP client cannot be told about folder trust, so the always-answers
    /// must not reach further than the ones the TUI offers.
    #[test_case(ALLOW_ONCE_ID, PermissionAnswer::AllowOnce ; "allow_once")]
    #[test_case(ALLOW_ALWAYS_ID, PermissionAnswer::AllowSession ; "allow_always_lasts_for_the_session")]
    #[test_case(REJECT_ONCE_ID, PermissionAnswer::Deny ; "reject_once")]
    #[test_case(REJECT_ALWAYS_ID, PermissionAnswer::DenyAlwaysProject ; "reject_always_follows_folder_trust")]
    fn every_offered_option_maps_to_an_answer(id: &str, expected: PermissionAnswer) {
        assert!(
            permission_options(true)
                .iter()
                .any(|option| option.option_id.0.as_ref() == id),
            "option {id} is not offered"
        );
        let outcome =
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id.to_owned()));
        assert_eq!(outcome_to_answer(&outcome), expected);
    }

    /// Nothing is saved into an untrusted folder, so a client that reads
    /// "Reject always" there would be told something untrue.
    #[test_case(true, REJECT_ALWAYS_LABEL ; "trusted_folder_saves_the_rule")]
    #[test_case(false, REJECT_SESSION_LABEL ; "untrusted_folder_keeps_it_in_memory")]
    fn the_reject_label_says_how_long_the_answer_lives(project_trusted: bool, expected: &str) {
        let options = permission_options(project_trusted);
        let reject = options
            .iter()
            .find(|option| option.option_id.0.as_ref() == REJECT_ALWAYS_ID)
            .expect("reject always is offered");

        assert_eq!(reject.name, expected);
    }
}
