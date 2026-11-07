pub(crate) mod alert_modal;
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
pub(crate) mod pack_review;
pub(crate) mod permission_prompt;
pub(crate) mod plan_form;
pub(crate) mod progress_bar;
pub(crate) mod provider_auth;
pub mod queue_panel;
pub(crate) mod rewind_picker;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod split_layout;
pub mod status_bar;
pub(crate) mod streaming_content;
pub(crate) mod theme_picker;
pub(crate) mod thinking;
pub(crate) mod tool_display;
pub(crate) mod usage_modal;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_agent::AgentInput;
use maki_agent::{BufferSnapshot, TaskEnd, ToolDoneEvent, ToolInput, ToolOutput};
use maki_lua::{PackCommand, PackPlan};
use maki_providers::{ImageSource, Message, ModelTier};
use ratatui::Frame;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::OpenSession;

pub(crate) use maki_providers::IMAGE_PLACEHOLDER;

pub(crate) const CHEVRON: &str = "❯ ";

pub(crate) fn chevron_span() -> ratatui::text::Span<'static> {
    ratatui::text::Span::styled(CHEVRON, crate::theme::current().tool_dim)
}

/// A field's caret: the span it paints and the display column it should land
/// on, measured from the start of its line.
pub(crate) struct Caret {
    pub col: usize,
    pub span: Span<'static>,
}

/// Cells {spans} take once painted, the way ratatui counts them: it drops
/// every grapheme holding a control char, such as a pasted newline.
pub(crate) fn painted_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .flat_map(|span| span.styled_graphemes(Style::default()))
        .map(|grapheme| usize::from(grapheme.symbol.cell_width()))
        .sum()
}

/// Lays {text} out as a field line after {prefix}, the char at {cursor_byte}
/// in {caret_style} and what follows it in {tail_style}. A caret on a control
/// char or at the end is painted as a space, the only thing that shows.
pub(crate) fn field_line(
    prefix: Vec<Span<'static>>,
    text: &str,
    cursor_byte: usize,
    caret_style: Style,
    tail_style: Style,
) -> (Line<'static>, Caret) {
    let (before, rest) = text.split_at(cursor_byte);
    let mut chars = rest.chars();
    let caret_char = chars.next().filter(|c| !c.is_control()).unwrap_or(' ');
    let mut spans = prefix;
    spans.push(Span::raw(before.to_owned()));
    let col = painted_width(&spans);
    let span = Span::styled(caret_char.to_string(), caret_style);
    spans.push(span.clone());
    spans.push(Span::styled(chars.as_str().to_owned(), tail_style));
    (Line::from(spans), Caret { col, span })
}

/// The cell of {caret} on {row} of {area}, but only if {buf} really holds the
/// caret there. ratatui decides whether text clips, wraps or merges into its
/// neighbour, and a copy of those rules would drift. So the cell is read back,
/// and a mismatch reports nothing rather than a guess. An overlay painted over
/// the field later is not seen.
pub(crate) fn painted_caret(
    buf: &Buffer,
    area: Rect,
    caret: &Caret,
    row: usize,
) -> Option<Position> {
    let x = u16::try_from(usize::from(area.x) + caret.col).ok()?;
    let y = u16::try_from(usize::from(area.y) + row).ok()?;
    let pos = Position::new(x, y);
    let cell = buf.cell(pos).filter(|_| area.contains(pos))?;
    let style = cell.style();
    let painted = caret.span.styled_graphemes(Style::default()).next()?;
    (cell.symbol() == painted.symbol && style.patch(caret.span.style) == style).then_some(pos)
}

/// Paints a one row text field into {area} and returns the cell of its caret.
pub(crate) fn render_field(
    frame: &mut Frame,
    area: Rect,
    prefix: Vec<Span<'static>>,
    text: &str,
    cursor_byte: usize,
    caret_style: Style,
) -> Option<Position> {
    let (line, caret) = field_line(prefix, text, cursor_byte, caret_style, Style::default());
    frame.render_widget(Paragraph::new(line), area);
    painted_caret(frame.buffer_mut(), area, &caret, 0)
}

pub(crate) trait Overlay {
    fn is_open(&self) -> bool;
    fn close(&mut self);
    /// Whether it is up and answers a single key on its own, like `y` on a
    /// permission prompt. Keys typed before one opened must never answer it,
    /// so each overlay has to say.
    fn is_prompting(&self) -> bool;
    /// Modal overlays block clicks behind them. The wheel ignores this flag:
    /// `App::scroll_at` decides per overlay where it goes.
    fn is_modal(&self) -> bool {
        true
    }
    /// Override when the overlay draws something that moves on the clock
    /// alone, like a spinner or a reveal. `App::cadence` asks every overlay,
    /// so this is the only place one has to say so.
    fn cadence(&self) -> crate::repaint::Cadence {
        crate::repaint::Cadence::IDLE
    }
}

pub(crate) fn hint_line<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    Line::from(hint_spans(pairs, "  "))
}

/// The same hints, sized for a bottom border: tighter gaps, and a space at
/// each end so they never touch a corner.
pub(crate) fn hint_footer<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    let mut spans = hint_spans(pairs, " ");
    spans.push(Span::raw(" "));
    Line::from(spans).right_aligned()
}

fn hint_spans<K: AsRef<str>, V: AsRef<str>>(
    pairs: &[(K, V)],
    gap: &'static str,
) -> Vec<Span<'static>> {
    let t = crate::theme::current();
    let mut spans = Vec::with_capacity(pairs.len() * 3 + 1);
    for (key, desc) in pairs {
        spans.push(Span::raw(gap));
        for (i, part) in key.as_ref().split('/').enumerate() {
            if i > 0 {
                spans.push(Span::styled("/", t.tool_dim));
            }
            spans.push(Span::styled(part.to_string(), t.keybind_key));
        }
        spans.push(Span::styled(format!(" {}", desc.as_ref()), t.tool_dim));
    }
    spans
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
            _ if key::SCROLL_PAGE_UP.matches(key_event) => self.scroll(self.page()),
            _ if key::SCROLL_PAGE_DOWN.matches(key_event) => self.scroll(-self.page()),
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

    fn page(&self) -> i32 {
        self.viewport_h.max(1) as i32
    }

    fn clamp(&mut self) {
        self.offset = self.offset.min(self.max_offset);
    }
}

use std::path::PathBuf;

pub enum Action {
    SendMessage(Box<AgentInput>),
    ManualExit,
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
    /// Esc in a plugin task's tab: closes the plugin session behind it.
    CancelPluginTask {
        id: String,
        /// Read at the stop: a rewind drops the tab before this runs.
        plugin: Arc<str>,
    },
    /// The history under this runtime changed (reset, load, rewind), so the
    /// agent has to be respawned on it.
    RestartAgent(Vec<Message>),
    /// A copy of this session, to run beside it and take the focus.
    OpenFork(Box<OpenSession>),
    ChangeModel(String),
    RefreshProvider {
        slug: String,
    },
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    RefreshModels,
    RefreshUsage,
    Compact(Option<String>),
    ToggleMcp(String, bool),
    OpenEditor(PathBuf),
    EditInputInEditor,
    Btw(String),
    PreparePack(PackCommand),
    Suspend,
}

const ERROR_DISPLAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExitRequest {
    #[default]
    None,
    Success,
    Error,
    Reload,
    Pack(PackPlan),
}

impl ExitRequest {
    pub fn code(&self) -> i32 {
        match self {
            Self::None | Self::Success | Self::Reload | Self::Pack(_) => 0,
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
    pub images: Vec<ImageSource>,
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
    /// Only a click sets it, so thinking starts the way `ui.thinking_lines`
    /// says however the message got here.
    pub thinking_expanded: bool,
    /// A running row a Ctrl+B press would move now.
    pub movable: bool,
    /// Set once the call moved: the row keeps its receipt and follows the task.
    pub background: Option<BackgroundBadge>,
    /// The full body behind a one-line notice, shown when the row expands.
    pub detail: Option<NoticeDetail>,
    /// Whether [`Self::detail`] is shown. Only a click sets it, so a notice
    /// starts collapsed however it got here.
    pub detail_expanded: bool,
    /// On a notice that answers a background task: how the task ended.
    pub task_end: Option<BackgroundOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeDetail {
    pub text: String,
    pub markdown: bool,
}

/// How a moved call's row tracks its background task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundBadge {
    pub id: String,
    /// `None` after a restore: the row shows a neutral marker.
    pub started_at: Option<Instant>,
    pub outcome: Option<BackgroundOutcome>,
}

impl BackgroundBadge {
    /// The badge {receipt} puts on its row. It counts from the call's start,
    /// as the call's own marks do, not from the move.
    pub fn moved(receipt: &ToolDoneEvent) -> Self {
        let now = Instant::now();
        Self {
            id: receipt.id.clone(),
            started_at: Some(now.checked_sub(receipt.ran_for()).unwrap_or(now)),
            outcome: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundOutcome {
    Done,
    Error,
    Cancelled,
    /// The task died with the maki that ran it and never reported back.
    Lost,
}

impl From<TaskEnd> for BackgroundOutcome {
    fn from(end: TaskEnd) -> Self {
        match end {
            TaskEnd::Finished => Self::Done,
            TaskEnd::Failed => Self::Error,
            TaskEnd::Cancelled => Self::Cancelled,
            TaskEnd::Lost => Self::Lost,
        }
    }
}

impl DisplayMessage {
    pub fn new(role: DisplayRole, text: String) -> Self {
        Self {
            role,
            text,
            images: Vec::new(),
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
            thinking_expanded: false,
            movable: false,
            background: None,
            detail: None,
            detail_expanded: false,
            task_end: None,
        }
    }

    pub fn with_images(role: DisplayRole, text: String, images: Vec<ImageSource>) -> Self {
        let text = if text.trim().is_empty() && !images.is_empty() {
            IMAGE_PLACEHOLDER.into()
        } else {
            text
        };
        Self {
            images,
            ..Self::new(role, text)
        }
    }

    pub fn plan(text: String, plan_path: String) -> Self {
        Self {
            role: DisplayRole::Assistant,
            text,
            images: Vec::new(),
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
            thinking_expanded: false,
            movable: false,
            background: None,
            detail: None,
            detail_expanded: false,
            task_end: None,
        }
    }

    pub fn snapshot_is_stale(&self, current_gen: u64) -> bool {
        (self.render_snapshot.is_some() || self.render_header.is_some())
            && self.snapshot_theme_gen != current_gen
    }

    /// Whether its call still writes to the buf the row draws: a moved call
    /// until its badge has an outcome, any other one while it runs.
    pub fn streams(&self) -> bool {
        match (&self.background, &self.role) {
            (Some(badge), _) => badge.outcome.is_none(),
            (None, DisplayRole::Tool(tool)) => tool.status == ToolStatus::InProgress,
            (None, _) => false,
        }
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
    /// One dim line about what the host told the model on the user's behalf.
    Notice,
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
    ModelPricing::per_million(3.0, 15.0, 3.75, 0.30)
}

#[cfg(test)]
pub(crate) fn test_model() -> maki_providers::Model {
    maki_providers::Model {
        id: "test-model".into(),
        provider: std::sync::Arc::<str>::from("anthropic"),
        tier: maki_providers::ModelTier::Medium,
        family: maki_providers::ModelFamily::Claude,
        supports_tool_examples_override: None,
        thinking_override: None,
        supports_vision_override: Some(true),
        supports_fast_override: None,
        pricing: test_pricing(),
        subsidised_by: None,
        discovered_free: false,
        max_output_tokens: Some(8192),
        turn_output_tokens: None,
        context_window: TEST_CONTEXT_WINDOW,
        thinking_fields: None,
    }
}

/// Every symbol in the buffer, row by row, for `assert!(screen.contains(..))`.
#[cfg(test)]
pub(crate) fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    buf.content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
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
    use ratatui::widgets::Widget;
    use test_case::test_case;

    const SNAPSHOT_GEN: u64 = 7;
    const FIELD_PREFIX: &str = "❯ ";
    const FIELD_AREA: Rect = Rect::new(5, 7, 10, 3);
    const FIELD_SCREEN: Rect = Rect::new(0, 0, 40, 20);

    fn field_cell(col: u16, row: u16) -> Option<Position> {
        Some(Position::new(FIELD_AREA.x + col, FIELD_AREA.y + row))
    }

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

    #[test_case(10, 3, 7   ; "scroll_up")]
    #[test_case(10, -3, 13 ; "scroll_down")]
    #[test_case(0, 5, 0    ; "clamp_underflow")]
    fn apply_scroll_delta_cases(offset: u16, delta: i32, expected: u16) {
        assert_eq!(apply_scroll_delta(offset, delta), expected);
    }

    #[test_case("abc", 1 => ("❯ abc".to_string(), 3, "b".to_string()) ; "ascii_mid_text")]
    #[test_case("abc", 3 => ("❯ abc ".to_string(), 5, " ".to_string()) ; "ascii_at_the_end")]
    #[test_case("", 0 => ("❯  ".to_string(), 2, " ".to_string()) ; "empty_text")]
    #[test_case("a漢b", 4 => ("❯ a漢b".to_string(), 5, "b".to_string()) ; "after_a_wide_char")]
    #[test_case("a漢b", 1 => ("❯ a漢b".to_string(), 3, "漢".to_string()) ; "on_a_wide_char")]
    #[test_case("ab\ncd", 5 => ("❯ abcd ".to_string(), 6, " ".to_string()) ; "after_a_newline")]
    #[test_case("ab\n", 3 => ("❯ ab ".to_string(), 4, " ".to_string()) ; "after_a_trailing_newline")]
    #[test_case("ab\ncd", 2 => ("❯ ab cd".to_string(), 4, " ".to_string()) ; "on_a_newline")]
    #[test_case("ｶﾞx", 6 => ("❯ ｶﾞx".to_string(), 5, "x".to_string()) ; "after_a_halfwidth_voiced_mark")]
    fn field_line_places_the_caret(text: &str, cursor_byte: usize) -> (String, usize, String) {
        let (line, caret) = field_line(
            vec![Span::raw(FIELD_PREFIX)],
            text,
            cursor_byte,
            Style::new().reversed(),
            Style::default(),
        );
        let painted: String = line
            .spans
            .iter()
            .flat_map(|span| span.styled_graphemes(Style::default()))
            .map(|grapheme| grapheme.symbol)
            .collect();
        (painted, caret.col, caret.span.content.to_string())
    }

    fn painted_caret_in(text: &str, cursor_byte: usize, width: u16) -> Option<Position> {
        let area = Rect {
            width,
            ..FIELD_AREA
        };
        let mut buf = Buffer::empty(FIELD_SCREEN);
        let (line, caret) = field_line(
            vec![Span::raw(FIELD_PREFIX)],
            text,
            cursor_byte,
            Style::new().reversed(),
            Style::default(),
        );
        Paragraph::new(line).render(area, &mut buf);
        painted_caret(&buf, area, &caret, 0)
    }

    #[test_case("abc", 1, 10 => field_cell(3, 0) ; "mid_text")]
    #[test_case("abc", 3, 10 => field_cell(5, 0) ; "end_of_text_space")]
    #[test_case("a漢b", 4, 10 => field_cell(5, 0) ; "after_a_wide_char")]
    #[test_case("ｶﾞx", 6, 10 => field_cell(5, 0) ; "after_a_halfwidth_voiced_mark")]
    #[test_case("abcdefghij", 8, 10 => None ; "clipped_past_the_right_edge")]
    #[test_case("xxxxxx漢", 6, 9 => None ; "wide_caret_cut_by_the_edge")]
    #[test_case("a\u{301}b", 1, 10 => None ; "zero_width_caret_merges_into_its_neighbour")]
    fn painted_caret_reports_only_what_ratatui_painted(
        text: &str,
        cursor_byte: usize,
        width: u16,
    ) -> Option<Position> {
        painted_caret_in(text, cursor_byte, width)
    }
}
