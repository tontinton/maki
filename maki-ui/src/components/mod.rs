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
pub mod queue_panel;
pub(crate) mod render_hints;
pub(crate) mod restore_mode_picker;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod session_picker;
pub(crate) mod split_layout;
pub mod status_bar;
pub(crate) mod streaming_content;
pub(crate) mod theme_picker;
pub(crate) mod tool_display;
pub(crate) mod tree_selector;
pub(crate) mod usage_modal;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

/// Shared render store (§10.1): the UI-only `ToolOutput` memo. Read-your-writes
/// by construction — the agent inserts each completed render synchronously, so a
/// tool that just completed is never unreadable regardless of writer batching.
/// For C1 the memo is the in-memory `HashMap` populated at load and on `ToolDone`;
/// the file-backed `RenderStore` (lazy on-disk decode) swaps in incrementally as
/// the folder-format load path lands, without changing this interface.
#[derive(Clone)]
pub(crate) struct Renders {
    memo: Arc<Mutex<HashMap<String, ToolOutput>>>,
}

impl Renders {
    pub(crate) fn from_memo(memo: Arc<Mutex<HashMap<String, ToolOutput>>>) -> Self {
        Self { memo }
    }

    pub(crate) fn empty() -> Self {
        Self {
            memo: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolve a deferred render at segment-build time (§11). Only segments
    /// that are actually built decode a render, so restores never materialize
    /// the whole store up front.
    pub(crate) fn resolve(&self, id: &str) -> Option<Arc<ToolOutput>> {
        self.memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .map(Arc::new)
    }
}

/// Lazy handle to a tool's render (§11). `Deferred` carries the `tool_use_id`
/// and is resolved through `Renders` only when the segment is built; `Ready`
/// holds an already-resolved render for live tool completions in the current
/// session.
#[derive(Debug, Clone)]
pub enum ToolOutputHandle {
    Deferred(String),
    Ready(Arc<ToolOutput>),
}

impl ToolOutputHandle {
    pub(crate) fn ready(output: ToolOutput) -> Self {
        Self::Ready(Arc::new(output))
    }

    pub(crate) fn ready_arc(&self) -> Option<Arc<ToolOutput>> {
        match self {
            Self::Ready(arc) => Some(Arc::clone(arc)),
            Self::Deferred(_) => None,
        }
    }

    /// Mutable access to the live `Arc` for in-place batch entry updates
    /// (only valid for `Ready` handles — a `Deferred` render is immutable).
    pub(crate) fn as_ready_arc_mut(&mut self) -> Option<&mut Arc<ToolOutput>> {
        match self {
            Self::Ready(arc) => Some(arc),
            Self::Deferred(_) => None,
        }
    }

    /// Borrow the render if already resolved (`Ready`). Returns `None` for a
    /// `Deferred` handle — callers must have upgraded via `resolve_tool_output`
    /// first. Keeps the segment-build path's `Option<&ToolOutput>` shape.
    pub(crate) fn as_resolved(&self) -> Option<&ToolOutput> {
        match self {
            Self::Ready(arc) => Some(arc),
            Self::Deferred(_) => None,
        }
    }
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
    LoadForkedSession {
        id: String,
    },
    ChangeModel(String),
    RefreshProvider {
        slug: String,
    },
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    RefreshModels,
    RefreshUsage,
    Compact,
    CompactSession,
    ToggleMcp(String, bool),
    OpenEditor(PathBuf),
    EditInputInEditor,
    Btw(String),
    Suspend,
    Quit,
}

const ERROR_DISPLAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitRequest {
    #[default]
    None,
    Success,
    Error,
}

impl ExitRequest {
    pub fn code(&self) -> i32 {
        match self {
            Self::None | Self::Success => 0,
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
    pub tool_output: Option<ToolOutputHandle>,
    pub live_output: Option<String>,
    pub annotation: Option<String>,
    pub plan_path: Option<String>,
    pub timestamp: Option<String>,
    pub turn_usage: Option<String>,
    pub truncated_lines: usize,
    pub render_snapshot: Option<BufferSnapshot>,
    pub render_header: Option<BufferSnapshot>,
    pub snapshot_theme_gen: u64,
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
        }
    }

    pub fn snapshot_is_stale(&self, current_gen: u64) -> bool {
        (self.render_snapshot.is_some() || self.render_header.is_some())
            && self.snapshot_theme_gen != current_gen
    }

    /// Upgrade a `Deferred` handle to `Ready` in place once the render is needed
    /// (§11). Called at segment-build time, so only built (viewport-visible)
    /// tools pay the decode; the rest stay deferred. Idempotent for `Ready`.
    pub(crate) fn resolve_tool_output(&mut self, renders: &Renders) {
        if let Some(ToolOutputHandle::Deferred(id)) = &self.tool_output
            && let Some(arc) = renders.resolve(id)
        {
            self.tool_output = Some(ToolOutputHandle::Ready(arc));
        }
    }

    /// Borrow the render if already resolved (`Ready`); `None` for a `Deferred`
    /// handle that `resolve_tool_output` has not yet upgraded. Segment-build
    /// callers upgrade first, so this is the post-resolution `&ToolOutput` view.
    pub fn output_ref(&self) -> Option<&ToolOutput> {
        self.tool_output.as_ref()?.as_resolved()
    }

    /// §11: cheap per-message height estimate used to size out-of-viewport
    /// stub segments so `total_height`/scrollbar stay valid before the real
    /// segment is built on scroll-in. Counts wrapped text lines plus the
    /// inter-message gap; never materializes `Line`s or decodes a render.
    pub(crate) fn estimate_segment_height(&self, width: u16) -> u16 {
        const TOOL_HEADER_LINES: u16 = 2;
        const GAP_LINE: u16 = 1;

        let w = width.max(1) as usize;
        let text_lines: u16 = if self.text.is_empty() {
            1
        } else {
            self.text
                .split('\n')
                .map(|line| line.len().div_ceil(w).max(1) as u16)
                .sum()
        };
        let base = match &self.role {
            DisplayRole::Tool(_) => text_lines.saturating_add(TOOL_HEADER_LINES),
            _ => text_lines,
        };
        base.saturating_add(GAP_LINE).max(1)
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
        vision: true,
        pricing: test_pricing(),
        max_output_tokens: 8192,
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
