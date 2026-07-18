pub(crate) mod btw_modal;
pub(crate) mod code_view;
pub mod command;
pub(crate) mod file_picker;
pub(crate) mod form;
pub(crate) mod help_modal;
pub mod input;
pub mod keybindings;
pub(crate) mod list_picker;
pub(crate) mod login_picker;
pub(crate) mod lua_float;
pub(crate) mod mcp_picker;
pub mod messages;
pub(crate) mod modal;
pub(crate) mod model_picker;
pub(crate) mod permission_prompt;
pub(crate) mod plan_form;
pub(crate) mod progress_bar;
pub mod queue_panel;
pub(crate) mod rewind_picker;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod split_layout;
pub mod status_bar;
pub(crate) mod streaming_content;
pub(crate) mod theme_picker;
pub(crate) mod tool_display;
pub(crate) mod usage_modal;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_agent::AgentInput;
use maki_agent::{BufferSnapshot, ToolInput, ToolOutput};
use maki_providers::{Message, ModelTier};
use ratatui::text::{Line, Span};

pub(crate) const CHEVRON: &str = "❯ ";

pub(crate) fn chevron_span() -> ratatui::text::Span<'static> {
    ratatui::text::Span::styled(CHEVRON, crate::theme::current().tool_dim)
}

pub(crate) trait Overlay {
    fn is_open(&self) -> bool;
    fn close(&mut self);
    /// Modal overlays block mouse interaction behind them.
    fn is_modal(&self) -> bool {
        true
    }
}

pub(crate) fn hint_line<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    let t = crate::theme::current();
    let mut spans = Vec::with_capacity(pairs.len() * 3);
    for (key, desc) in pairs {
        spans.push(Span::raw("  "));
        for (i, part) in key.as_ref().split('/').enumerate() {
            if i > 0 {
                spans.push(Span::styled("/", t.tool_dim));
            }
            spans.push(Span::styled(part.to_string(), t.keybind_key));
        }
        spans.push(Span::styled(format!(" {}", desc.as_ref()), t.tool_dim));
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

    pub fn new_top() -> Self {
        Self {
            auto_scroll: false,
            ..Self::new()
        }
    }

    pub fn reset(&mut self) {
        let auto_scroll = self.auto_scroll;
        *self = Self::new();
        self.auto_scroll = auto_scroll;
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
    pub model_spec: String,
}

use std::path::PathBuf;

pub enum Action {
    SendMessage(Box<AgentInput>),
    ShellCommand {
        id: String,
        command: String,
        visible: bool,
    },
    CancelAgent {
        run_id: u64,
    },
    CancelSubagent {
        tool_use_id: String,
    },
    NewSession,
    LoadSession(Box<LoadedSession>),
    ChangeModel(String),
    RefreshProvider {
        slug: String,
    },
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    RefreshModels,
    RefreshUsage,
    Compact,
    ToggleMcp(String, bool),
    OpenEditor(PathBuf),
    EditInputInEditor,
    Btw(String),
    Suspend,
}

const ERROR_DISPLAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitRequest {
    #[default]
    None,
    Success,
    Error,
    Reload,
}

impl ExitRequest {
    pub fn code(&self) -> i32 {
        match self {
            Self::None | Self::Success | Self::Reload => 0,
            Self::Error => 1,
        }
    }
}

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
    pub tool_raw_input: Option<Arc<serde_json::Value>>,
    pub tool_output: Option<Arc<ToolOutput>>,
    pub live_output: Option<String>,
    pub annotation: Option<String>,
    pub plan_path: Option<String>,
    pub timestamp: Option<String>,
    pub turn_usage: Option<String>,
    pub truncated_lines: usize,
    pub render_snapshot: Option<BufferSnapshot>,
    pub render_header: Option<BufferSnapshot>,
    pub snapshot_theme_gen: u64,
    pub thinking_collapsed: bool,
}

impl DisplayMessage {
    pub fn new(role: DisplayRole, text: String) -> Self {
        Self {
            role,
            text,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    pub fn plan(text: String, plan_path: String) -> Self {
        Self {
            role: DisplayRole::Assistant,
            text,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            plan_path: Some(plan_path),
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    pub fn snapshot_is_stale(&self, current_gen: u64) -> bool {
        (self.render_snapshot.is_some() || self.render_header.is_some())
            && self.snapshot_theme_gen != current_gen
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRole {
    pub id: String,
    pub status: ToolStatus,
    pub name: Arc<str>,
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
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            DisplayRole::Tool(t) => Some(&t.name),
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
        fast: None,
    }
}

#[cfg(test)]
pub(crate) fn test_model() -> maki_providers::Model {
    maki_providers::Model {
        id: "test-model".into(),
        provider: maki_providers::provider::ProviderKind::Anthropic,
        dynamic_slug: None,
        tier: maki_providers::ModelTier::Medium,
        family: maki_providers::ModelFamily::Claude,
        supports_tool_examples_override: None,
        supports_thinking_override: None,
        supports_vision_override: Some(true),
        pricing: test_pricing(),
        max_output_tokens: Some(8192),
        context_window: TEST_CONTEXT_WINDOW,
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
    use maki_agent::{SnapshotLine, SnapshotSpan, SpanStyle};
    use test_case::test_case;

    const SNAPSHOT_GEN: u64 = 7;

    fn snapshot() -> BufferSnapshot {
        BufferSnapshot::from_arc(Arc::new(vec![SnapshotLine {
            spans: vec![SnapshotSpan {
                text: "baked".into(),
                style: SpanStyle::Default,
            }],
        }]))
    }

    #[test_case(false, false, false => false ; "no_snapshot_never_stale")]
    #[test_case(true,  false, true  => false ; "has_snapshot_matching_gen_fresh")]
    #[test_case(true,  false, false => true  ; "has_snapshot_mismatched_gen_stale")]
    fn snapshot_is_stale_cases(has_body: bool, has_header: bool, gen_match: bool) -> bool {
        let mut msg = DisplayMessage::new(DisplayRole::Assistant, "hi".into());
        msg.snapshot_theme_gen = SNAPSHOT_GEN;
        if has_body {
            msg.render_snapshot = Some(snapshot());
        }
        if has_header {
            msg.render_header = Some(snapshot());
        }
        let current_gen = if gen_match {
            SNAPSHOT_GEN
        } else {
            SNAPSHOT_GEN + 1
        };
        msg.snapshot_is_stale(current_gen)
    }

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
}
