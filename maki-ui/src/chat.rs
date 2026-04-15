//! `history_to_display` rebuilds UI messages from raw API history + stored tool outputs.
//! Stored `ToolOutput` gets syntax highlighting; absent outputs fall back to plain text
//! from `ToolResult`. Webfetch bodies are hidden to save screen space.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::components::messages::MessagesPanel;
use crate::components::tool_display::{ToolKind, append_annotation, tool_output_annotation};
use crate::components::{DisplayMessage, DisplayRole, ToolRole, ToolStatus};
use crate::markdown::truncate_output;

use crate::selection::Selection;
use maki_agent::tools::{ToolCall, WEBFETCH_TOOL_NAME};
use maki_agent::{
    AgentEvent, BatchToolStatus, NO_FILES_FOUND, QuestionInfo, ToolDoneEvent, ToolOutput,
    ToolStartEvent,
};
use maki_config::{ToolOutputLines, UiConfig};
use maki_providers::{ContentBlock, Message, Role, TokenUsage};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

pub(crate) const DONE_TEXT: &str = "Done!";
pub(crate) const ERROR_TEXT: &str = "Error";
pub(crate) const CANCELLED_TEXT: &str = "Cancelled";

pub enum ChatEventResult {
    Continue,
    Done,
    QueueItemConsumed {
        text: String,
        image_count: usize,
    },
    Error(String),
    QuestionPrompt {
        questions: Vec<QuestionInfo>,
    },
    PermissionRequest {
        id: String,
        tool: String,
        scopes: Vec<String>,
    },
    AuthRequired,
}

pub struct Chat {
    pub name: String,
    pub token_usage: TokenUsage,
    pub context_size: u32,
    pub model_id: Option<String>,
    pending_turn_usage: Option<String>,
    messages_panel: MessagesPanel,
    finished: bool,
}

impl Chat {
    pub fn new(name: String, ui_config: UiConfig) -> Self {
        Self {
            name,
            token_usage: TokenUsage::default(),
            context_size: 0,
            model_id: None,
            pending_turn_usage: None,
            messages_panel: MessagesPanel::new(ui_config),
            finished: false,
        }
    }

    pub fn set_pending_turn_usage(&mut self, usage: String) {
        self.pending_turn_usage = Some(usage);
    }

    pub fn handle_event(&mut self, event: AgentEvent, plan_path: Option<&Path>) -> ChatEventResult {
        match event {
            AgentEvent::ThinkingDelta { text } => self.messages_panel.thinking_delta(&text),
            AgentEvent::TextDelta { text } => self.messages_panel.text_delta(&text),
            AgentEvent::ToolPending { id, name } => self.messages_panel.tool_pending(id, &name),
            AgentEvent::ToolStart(e) => self.messages_panel.tool_start(*e),
            AgentEvent::ToolOutput { id, content } => {
                self.messages_panel.tool_output(&id, &content)
            }
            AgentEvent::ToolDone(e) => {
                let plan_write = plan_path.filter(|pp| e.wrote_to(pp));
                let is_write = matches!(e.output, ToolOutput::WriteCode { .. });
                self.messages_panel.tool_done(*e);
                if let Some(pp) = plan_write {
                    let content = if is_write {
                        std::fs::read_to_string(pp).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    self.messages_panel
                        .push(DisplayMessage::plan(content, pp.display().to_string()));
                }
            }
            AgentEvent::BatchProgress(e) => {
                self.messages_panel
                    .batch_progress(&e.batch_id, e.index, e.status, e.output);
            }
            AgentEvent::QuestionPrompt { questions, .. } => {
                return ChatEventResult::QuestionPrompt { questions };
            }
            AgentEvent::TurnComplete(_) => {}
            AgentEvent::ToolResultsSubmitted { .. } => {
                if let Some(usage) = self.pending_turn_usage.take() {
                    self.messages_panel.set_turn_usage_on_last_tool(usage);
                }
            }
            AgentEvent::AutoCompacting => {
                self.messages_panel.flush();
                self.messages_panel.push(DisplayMessage::new(
                    DisplayRole::Assistant,
                    "Auto-compacting conversation...".into(),
                ));
            }
            AgentEvent::QueueItemConsumed { text, image_count } => {
                return ChatEventResult::QueueItemConsumed { text, image_count };
            }
            AgentEvent::Retry { .. } => unreachable!("handled before handle_event"),
            AgentEvent::Done { .. } => {
                self.messages_panel.flush();
                return ChatEventResult::Done;
            }
            AgentEvent::Error { message } => {
                self.messages_panel.flush();
                return ChatEventResult::Error(message);
            }
            AgentEvent::PermissionRequest { id, tool, scopes } => {
                return ChatEventResult::PermissionRequest { id, tool, scopes };
            }
            AgentEvent::AuthRequired => {
                return ChatEventResult::AuthRequired;
            }
            AgentEvent::SubagentHistory { .. } => {}
        }
        ChatEventResult::Continue
    }

    pub fn scroll(&mut self, delta: i32) {
        self.messages_panel.scroll(delta);
    }

    pub fn half_page(&self) -> i32 {
        self.messages_panel.half_page()
    }

    pub fn auto_scroll(&self) -> bool {
        self.messages_panel.auto_scroll()
    }

    pub fn scroll_to_top(&mut self) {
        self.messages_panel.scroll_to_top();
    }

    pub fn enable_auto_scroll(&mut self) {
        self.messages_panel.enable_auto_scroll();
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.messages_panel.scroll_to_segment(segment_index);
    }

    pub fn restore_scroll(&mut self, scroll_top: u16, auto_scroll: bool) {
        self.messages_panel.restore_scroll(scroll_top, auto_scroll);
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.messages_panel.set_highlight_segment(idx);
    }

    pub fn set_accent(&mut self, color: Color) {
        self.messages_panel.set_accent(color);
    }

    pub fn is_animating(&self) -> bool {
        self.messages_panel.is_animating()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool) {
        self.messages_panel.view(frame, area, has_selection);
    }

    pub fn scroll_top(&self) -> u16 {
        self.messages_panel.scroll_top()
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        self.messages_panel.segment_heights()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.messages_panel.segment_search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        self.messages_panel.extract_selection_text(sel, msg_area)
    }

    pub fn toggle_expansion_at(&mut self, row: u16, area: Rect) -> bool {
        self.messages_panel.toggle_expansion_at(row, area)
    }

    pub fn stream_reset(&mut self) {
        self.messages_panel.stream_reset();
    }

    pub fn flush(&mut self) {
        self.messages_panel.flush();
    }

    pub fn cancel_in_progress(&mut self) {
        self.messages_panel.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.messages_panel.fail_in_progress_with_message(message);
    }

    pub fn push(&mut self, msg: DisplayMessage) {
        self.messages_panel.push(msg);
    }

    pub fn mark_finished(&mut self, role: DisplayRole, text: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.messages_panel.flush();
        self.messages_panel
            .push(DisplayMessage::new(role, text.into()));
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.messages_panel.update_tool_summary(tool_id, summary);
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.messages_panel.update_tool_model(tool_id, model);
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.messages_panel.load_messages(msgs);
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages_panel
            .push(DisplayMessage::new(DisplayRole::User, text.into()));
    }

    pub fn shell_tool_start(&mut self, event: ToolStartEvent) {
        self.messages_panel.tool_start(event);
    }

    pub fn shell_tool_output(&mut self, id: &str, content: &str) {
        self.messages_panel.tool_output(id, content);
    }

    pub fn shell_tool_done(&mut self, event: ToolDoneEvent) {
        self.messages_panel.tool_done(event);
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages_panel.message_count()
    }

    #[cfg(test)]
    pub fn in_progress_count(&self) -> usize {
        self.messages_panel.in_progress_count()
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages_panel.last_message_text()
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages_panel.last_message_is_plan()
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages_panel.last_message_role()
    }
}

pub fn history_to_display(
    messages: &[Message],
    tool_outputs: &HashMap<String, ToolOutput>,
    tool_output_lines: &ToolOutputLines,
) -> Vec<DisplayMessage> {
    let results = build_tool_results_map(messages);
    let mut display = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                if let Some(text) = msg.user_text() {
                    display.push(DisplayMessage::new(DisplayRole::User, text.to_owned()));
                }
            }
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            display.push(DisplayMessage::new(DisplayRole::Assistant, text.clone()));
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            display
                                .push(DisplayMessage::new(DisplayRole::Thinking, thinking.clone()));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            let static_name = ToolCall::static_name(name).unwrap_or("unknown");
                            let tool_call = ToolCall::from_api(name, input).ok();
                            let summary = tool_call
                                .as_ref()
                                .map(|tc| tc.start_summary())
                                .unwrap_or_else(|| name.clone());
                            let tool_input = tool_call.as_ref().and_then(|tc| tc.start_input());
                            let (status, result_text) = results
                                .get(id.as_str())
                                .map(|(err, text)| {
                                    let s = if *err {
                                        ToolStatus::Error
                                    } else {
                                        ToolStatus::Success
                                    };
                                    (s, Some(&**text))
                                })
                                .unwrap_or((ToolStatus::Success, None));
                            let stored = tool_outputs.get(id).cloned();
                            let (text, truncated_lines, tool_output, mut annotation) =
                                build_loaded_tool(
                                    static_name,
                                    &summary,
                                    stored,
                                    result_text,
                                    tool_output_lines,
                                );
                            if let Some(ta) =
                                tool_call.as_ref().and_then(|tc| tc.start_annotation())
                            {
                                append_annotation(&mut annotation, &ta);
                            }
                            display.push(DisplayMessage {
                                role: DisplayRole::Tool(Box::new(ToolRole {
                                    id: id.clone(),
                                    status,
                                    name: static_name,
                                })),
                                text,
                                tool_input: tool_input.map(Arc::new),
                                tool_output,
                                live_output: None,
                                annotation,
                                plan_path: None,
                                timestamp: None,
                                turn_usage: None,
                                truncated_lines,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    display
}

/// Replicate the live `tool_done` rendering: set structured output for
/// syntax highlighting, truncate plain text bodies, compute annotations.
fn build_loaded_tool(
    tool: &str,
    summary: &str,
    reconstructed: Option<ToolOutput>,
    result_text: Option<&str>,
    tool_output_lines: &ToolOutputLines,
) -> (String, usize, Option<Arc<ToolOutput>>, Option<String>) {
    let kind = ToolKind::from_name(tool);
    match reconstructed {
        Some(ref output @ ToolOutput::GlobResult { .. }) => {
            let annotation = tool_output_annotation(output, kind);
            let text = if output.is_empty_result() {
                format!("{summary}\n{NO_FILES_FOUND}")
            } else {
                let display = output.as_display_text();
                let limits = kind.output_limits(tool_output_lines);
                let tr = truncate_output(&display, limits.max_lines, limits.keep);
                format!("{}\n{}", summary, tr.kept)
            };
            (text, 0, reconstructed.map(Arc::new), annotation)
        }
        Some(ref output @ ToolOutput::GrepResult { .. }) => {
            let annotation = tool_output_annotation(output, kind);
            (
                summary.to_owned(),
                0,
                reconstructed.map(Arc::new),
                annotation,
            )
        }
        Some(ToolOutput::Batch { ref entries, .. }) => {
            let failed = entries
                .iter()
                .filter(|e| e.status == BatchToolStatus::Error)
                .count();
            let text = if failed > 0 {
                let total = entries.len();
                format!("{}/{total} tools succeeded", total - failed)
            } else {
                summary.to_owned()
            };
            (text, 0, reconstructed.map(Arc::new), None)
        }
        Some(ref output) => {
            let annotation = tool_output_annotation(output, kind);
            (
                summary.to_owned(),
                0,
                reconstructed.map(Arc::new),
                annotation,
            )
        }
        None => {
            let result = result_text.unwrap_or("");
            let annotation = if !result.is_empty() {
                tool_output_annotation(&ToolOutput::Plain(result.into()), kind)
            } else {
                None
            };
            if result.is_empty() || matches!(tool, WEBFETCH_TOOL_NAME) {
                (summary.to_owned(), 0, None, annotation)
            } else {
                let limits = kind.output_limits(tool_output_lines);
                let tr = truncate_output(result, limits.max_lines, limits.keep);
                (
                    format!("{}\n{}", summary, tr.kept),
                    tr.skipped,
                    None,
                    annotation,
                )
            }
        }
    }
}

fn build_tool_results_map(messages: &[Message]) -> HashMap<&str, (bool, &str)> {
    let mut map = HashMap::new();
    for msg in messages {
        if !matches!(msg.role, Role::User) {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            {
                map.insert(tool_use_id.as_str(), (*is_error, content.as_str()));
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::{
        AgentEvent, BatchToolEntry, BatchToolStatus, ToolDoneEvent, ToolInput, ToolOutput,
        ToolStartEvent,
    };
    use maki_config::UiConfig;
    use test_case::test_case;

    fn tool_start(id: &str, tool: &'static str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.into(),
            tool,
            summary: String::new(),
            annotation: None,
            input: None,
            output: None,
        }))
    }

    fn tool_done(id: &str, tool: &'static str, output: ToolOutput) -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: id.into(),
            tool,
            output,
            is_error: false,
        }))
    }

    fn write_output(path: &str) -> ToolOutput {
        ToolOutput::WriteCode {
            path: path.into(),
            byte_count: 42,
            lines: vec![],
        }
    }

    fn edit_output(path: &str) -> ToolOutput {
        ToolOutput::Diff {
            path: path.into(),
            before: String::new(),
            after: String::new(),
            summary: String::new(),
        }
    }

    fn empty_outputs() -> HashMap<String, ToolOutput> {
        HashMap::new()
    }

    #[test]
    fn tool_lifecycle() {
        let mut chat = Chat::new("Main".into(), UiConfig::default());
        chat.handle_event(tool_start("t1", "bash"), None);
        assert_eq!(chat.in_progress_count(), 1);

        chat.handle_event(
            tool_done("t1", "bash", ToolOutput::Plain("ok".into())),
            None,
        );
        assert_eq!(chat.in_progress_count(), 0);
    }

    #[test]
    fn plan_write_renders_file_content() {
        let mut chat = Chat::new("Main".into(), UiConfig::default());
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("w1", "write"), Some(plan_path.as_path()));
        chat.handle_event(
            tool_done("w1", "write", write_output(plan_str)),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        let last = chat.last_message_text();
        assert!(last.contains("# My Plan"));
    }

    #[test]
    fn plan_write_ignores_different_path() {
        let mut chat = Chat::new("Main".into(), UiConfig::default());
        let plan_path = Path::new("/plans/123.md");
        chat.handle_event(tool_start("w1", "write"), Some(plan_path));
        chat.handle_event(
            tool_done("w1", "write", write_output("src/main.rs")),
            Some(plan_path),
        );
        assert!(!chat.last_message_is_plan());
    }

    #[test]
    fn plan_edit_shows_path_only() {
        let mut chat = Chat::new("Main".into(), UiConfig::default());
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("e1", "edit"), Some(plan_path.as_path()));
        chat.handle_event(
            tool_done("e1", "edit", edit_output(plan_str)),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        assert!(chat.last_message_text().is_empty());
    }

    #[test]
    fn history_skips_empty_text() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: String::new(),
            }],
            ..Default::default()
        }];
        assert!(
            history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default()).is_empty()
        );
    }

    fn tool_use_pair(
        tool: &str,
        input: serde_json::Value,
        result: &str,
        is_error: bool,
    ) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: tool.into(),
                    input,
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: result.into(),
                    is_error,
                }],
                ..Default::default()
            },
        ]
    }

    #[test_case(false, ToolStatus::Success ; "success")]
    #[test_case(true,  ToolStatus::Error   ; "error")]
    fn history_tool_result_status(is_error: bool, expected: ToolStatus) {
        let msgs = tool_use_pair(
            "bash",
            serde_json::json!({"command": "ls"}),
            "output",
            is_error,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        assert_eq!(display.len(), 1);
        assert!(matches!(&display[0].role, DisplayRole::Tool(t) if t.status == expected));
    }

    #[test]
    fn history_mixed_conversation() {
        let msgs = vec![
            Message::user("do something".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Sure, let me help.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "echo hi"}),
                    },
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "hi".into(),
                    is_error: false,
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Done!".into(),
                }],
                ..Default::default()
            },
        ];
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        assert_eq!(display.len(), 4);
        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[1].role, DisplayRole::Assistant);
        assert!(matches!(display[2].role, DisplayRole::Tool(_)));
        assert_eq!(display[3].role, DisplayRole::Assistant);
        assert_eq!(display[3].text, "Done!");
    }

    #[test]
    fn history_stored_output_variants_pass_through() {
        let variants: Vec<(&str, serde_json::Value, ToolOutput)> = vec![
            (
                "edit",
                serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
                ToolOutput::Diff {
                    path: "a".into(),
                    before: "x\n".into(),
                    after: "y\n".into(),
                    summary: "edited a".into(),
                },
            ),
            (
                "read",
                serde_json::json!({"path": "/src/main.rs"}),
                ToolOutput::ReadCode {
                    path: "/src/main.rs".into(),
                    start_line: 1,
                    lines: vec!["fn main() {}".into()],
                    total_lines: 1,
                    instructions: None,
                },
            ),
            (
                "grep",
                serde_json::json!({"pattern": "TODO"}),
                ToolOutput::GrepResult { entries: vec![] },
            ),
            (
                "todo_write",
                serde_json::json!({"todos": []}),
                ToolOutput::TodoList(vec![]),
            ),
        ];
        for (tool_name, input_json, output) in variants {
            let discriminant = std::mem::discriminant(&output);
            let msgs = tool_use_pair(tool_name, input_json, "ok", false);
            let outputs = HashMap::from([("t1".into(), output)]);
            let display = history_to_display(&msgs, &outputs, &ToolOutputLines::default());
            assert_eq!(
                std::mem::discriminant(display[0].tool_output.as_deref().unwrap()),
                discriminant,
                "stored {tool_name} output should pass through"
            );
        }
    }

    #[test]
    fn history_stored_write_has_annotation() {
        let write_output = ToolOutput::WriteCode {
            path: "/src/main.rs".into(),
            byte_count: 12,
            lines: vec!["fn main() {}".into()],
        };
        let msgs = tool_use_pair(
            "write",
            serde_json::json!({"path": "/src/main.rs", "content": "fn main() {}"}),
            "wrote 12 bytes",
            false,
        );
        let outputs = HashMap::from([("t1".into(), write_output)]);
        let display = history_to_display(&msgs, &outputs, &ToolOutputLines::default());
        assert!(display[0].annotation.is_some());
    }

    #[test]
    fn history_bash_has_code_input() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            }],
            ..Default::default()
        }];
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        assert!(
            matches!(
                display[0].tool_input.as_deref(),
                Some(ToolInput::Code { .. })
            ),
            "bash tool should produce Code input for syntax highlighting"
        );
    }

    #[test]
    fn history_bash_output_truncated() {
        let long_output = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let joined = long_output.join("\n");
        let msgs = tool_use_pair(
            "bash",
            serde_json::json!({"command": "cmd"}),
            &joined,
            false,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        let line_count = display[0].text.lines().count();
        assert!(
            line_count < long_output.len(),
            "output should be truncated, got {line_count} lines for {} input lines",
            long_output.len()
        );
    }

    #[test]
    fn history_webfetch_hides_body() {
        let msgs = tool_use_pair(
            "webfetch",
            serde_json::json!({"url": "https://example.com"}),
            "fetched content\nmore lines",
            false,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        assert!(
            !display[0].text.contains('\n'),
            "webfetch should hide body text"
        );
    }

    #[test]
    fn history_stored_batch_with_errors_shows_count() {
        let batch_output = ToolOutput::Batch {
            entries: vec![
                BatchToolEntry {
                    tool: "read".into(),
                    summary: "/a.rs".into(),
                    status: BatchToolStatus::Success,
                    input: None,
                    output: None,
                    annotation: None,
                },
                BatchToolEntry {
                    tool: "read".into(),
                    summary: "/missing".into(),
                    status: BatchToolStatus::Error,
                    input: None,
                    output: None,
                    annotation: None,
                },
            ],
            text: String::new(),
        };
        let msgs = tool_use_pair("batch", serde_json::json!({"tool_calls": []}), "", false);
        let outputs = HashMap::from([("t1".into(), batch_output)]);
        let display = history_to_display(&msgs, &outputs, &ToolOutputLines::default());
        let ToolOutput::Batch { entries, .. } = display[0].tool_output.as_deref().unwrap() else {
            panic!("expected Batch output");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].status, BatchToolStatus::Error);
        assert!(display[0].text.contains("1/2"));
    }

    #[test]
    fn history_no_stored_output_falls_back_to_plain_text() {
        let msgs = tool_use_pair(
            "read",
            serde_json::json!({"path": "/src/main.rs"}),
            "1: fn main() {}",
            false,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        assert!(display[0].tool_output.is_none());
        assert!(display[0].text.contains("fn main"));
    }

    #[test]
    fn history_to_display_thinking_blocks() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::RedactedThinking { data: "x".into() },
            ],
            ..Default::default()
        }];
        let display = history_to_display(&msgs, &HashMap::new(), &ToolOutputLines::default());
        assert_eq!(display.len(), 2);
        assert_eq!(display[0].role, DisplayRole::Thinking);
        assert_eq!(display[0].text, "reasoning");
        assert_eq!(display[1].role, DisplayRole::Assistant);
    }
}
