use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_providers::{AgentError, ContentBlock, Message, Role, StopReason, TokenUsage};
use maki_tool_macro::{ArgEnum, Args};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

pub const NO_FILES_FOUND: &str = "No files found";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileEntry {
    pub path: String,
    pub groups: Vec<GrepMatchGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatchGroup {
    pub lines: Vec<GrepLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLine {
    pub line_nr: usize,
    pub text: String,
    pub is_match: bool,
}

impl GrepLine {
    pub fn matched(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: true,
        }
    }

    pub fn context(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: false,
        }
    }
}

impl GrepMatchGroup {
    pub fn single(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            lines: vec![GrepLine::matched(line_nr, text)],
        }
    }

    pub fn match_count(&self) -> usize {
        self.lines.iter().filter(|l| l.is_match).count()
    }
}

impl GrepFileEntry {
    pub fn match_count(&self) -> usize {
        self.groups.iter().map(|g| g.match_count()).sum()
    }
}

#[derive(Args, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    #[param(description = "Task description")]
    pub content: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub priority: TodoPriority,
}

#[derive(ArgEnum, Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
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

#[derive(
    ArgEnum, Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TodoPriority {
    High,
    #[default]
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolInput {
    Code {
        language: String,
        code: String,
    },
    /// Nothing produces this anymore (script rendering moved to Lua), but
    /// old persisted sessions still contain it and must keep loading.
    Script {
        language: String,
        code: String,
    },
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
    /// Lua plugins need the full JSON to re-run their snapshot on theme switch.
    /// `input` only covers code_execution, so this stores the original call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
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

#[derive(Debug, Clone, Serialize)]
pub struct TextOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<InstructionBlock>>,
}

impl From<String> for TextOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            instructions: None,
        }
    }
}

impl From<&str> for TextOutput {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            instructions: None,
        }
    }
}

impl<'de> Deserialize<'de> for TextOutput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy(String),
            Full {
                text: String,
                #[serde(default)]
                instructions: Option<Vec<InstructionBlock>>,
            },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Legacy(text) => Ok(Self {
                text,
                instructions: None,
            }),
            Raw::Full { text, instructions } => Ok(Self { text, instructions }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Plain(TextOutput),
    Markdown(TextOutput),
    ReadCode {
        path: String,
        start_line: usize,
        lines: Vec<String>,
        #[serde(default)]
        total_lines: usize,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
    },
    ReadDir(TextOutput),
    Diff {
        path: String,
        before: String,
        after: String,
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
    Batch {
        entries: Vec<BatchToolEntry>,
        text: String,
    },
    Instructions {
        blocks: Vec<InstructionBlock>,
    },
    Image {
        source: maki_providers::ImageSource,
        /// Caption for the tool_result block, e.g. "[image: slack.jpeg 222KB]";
        /// the pixels ride separately as a `ContentBlock::Image`.
        text: String,
    },
}

/// Saturating arithmetic so callers can't overflow with any combination of inputs.
fn lines_remaining_after(total: usize, start_line: usize, shown: usize) -> usize {
    let end = start_line.saturating_add(shown).saturating_sub(1);
    total.saturating_sub(end)
}

impl ToolOutput {
    /// Only here for old persisted sessions that still have `WriteCode`/`Diff` variants.
    /// New code should use `ToolDoneEvent::written_path` instead.
    pub fn written_path(&self) -> Option<&str> {
        match self {
            Self::WriteCode { path, .. } | Self::Diff { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn instructions(&self) -> Option<&[InstructionBlock]> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.instructions.as_deref(),
            Self::ReadCode { instructions, .. } => instructions.as_deref(),
            _ => None,
        }
    }

    pub fn owned_instructions(&self) -> Option<Vec<InstructionBlock>> {
        self.instructions()
            .filter(|b| !b.is_empty())
            .map(|b| b.to_vec())
    }

    pub fn is_markdown(&self) -> bool {
        matches!(self, Self::Markdown(_))
    }

    pub fn structured_display_text(&self) -> Option<String> {
        match self {
            Self::Diff { .. }
            | Self::ReadCode { .. }
            | Self::ReadDir(_)
            | Self::WriteCode { .. }
            | Self::GrepResult { .. }
            | Self::TodoList(_) => Some(self.as_display_text()),
            _ => None,
        }
    }

    pub fn is_empty_result(&self) -> bool {
        match self {
            Self::GrepResult { entries } => entries.is_empty(),
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.text.is_empty(),
            _ => false,
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            Self::Diff { summary, .. } => summary.clone(),
            Self::TodoList(_) => "ok".into(),
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => {
                let mut out = t.text.clone();
                if let Some(blocks) = &t.instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            Self::ReadCode { instructions, .. } => {
                let mut out = self.as_display_text();
                if let Some(blocks) = instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            _ => self.as_display_text(),
        }
    }

    pub fn as_display_text(&self) -> String {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.text.clone(),
            Self::ReadCode {
                start_line,
                lines,
                total_lines,
                ..
            } => {
                let mut out: String = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {line}", start_line + i))
                    .collect::<Vec<_>>()
                    .join("\n");
                let remaining = lines_remaining_after(*total_lines, *start_line, lines.len());
                if remaining > 0 {
                    out.push_str(&format!(
                        "\n\n...\n\nTruncated lines: {}-{}. Use offset={} to read further.",
                        start_line + lines.len(),
                        total_lines,
                        start_line + lines.len(),
                    ));
                }
                out
            }
            Self::Diff {
                path,
                before,
                after,
                summary,
            } => crate::diff::unified_text(
                before,
                after,
                summary,
                &crate::tools::relative_path(path),
            ),
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
            } => {
                let display = crate::tools::relative_path(path);
                format!("wrote {byte_count} bytes to {display}")
            }
            Self::GrepResult { entries } => {
                let mut out = String::new();
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&entry.path);
                    out.push(':');
                    let has_context = entry.groups.iter().any(|g| g.lines.len() > 1);
                    for (gi, group) in entry.groups.iter().enumerate() {
                        if gi > 0 && has_context {
                            out.push_str("\n  --");
                        }
                        for line in &group.lines {
                            let sep = if line.is_match { ":" } else { " " };
                            let _ = write!(out, "\n  {}{sep} {}", line.line_nr, line.text);
                        }
                    }
                }
                out
            }
            Self::Batch { text, .. } | Self::Image { text, .. } => text.clone(),
            Self::Instructions { blocks } => {
                let mut out = String::new();
                append_instructions(&mut out, blocks);
                out
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStartEvent {
    pub id: String,
    pub tool: Arc<str>,
    pub summary: String,
    pub render_header: Option<BufferSnapshot>,
    pub annotation: Option<String>,
    pub input: Option<ToolInput>,
    pub raw_input: Option<serde_json::Value>,
    pub output: Option<ToolOutput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDoneEvent {
    pub id: String,
    pub tool: Arc<str>,
    pub output: ToolOutput,
    pub is_error: bool,
    pub annotation: Option<String>,
    pub written_path: Option<String>,
}

const UNKNOWN_TOOL: &str = "unknown";

impl ToolDoneEvent {
    pub fn error(id: String, message: impl Into<String>) -> Self {
        let message: String = message.into();
        Self {
            id,
            tool: Arc::from(UNKNOWN_TOOL),
            output: ToolOutput::Plain(message.into()),
            is_error: true,
            annotation: None,
            written_path: None,
        }
    }

    pub fn written_path(&self) -> Option<&str> {
        if self.is_error {
            return None;
        }
        self.written_path
            .as_deref()
            .or_else(|| self.output.written_path())
    }

    pub fn wrote_to(&self, plan_path: &Path) -> bool {
        self.written_path()
            .is_some_and(|wp| Path::new(wp) == plan_path)
    }
}

fn collect_images(output: &ToolOutput, images: &mut Vec<ContentBlock>) {
    match output {
        ToolOutput::Image { source, .. } => images.push(ContentBlock::Image {
            source: source.clone(),
        }),
        ToolOutput::Batch { entries, .. } => {
            for entry in entries {
                if let Some(output) = &entry.output {
                    collect_images(output, images);
                }
            }
        }
        _ => {}
    }
}

pub fn tool_results(results: Vec<ToolDoneEvent>) -> Message {
    let mut content = Vec::with_capacity(results.len());
    let mut images = Vec::new();
    for r in results {
        content.push(ContentBlock::ToolResult {
            tool_use_id: r.id,
            content: r.output.as_text(),
            is_error: r.is_error,
        });
        collect_images(&r.output, &mut images);
    }
    // Anthropic wants every tool_result before other content in the user
    // message, so images go after all results.
    content.extend(images);
    Message {
        role: Role::User,
        content,
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
    /// `content` is the **full accumulated output** so far, not a delta.
    /// Producers must accumulate into a growing buffer and send the whole thing each flush.
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
    QueueItemConsumed {
        text: String,
        image_count: usize,
    },
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
        scopes: Vec<String>,
    },
    AuthRequired,
    Nudge,
    SubagentHistory {
        tool_use_id: String,
        messages: Vec<Message>,
    },
    ToolSnapshot {
        id: String,
        snapshot: BufferSnapshot,
        /// Which theme baked these colors. `None` for live output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_gen: Option<u64>,
    },
    ToolHeaderSnapshot {
        id: String,
        snapshot: BufferSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_gen: Option<u64>,
    },
    LiveToolBuf {
        id: String,
        body: Arc<SharedBuf>,
    },
}

/// Append-only buffer for streaming tool output to the UI. Writers append
/// under a Mutex, readers get a cheap Arc clone via `read_if_dirty()`.
pub struct SharedBuf {
    committed: Mutex<Arc<Vec<SnapshotLine>>>,
    dirty: AtomicBool,
}

impl SharedBuf {
    pub fn new() -> Self {
        Self {
            committed: Mutex::new(Arc::new(Vec::new())),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn append(&self, line: SnapshotLine) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Arc::make_mut(&mut guard).push(line);
        drop(guard);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn set_lines(&self, lines: Vec<SnapshotLine>) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Arc::new(lines);
        drop(guard);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn len(&self) -> usize {
        self.committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read_if_dirty(&self) -> Option<Arc<Vec<SnapshotLine>>> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(&guard))
    }

    pub fn take(&self) -> BufferSnapshot {
        self.dirty.store(false, Ordering::Release);
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        BufferSnapshot::from_arc(Arc::clone(&guard))
    }
}

impl Default for SharedBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SharedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBuf").finish_non_exhaustive()
    }
}

impl Serialize for SharedBuf {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_unit()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BufferSnapshot {
    pub lines: Arc<Vec<SnapshotLine>>,
}

impl BufferSnapshot {
    pub fn from_arc(lines: Arc<Vec<SnapshotLine>>) -> Self {
        Self { lines }
    }

    pub fn plain_text(text: String) -> Self {
        Self::from_arc(Arc::new(vec![SnapshotLine::plain(text)]))
    }

    pub fn first_line_text(&self) -> String {
        self.lines
            .first()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .unwrap_or_default()
    }

    /// Search matches against this, so it must mirror exactly what the UI
    /// renders.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(&span.text);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotLine {
    pub spans: Vec<SnapshotSpan>,
}

impl SnapshotLine {
    pub fn plain(text: String) -> Self {
        Self {
            spans: vec![SnapshotSpan {
                text,
                style: SpanStyle::Default,
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSpan {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum SpanStyle {
    #[default]
    Default,
    Named(String),
    Inline(InlineStyle),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InlineStyle {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub strikethrough: bool,
    pub reversed: bool,
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
    pub tool: String,
    pub status: BatchToolStatus,
    pub output: Option<ToolOutput>,
    pub summary: Option<String>,
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
    fn batch_entry_with_script_input_still_deserializes() {
        let json = r#"{"tool":"code_execution","summary":"1 lines","status":"Success","input":{"Script":{"language":"python","code":"print('x')"}},"output":null}"#;
        let entry: BatchToolEntry =
            serde_json::from_str(json).expect("old persisted session JSON must load");
        assert_eq!(
            entry.input,
            Some(ToolInput::Script {
                language: "python".into(),
                code: "print('x')".into(),
            })
        );
    }

    /// Search and the UI's error-dedup guard depend on this exact shape:
    /// spans join bare, lines join with one newline, no trailing newline.
    #[test]
    fn buffer_snapshot_text_joins_spans_and_lines() {
        let snap = BufferSnapshot::from_arc(Arc::new(vec![
            SnapshotLine {
                spans: vec![
                    SnapshotSpan {
                        text: "1 ".into(),
                        style: SpanStyle::Named("line_nr".into()),
                    },
                    SnapshotSpan {
                        text: "print('hi')".into(),
                        style: SpanStyle::Default,
                    },
                ],
            },
            SnapshotLine { spans: vec![] },
            SnapshotLine::plain("out".into()),
        ]));
        assert_eq!(snap.text(), "1 print('hi')\n\nout");
        assert_eq!(BufferSnapshot::from_arc(Arc::new(vec![])).text(), "");
    }

    #[test]
    fn as_display_text_diff_renders_unified_text() {
        let output = ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "keep\nold\n".into(),
            after: "keep\nnew\n".into(),
            summary: "Updated value".into(),
        };
        let display = output.as_display_text();
        assert!(display.starts_with("Updated value"));
        assert!(display.contains("--- src/main.rs"));
        assert!(display.contains("+++ src/main.rs"));
        assert!(display.contains("  keep"));
        assert!(display.contains("- old"));
        assert!(display.contains("+ new"));
        assert_eq!(output.as_text(), "Updated value");
    }

    #[test]
    fn as_text_grep_result_multi_file() {
        let output = ToolOutput::GrepResult {
            entries: vec![
                GrepFileEntry {
                    path: "src/a.rs".into(),
                    groups: vec![
                        GrepMatchGroup::single(3, "fn foo()"),
                        GrepMatchGroup::single(10, "fn bar()"),
                    ],
                },
                GrepFileEntry {
                    path: "src/b.rs".into(),
                    groups: vec![GrepMatchGroup::single(1, "use crate")],
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

    #[test]
    fn as_text_grep_result_with_context() {
        let output = ToolOutput::GrepResult {
            entries: vec![GrepFileEntry {
                path: "src/a.rs".into(),
                groups: vec![
                    GrepMatchGroup {
                        lines: vec![
                            GrepLine::context(2, "let x = 1;"),
                            GrepLine::matched(3, "fn foo()"),
                            GrepLine::context(4, "let y = 2;"),
                        ],
                    },
                    GrepMatchGroup::single(20, "fn bar()"),
                ],
            }],
        };
        let text = output.as_text();
        assert!(text.contains("2  let x = 1;"), "context before: {text}");
        assert!(text.contains("3: fn foo()"), "match line: {text}");
        assert!(text.contains("4  let y = 2;"), "context after: {text}");
        assert!(text.contains("--"), "group separator: {text}");
        assert!(text.contains("20: fn bar()"), "second group: {text}");
    }

    #[test_case(ToolOutput::WriteCode { path: "src/lib.rs".into(), byte_count: 10, lines: vec![] }, Some("src/lib.rs") ; "write_code")]
    #[test_case(ToolOutput::Diff { path: "src/lib.rs".into(), before: String::new(), after: String::new(), summary: String::new() }, Some("src/lib.rs") ; "diff")]
    #[test_case(ToolOutput::Plain("ok".into()), None ; "non_write_variant")]
    fn output_written_path(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.written_path(), expected);
    }

    #[test]
    fn tool_results_builds_message_with_tool_result_blocks() {
        let msg = tool_results(vec![
            ToolDoneEvent {
                id: "t1".into(),
                tool: Arc::from("bash"),
                output: ToolOutput::Plain("ok".into()),
                is_error: false,
                annotation: None,
                written_path: None,
            },
            ToolDoneEvent {
                id: "t2".into(),
                tool: Arc::from("read"),
                output: ToolOutput::Plain("fail".into()),
                is_error: true,
                annotation: None,
                written_path: None,
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

    #[test]
    fn tool_results_appends_images_after_all_results_including_batch() {
        let image = |data: &str| ToolOutput::Image {
            source: maki_providers::ImageSource::new(
                maki_providers::ImageMediaType::Png,
                Arc::from(data),
            ),
            text: "[image: pic.png 1KB]".into(),
        };
        let batch_output = ToolOutput::Batch {
            entries: vec![BatchToolEntry {
                tool: "view_image".into(),
                summary: String::new(),
                status: BatchToolStatus::Success,
                input: None,
                raw_input: None,
                output: Some(image("aW1n")),
                annotation: None,
            }],
            text: "batch output".into(),
        };
        let done = |id: &str, output: ToolOutput| ToolDoneEvent {
            id: id.into(),
            tool: Arc::from("t"),
            output,
            is_error: false,
            annotation: None,
            written_path: None,
        };

        let msg = tool_results(vec![
            done("t1", image("aGVsbG8=")),
            done("t2", batch_output),
            done("t3", ToolOutput::Plain("ok".into())),
        ]);
        assert_eq!(msg.content.len(), 5);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content == "[image: pic.png 1KB]")
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "t2" && content == "batch output")
        );
        assert!(
            matches!(&msg.content[2], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t3")
        );
        assert!(
            matches!(&msg.content[3], ContentBlock::Image { source } if &*source.data == "aGVsbG8=")
        );
        assert!(
            matches!(&msg.content[4], ContentBlock::Image { source } if &*source.data == "aW1n")
        );
    }

    #[test_case(
        10,
        vec!["fn foo()".into(), "fn bar()".into()],
        Some(vec![InstructionBlock { path: "AGENTS.md".into(), content: "do stuff".into() }]),
        "10: fn foo()\n11: fn bar()\n\n...\n\nTruncated lines: 12-100. Use offset=12 to read further."
        ; "with_instructions"
    )]
    #[test_case(
        1,
        vec!["line1".into()],
        None,
        "1: line1\n\n...\n\nTruncated lines: 2-100. Use offset=2 to read further."
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
    fn read_code_as_text_includes_instructions() {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line: 1,
            lines: vec!["fn main()".into()],
            total_lines: 1,
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
        };
        let text = output.as_text();
        assert!(text.contains("1: fn main()"));
        assert!(text.contains("Instructions from: AGENTS.md"));
        assert!(text.contains("do stuff"));
    }

    #[test]
    fn wrote_to_checks_path_and_error_flag() {
        let ok_event = ToolDoneEvent {
            id: "id".into(),
            tool: Arc::from("write"),
            output: ToolOutput::Plain("wrote 10 bytes".into()),
            is_error: false,
            annotation: None,
            written_path: Some("/plans/slug.md".into()),
        };
        assert!(!ok_event.wrote_to(Path::new("/plans/other.md")));

        let err_event = ToolDoneEvent {
            is_error: true,
            ..ok_event
        };
        assert!(!err_event.wrote_to(Path::new("/plans/slug.md")));
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

    #[test_case(100, 10, 2, 89 ; "middle_of_file")]
    #[test_case(100, 1, 1, 99  ; "first_line_only")]
    #[test_case(5, 1, 5, 0     ; "all_lines_shown")]
    #[test_case(5, 1, 2, 3     ; "partial_from_start")]
    #[test_case(5, 3, 3, 0     ; "partial_to_end")]
    #[test_case(0, 1, 1, 0     ; "backward_compat_total_zero")]
    #[test_case(0, 1, 0, 0     ; "empty_lines_total_zero")]
    #[test_case(10, 10, 1, 0   ; "last_line")]
    fn lines_remaining(total: usize, start: usize, shown: usize, expected: usize) {
        assert_eq!(lines_remaining_after(total, start, shown), expected);
    }

    fn line(text: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        }
    }

    #[test]
    fn shared_buf_lifecycle() {
        let buf = SharedBuf::new();

        assert!(buf.is_empty());
        assert!(buf.read_if_dirty().is_none());

        for i in 0..3 {
            buf.append(line(&format!("l{i}")));
        }
        assert_eq!(buf.len(), 3);

        let snap = buf.read_if_dirty().expect("dirty after appends");
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].spans[0].text, "l0");
        assert!(buf.read_if_dirty().is_none(), "clean after read");

        buf.append(line("l3"));
        let _ = buf.take();
        assert!(buf.read_if_dirty().is_none(), "take clears dirty");
    }

    #[test]
    fn shared_buf_arc_snapshot_isolation() {
        let buf = SharedBuf::new();
        buf.append(line("a"));
        buf.append(line("b"));
        let snap = buf.read_if_dirty().unwrap();
        buf.append(line("c"));
        assert_eq!(snap.len(), 2, "held Arc must not see new appends");
        let snap2 = buf.read_if_dirty().unwrap();
        assert_eq!(snap2.len(), 3);
    }

    #[test]
    fn shared_buf_poisoned_mutex_recovery() {
        let buf = Arc::new(SharedBuf::new());
        let buf2 = Arc::clone(&buf);
        let h = std::thread::spawn(move || {
            let _guard = buf2.committed.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join();
        buf.append(SnapshotLine { spans: vec![] });
    }

    #[test]
    fn buffer_snapshot_first_line_text() {
        let empty = BufferSnapshot {
            lines: Arc::new(vec![]),
        };
        assert_eq!(empty.first_line_text(), "");

        let multi = BufferSnapshot {
            lines: Arc::new(vec![SnapshotLine {
                spans: vec![
                    SnapshotSpan {
                        text: "hello ".into(),
                        style: SpanStyle::Default,
                    },
                    SnapshotSpan {
                        text: "world".into(),
                        style: SpanStyle::Named("bold".into()),
                    },
                ],
            }]),
        };
        assert_eq!(multi.first_line_text(), "hello world");
    }

    #[test_case(SpanStyle::Default ; "default")]
    #[test_case(SpanStyle::Named("comment".into()) ; "named")]
    #[test_case(SpanStyle::Inline(InlineStyle {
        fg: Some((255, 0, 0)),
        bg: None,
        bold: true,
        italic: false,
        underline: true,
        dim: false,
        strikethrough: false,
        reversed: true,
    }) ; "inline")]
    fn snapshot_span_serde_roundtrip(style: SpanStyle) {
        let span = SnapshotSpan {
            text: "test".into(),
            style,
        };
        let json = serde_json::to_string(&span).unwrap();
        let parsed: SnapshotSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, span);
    }

    #[test_case("", true  ; "plain_output_is_empty_for_empty_string")]
    #[test_case("a.rs\nb.rs", false ; "plain_output_not_empty_for_content")]
    fn plain_output_is_empty(text: &str, expected: bool) {
        assert_eq!(ToolOutput::Plain(text.into()).is_empty_result(), expected);
    }

    #[test]
    fn batch_tool_entry_raw_input_serde_round_trip() {
        const MSG: &str = "raw_input should survive serde round-trip";
        let entry = BatchToolEntry {
            tool: "read".into(),
            summary: "read a file".into(),
            status: BatchToolStatus::Success,
            input: None,
            raw_input: Some(serde_json::json!({"path": "/a.rs"})),
            output: None,
            annotation: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: BatchToolEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.raw_input,
            Some(serde_json::json!({"path": "/a.rs"})),
            "{MSG}"
        );
    }

    #[test]
    fn batch_tool_entry_raw_input_none_omitted_in_json() {
        const MSG: &str = "raw_input: None must not appear in serialized JSON";
        let entry = BatchToolEntry {
            tool: "bash".into(),
            summary: "run cmd".into(),
            status: BatchToolStatus::Pending,
            input: None,
            raw_input: None,
            output: None,
            annotation: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("raw_input"), "{MSG}");
    }

    #[test]
    fn batch_tool_entry_missing_raw_input_deserializes_as_none() {
        const MSG: &str = "missing raw_input field must deserialize as None (backwards compat)";
        let json =
            r#"{"tool":"grep","summary":"search","status":"Pending","input":null,"output":null}"#;
        let entry: BatchToolEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.raw_input, None, "{MSG}");
    }

    #[test]
    fn agent_event_tool_snapshot_theme_gen_backwards_compat() {
        const OMIT_MSG: &str = "theme_gen: None must not appear in serialized JSON";
        const COMPAT_MSG: &str = "missing theme_gen must deserialize as None (backwards compat)";

        let event = AgentEvent::ToolSnapshot {
            id: "t1".into(),
            snapshot: BufferSnapshot {
                lines: Arc::new(vec![]),
            },
            theme_gen: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("theme_gen"), "{OMIT_MSG}");

        #[derive(Deserialize)]
        struct ToolSnapshotFields {
            #[allow(dead_code)]
            id: String,
            #[serde(default)]
            theme_gen: Option<u64>,
        }
        let json_without = r#"{"id":"t1"}"#;
        let parsed: ToolSnapshotFields = serde_json::from_str(json_without).unwrap();
        assert_eq!(parsed.theme_gen, None, "{COMPAT_MSG}");
    }

    #[test]
    fn text_output_serde_legacy_bare_string() {
        const MSG: &str = "old sessions store Plain as a bare string";
        let json = r#"{"Plain":"hello world"}"#;
        let output: ToolOutput = serde_json::from_str(json).unwrap();
        match &output {
            ToolOutput::Plain(t) => {
                assert_eq!(t.text, "hello world", "{MSG}");
                assert!(t.instructions.is_none(), "{MSG}");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn text_output_serde_full_roundtrip() {
        const MSG: &str = "new format with instructions must roundtrip";
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: "be nice".into(),
        }];
        let output = ToolOutput::Plain(TextOutput {
            text: "file contents".into(),
            instructions: Some(blocks),
        });
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ToolOutput = serde_json::from_str(&json).unwrap();
        match &parsed {
            ToolOutput::Plain(t) => {
                assert_eq!(t.text, "file contents", "{MSG}");
                let inst = t.instructions.as_ref().expect("instructions missing");
                assert_eq!(inst.len(), 1, "{MSG}");
                assert_eq!(inst[0].path, "AGENTS.md", "{MSG}");
                assert_eq!(inst[0].content, "be nice", "{MSG}");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test_case(
        ToolOutput::WriteCode { path: "/old/path".into(), byte_count: 10, lines: vec![] },
        Some("/new/path".into()), false, Some("/new/path")
        ; "prefers_field_over_output"
    )]
    #[test_case(
        ToolOutput::Diff { path: "/diff/path".into(), before: String::new(), after: String::new(), summary: String::new() },
        None, false, Some("/diff/path")
        ; "falls_back_to_output"
    )]
    #[test_case(
        ToolOutput::Plain("failed".into()), Some("/some/path".into()), true, None
        ; "none_when_error"
    )]
    fn tool_done_written_path(
        output: ToolOutput,
        written_path: Option<String>,
        is_error: bool,
        expected: Option<&str>,
    ) {
        let event = ToolDoneEvent {
            id: "id".into(),
            tool: Arc::from("tool"),
            output,
            is_error,
            annotation: None,
            written_path,
        };
        assert_eq!(event.written_path(), expected);
    }

    #[test]
    fn plain_with_instructions_as_text_includes_instructions() {
        const INCLUDES_MSG: &str = "as_text must include instructions";
        const EXCLUDES_MSG: &str = "as_display_text must exclude instructions";
        let output = ToolOutput::Plain(TextOutput {
            text: "fn main()".into(),
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
        });
        let text = output.as_text();
        assert!(text.contains("fn main()"), "{INCLUDES_MSG}");
        assert!(
            text.contains("Instructions from: AGENTS.md"),
            "{INCLUDES_MSG}"
        );
        assert!(text.contains("do stuff"), "{INCLUDES_MSG}");

        let display = output.as_display_text();
        assert!(display.contains("fn main()"), "{EXCLUDES_MSG}");
        assert!(!display.contains("Instructions from:"), "{EXCLUDES_MSG}");
    }
}
