pub(crate) mod btw_modal;
pub(crate) mod code_view;
pub mod command;
pub(crate) mod form;
pub(crate) mod help_modal;
pub(crate) mod index_highlight;
pub mod input;
pub(crate) mod keybindings;
pub(crate) mod list_picker;
pub(crate) mod mcp_picker;
pub(crate) mod memory_modal;
pub mod messages;
pub(crate) mod modal;
pub(crate) mod model_picker;
pub(crate) mod permission_prompt;
pub(crate) mod plan_form;
pub mod question_form;
pub mod queue_panel;
pub(crate) mod rewind_picker;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod session_picker;
pub mod status_bar;
pub(crate) mod streaming_content;
pub(crate) mod theme_picker;
pub(crate) mod todo_panel;
pub(crate) mod tool_display;

pub(crate) const CHEVRON: &str = "❯ ";

pub(crate) trait Overlay {
    fn is_open(&self) -> bool;
    fn close(&mut self);
    /// Modal overlays block mouse interaction behind them.
    fn is_modal(&self) -> bool {
        true
    }
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_agent::AgentInput;
use maki_agent::{ToolInput, ToolOutput};
use maki_providers::Message;
use ratatui::text::{Line, Span};

pub(crate) fn hint_line(pairs: &[(&str, &str)]) -> Line<'static> {
    let t = crate::theme::current();
    let mut spans = Vec::with_capacity(pairs.len() * 2);
    for (key, desc) in pairs {
        spans.push(Span::styled(format!("  {key}"), t.keybind_key));
        spans.push(Span::styled(format!(" {desc}"), t.form_hint));
    }
    Line::from(spans)
}

pub(crate) fn visual_line_count(text_len: usize, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    text_len.div_ceil(width).max(1)
}

pub(crate) fn apply_scroll_delta(offset: u16, delta: i32) -> u16 {
    if delta > 0 {
        offset.saturating_sub(delta as u16)
    } else {
        offset.saturating_add(delta.unsigned_abs() as u16)
    }
}

pub fn is_ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

pub(crate) struct ModalScroll {
    offset: u16,
    max_offset: u16,
    viewport_h: u16,
    auto_scroll: bool,
}

impl ModalScroll {
    pub fn new() -> Self {
        Self {
            offset: 0,
            max_offset: 0,
            viewport_h: 0,
            auto_scroll: true,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn offset(&self) -> u16 {
        self.offset
    }

    pub fn update_dimensions(&mut self, total: u16, viewport_h: u16) {
        self.viewport_h = viewport_h;
        self.max_offset = total.saturating_sub(viewport_h);
        if self.auto_scroll {
            self.offset = self.max_offset;
        } else {
            self.clamp();
            if self.offset >= self.max_offset {
                self.auto_scroll = true;
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.offset = apply_scroll_delta(self.offset, delta);
        self.clamp();
        self.auto_scroll = self.offset >= self.max_offset;
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        use keybindings::key;
        match key_event.code {
            KeyCode::Up => self.scroll(1),
            KeyCode::Down => self.scroll(-1),
            _ if key::SCROLL_HALF_UP.matches(key_event) => self.scroll(self.half_page()),
            _ if key::SCROLL_HALF_DOWN.matches(key_event) => self.scroll(-self.half_page()),
            _ if key::SCROLL_LINE_UP.matches(key_event) => self.scroll(1),
            _ if key::SCROLL_LINE_DOWN.matches(key_event) => self.scroll(-1),
            _ if key::SCROLL_TOP.matches(key_event) => {
                self.offset = 0;
                self.auto_scroll = false;
            }
            _ if key::SCROLL_BOTTOM.matches(key_event) => {
                self.auto_scroll = true;
                self.offset = self.max_offset;
            }
            _ => return false,
        }
        true
    }

    fn half_page(&self) -> i32 {
        (self.viewport_h / 2).max(1) as i32
    }

    fn clamp(&mut self) {
        self.offset = self.offset.min(self.max_offset);
    }
}

pub struct LoadedSession {
    pub messages: Vec<Message>,
    pub tool_outputs: HashMap<String, ToolOutput>,
}

use std::path::PathBuf;

pub enum Action {
    SendMessage(Box<AgentInput>),
    ShellCommand {
        id: String,
        command: String,
        visible: bool,
    },
    CancelAgent,
    NewSession,
    LoadSession(Box<LoadedSession>),
    ChangeModel(String),
    Compact,
    ToggleMcp(String, bool),
    OpenEditor(PathBuf),
    Btw(String),
    Quit,
}

const ERROR_DISPLAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum Status {
    Idle,
    Streaming,
    Error { message: String, since: Instant },
}

impl Status {
    pub fn error(message: String) -> Self {
        Self::Error {
            message,
            since: Instant::now(),
        }
    }

    pub fn is_error_expired(&self) -> bool {
        matches!(self, Self::Error { since, .. } if since.elapsed() >= ERROR_DISPLAY)
    }
}

impl PartialEq for Status {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Idle, Self::Idle)
                | (Self::Streaming, Self::Streaming)
                | (Self::Error { .. }, Self::Error { .. })
        )
    }
}

pub struct RetryInfo {
    pub attempt: u32,
    pub message: String,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolStatus {
    InProgress,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: DisplayRole,
    pub text: String,
    pub tool_input: Option<Arc<ToolInput>>,
    pub tool_output: Option<Arc<ToolOutput>>,
    pub live_output: Option<String>,
    pub annotation: Option<String>,
    pub plan_path: Option<String>,
    pub timestamp: Option<String>,
    pub turn_usage: Option<String>,
    pub truncated_lines: usize,
}

impl DisplayMessage {
    pub fn new(role: DisplayRole, text: String) -> Self {
        Self {
            role,
            text,
            tool_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
        }
    }

    /// Source of truth for `Segment::copy_text`. Tool messages reconstruct
    /// from structured data (diffs, grep, etc.); non-tool messages return raw
    /// text (preserving markdown).
    pub fn copy_text(&self) -> String {
        match &self.role {
            DisplayRole::Tool(t) => {
                let name = t.name;
                let (header, body) = self.text.split_once('\n').unwrap_or((&self.text, ""));
                let mut out = format!("{name}> {header}");
                if let Some(ToolInput::Code { code, .. }) = self.tool_input.as_deref() {
                    out.push('\n');
                    out.push_str(code.trim_end());
                }
                match self.tool_output.as_deref() {
                    Some(
                        structured @ (ToolOutput::Diff { .. }
                        | ToolOutput::ReadCode { .. }
                        | ToolOutput::ReadDir { .. }
                        | ToolOutput::WriteCode { .. }
                        | ToolOutput::GrepResult { .. }
                        | ToolOutput::GlobResult { .. }
                        | ToolOutput::TodoList(_)),
                    ) => {
                        out.push('\n');
                        out.push_str(&structured.as_display_text());
                    }
                    Some(ToolOutput::Batch { entries, .. }) => {
                        for entry in entries {
                            out.push_str(&format!("\n  {} {}", entry.tool, entry.summary));
                            if let Some(output) = &entry.output {
                                let text = output.as_display_text();
                                if !text.is_empty() {
                                    out.push('\n');
                                    out.push_str(&text);
                                }
                            }
                        }
                    }
                    _ if !body.is_empty() => {
                        out.push('\n');
                        out.push_str(body);
                    }
                    _ => {}
                }
                out
            }
            _ => self.text.clone(),
        }
    }

    pub fn plan(text: String, plan_path: String) -> Self {
        Self {
            role: DisplayRole::Assistant,
            text,
            tool_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            plan_path: Some(plan_path),
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRole {
    pub id: String,
    pub status: ToolStatus,
    pub name: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    Thinking,
    Tool(Box<ToolRole>),
    Error,
    Done,
}

impl DisplayRole {
    pub fn tool_name(&self) -> Option<&'static str> {
        match self {
            DisplayRole::Tool(t) => Some(t.name),
            _ => None,
        }
    }
}

#[cfg(test)]
use maki_providers::ModelPricing;

#[cfg(test)]
pub(crate) const TEST_CONTEXT_WINDOW: u32 = 200_000;

#[cfg(test)]
pub(crate) fn test_pricing() -> ModelPricing {
    ModelPricing {
        input: 3.0,
        output: 15.0,
        cache_write: 3.75,
        cache_read: 0.30,
    }
}

#[cfg(test)]
pub(crate) fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent {
        code,
        modifiers: crossterm::event::KeyModifiers::NONE,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(0, 80, 1 ; "empty_text")]
    #[test_case(0, 0, 1 ; "zero_width")]
    #[test_case(5, 5, 1 ; "exact_fit")]
    #[test_case(6, 5, 2 ; "one_char_overflow")]
    fn visual_line_count_cases(text_len: usize, width: usize, expected: usize) {
        assert_eq!(visual_line_count(text_len, width), expected);
    }

    #[test_case(10, 3, 7   ; "scroll_up")]
    #[test_case(10, -3, 13 ; "scroll_down")]
    #[test_case(0, 5, 0    ; "clamp_underflow")]
    fn apply_scroll_delta_cases(offset: u16, delta: i32, expected: u16) {
        assert_eq!(apply_scroll_delta(offset, delta), expected);
    }

    fn tool_msg(text: &str) -> DisplayMessage {
        DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: "read",
            })),
            text.into(),
        )
    }

    #[test]
    fn copy_text_non_tool_returns_text() {
        let msg = DisplayMessage::new(DisplayRole::Assistant, "hello\nworld".into());
        assert_eq!(msg.copy_text(), "hello\nworld");
    }

    #[test]
    fn copy_text_tool_structured_output_uses_as_display_text() {
        let mut msg = tool_msg("read /src/main.rs\nignored body");
        msg.tool_output = Some(Arc::new(ToolOutput::ReadCode {
            path: "main.rs".into(),
            start_line: 1,
            lines: vec!["fn main() {}".into()],
            total_lines: 1,
            instructions: None,
        }));
        assert_eq!(msg.copy_text(), "read> read /src/main.rs\n1: fn main() {}");
    }

    #[test]
    fn copy_text_tool_with_code_input() {
        let mut msg = tool_msg("bash\nold body");
        msg.tool_input = Some(Arc::new(ToolInput::Code {
            language: "bash".into(),
            code: "echo hi\n".into(),
        }));
        msg.tool_output = Some(Arc::new(ToolOutput::Plain("done".into())));
        assert_eq!(msg.copy_text(), "read> bash\necho hi\nold body");
    }

    #[test_case("header\nbody text", Some(ToolOutput::Plain("done".into())), "read> header\nbody text" ; "plain_falls_through_to_body")]
    #[test_case("header only",       None,                                      "read> header only"       ; "no_output_no_body")]
    fn copy_text_tool_fallback(text: &str, output: Option<ToolOutput>, expected: &str) {
        let mut msg = tool_msg(text);
        msg.tool_output = output.map(Arc::new);
        assert_eq!(msg.copy_text(), expected);
    }

    #[test]
    fn copy_text_tool_batch_empty_entries() {
        let mut msg = tool_msg("batch\n3 tools ran");
        msg.tool_output = Some(Arc::new(ToolOutput::Batch {
            entries: vec![],
            text: "ignored".into(),
        }));
        assert_eq!(msg.copy_text(), "read> batch");
    }
}
