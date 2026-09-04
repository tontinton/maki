//! One place that turns agent events into autocmds, shared by the UI app loop,
//! `maki -p` and sdk mode, so a plugin sees the same events whichever way maki
//! was started. `maki-acp` drives the agent from its own loop and does not call
//! this yet, so plugins loaded under ACP get no turn events.

use std::fmt::Display;

use maki_agent::{AgentEvent, DoneReason, Envelope};
use serde_json::{Value, json};

use crate::EventHandle;

pub fn dispatch(
    handle: &EventHandle,
    session_id: &dyn Display,
    envelope: &Envelope,
    subagent_bounded: bool,
) {
    if let Some((event, data)) = autocmd_for(&envelope.event, session_id, subagent_bounded) {
        handle.fire_autocmd(event, data);
    }
}

/// Subagent envelopes carry the parent's `session_id`, so firing on them too
/// would double count every budget and every tool the parent already reported,
/// and a subagent compacting would look like the main context shrank.
///
/// `session_id` stays a `Display` because this runs on every envelope, streaming
/// deltas included, and almost all of them map to nothing. Only the arms that
/// build a payload pay for formatting the id.
pub fn autocmd_for(
    event: &AgentEvent,
    session_id: &dyn Display,
    subagent_bounded: bool,
) -> Option<(&'static str, Value)> {
    if subagent_bounded {
        return None;
    }
    let sid = || Value::String(session_id.to_string());
    match event {
        AgentEvent::ToolStart(e) => Some((
            "ToolStart",
            json!({ "session_id": sid(), "tool_id": e.id, "tool": e.tool }),
        )),
        AgentEvent::ToolDone(e) => Some((
            "ToolDone",
            json!({ "session_id": sid(), "tool_id": e.id, "tool": e.tool }),
        )),
        AgentEvent::AutoCompacting {
            context_size,
            context_window,
        } => Some((
            "AutoCompacting",
            json!({
                "session_id": sid(),
                "context_size": context_size,
                "context_window": context_window,
            }),
        )),
        AgentEvent::CompactionDone {
            context_size_before,
            context_size_after,
            context_window,
        } => Some((
            "CompactionDone",
            json!({
                "session_id": sid(),
                "context_size_before": context_size_before,
                "context_size_after": context_size_after,
                "context_window": context_window,
            }),
        )),
        AgentEvent::Done {
            reason,
            usage,
            cost,
            list_cost,
            context_size,
            context_window,
            num_turns,
        } => Some((
            "TurnEnd",
            json!({
                "session_id": sid(),
                "reason": turn_end_reason(*reason)?,
                "usage": usage,
                "cost": cost,
                "list_cost": list_cost,
                "context_size": context_size,
                "context_window": context_window,
                "num_turns": num_turns,
            }),
        )),
        AgentEvent::Error { message } => Some((
            "TurnError",
            json!({ "session_id": sid(), "message": message }),
        )),
        AgentEvent::ReviewerVerdict(e) => Some((
            "ToolReviewed",
            json!({
                "session_id": sid(),
                "tool": e.tool.to_string(),
                "reviewer": e.reviewer,
                "model": e.model,
                "verdict": e.verdict,
                "reason": e.reason,
                "resolution": e.resolution,
                "cost": e.billed_cost,
                "list_cost": e.list_cost,
            }),
        )),
        _ => None,
    }
}

/// Nothing for a manual `/compact`: it closes a run without ending a turn, and
/// a goal loop hearing TurnEnd there would inject "continue" mid cleanup.
fn turn_end_reason(reason: DoneReason) -> Option<&'static str> {
    match reason {
        DoneReason::EndTurn => Some("finished"),
        DoneReason::MaxTokens => Some("max_tokens"),
        DoneReason::MaxTurns => Some("max_turns"),
        DoneReason::Cancelled => Some("cancelled"),
        DoneReason::Compact => None,
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::{ReviewerVerdictEvent, ToolDoneEvent, ToolOutput, ToolStartEvent};
    use maki_config::ToolKey;
    use maki_providers::TokenUsage;
    use test_case::test_case;

    use super::*;

    const SESSION: &str = "session-x";

    fn done(reason: DoneReason) -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage {
                input: 1_000,
                output: 500,
                ..Default::default()
            },
            cost: Some(0.05),
            list_cost: Some(0.25),
            context_size: 12_000,
            context_window: 200_000,
            num_turns: 4,
            reason,
        }
    }

    fn every_event() -> Vec<AgentEvent> {
        vec![
            AgentEvent::ToolStart(Box::new(ToolStartEvent {
                id: "t1".into(),
                tool: "bash".into(),
                summary: String::new(),
                render_header: None,
                annotation: None,
                input: None,
                raw_input: None,
                output: None,
            })),
            AgentEvent::ToolDone(Box::new(ToolDoneEvent {
                id: "t1".into(),
                tool: "bash".into(),
                output: ToolOutput::Plain("ok".into()),
                is_error: false,
                annotation: None,
                written_path: None,
            })),
            AgentEvent::AutoCompacting {
                context_size: 1,
                context_window: 2,
            },
            AgentEvent::CompactionDone {
                context_size_before: 100_000,
                context_size_after: 30_000,
                context_window: 200_000,
            },
            done(DoneReason::EndTurn),
            AgentEvent::Error {
                message: "boom".into(),
            },
            AgentEvent::ReviewerVerdict(Box::new(ReviewerVerdictEvent {
                tool: ToolKey::native("bash"),
                reviewer: "guard".into(),
                model: "claude-sonnet-4".into(),
                verdict: "ALLOW".into(),
                reason: Some("safe".into()),
                resolution: "allowed".into(),
                usage: TokenUsage::default(),
                billed_cost: Some(0.01),
                list_cost: Some(0.05),
            })),
        ]
    }

    #[test]
    fn reviewer_verdict_shape() {
        let event = AgentEvent::ReviewerVerdict(Box::new(ReviewerVerdictEvent {
            tool: ToolKey::native("bash"),
            reviewer: "guard".into(),
            model: "claude-sonnet-4".into(),
            verdict: "DENY".into(),
            reason: Some("nope".into()),
            resolution: "denied".into(),
            usage: TokenUsage::default(),
            billed_cost: Some(0.02),
            list_cost: Some(0.1),
        }));
        let (name, data) = autocmd_for(&event, &SESSION, false).unwrap();
        assert_eq!(name, "ToolReviewed");
        assert_eq!(data["tool"], "bash");
        assert_eq!(data["reviewer"], "guard");
        assert_eq!(data["verdict"], "DENY");
        assert_eq!(data["resolution"], "denied");
        assert_eq!(data["cost"], 0.02);
        assert_eq!(data["list_cost"], 0.1);
    }

    #[test_case(DoneReason::EndTurn, Some("finished") ; "finished")]
    #[test_case(DoneReason::MaxTokens, Some("max_tokens") ; "max_tokens")]
    #[test_case(DoneReason::MaxTurns, Some("max_turns") ; "max_turns")]
    #[test_case(DoneReason::Cancelled, Some("cancelled") ; "cancelled")]
    #[test_case(DoneReason::Compact, None ; "manual_compact_is_not_a_turn")]
    fn done_maps_to_a_turn_end_reason(reason: DoneReason, expected: Option<&str>) {
        let fired = autocmd_for(&done(reason), &SESSION, false);
        let got = fired
            .as_ref()
            .map(|(event, data)| (*event, data["reason"].as_str().unwrap_or_default()));
        assert_eq!(got, expected.map(|reason| ("TurnEnd", reason)));
    }

    /// A budget watchdog reads the whole bill off this one payload, so the keys
    /// it charges against have to survive refactors.
    #[test]
    fn turn_end_carries_the_turn_totals() {
        let (_, data) = autocmd_for(&done(DoneReason::EndTurn), &SESSION, false).unwrap();
        assert_eq!(data["usage"]["input_tokens"], 1_000);
        assert_eq!(data["cost"], 0.05);
        assert_eq!(data["list_cost"], 0.25);
        assert_eq!(data["context_size"], 12_000);
        assert_eq!(data["context_window"], 200_000);
        assert_eq!(data["num_turns"], 4);
    }

    #[test]
    fn every_fired_event_names_its_session() {
        for event in every_event() {
            let (name, data) = autocmd_for(&event, &SESSION, false)
                .unwrap_or_else(|| panic!("{event:?} fires nothing"));
            assert_eq!(data["session_id"], SESSION, "{name}");
        }
    }

    #[test]
    fn subagent_envelopes_fire_nothing() {
        for event in every_event() {
            assert!(
                autocmd_for(&event, &SESSION, true).is_none(),
                "{event:?} leaked from a subagent"
            );
        }
    }
}
