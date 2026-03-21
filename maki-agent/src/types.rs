use std::fmt::Write;
use std::path::Path;

use flume::Sender;
use maki_providers::{AgentError, ContentBlock, Message, Role, StopReason, TokenUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const NO_FILES_FOUND: &str = "No files found";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffSpan {
    pub text: String,
    pub emphasized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiffLine {
    Unchanged(String),
    Added(Vec<DiffSpan>),
    Removed(Vec<DiffSpan>),
}

impl DiffSpan {
    pub fn plain(text: String) -> Self {
        Self {
            text,
            emphasized: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunk {
    pub start_line: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileEntry {
    pub path: String,
    pub matches: Vec<GrepMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub line_nr: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

impl QuestionOption {
    pub fn item_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": { "type": "string", "description": "Option label" },
                "description": { "type": "string", "description": "Option description" }
            },
            "required": ["label"]
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionInfo {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
}

impl QuestionInfo {
    pub fn item_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question text" },
                "header": { "type": "string", "description": "Short tab header for the question" },
                "options": {
                    "type": "array",
                    "description": "List of predefined options",
                    "items": QuestionOption::item_schema()
                },
                "multiple": { "type": "boolean", "description": "Whether multiple options can be selected" }
            },
            "required": ["question"]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub priority: TodoPriority,
}

impl TodoItem {
    pub fn item_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Task description" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] },
                "priority": { "type": "string", "enum": ["high", "medium", "low"] }
            },
            "required": ["content", "status", "priority"]
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Completed => "[✓]",
            Self::InProgress => "[•]",
            Self::Pending => "[ ]",
            Self::Cancelled => "[x]",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TodoPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolInput {
    Code { language: String, code: String },
    Script { language: String, code: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BatchToolStatus {
    Pending,
    InProgress,
    Success,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchToolEntry {
    pub tool: String,
    pub summary: String,
    pub status: BatchToolStatus,
    pub input: Option<ToolInput>,
    pub output: Option<ToolOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionBlock {
    pub path: String,
    pub content: String,
}

fn append_instructions(out: &mut String, blocks: &[InstructionBlock]) {
    for block in blocks {
        out.push_str("\n\n---\nInstructions from: ");
        out.push_str(&block.path);
        out.push('\n');
        out.push_str(&block.content);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Plain(String),
    ReadCode {
        path: String,
        start_line: usize,
        lines: Vec<String>,
        #[serde(default)]
        total_lines: usize,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
    },
    ReadDir {
        text: String,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
    },
    Diff {
        path: String,
        hunks: Vec<DiffHunk>,
        summary: String,
    },
    TodoList(Vec<TodoItem>),
    WriteCode {
        path: String,
        byte_count: usize,
        lines: Vec<String>,
    },
    GrepResult {
        entries: Vec<GrepFileEntry>,
    },
    GlobResult {
        files: Vec<String>,
    },
    Batch {
        entries: Vec<BatchToolEntry>,
        text: String,
    },
    QuestionAnswers(Vec<QuestionAnswer>),
}

impl ToolOutput {
    pub fn written_path(&self) -> Option<&str> {
        match self {
            Self::WriteCode { path, .. } | Self::Diff { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn is_empty_result(&self) -> bool {
        match self {
            Self::GlobResult { files } => files.is_empty(),
            Self::GrepResult { entries } => entries.is_empty(),
            Self::ReadDir { text, .. } => text.is_empty(),
            _ => false,
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            Self::Diff { summary, .. } => summary.clone(),
            Self::TodoList(_) => "ok".into(),
            _ => self.as_display_text(),
        }
    }

    pub fn as_display_text(&self) -> String {
        match self {
            Self::Plain(s) => s.clone(),
            Self::ReadDir { text, instructions } => {
                let mut out = text.clone();
                if let Some(blocks) = instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            Self::ReadCode {
                start_line,
                lines,
                instructions,
                ..
            } => {
                let mut out: String = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {line}", start_line + i))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(blocks) = instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            Self::Diff {
                path,
                hunks,
                summary,
            } => {
                let mut out = format!("{summary}\n--- {path}\n+++ {path}");
                for hunk in hunks {
                    out.push('\n');
                    for dl in &hunk.lines {
                        match dl {
                            DiffLine::Unchanged(t) => {
                                let _ = write!(out, "\n  {t}");
                            }
                            DiffLine::Removed(spans) | DiffLine::Added(spans) => {
                                let prefix = if matches!(dl, DiffLine::Removed(_)) {
                                    "- "
                                } else {
                                    "+ "
                                };

                                let _ = write!(out, "\n{prefix}");
                                for s in spans {
                                    out.push_str(&s.text);
                                }
                            }
                        }
                    }
                }
                out
            }
            Self::TodoList(items) => {
                if items.is_empty() {
                    return "No todos.".into();
                }
                items
                    .iter()
                    .map(|t| format!("{} ({}) {}", t.status.marker(), t.priority, t.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::WriteCode {
                path, byte_count, ..
            } => format!("wrote {byte_count} bytes to {path}"),
            Self::GlobResult { files } => {
                if files.is_empty() {
                    return NO_FILES_FOUND.into();
                }
                files.join("\n")
            }
            Self::GrepResult { entries } => {
                let mut out = String::new();
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&entry.path);
                    out.push(':');
                    for m in &entry.matches {
                        out.push_str(&format!("\n  {}: {}", m.line_nr, m.text));
                    }
                }
                out
            }
            Self::Batch { text, .. } => text.clone(),
            Self::QuestionAnswers(pairs) => {
                let mut table = String::from("| Question | Answer |\n|----------|--------|\n");
                for pair in pairs {
                    table.push_str(&format!("| {} | {} |\n", pair.question, pair.answer));
                }
                table.truncate(table.trim_end().len());
                table
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStartEvent {
    pub id: String,
    pub tool: &'static str,
    pub summary: String,
    pub annotation: Option<String>,
    pub input: Option<ToolInput>,
    pub output: Option<ToolOutput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDoneEvent {
    pub id: String,
    pub tool: &'static str,
    pub output: ToolOutput,
    pub is_error: bool,
}

impl ToolDoneEvent {
    pub fn error(id: String, message: impl Into<String>) -> Self {
        Self {
            id,
            tool: "unknown",
            output: ToolOutput::Plain(message.into()),
            is_error: true,
        }
    }

    pub fn written_path(&self) -> Option<&str> {
        if self.is_error {
            return None;
        }
        self.output.written_path()
    }

    pub fn wrote_to(&self, plan_path: &Path) -> bool {
        self.written_path()
            .is_some_and(|wp| Path::new(wp) == plan_path || plan_path.ends_with(wp))
    }
}

pub fn tool_results(results: Vec<ToolDoneEvent>) -> Message {
    Message {
        role: Role::User,
        content: results
            .into_iter()
            .map(|r| ContentBlock::ToolResult {
                tool_use_id: r.id,
                content: r.output.as_text(),
                is_error: r.is_error,
            })
            .collect(),
        ..Default::default()
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolPending {
        id: String,
        name: String,
    },
    ToolStart(Box<ToolStartEvent>),
    ToolOutput {
        id: String,
        content: String,
    },
    ToolDone(Box<ToolDoneEvent>),
    BatchProgress(Box<BatchProgressEvent>),
    TurnComplete(Box<TurnCompleteEvent>),
    ToolResultsSubmitted {
        message: Box<Message>,
    },
    QuestionPrompt {
        id: String,
        questions: Vec<QuestionInfo>,
    },
    QueueItemConsumed,
    Done {
        usage: TokenUsage,
        num_turns: u32,
        stop_reason: Option<StopReason>,
    },
    AutoCompacting,
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    Error {
        message: String,
    },
    PermissionRequest {
        id: String,
        tool: String,
        scope: String,
    },
    AuthRequired,
}

#[derive(Debug, Serialize)]
pub struct TurnCompleteEvent {
    pub message: Message,
    pub usage: TokenUsage,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BatchProgressEvent {
    pub batch_id: String,
    pub index: usize,
    pub status: BatchToolStatus,
    pub output: Option<ToolOutput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentInfo {
    pub parent_tool_use_id: String,
    #[serde(rename = "parent_name")]
    pub name: String,
    #[serde(rename = "parent_prompt", skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "parent_model", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip)]
    pub answer_tx: Option<flume::Sender<String>>,
}

#[derive(Debug, Clone)]
pub struct EventSender {
    tx: Sender<Envelope>,
    run_id: u64,
}

impl EventSender {
    pub fn new(tx: Sender<Envelope>, run_id: u64) -> Self {
        Self { tx, run_id }
    }

    pub fn send(&self, event: impl Into<AgentEvent>) -> Result<(), AgentError> {
        self.tx
            .try_send(Envelope {
                event: event.into(),
                subagent: None,
                run_id: self.run_id,
            })
            .map_err(|_| AgentError::Channel)
    }

    pub fn send_envelope(&self, envelope: Envelope) -> Result<(), AgentError> {
        self.tx.try_send(envelope).map_err(|_| AgentError::Channel)
    }

    pub fn try_send(&self, event: impl Into<AgentEvent>) {
        let _ = self.tx.try_send(Envelope {
            event: event.into(),
            subagent: None,
            run_id: self.run_id,
        });
    }

    pub fn run_id(&self) -> u64 {
        self.run_id
    }

    pub fn raw_tx(&self) -> &Sender<Envelope> {
        &self.tx
    }
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub event: AgentEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    pub run_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn as_display_text_diff_covers_all_line_types_and_multiple_hunks() {
        let output = ToolOutput::Diff {
            path: "src/main.rs".into(),
            hunks: vec![
                DiffHunk {
                    start_line: 1,
                    lines: vec![
                        DiffLine::Unchanged("keep".into()),
                        DiffLine::Removed(vec![DiffSpan::plain("old".into())]),
                        DiffLine::Added(vec![DiffSpan::plain("new".into())]),
                    ],
                },
                DiffHunk {
                    start_line: 10,
                    lines: vec![
                        DiffLine::Removed(vec![DiffSpan::plain("c".into())]),
                        DiffLine::Added(vec![DiffSpan::plain("d".into())]),
                    ],
                },
            ],
            summary: "Updated value".into(),
        };
        let display = output.as_display_text();
        assert!(display.starts_with("Updated value"));
        assert!(display.contains("--- src/main.rs"));
        assert!(display.contains("+++ src/main.rs"));
        assert!(display.contains("  keep"));
        assert!(display.contains("- old"));
        assert!(display.contains("+ new"));
        assert!(display.contains("- c"));
        assert!(display.contains("+ d"));
        assert_eq!(output.as_text(), "Updated value");
    }

    #[test]
    fn as_display_text_todolist_formats_all_statuses() {
        let output = ToolOutput::TodoList(vec![
            TodoItem {
                content: "done".into(),
                status: TodoStatus::Completed,
                priority: TodoPriority::High,
            },
            TodoItem {
                content: "wip".into(),
                status: TodoStatus::InProgress,
                priority: TodoPriority::Medium,
            },
            TodoItem {
                content: "todo".into(),
                status: TodoStatus::Pending,
                priority: TodoPriority::Low,
            },
            TodoItem {
                content: "nope".into(),
                status: TodoStatus::Cancelled,
                priority: TodoPriority::Low,
            },
        ]);
        let display = output.as_display_text();
        assert!(display.contains("[✓] (high) done"));
        assert!(display.contains("[•] (medium) wip"));
        assert!(display.contains("[ ] (low) todo"));
        assert!(display.contains("[x] (low) nope"));
        assert_eq!(output.as_text(), "ok");
    }

    #[test_case(vec!["src/a.rs".into(), "src/b.rs".into()], "src/a.rs\nsrc/b.rs" ; "with_files")]
    #[test_case(vec![],                                       NO_FILES_FOUND       ; "empty")]
    fn as_text_glob_result(files: Vec<String>, expected: &str) {
        let output = ToolOutput::GlobResult { files };
        assert_eq!(output.as_text(), expected);
    }

    #[test]
    fn as_text_grep_result_multi_file() {
        let output = ToolOutput::GrepResult {
            entries: vec![
                GrepFileEntry {
                    path: "src/a.rs".into(),
                    matches: vec![
                        GrepMatch {
                            line_nr: 3,
                            text: "fn foo()".into(),
                        },
                        GrepMatch {
                            line_nr: 10,
                            text: "fn bar()".into(),
                        },
                    ],
                },
                GrepFileEntry {
                    path: "src/b.rs".into(),
                    matches: vec![GrepMatch {
                        line_nr: 1,
                        text: "use crate".into(),
                    }],
                },
            ],
        };
        let text = output.as_text();
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("3: fn foo()"));
        assert!(text.contains("10: fn bar()"));
        assert!(text.contains("src/b.rs"));
        assert!(text.contains("1: use crate"));
    }

    #[test_case(ToolOutput::WriteCode { path: "src/lib.rs".into(), byte_count: 10, lines: vec![] }, Some("src/lib.rs") ; "write_code")]
    #[test_case(ToolOutput::Diff { path: "src/lib.rs".into(), hunks: vec![], summary: String::new() }, Some("src/lib.rs") ; "diff")]
    #[test_case(ToolOutput::Plain("ok".into()), None ; "non_write_variant")]
    fn output_written_path(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.written_path(), expected);
    }

    #[test]
    fn event_written_path_none_on_error() {
        let event = ToolDoneEvent {
            id: "id".into(),
            tool: "write",
            output: ToolOutput::WriteCode {
                path: "src/lib.rs".into(),
                byte_count: 10,
                lines: vec![],
            },
            is_error: true,
        };
        assert_eq!(event.written_path(), None);
    }

    #[test]
    fn tool_results_builds_message_with_tool_result_blocks() {
        let msg = tool_results(vec![
            ToolDoneEvent {
                id: "t1".into(),
                tool: "bash",
                output: ToolOutput::Plain("ok".into()),
                is_error: false,
            },
            ToolDoneEvent {
                id: "t2".into(),
                tool: "read",
                output: ToolOutput::Plain("fail".into()),
                is_error: true,
            },
        ]);
        assert!(matches!(msg.role, Role::User));
        assert_eq!(msg.content.len(), 2);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t1" && !is_error)
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t2" && *is_error)
        );
    }

    #[test_case(
        10,
        vec!["fn foo()".into(), "fn bar()".into()],
        Some(vec![InstructionBlock { path: "AGENTS.md".into(), content: "do stuff".into() }]),
        "10: fn foo()\n11: fn bar()\n\n---\nInstructions from: AGENTS.md\ndo stuff"
        ; "with_instructions"
    )]
    #[test_case(
        1,
        vec!["line1".into()],
        None,
        "1: line1"
        ; "without_instructions"
    )]
    fn read_code_display_text(
        start_line: usize,
        lines: Vec<String>,
        instructions: Option<Vec<InstructionBlock>>,
        expected: &str,
    ) {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line,
            lines,
            total_lines: 100,
            instructions,
        };
        assert_eq!(output.as_display_text(), expected);
    }

    #[test]
    fn read_code_backward_compat_deserialization() {
        let json = r#"{"ReadCode":{"path":"a.rs","start_line":1,"lines":["x"]}}"#;
        let output: ToolOutput = serde_json::from_str(json).unwrap();
        match output {
            ToolOutput::ReadCode {
                total_lines,
                instructions,
                ..
            } => {
                assert_eq!(total_lines, 0);
                assert!(instructions.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }
}
