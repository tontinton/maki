use std::borrow::Cow;
use std::mem;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_agent::pricing::SONNET_4;
use maki_agent::{AgentEvent, AgentInput, AgentMode, TokenUsage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

const TOOL_OUTPUT_MAX_DISPLAY_LINES: usize = 20;

const USER_STYLE: Style = Style::new().fg(Color::Cyan);
const ASSISTANT_STYLE: Style = Style::new().fg(Color::White);
const TOOL_STYLE: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM);
const CURSOR_STYLE: Style = Style::new()
    .fg(Color::White)
    .add_modifier(Modifier::SLOW_BLINK);
const STATUS_IDLE_STYLE: Style = Style::new().fg(Color::DarkGray);
const STATUS_STREAMING_STYLE: Style = Style::new().fg(Color::Yellow);
const STATUS_ERROR_STYLE: Style = Style::new().fg(Color::Red);
const BOLD_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const CODE_STYLE: Style = Style::new().fg(Color::Magenta);
const MODE_BUILD_STYLE: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
const MODE_PLAN_STYLE: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);

struct Delimiter {
    open: &'static str,
    style: Style,
}

const DELIMITERS: [Delimiter; 2] = [
    Delimiter {
        open: "**",
        style: BOLD_STYLE,
    },
    Delimiter {
        open: "`",
        style: CODE_STYLE,
    },
];

fn parse_inline_markdown<'a>(text: &'a str, base_style: Style) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        let next = DELIMITERS
            .iter()
            .filter_map(|d| remaining.find(d.open).map(|pos| (pos, d)))
            .min_by_key(|(pos, _)| *pos);

        let Some((pos, delim)) = next else {
            spans.push(Span::styled(remaining, base_style));
            break;
        };

        if pos > 0 {
            spans.push(Span::styled(&remaining[..pos], base_style));
        }

        let after_open = &remaining[pos + delim.open.len()..];
        if let Some(close) = after_open.find(delim.open) {
            spans.push(Span::styled(&after_open[..close], delim.style));
            remaining = &after_open[close + delim.open.len()..];
        } else {
            spans.push(Span::styled(&remaining[pos..], base_style));
            break;
        }
    }

    spans
}

fn text_to_lines<'a>(
    text: &'a str,
    prefix: &'a str,
    prefix_style: Style,
    base_style: Style,
) -> Vec<Line<'a>> {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| {
            let mut spans = Vec::new();
            if i == 0 {
                spans.push(Span::styled(prefix, prefix_style));
            }
            spans.extend(parse_inline_markdown(line, base_style));
            Line::from(spans)
        })
        .collect()
}

fn truncate_lines(s: &str, max_lines: usize) -> Cow<'_, str> {
    match s.match_indices('\n').nth(max_lines.saturating_sub(1)) {
        Some((i, _)) => Cow::Owned(format!("{}...", &s[..i])),
        None => Cow::Borrowed(s),
    }
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: DisplayRole,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    Streaming,
    Error(String),
}

pub enum Msg {
    Key(KeyEvent),
    Agent(AgentEvent),
}

pub enum Action {
    SendMessage(AgentInput),
    Quit,
}

pub fn tool_start_msg(name: &str, input: &str) -> String {
    format!("[{name}] {input}")
}

pub fn tool_done_msg(name: &str, output: &str) -> String {
    format!("[{name} done] {output}")
}

pub struct App {
    pub messages: Vec<DisplayMessage>,
    pub input: String,
    pub cursor_pos: usize,
    streaming_text: String,
    pub status: Status,
    scroll_top: u16,
    auto_scroll: bool,
    viewport_height: u16,
    pub token_usage: TokenUsage,
    pub should_quit: bool,
    pub mode: AgentMode,
    pending_plan: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            streaming_text: String::new(),
            status: Status::Idle,
            scroll_top: u16::MAX,
            auto_scroll: true,
            viewport_height: 24,
            token_usage: TokenUsage::default(),
            should_quit: false,
            mode: AgentMode::Build,
            pending_plan: None,
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key),
            Msg::Agent(event) => self.handle_agent_event(event),
        }
    }

    fn scroll(&mut self, delta: i32) {
        if delta > 0 {
            self.scroll_top = self.scroll_top.saturating_sub(delta as u16);
        } else {
            self.scroll_top = self.scroll_top.saturating_add(delta.unsigned_abs() as u16);
        }
        self.auto_scroll = false;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let half = self.viewport_height as i32 / 2;
            return match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    vec![Action::Quit]
                }
                KeyCode::Char('u') => {
                    self.scroll(half);
                    vec![]
                }
                KeyCode::Char('d') => {
                    self.scroll(-half);
                    vec![]
                }
                _ => vec![],
            };
        }

        match key.code {
            KeyCode::Up => {
                self.scroll(1);
                return vec![];
            }
            KeyCode::Down => {
                self.scroll(-1);
                return vec![];
            }
            KeyCode::Tab => {
                return self.toggle_mode();
            }
            _ => {}
        }

        if self.status == Status::Streaming {
            return vec![];
        }

        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return vec![];
                }

                let pending_plan = self.pending_plan.take();

                self.messages.push(DisplayMessage {
                    role: DisplayRole::User,
                    text: text.clone(),
                });
                self.input.clear();
                self.cursor_pos = 0;
                self.streaming_text.clear();
                self.status = Status::Streaming;
                self.auto_scroll = true;
                vec![Action::SendMessage(AgentInput {
                    message: text,
                    mode: self.mode.clone(),
                    pending_plan,
                })]
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                vec![]
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input.remove(self.cursor_pos);
                }
                vec![]
            }
            KeyCode::Left => {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
                vec![]
            }
            KeyCode::Right => {
                self.cursor_pos = (self.cursor_pos + 1).min(self.input.len());
                vec![]
            }
            _ => vec![],
        }
    }

    fn handle_agent_event(&mut self, event: AgentEvent) -> Vec<Action> {
        match event {
            AgentEvent::TextDelta(text) => {
                self.streaming_text.push_str(&text);
            }
            AgentEvent::ToolStart { name, input } => {
                self.flush_streaming_text();
                self.messages.push(DisplayMessage {
                    role: DisplayRole::Tool,
                    text: tool_start_msg(&name, &input),
                });
            }
            AgentEvent::ToolDone { name, output } => {
                let truncated = truncate_lines(&output, TOOL_OUTPUT_MAX_DISPLAY_LINES);
                self.messages.push(DisplayMessage {
                    role: DisplayRole::Tool,
                    text: tool_done_msg(&name, &truncated),
                });
            }
            AgentEvent::Done { usage } => {
                self.flush_streaming_text();
                self.token_usage += usage;
                self.status = Status::Idle;
            }
            AgentEvent::Error(err) => {
                self.flush_streaming_text();
                self.status = Status::Error(err);
            }
        }
        vec![]
    }

    fn toggle_mode(&mut self) -> Vec<Action> {
        if self.status == Status::Streaming {
            return vec![];
        }
        match &self.mode {
            AgentMode::Build => {
                self.mode = AgentMode::Plan(maki_agent::new_plan_path());
            }
            AgentMode::Plan(path) => {
                self.pending_plan = Some(path.clone());
                self.mode = AgentMode::Build;
            }
        }
        vec![]
    }

    fn flush_streaming_text(&mut self) {
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage {
                role: DisplayRole::Assistant,
                text: mem::take(&mut self.streaming_text),
            });
        }
    }

    pub fn view(&mut self, frame: &mut Frame) {
        let [messages_area, input_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        self.render_messages(frame, messages_area);
        self.render_input(frame, input_area);
        self.render_status(frame, status_area);
    }

    fn render_messages(&mut self, frame: &mut Frame, area: Rect) {
        self.viewport_height = area.height;
        let mut lines: Vec<Line> = Vec::new();

        for msg in &self.messages {
            let (prefix, base_style) = match msg.role {
                DisplayRole::User => ("you> ", USER_STYLE),
                DisplayRole::Assistant => ("maki> ", ASSISTANT_STYLE),
                DisplayRole::Tool => ("tool> ", TOOL_STYLE),
            };
            let prefix_style = base_style.add_modifier(Modifier::BOLD);
            lines.extend(text_to_lines(&msg.text, prefix, prefix_style, base_style));
        }

        if !self.streaming_text.is_empty() {
            let prefix_style = ASSISTANT_STYLE.add_modifier(Modifier::BOLD);
            let mut parsed = text_to_lines(
                &self.streaming_text,
                "maki> ",
                prefix_style,
                ASSISTANT_STYLE,
            );
            if let Some(last) = parsed.last_mut() {
                last.spans.push(Span::styled("_", CURSOR_STYLE));
            }
            lines.extend(parsed);
        }

        let total_lines = lines.len() as u16 + 1;
        let max_scroll = total_lines.saturating_sub(area.height);
        if self.auto_scroll {
            self.scroll_top = max_scroll;
        }
        self.scroll_top = self.scroll_top.min(max_scroll);

        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_top, 0));

        frame.render_widget(paragraph, area);
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let indicator = if self.status == Status::Streaming {
            "..."
        } else {
            "> "
        };
        let input_text = format!("{indicator}{}", self.input);
        let paragraph = Paragraph::new(input_text).block(Block::default().borders(Borders::ALL));

        frame.render_widget(paragraph, area);

        if self.status != Status::Streaming {
            let cursor_x = area.x + 1 + indicator.len() as u16 + self.cursor_pos as u16;
            let cursor_y = area.y + 1;
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let (mode_label, mode_style) = match &self.mode {
            AgentMode::Build => ("[BUILD]", MODE_BUILD_STYLE),
            AgentMode::Plan(_) => ("[PLAN]", MODE_PLAN_STYLE),
        };

        let (status_text, status_style) = match &self.status {
            Status::Idle => (
                format!(
                    " tokens: {}in / {}out (${:.3})",
                    self.token_usage.input,
                    self.token_usage.output,
                    self.token_usage.cost(&SONNET_4)
                ),
                STATUS_IDLE_STYLE,
            ),
            Status::Streaming => (" streaming...".to_string(), STATUS_STREAMING_STYLE),
            Status::Error(e) => (format!(" error: {e}"), STATUS_ERROR_STYLE),
        };

        let line = Line::from(vec![
            Span::styled(format!(" {mode_label}"), mode_style),
            Span::styled(status_text, status_style),
        ]);

        frame.render_widget(Paragraph::new(line), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn typing_and_submit() {
        let mut app = App::new();
        app.update(Msg::Key(key(KeyCode::Char('h'))));
        app.update(Msg::Key(key(KeyCode::Char('i'))));
        assert_eq!(app.input, "hi");
        assert_eq!(app.cursor_pos, 2);

        let actions = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendMessage(s) if s.message == "hi"));
        assert!(app.input.is_empty());
        assert_eq!(app.status, Status::Streaming);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, DisplayRole::User);
    }

    #[test]
    fn agent_done_flushes_and_tracks_tokens() {
        let mut app = App::new();
        app.status = Status::Streaming;
        app.streaming_text = "response text".into();
        let usage = TokenUsage {
            input: 100,
            output: 50,
            ..Default::default()
        };
        app.update(Msg::Agent(AgentEvent::Done {
            usage: usage.clone(),
        }));

        assert_eq!(app.status, Status::Idle);
        assert_eq!(app.token_usage, usage);
        assert!(app.streaming_text.is_empty());
        assert_eq!(app.messages.last().unwrap().text, "response text");
        assert_eq!(app.messages.last().unwrap().role, DisplayRole::Assistant);
    }

    #[test]
    fn tool_events_create_messages() {
        let mut app = App::new();
        app.status = Status::Streaming;
        app.update(Msg::Agent(AgentEvent::ToolStart {
            name: "bash".into(),
            input: "ls".into(),
        }));
        app.update(Msg::Agent(AgentEvent::ToolDone {
            name: "bash".into(),
            output: "file.txt".into(),
        }));

        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].role, DisplayRole::Tool);
        assert_eq!(app.messages[0].text, tool_start_msg("bash", "ls"));
        assert_eq!(app.messages[1].text, tool_done_msg("bash", "file.txt"));
    }

    #[test]
    fn backspace_and_cursor_movement() {
        let mut app = App::new();
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert_eq!(app.input, "abc");

        app.update(Msg::Key(key(KeyCode::Left)));
        assert_eq!(app.cursor_pos, 2);

        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.input, "ac");
        assert_eq!(app.cursor_pos, 1);
    }

    #[test_case(10, 'u', 0  ; "ctrl_u_saturates_at_zero")]
    #[test_case(20, 'u', 10 ; "ctrl_u_scrolls_up")]
    #[test_case(5,  'd', 15 ; "ctrl_d_scrolls_down")]
    #[test_case(0,  'd', 10 ; "ctrl_d_from_top")]
    fn half_page_scroll(initial: u16, key_char: char, expected: u16) {
        let mut app = App::new();
        app.viewport_height = 20;
        app.scroll_top = initial;
        app.update(Msg::Key(ctrl(key_char)));
        assert_eq!(app.scroll_top, expected);
    }

    #[test]
    fn scroll_top_clamped_to_content() {
        let mut app = App::new();
        app.messages.push(DisplayMessage {
            role: DisplayRole::User,
            text: "short".into(),
        });

        app.scroll_top = 1000;
        app.auto_scroll = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();

        assert_eq!(app.scroll_top, 0);
    }

    #[test]
    fn scroll_up_pins_viewport_during_streaming() {
        let mut app = App::new();
        app.status = Status::Streaming;
        app.streaming_text = "a\n".repeat(30);

        let backend = TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();

        app.update(Msg::Key(key(KeyCode::Up)));
        app.update(Msg::Key(key(KeyCode::Up)));
        terminal.draw(|f| app.view(f)).unwrap();
        let pinned = app.scroll_top;

        app.update(Msg::Agent(AgentEvent::TextDelta("b\nb\nb\n".into())));
        terminal.draw(|f| app.view(f)).unwrap();

        assert!(!app.auto_scroll);
        assert_eq!(app.scroll_top, pinned);
    }

    #[test_case("a **bold** b", &[("a ", None), ("bold", Some(BOLD_STYLE)), (" b", None)] ; "bold")]
    #[test_case("use `foo` here", &[("use ", None), ("foo", Some(CODE_STYLE)), (" here", None)] ; "inline_code")]
    #[test_case("a `code` then **bold**", &[("a ", None), ("code", Some(CODE_STYLE)), (" then ", None), ("bold", Some(BOLD_STYLE))] ; "code_before_bold")]
    #[test_case("a **unclosed", &[("a ", None), ("**unclosed", None)] ; "unclosed_delimiter")]
    fn parse_inline_markdown_cases(input: &str, expected: &[(&str, Option<Style>)]) {
        let base = Style::default();
        let spans = parse_inline_markdown(input, base);
        assert_eq!(spans.len(), expected.len());
        for (span, (text, style)) in spans.iter().zip(expected) {
            assert_eq!(span.content, *text);
            assert_eq!(span.style, style.unwrap_or(base));
        }
    }

    #[test]
    fn text_to_lines_splits_newlines() {
        let style = Style::default();
        let prefix_style = style.add_modifier(Modifier::BOLD);
        let lines = text_to_lines("line1\nline2\nline3", "p> ", prefix_style, style);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "p> ");
        assert_eq!(lines[1].spans.len(), 1);
    }

    #[test_case("a\nb\nc", 5, "a\nb\nc" ; "under_limit")]
    #[test_case("a\nb\nc\nd", 2, "a\nb..." ; "over_limit")]
    #[test_case("single", 1, "single" ; "single_line")]
    fn truncate_lines_cases(input: &str, max: usize, expected: &str) {
        assert_eq!(truncate_lines(input, max), expected);
    }

    #[test]
    fn tab_toggles_mode_and_sets_pending_plan() {
        let mut app = App::new();
        assert_eq!(app.mode, AgentMode::Build);

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert!(matches!(app.mode, AgentMode::Plan(ref p) if p.contains(maki_agent::PLANS_DIR)));

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.mode, AgentMode::Build);
        assert!(app.pending_plan.is_some());
    }

    #[test]
    fn submit_consumes_pending_plan() {
        let mut app = App::new();
        app.pending_plan = Some("plan.md".into());
        app.update(Msg::Key(key(KeyCode::Char('x'))));
        let actions = app.update(Msg::Key(key(KeyCode::Enter)));
        let Action::SendMessage(ref input) = actions[0] else {
            panic!("expected SendMessage");
        };
        assert_eq!(input.pending_plan.as_deref(), Some("plan.md"));
        assert!(app.pending_plan.is_none());
    }
}
