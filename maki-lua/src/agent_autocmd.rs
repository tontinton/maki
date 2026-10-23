//! Every event the host fires, and the one place that classifies its fields.
//! Agent events are turned into host events here too, shared by the UI app
//! loop, `maki -p` and sdk mode, so a plugin sees the same events whichever
//! way maki was started. `maki-acp` drives the agent from its own loop and
//! does not call this yet, so plugins loaded under ACP get no turn events.

use std::fmt::Display;

use maki_agent::{AgentEvent, DoneReason, Envelope};
use maki_providers::TokenUsage;
use serde::Serialize;
use serde_json::Value;
use strum::{EnumDiscriminants, EnumString, IntoStaticStr, VariantArray};

use crate::EventHandle;

const SESSION_ID: &str = "session_id";

/// Every event the host fires, one variant each, and [`EventHandle::fire`]
/// takes nothing else. Each field lands in the payload, so
/// `every_payload_key_is_classified_once` sees a new field before any plugin
/// does.
#[derive(Debug, Serialize, IntoStaticStr, EnumDiscriminants)]
#[serde(untagged)]
#[strum_discriminants(name(HostEventKind), derive(VariantArray, IntoStaticStr, EnumString))]
pub enum HostEvent<'a> {
    TurnStart {
        text: &'a str,
    },
    ToolStart {
        tool_id: &'a str,
        tool: &'a str,
    },
    ToolDone {
        tool_id: &'a str,
        tool: &'a str,
        is_error: bool,
        bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<&'a Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    AutoCompacting {
        context_size: u32,
        context_window: u32,
    },
    CompactionDone {
        context_size_before: u32,
        context_size_after: u32,
        context_window: u32,
        summary: &'a str,
    },
    TurnEnd {
        reason: &'static str,
        usage: &'a TokenUsage,
        cost: Option<f64>,
        list_cost: Option<f64>,
        context_size: u32,
        context_window: u32,
        num_turns: u32,
    },
    TurnError {
        message: &'a str,
    },
    PlanReady {
        path: &'a str,
    },
    SessionReset {},
    SessionEnd {
        reason: &'a str,
        deadline_ms: Option<u64>,
    },
    SessionFocusChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_session_id: Option<String>,
    },
    SessionStatusChanged {
        title: &'a str,
        status: &'a str,
        focused: bool,
    },
    SessionTitleChanged {
        title: &'a str,
        focused: bool,
    },
    SessionCwdChanged {
        cwd: &'a str,
    },
    TaskStatusChanged {
        id: &'a str,
        name: &'a str,
        status: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin: Option<&'a str>,
    },
    TaskStopped {
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin: Option<&'a str>,
    },
    TaskFocusChanged {
        id: &'a str,
    },
    ModelChanged {
        model: Value,
        previous_spec: String,
    },
    InputChanged {
        text: &'a str,
        cursor: usize,
        version: u64,
        source: Option<&'a str>,
        cursor_only: bool,
    },
    FileIndexReady {
        root: Option<&'a str>,
        files: usize,
        crashed: bool,
        truncated: bool,
    },
}

impl HostEvent<'_> {
    pub fn name(&self) -> &'static str {
        self.into()
    }

    /// The `ev.data` subscribers get. {session_id} is the session the event
    /// is about, absent only for one that is about no session.
    pub fn payload(&self, session_id: Option<&dyn Display>) -> Value {
        let mut data = serde_json::to_value(self).unwrap_or_else(|e| {
            tracing::warn!(event = self.name(), error = %e, "host event payload did not serialize");
            Value::Null
        });
        if let (Some(id), Some(map)) = (session_id, data.as_object_mut()) {
            map.insert(SESSION_ID.into(), Value::String(id.to_string()));
        }
        data
    }
}

/// The keys of one host event's payload, sorted by whether they are
/// conversation content. A subscriber without `session_read` gets only the
/// metadata, so a key nobody sorted is left out rather than leaked.
pub struct EventFields {
    pub content: &'static [&'static str],
    pub metadata: &'static [&'static str],
}

impl EventFields {
    /// [`HostEvent::payload`] adds `session_id` to every event about a
    /// session, so no row has to list it.
    pub fn is_metadata(&self, key: &str) -> bool {
        key == SESSION_ID || self.metadata.contains(&key)
    }
}

impl HostEventKind {
    pub fn fields(self) -> EventFields {
        let (content, metadata): (&'static [&'static str], &'static [&'static str]) = match self {
            Self::TurnStart => (&["text"], &[]),
            Self::ToolStart => (&[], &["tool_id", "tool"]),
            Self::ToolDone => (
                &["input"],
                &["tool_id", "tool", "is_error", "bytes", "duration_ms"],
            ),
            Self::AutoCompacting => (&[], &["context_size", "context_window"]),
            Self::CompactionDone => (
                &["summary"],
                &[
                    "context_size_before",
                    "context_size_after",
                    "context_window",
                ],
            ),
            Self::TurnEnd => (
                &[],
                &[
                    "reason",
                    "usage",
                    "cost",
                    "list_cost",
                    "context_size",
                    "context_window",
                    "num_turns",
                ],
            ),
            Self::TurnError => (&[], &["message"]),
            Self::PlanReady => (&[], &["path"]),
            Self::SessionReset => (&[], &[]),
            Self::SessionEnd => (&[], &["reason", "deadline_ms"]),
            Self::SessionFocusChanged => (&[], &["previous_session_id"]),
            Self::SessionStatusChanged => (&[], &["title", "status", "focused"]),
            Self::SessionTitleChanged => (&[], &["title", "focused"]),
            Self::SessionCwdChanged => (&[], &["cwd"]),
            Self::TaskStatusChanged => (&[], &["id", "name", "status", "plugin"]),
            Self::TaskStopped => (&[], &["id", "plugin"]),
            Self::TaskFocusChanged => (&[], &["id"]),
            Self::ModelChanged => (&[], &["model", "previous_spec"]),
            Self::InputChanged => (&["text"], &["cursor", "version", "source", "cursor_only"]),
            Self::FileIndexReady => (&[], &["root", "files", "crashed", "truncated"]),
        };
        EventFields { content, metadata }
    }
}

/// `None` for a name the host never fires, which is a plugin's own event and
/// nobody else's to classify.
pub fn host_event_fields(event: &str) -> Option<EventFields> {
    event.parse().ok().map(HostEventKind::fields)
}

pub fn dispatch(
    handle: &EventHandle,
    session_id: &dyn Display,
    envelope: &Envelope,
    subagent_bounded: bool,
) {
    if let Some(event) = host_event_for(&envelope.event, subagent_bounded) {
        handle.fire(Some(session_id), event);
    }
}

/// Subagent envelopes carry the parent's `session_id`, so firing on them too
/// would double count every budget and every tool the parent already reported,
/// and a subagent compacting would look like the main context shrank.
///
/// This runs on every envelope, streaming deltas included, and almost all of
/// them map to nothing, so the event borrows and only a mapped one pays for
/// a payload.
pub fn host_event_for(event: &AgentEvent, subagent_bounded: bool) -> Option<HostEvent<'_>> {
    if subagent_bounded {
        return None;
    }
    match event {
        AgentEvent::ToolStart(e) => Some(HostEvent::ToolStart {
            tool_id: &e.id,
            tool: &e.tool,
        }),
        AgentEvent::ToolDone(e) => Some(HostEvent::ToolDone {
            tool_id: &e.id,
            tool: &e.tool,
            is_error: e.is_error,
            bytes: e.output.as_text().len(),
            input: e.call.as_ref().map(|call| &call.input),
            duration_ms: e.call.as_ref().map(|call| call.duration.as_millis() as u64),
        }),
        AgentEvent::AutoCompacting {
            context_size,
            context_window,
        } => Some(HostEvent::AutoCompacting {
            context_size: *context_size,
            context_window: *context_window,
        }),
        AgentEvent::CompactionDone {
            context_size_before,
            context_size_after,
            context_window,
            summary,
        } => Some(HostEvent::CompactionDone {
            context_size_before: *context_size_before,
            context_size_after: *context_size_after,
            context_window: *context_window,
            summary,
        }),
        AgentEvent::Done {
            reason,
            usage,
            cost,
            list_cost,
            context_size,
            context_window,
            num_turns,
        } => Some(HostEvent::TurnEnd {
            reason: turn_end_reason(*reason)?,
            usage,
            cost: *cost,
            list_cost: *list_cost,
            context_size: *context_size,
            context_window: *context_window,
            num_turns: *num_turns,
        }),
        AgentEvent::Error { message, .. } => Some(HostEvent::TurnError { message }),
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
        DoneReason::Dropped => Some("dropped"),
        DoneReason::Compact => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use std::time::Duration;

    use maki_agent::{CallRecord, ToolDoneEvent, ToolOutput, ToolStartEvent};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SESSION: &str = "session-x";
    const TOOL_TEXT: &str = "ok";
    const TOOL_COMMAND: &str = "cargo test";
    const TOOL_MILLIS: u64 = 42;

    fn fired(event: &AgentEvent) -> Option<(&'static str, Value)> {
        host_event_for(event, false).map(|e| (e.name(), e.payload(Some(&SESSION))))
    }

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

    fn tool_done() -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: "t1".into(),
            tool: "bash".into(),
            output: Arc::new(ToolOutput::Plain(TOOL_TEXT.into())),
            is_error: false,
            annotation: None,
            written_path: None,
            call: Some(Box::new(CallRecord {
                input: json!({ "command": TOOL_COMMAND }),
                duration: Duration::from_millis(TOOL_MILLIS),
            })),
        }))
    }

    #[test]
    fn tool_done_carries_the_call() {
        let (_, data) = fired(&tool_done()).unwrap();
        assert_eq!(data["input"]["command"], TOOL_COMMAND);
        assert_eq!(data["duration_ms"], TOOL_MILLIS);
        assert_eq!(data["bytes"], TOOL_TEXT.len());
        assert_eq!(data["is_error"], false);
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
            tool_done(),
            AgentEvent::AutoCompacting {
                context_size: 1,
                context_window: 2,
            },
            AgentEvent::CompactionDone {
                context_size_before: 100_000,
                context_size_after: 30_000,
                context_window: 200_000,
                summary: String::new(),
            },
            done(DoneReason::EndTurn),
            AgentEvent::Error {
                message: "boom".into(),
                auth: false,
            },
        ]
    }

    #[test_case(DoneReason::EndTurn, Some("finished") ; "finished")]
    #[test_case(DoneReason::MaxTokens, Some("max_tokens") ; "max_tokens")]
    #[test_case(DoneReason::MaxTurns, Some("max_turns") ; "max_turns")]
    #[test_case(DoneReason::Cancelled, Some("cancelled") ; "cancelled")]
    #[test_case(DoneReason::Dropped, Some("dropped") ; "dropped")]
    #[test_case(DoneReason::Compact, None ; "manual_compact_is_not_a_turn")]
    fn done_maps_to_a_turn_end_reason(reason: DoneReason, expected: Option<&str>) {
        let fired = fired(&done(reason));
        let got = fired
            .as_ref()
            .map(|(event, data)| (*event, data["reason"].as_str().unwrap_or_default()));
        assert_eq!(got, expected.map(|reason| ("TurnEnd", reason)));
    }

    /// A budget watchdog reads the whole bill off this one payload, so the keys
    /// it charges against have to survive refactors.
    #[test]
    fn turn_end_carries_the_turn_totals() {
        let (_, data) = fired(&done(DoneReason::EndTurn)).unwrap();
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
            let (name, data) = fired(&event).unwrap_or_else(|| panic!("{event:?} fires nothing"));
            assert_eq!(data[SESSION_ID], SESSION, "{name}");
        }
    }

    const SAMPLE_TEXT: &str = "some text";

    /// One of each event with every optional field set, so the payload shows
    /// every key the event can carry. The match is exhaustive, so a new event
    /// does not compile until it is sampled here.
    fn sample(kind: HostEventKind) -> HostEvent<'static> {
        static INPUT: Value = Value::Null;
        match kind {
            HostEventKind::TurnStart => HostEvent::TurnStart { text: SAMPLE_TEXT },
            HostEventKind::ToolStart => HostEvent::ToolStart {
                tool_id: SAMPLE_TEXT,
                tool: SAMPLE_TEXT,
            },
            HostEventKind::ToolDone => HostEvent::ToolDone {
                tool_id: SAMPLE_TEXT,
                tool: SAMPLE_TEXT,
                is_error: false,
                bytes: 0,
                input: Some(&INPUT),
                duration_ms: Some(0),
            },
            HostEventKind::AutoCompacting => HostEvent::AutoCompacting {
                context_size: 0,
                context_window: 0,
            },
            HostEventKind::CompactionDone => HostEvent::CompactionDone {
                context_size_before: 0,
                context_size_after: 0,
                context_window: 0,
                summary: SAMPLE_TEXT,
            },
            HostEventKind::TurnEnd => HostEvent::TurnEnd {
                reason: SAMPLE_TEXT,
                usage: Box::leak(Box::default()),
                cost: Some(0.0),
                list_cost: Some(0.0),
                context_size: 0,
                context_window: 0,
                num_turns: 0,
            },
            HostEventKind::TurnError => HostEvent::TurnError {
                message: SAMPLE_TEXT,
            },
            HostEventKind::PlanReady => HostEvent::PlanReady { path: SAMPLE_TEXT },
            HostEventKind::SessionReset => HostEvent::SessionReset {},
            HostEventKind::SessionEnd => HostEvent::SessionEnd {
                reason: SAMPLE_TEXT,
                deadline_ms: Some(0),
            },
            HostEventKind::SessionFocusChanged => HostEvent::SessionFocusChanged {
                previous_session_id: Some(SAMPLE_TEXT.into()),
            },
            HostEventKind::SessionStatusChanged => HostEvent::SessionStatusChanged {
                title: SAMPLE_TEXT,
                status: SAMPLE_TEXT,
                focused: true,
            },
            HostEventKind::SessionTitleChanged => HostEvent::SessionTitleChanged {
                title: SAMPLE_TEXT,
                focused: true,
            },
            HostEventKind::SessionCwdChanged => HostEvent::SessionCwdChanged { cwd: SAMPLE_TEXT },
            HostEventKind::TaskStatusChanged => HostEvent::TaskStatusChanged {
                id: SAMPLE_TEXT,
                name: SAMPLE_TEXT,
                status: SAMPLE_TEXT,
                plugin: Some(SAMPLE_TEXT),
            },
            HostEventKind::TaskStopped => HostEvent::TaskStopped {
                id: SAMPLE_TEXT,
                plugin: Some(SAMPLE_TEXT),
            },
            HostEventKind::TaskFocusChanged => HostEvent::TaskFocusChanged { id: SAMPLE_TEXT },
            HostEventKind::ModelChanged => HostEvent::ModelChanged {
                model: json!({}),
                previous_spec: SAMPLE_TEXT.into(),
            },
            HostEventKind::InputChanged => HostEvent::InputChanged {
                text: SAMPLE_TEXT,
                cursor: 0,
                version: 0,
                source: Some(SAMPLE_TEXT),
                cursor_only: false,
            },
            HostEventKind::FileIndexReady => HostEvent::FileIndexReady {
                root: Some(SAMPLE_TEXT),
                files: 0,
                crashed: false,
                truncated: false,
            },
        }
    }

    /// A field nobody sorted never reaches a plugin without `session_read`,
    /// which for a metadata field is a silent bug, and a field sorted both
    /// ways leaks its content. Agent and UI events alike go through here.
    #[test]
    fn every_payload_key_is_classified_once() {
        for &kind in HostEventKind::VARIANTS {
            let event = sample(kind);
            let name = event.name();
            let fields = host_event_fields(name).expect("a host event finds its own fields");
            for key in fields.content {
                assert!(!fields.is_metadata(key), "{name}.{key} is both");
            }
            let data = event.payload(Some(&SESSION));
            for key in data.as_object().expect("payloads are objects").keys() {
                assert!(
                    fields.content.contains(&key.as_str()) || fields.is_metadata(key),
                    "{name}.{key} is neither content nor metadata"
                );
            }
        }
    }

    #[test]
    fn subagent_envelopes_fire_nothing() {
        for event in every_event() {
            assert!(
                host_event_for(&event, true).is_none(),
                "{event:?} leaked from a subagent"
            );
        }
    }
}
