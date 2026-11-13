//! Claude Code's task messages for headless output. Parallel subagents
//! interleave their messages on the wire, and these tell a client which
//! subagent said what, how far it got and how it ended.
//!
//! A client links a message to its subagent by matching `parent_tool_use_id`
//! to the task's `tool_use_id`. Events of a nested subagent come up stamped
//! with the top-level subagent above it, so one task here is a top-level
//! subagent together with everything under it.
//!
//! Events can arrive late, so each task is announced once and ended once, no
//! matter what comes after.

use std::collections::HashSet;
use std::sync::Arc;

use maki_agent::{
    AgentEvent, EndReason, Envelope, SessionControl, SubagentInfo, TurnCompleteEvent,
};
use serde::Serialize;

/// Claude Code's `task_type` for a subagent.
const SUBAGENT_TASK_TYPE: &str = "local_agent";
/// Claude Code points `output_file` at the task's output. Maki keeps a
/// subagent's transcript in the session, so there is no file to name.
const NO_OUTPUT_FILE: &str = "";

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum TaskMessage {
    Started(TaskStarted),
    Progress(TaskProgress),
    Notification(TaskNotification),
}

impl TaskMessage {
    pub(crate) fn subtype(&self) -> &'static str {
        match self {
            Self::Started(_) => "task_started",
            Self::Progress(_) => "task_progress",
            Self::Notification(_) => "task_notification",
        }
    }
}

/// Claude Code names a task twice. Built from one id, so the two never differ.
#[derive(Serialize)]
struct TaskRef {
    task_id: String,
    tool_use_id: String,
}

impl TaskRef {
    fn new(id: &str) -> Self {
        Self {
            task_id: id.to_owned(),
            tool_use_id: id.to_owned(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct TaskStarted {
    #[serde(flatten)]
    task: TaskRef,
    description: String,
    task_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct TaskProgress {
    #[serde(flatten)]
    task: TaskRef,
    description: String,
    usage: TaskUsage,
    last_tool_name: String,
}

#[derive(Serialize)]
pub(crate) struct TaskNotification {
    #[serde(flatten)]
    task: TaskRef,
    status: TaskStatus,
    output_file: &'static str,
    summary: String,
    usage: TaskUsage,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Completed,
    Failed,
    Stopped,
}

#[derive(Serialize)]
struct TaskUsage {
    total_tokens: u64,
    tool_uses: u32,
    duration_ms: u64,
}

/// What goes out around one envelope's own message.
#[derive(Default)]
pub(crate) struct TaskMessages {
    pub(crate) before: Vec<TaskMessage>,
    pub(crate) after: Vec<TaskMessage>,
}

struct Task {
    id: String,
    description: String,
    control: Arc<SessionControl>,
    /// The last turn's context, which is how Claude Code counts a subagent's
    /// tokens.
    total_tokens: u64,
    tool_uses: u32,
}

impl Task {
    fn usage(&self) -> TaskUsage {
        TaskUsage {
            total_tokens: self.total_tokens,
            tool_uses: self.tool_uses,
            duration_ms: self.control.opened().elapsed().as_millis() as u64,
        }
    }

    fn notification(self) -> TaskMessage {
        let (status, summary) = verdict(&self.control);
        TaskMessage::Notification(TaskNotification {
            task: TaskRef::new(&self.id),
            status,
            output_file: NO_OUTPUT_FILE,
            summary,
            usage: self.usage(),
        })
    }
}

pub(crate) struct SubagentTasks {
    /// In start order, so tasks that end together report in that order.
    running: Vec<Task>,
    /// Tasks already ended, so a straggling event cannot announce one again.
    ended: HashSet<String>,
    /// How the stream names a tool, so `last_tool_name` matches the tool
    /// calls next to it.
    tool_name: fn(&str) -> &str,
}

impl SubagentTasks {
    pub(crate) fn new(tool_name: fn(&str) -> &str) -> Self {
        Self {
            running: Vec::new(),
            ended: HashSet::new(),
            tool_name,
        }
    }

    pub(crate) fn observe(&mut self, envelope: &Envelope) -> TaskMessages {
        let mut messages = TaskMessages::default();
        match (&envelope.subagent, &envelope.event) {
            (Some(info), event) => {
                messages.before.extend(self.start(info));
                if let AgentEvent::TurnComplete(turn) = event {
                    messages
                        .after
                        .extend(self.progress(&info.parent_tool_use_id, turn));
                }
            }
            // A closing subagent hands its transcript up as its parent's event.
            (None, AgentEvent::SubagentHistory { tool_use_id, .. }) => {
                messages.after = self.end_where(|task| task.id == *tool_use_id);
            }
            // A subagent under a moved call sends its transcript to a chat that
            // headless mode never reads. The next result is the first moment
            // we can tell it is over.
            (None, AgentEvent::Done { .. } | AgentEvent::Error { .. }) => {
                messages.before = self.end_where(|task| task.control.ended().is_some());
            }
            (None, _) => {}
        }
        messages
    }

    /// For when the stream ends, so no `task_started` is left without its end.
    pub(crate) fn finish(&mut self) -> Vec<TaskMessage> {
        self.end_where(|_| true)
    }

    fn start(&mut self, info: &SubagentInfo) -> Option<TaskMessage> {
        let id = &info.parent_tool_use_id;
        if self.ended.contains(id) || self.running.iter().any(|task| task.id == *id) {
            return None;
        }
        self.running.push(Task {
            id: id.clone(),
            description: info.name.clone(),
            control: Arc::clone(&info.control),
            total_tokens: 0,
            tool_uses: 0,
        });
        Some(TaskMessage::Started(TaskStarted {
            task: TaskRef::new(id),
            description: info.name.clone(),
            task_type: SUBAGENT_TASK_TYPE,
            prompt: info.prompt.clone(),
        }))
    }

    fn progress(&mut self, id: &str, turn: &TurnCompleteEvent) -> Option<TaskMessage> {
        let task = self.running.iter_mut().find(|task| task.id == id)?;
        task.total_tokens = turn.usage.context_tokens();
        let mut last_tool = None;
        for (_, name, _) in turn.message.tool_uses() {
            task.tool_uses += 1;
            last_tool = Some(name);
        }
        let last_tool = last_tool?;
        Some(TaskMessage::Progress(TaskProgress {
            task: TaskRef::new(id),
            description: task.description.clone(),
            usage: task.usage(),
            last_tool_name: (self.tool_name)(last_tool).to_owned(),
        }))
    }

    fn end_where(&mut self, ends: impl Fn(&Task) -> bool) -> Vec<TaskMessage> {
        self.running
            .extract_if(.., |task| ends(task))
            .map(|task| {
                self.ended.insert(task.id.clone());
                task.notification()
            })
            .collect()
    }
}

/// A failure after a stop or cancel is how the stop reached the tool, so it
/// reads as stopped. With no report the plugin either closed the session
/// without one, which we take as done, or never closed it.
fn verdict(control: &SessionControl) -> (TaskStatus, String) {
    let ended = control.ended();
    let stopped = matches!(ended, Some(EndReason::Stopped(_) | EndReason::Cancelled));
    match control.result() {
        Some(Ok(answer)) => (TaskStatus::Completed, answer),
        Some(Err(why)) if stopped => (TaskStatus::Stopped, why),
        Some(Err(why)) => (TaskStatus::Failed, why),
        None if ended == Some(EndReason::Closed) => (TaskStatus::Completed, String::new()),
        None => {
            let why = ended.unwrap_or(EndReason::Cancelled);
            (TaskStatus::Stopped, why.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::{Cutter, DoneReason};
    use maki_providers::{ContentBlock, Message, Role, TokenUsage};
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    const TASK_ID: &str = "session-a";
    const OTHER_TASK_ID: &str = "session-b";
    const DESCRIPTION: &str = "Find the auth middleware";
    const PROMPT: &str = "find where auth is checked";
    const ANSWER: &str = "auth lives in src/auth.rs";
    const TOOL: &str = "read";
    const WIRE_TOOL: &str = "Read";
    const CONTEXT_TOKENS: u64 = 1_200;
    const STARTED: &str = "task_started";
    const PROGRESS: &str = "task_progress";
    const NOTIFICATION: &str = "task_notification";
    const ONCE: &str = "a task is announced once and ended once";

    fn wire_name(name: &str) -> &str {
        if name == TOOL { WIRE_TOOL } else { name }
    }

    fn from_subagent(id: &str, control: &Arc<SessionControl>, event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: Some(SubagentInfo {
                parent_tool_use_id: id.into(),
                name: DESCRIPTION.into(),
                prompt: Some(PROMPT.into()),
                model: None,
                opts: None,
                answer_tx: None,
                control: Arc::clone(control),
                parent_call: None,
            }),
            run_id: 0,
        }
    }

    fn top_level(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: 0,
        }
    }

    fn turn(content: Vec<ContentBlock>) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message {
                role: Role::Assistant,
                content,
                ..Default::default()
            },
            usage: TokenUsage {
                input: CONTEXT_TOKENS,
                ..Default::default()
            },
            model: String::new(),
            cost: None,
            subsidised_list_cost: None,
            context_size: None,
            context_window: 0,
        }))
    }

    fn tool_turn() -> AgentEvent {
        turn(vec![ContentBlock::ToolUse {
            id: String::new(),
            name: TOOL.into(),
            input: Value::Null,
            thought_signature: None,
        }])
    }

    fn answer_turn() -> AgentEvent {
        turn(vec![ContentBlock::Text {
            text: ANSWER.into(),
        }])
    }

    fn closed(id: &str) -> Envelope {
        top_level(AgentEvent::SubagentHistory {
            tool_use_id: id.into(),
            messages: Vec::new(),
        })
    }

    fn run_done() -> Envelope {
        top_level(AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: 0,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        })
    }

    fn wire(messages: Vec<TaskMessage>) -> Vec<(&'static str, Value)> {
        messages
            .into_iter()
            .map(|message| (message.subtype(), serde_json::to_value(message).unwrap()))
            .collect()
    }

    #[test]
    fn a_subagent_starts_progresses_and_ends() {
        let mut tasks = SubagentTasks::new(wire_name);
        let control = Arc::new(SessionControl::default());
        let event = |event| from_subagent(TASK_ID, &control, event);

        let first = tasks.observe(&event(tool_turn()));
        let [(STARTED, started)] = &wire(first.before)[..] else {
            panic!("the first event announces the task");
        };
        assert_eq!(
            *started,
            json!({
                "task_id": TASK_ID,
                "tool_use_id": TASK_ID,
                "description": DESCRIPTION,
                "task_type": SUBAGENT_TASK_TYPE,
                "prompt": PROMPT,
            })
        );
        let [(PROGRESS, progress)] = &wire(first.after)[..] else {
            panic!("a turn that called tools reports progress");
        };
        assert_eq!(progress["tool_use_id"], TASK_ID);
        assert_eq!(progress["last_tool_name"], WIRE_TOOL);
        assert_eq!(progress["usage"]["tool_uses"], 1);
        assert_eq!(progress["usage"]["total_tokens"], CONTEXT_TOKENS);

        let answered = tasks.observe(&event(answer_turn()));
        assert!(answered.before.is_empty(), "{ONCE}");
        assert!(answered.after.is_empty());

        control.report(Ok(ANSWER.into()));
        let [(NOTIFICATION, ended)] = &wire(tasks.observe(&closed(TASK_ID)).after)[..] else {
            panic!("a closed transcript ends the task");
        };
        assert_eq!(ended["tool_use_id"], TASK_ID);
        assert_eq!(ended["status"], "completed");
        assert_eq!(ended["summary"], ANSWER);
        assert_eq!(ended["output_file"], NO_OUTPUT_FILE);
        assert_eq!(ended["usage"]["tool_uses"], 1);

        assert!(tasks.observe(&closed(TASK_ID)).after.is_empty(), "{ONCE}");
        assert!(tasks.finish().is_empty(), "{ONCE}");
    }

    /// A cut prompt returns before its relay drains, so a subagent's last
    /// events can trail its transcript.
    #[test]
    fn a_straggling_event_does_not_reopen_an_ended_task() {
        let mut tasks = SubagentTasks::new(wire_name);
        let control = Arc::new(SessionControl::default());
        tasks.observe(&from_subagent(TASK_ID, &control, answer_turn()));
        tasks.observe(&closed(TASK_ID));

        let late = tasks.observe(&from_subagent(TASK_ID, &control, tool_turn()));

        assert!(late.before.is_empty(), "{ONCE}");
        assert!(late.after.is_empty(), "{ONCE}");
        assert!(tasks.finish().is_empty(), "{ONCE}");
    }

    /// A moved call's subagent sends no transcript here.
    #[test]
    fn a_task_without_a_transcript_ends_by_its_session() {
        let mut tasks = SubagentTasks::new(wire_name);
        let ended = Arc::new(SessionControl::default());
        let open = Arc::new(SessionControl::default());
        tasks.observe(&from_subagent(TASK_ID, &ended, tool_turn()));
        tasks.observe(&from_subagent(OTHER_TASK_ID, &open, tool_turn()));
        ended.end(EndReason::Closed);

        let done = tasks.observe(&run_done());

        let [(NOTIFICATION, first)] = &wire(done.before)[..] else {
            panic!("the run's result ends a task whose session ended");
        };
        assert_eq!(first["task_id"], TASK_ID);
        assert!(done.after.is_empty());
        let [(NOTIFICATION, last)] = &wire(tasks.finish())[..] else {
            panic!("the stream's end ends every task");
        };
        assert_eq!(last["task_id"], OTHER_TASK_ID);
    }

    #[test_case(Some(Ok(ANSWER.into())), Some(EndReason::Closed), TaskStatus::Completed ; "answered")]
    #[test_case(Some(Err(ANSWER.into())), Some(EndReason::Closed), TaskStatus::Failed ; "failed")]
    #[test_case(Some(Err(ANSWER.into())), Some(EndReason::Stopped(Cutter::Parent)), TaskStatus::Stopped ; "failed_after_a_stop")]
    #[test_case(Some(Err(ANSWER.into())), Some(EndReason::Cancelled), TaskStatus::Stopped ; "failed_after_a_cancel")]
    #[test_case(None, Some(EndReason::Closed), TaskStatus::Completed ; "closed_without_a_report")]
    #[test_case(None, None, TaskStatus::Stopped ; "still_open_when_the_stream_ended")]
    fn verdict_trusts_the_report_unless_a_stop_explains_the_failure(
        result: Option<Result<String, String>>,
        ended: Option<EndReason>,
        status: TaskStatus,
    ) {
        let control = SessionControl::default();
        if let Some(reason) = ended {
            control.end(reason);
        }
        if let Some(result) = result {
            control.report(result);
        }
        assert_eq!(verdict(&control).0, status);
    }
}
