use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::shell::parse_shell_prefix;
use crate::highlight;
use crate::text_buffer::{EditResult, TextBuffer, is_newline_key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_storage::input_history::InputHistory;
use std::mem;

use maki_providers::ImageSource;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::scrollbar::render_vertical_scrollbar;
use super::{apply_scroll_delta, visual_line_count};
use crate::selection::LineBreaks;

const MAX_INPUT_LINES: u16 = 20;
const CHEVRON: &str = super::CHEVRON;
const NEWLINE_PAD: &str = "  ";
const PREFIX_WIDTH: u16 = 2;
const PLACEHOLDER_SUGGESTIONS: &[&str] = &[
    "research how something works",
    "fix a bug",
    "add a feature",
    "add a database migration",
    "create a helm chart",
    "simplify some function",
    "remove trivial comments",
    "analyze data",
    "profile and improve performance",
    "add tests",
    "add benchmarks",
    "refactor a module",
    "remove dead code",
];

pub enum InputAction {
    Submit(Submission),
    ContinueLine,
    PaletteSync(String),
    Passthrough(KeyEvent),
    None,
}

pub struct Submission {
    pub text: String,
    pub images: Vec<ImageSource>,
}

impl Submission {
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            images: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }
}

pub struct InputBox {
    pub(crate) buffer: TextBuffer,
    history: InputHistory,
    history_index: Option<usize>,
    draft: String,
    scroll_y: u16,
    follow_cursor: bool,
    placeholder_hint: &'static str,
    pending_images: Vec<ImageSource>,
}

impl InputBox {
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        self.follow_cursor = true;

        match key.code {
            KeyCode::Up if self.is_at_first_line() => {
                self.history_up();
                return InputAction::None;
            }
            KeyCode::Down if self.is_at_last_line() => {
                self.history_down();
                return InputAction::None;
            }
            KeyCode::Tab | KeyCode::Esc => return InputAction::Passthrough(key),
            _ if is_newline_key(&key) => {
                self.buffer.add_line();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter if self.char_before_cursor_is_backslash() => {
                self.continue_line();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter => {
                return match self.submit() {
                    Some(sub) => InputAction::Submit(sub),
                    None => InputAction::Submit(Submission::empty()),
                };
            }
            _ => {}
        }

        match self.buffer.handle_key(key) {
            EditResult::Changed => InputAction::PaletteSync(self.buffer.value()),
            EditResult::Moved | EditResult::Ignored => InputAction::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> InputAction {
        self.follow_cursor = true;
        self.buffer.insert_text(text);
        InputAction::PaletteSync(self.buffer.value())
    }

    pub fn new(history: InputHistory) -> Self {
        Self {
            buffer: TextBuffer::new(String::new()),
            history,
            history_index: None,
            draft: String::new(),
            scroll_y: 0,
            follow_cursor: true,
            placeholder_hint: random_placeholder_hint(),
            pending_images: Vec::new(),
        }
    }

    pub fn copy_text(&self) -> String {
        self.buffer
            .lines()
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { CHEVRON } else { NEWLINE_PAD };
                format!("{prefix}{l}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn line_breaks(&self, content_width: u16) -> LineBreaks {
        let ew = effective_width(content_width as usize);
        LineBreaks::from_heights(
            self.buffer
                .lines()
                .iter()
                .map(|line| visual_line_count(line.width(), ew) as u16),
        )
    }

    pub fn height(&self, width: u16) -> u16 {
        let ew = effective_width(width as usize);
        let mut visual_lines = total_visual_lines(&self.buffer, ew, true);
        if !self.pending_images.is_empty() {
            visual_lines += 1;
        }
        (visual_lines as u16).min(MAX_INPUT_LINES) + 2
    }

    pub fn is_at_first_line(&self) -> bool {
        self.buffer.y() == 0
    }

    pub fn is_at_last_line(&self) -> bool {
        self.buffer.y() == self.buffer.line_count().saturating_sub(1)
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        let line = &self.buffer.lines()[self.buffer.y()];
        let x = self.buffer.x();
        if x == 0 {
            return false;
        }
        let byte_idx = TextBuffer::char_to_byte(line, x - 1);
        line.as_bytes()[byte_idx] == b'\\'
    }

    pub fn continue_line(&mut self) {
        self.buffer.remove_char();
        self.buffer.add_line();
    }

    pub fn submit(&mut self) -> Option<Submission> {
        let text = self.buffer.value().trim().to_string();
        let images = mem::take(&mut self.pending_images);
        if text.is_empty() && images.is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.discard();
        Some(Submission { text, images })
    }

    pub fn discard(&mut self) {
        self.pending_images.clear();
        self.history_index = None;
        self.draft.clear();
        self.buffer.clear();
        self.scroll_y = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.value().trim().is_empty() && self.pending_images.is_empty()
    }

    pub fn attach_image(&mut self, source: ImageSource) {
        self.pending_images.push(source);
    }

    pub fn set_input(&mut self, s: String) {
        self.buffer = TextBuffer::new(s);
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_index = match self.history_index {
            None => {
                self.draft = self.buffer.value();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(new_index);
        let entry = self.history.get(new_index).unwrap().to_string();
        self.set_input(entry);
        self.buffer.move_to_end();
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_index = Some(i + 1);
            let entry = self.history.get(i + 1).unwrap().to_string();
            self.set_input(entry);
        } else {
            self.history_index = None;
            let draft = mem::take(&mut self.draft);
            self.set_input(draft);
        }
    }

    fn visual_cursor_y(&self, ew: usize) -> u16 {
        let lines_above: u16 = self
            .buffer
            .lines()
            .iter()
            .take(self.buffer.y())
            .map(|line| visual_line_count(line.width(), ew) as u16)
            .sum();

        let wrap_row = if ew == 0 {
            0
        } else {
            let line = &self.buffer.lines()[self.buffer.y()];
            let cursor_col: usize = line
                .chars()
                .take(self.buffer.x())
                .map(|c| c.width().unwrap_or(1))
                .sum();
            (cursor_col / ew) as u16
        };

        lines_above + wrap_row
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        streaming: bool,
        border_style: Style,
        focused: bool,
        top_right_hint: Option<Line<'_>>,
    ) {
        let content_height = area.height.saturating_sub(2);
        let ew = effective_width(area.width as usize);

        if self.follow_cursor {
            let visual_cursor_y = self.visual_cursor_y(ew);
            if visual_cursor_y < self.scroll_y {
                self.scroll_y = visual_cursor_y;
            } else if visual_cursor_y >= self.scroll_y + content_height {
                self.scroll_y = visual_cursor_y - content_height + 1;
            }
        }

        let mut total_vl = total_visual_lines(&self.buffer, ew, focused) as u16;
        if !self.pending_images.is_empty() {
            total_vl += 1;
        }
        let max_scroll = total_vl.saturating_sub(content_height);
        self.scroll_y = self.scroll_y.min(max_scroll);

        let prefix_style = theme::current().input_placeholder;
        let is_empty = self.buffer.value().is_empty();
        let mut styled_lines: Vec<Line> = if is_empty && self.pending_images.is_empty() {
            let placeholder_base = theme::current().input_placeholder;
            if streaming {
                vec![Line::from(vec![
                    Span::styled(CHEVRON, prefix_style),
                    if focused {
                        Span::styled("Q", placeholder_base.reversed())
                    } else {
                        Span::styled("Q", placeholder_base)
                    },
                    Span::styled("ueue another prompt...", placeholder_base),
                ])]
            } else {
                vec![Line::from(vec![
                    Span::styled(CHEVRON, prefix_style),
                    if focused {
                        Span::styled("A", placeholder_base.reversed())
                    } else {
                        Span::styled("A", placeholder_base)
                    },
                    Span::styled("sk maki to ", placeholder_base),
                    Span::styled(
                        self.placeholder_hint,
                        placeholder_base.add_modifier(ratatui::style::Modifier::ITALIC),
                    ),
                    Span::styled("...", placeholder_base),
                ])]
            }
        } else {
            let cursor_y = self.buffer.y();
            let cursor_x = self.buffer.x();
            self.buffer
                .lines()
                .iter()
                .enumerate()
                .flat_map(|(i, line)| {
                    let is_cursor_line = i == cursor_y && focused;
                    let shell_spans = if i == 0 {
                        shell_highlight_spans(line)
                    } else {
                        None
                    };
                    wrap_line(
                        line,
                        ew,
                        is_cursor_line,
                        cursor_x,
                        i == 0,
                        prefix_style,
                        shell_spans.as_deref(),
                    )
                })
                .collect()
        };

        if !self.pending_images.is_empty() {
            let n = self.pending_images.len();
            let label = match n {
                1 => "1 image".to_string(),
                _ => format!("{n} images"),
            };
            styled_lines.push(Line::from(Span::styled(
                label,
                theme::current().input_placeholder,
            )));
        }

        let text = Text::from(styled_lines);
        let mut block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_type(BorderType::Plain)
            .border_style(border_style);
        if let Some(hint) = top_right_hint {
            block = block.title_top(hint.right_aligned());
        }
        let paragraph = Paragraph::new(text)
            .style(Style::new().fg(theme::current().foreground))
            .scroll((self.scroll_y, 0))
            .block(block);
        frame.render_widget(paragraph, area);

        if max_scroll > 0 {
            let inner = area.inner(ratatui::layout::Margin::new(0, 1));
            render_vertical_scrollbar(frame, inner, total_vl, self.scroll_y);
        }
    }

    pub fn scroll_y(&self) -> u16 {
        self.scroll_y
    }

    pub fn history(&self) -> &InputHistory {
        &self.history
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta);
        self.follow_cursor = false;
    }
}

fn random_placeholder_hint() -> &'static str {
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as usize % PLACEHOLDER_SUGGESTIONS.len())
        .unwrap_or(0);
    PLACEHOLDER_SUGGESTIONS[idx]
}

fn effective_width(content_width: usize) -> usize {
    content_width.saturating_sub(PREFIX_WIDTH as usize)
}

fn wrap_line(
    line: &str,
    ew: usize,
    is_cursor_line: bool,
    cursor_x: usize,
    is_first_line: bool,
    prefix_style: Style,
    shell_spans: Option<&[Span<'static>]>,
) -> Vec<Line<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(1)).collect();
    let row_width = ew.max(1);

    let mut row_ranges: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0;
    let mut row_col = 0;
    for (i, &w) in widths.iter().enumerate() {
        if row_col + w > row_width && row_col > 0 {
            row_ranges.push((row_start, i));
            row_start = i;
            row_col = 0;
        }
        row_col += w;
    }
    if row_start < chars.len() || row_ranges.is_empty() {
        row_ranges.push((row_start, chars.len()));
    }
    if is_cursor_line && row_col + 1 > row_width {
        row_ranges.push((chars.len(), chars.len()));
    }

    row_ranges
        .into_iter()
        .enumerate()
        .map(|(row, (start, end))| {
            let prefix = if row == 0 && is_first_line {
                CHEVRON
            } else if row == 0 {
                NEWLINE_PAD
            } else {
                ""
            };
            let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];

            let chunk_spans = if let Some(styled) = &shell_spans {
                slice_styled_spans(styled, start, end)
            } else {
                let chunk_text: String = chars[start..end].iter().collect();
                vec![Span::raw(chunk_text)]
            };

            if is_cursor_line && cursor_x >= start && cursor_x <= end {
                let local_cursor = cursor_x.saturating_sub(start);
                spans.extend(overlay_cursor(chunk_spans, local_cursor));
            } else {
                spans.extend(chunk_spans);
            }

            Line::from(spans)
        })
        .collect()
}

fn shell_highlight_spans(line: &str) -> Option<Vec<Span<'static>>> {
    if !highlight::is_ready() {
        return None;
    }
    let parsed = parse_shell_prefix(line)?;
    let prefix = &line[..parsed.prefix_len];
    let command = &line[parsed.prefix_len..];
    let shell_style = theme::current().shell_prefix;
    let mut spans = vec![Span::styled(prefix.to_owned(), shell_style)];
    let mut hl = highlight::highlighter_for_token("bash");
    for (style, text) in highlight::highlight_line(&mut hl, command) {
        spans.push(Span::styled(text, style));
    }
    Some(spans)
}

fn slice_styled_spans(
    spans: &[Span<'static>],
    char_start: usize,
    char_end: usize,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut pos = 0;
    for span in spans {
        let span_len = span.content.chars().count();
        let span_end = pos + span_len;
        if span_end <= char_start || pos >= char_end {
            pos = span_end;
            continue;
        }
        let lo = char_start.saturating_sub(pos);
        let hi = (char_end - pos).min(span_len);
        let slice: String = span.content.chars().skip(lo).take(hi - lo).collect();
        if !slice.is_empty() {
            result.push(Span::styled(slice, span.style));
        }
        pos = span_end;
    }
    result
}

fn overlay_cursor(spans: Vec<Span<'static>>, cursor_char_pos: usize) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut pos = 0;
    let mut cursor_placed = false;
    for span in spans {
        let span_len = span.content.chars().count();
        if !cursor_placed && cursor_char_pos >= pos && cursor_char_pos < pos + span_len {
            let local = cursor_char_pos - pos;
            let byte_pos = TextBuffer::char_to_byte(&span.content, local);
            let (before, after) = span.content.split_at(byte_pos);
            if !before.is_empty() {
                result.push(Span::styled(before.to_string(), span.style));
            }
            let mut cs = after.chars();
            let Some(cursor_char) = cs.next() else {
                break;
            };
            result.push(Span::styled(cursor_char.to_string(), span.style.reversed()));
            let rest: String = cs.collect();
            if !rest.is_empty() {
                result.push(Span::styled(rest.to_string(), span.style));
            }
            cursor_placed = true;
        } else {
            result.push(span);
        }
        pos += span_len;
    }
    if !cursor_placed {
        result.push(Span::styled(" ", Style::new().reversed()));
    }
    result
}

fn total_visual_lines(buffer: &TextBuffer, ew: usize, cursor_visible: bool) -> usize {
    let cursor_y = buffer.y();
    buffer
        .lines()
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mut text_len = line.width();
            if cursor_visible && i == cursor_y {
                text_len += 1;
            }
            visual_line_count(text_len, ew)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use ratatui::style::Color;
    use test_case::test_case;

    fn type_text(input: &mut InputBox, text: &str) {
        for c in text.chars() {
            input.buffer.push_char(c);
        }
    }

    fn submit_text(input: &mut InputBox, text: &str) {
        type_text(input, text);
        input.submit();
    }

    #[test]
    fn submit() {
        let mut input = InputBox::new(InputHistory::default());
        assert!(input.submit().is_none());

        type_text(&mut input, " ");
        assert!(input.submit().is_none());

        type_text(&mut input, " x ");
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "x");
        assert!(sub.images.is_empty());
        assert_eq!(input.buffer.value(), "");

        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.submit().unwrap().text, "line1\nline2");
    }

    #[test]
    fn backslash_continuation() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello\\");
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["hello", ""]);

        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "asd\\asd");
        for _ in 0..3 {
            input.buffer.move_left();
        }
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["asd", "asd"]);
    }

    const TEST_WIDTH: u16 = 80;

    #[test]
    fn height_capped_at_max() {
        let mut input = InputBox::new(InputHistory::default());
        let base = input.height(TEST_WIDTH);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        assert!(input.height(TEST_WIDTH) > base);
        assert!(input.height(TEST_WIDTH) <= MAX_INPUT_LINES + 2);
    }

    #[test]
    fn first_last_line() {
        let mut input = InputBox::new(InputHistory::default());
        assert!(input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.add_line();
        assert!(!input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.move_up();
        assert!(input.is_at_first_line());
        assert!(!input.is_at_last_line());
    }

    #[test]
    fn history() {
        let mut input = InputBox::new(InputHistory::default());

        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "");

        submit_text(&mut input, "a");
        submit_text(&mut input, "b");
        type_text(&mut input, "draft");

        input.history_up();
        assert_eq!(input.buffer.value(), "b");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");

        input.history_down();
        assert_eq!(input.buffer.value(), "b");
        input.history_down();
        assert_eq!(input.buffer.value(), "draft");

        input.buffer.clear();
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert!(input.is_at_last_line());
        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_first_line());

        input.submit();
        input.history_up();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");

        input.set_input("alpha\nbeta".into());
        input.submit();
        input.set_input("gamma\ndelta".into());
        input.submit();

        input.history_up();
        input.history_up();
        assert_eq!(input.buffer.value(), "alpha\nbeta");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "gamma\ndelta");
        assert!(input.is_at_first_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");
    }

    #[test]
    fn cursor_adds_extra_wrap_row_at_boundary() {
        let width: u16 = 12;
        let ew = effective_width(width as usize);

        let mut at_boundary = InputBox::new(InputHistory::default());
        type_text(&mut at_boundary, &"x".repeat(ew));

        let mut before_boundary = InputBox::new(InputHistory::default());
        type_text(&mut before_boundary, &"x".repeat(ew - 1));

        assert_eq!(
            at_boundary.height(width),
            before_boundary.height(width) + 1,
            "cursor at boundary should cause one extra visual line"
        );
    }

    fn render_input_with(
        input: &mut InputBox,
        width: u16,
        height: u16,
        streaming: bool,
        border_style: Style,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                input.view(frame, area, streaming, border_style, true, None);
            })
            .unwrap();
        terminal
    }

    fn render_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        render_input_with(
            input,
            width,
            height,
            false,
            Style::new().fg(theme::current().mode_build),
        )
    }

    fn has_scrollbar_thumb(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> bool {
        let buf = terminal.backend().buffer();
        (0..buf.area.height).any(|y| {
            buf.cell((buf.area.width - 1, y))
                .is_some_and(|c| c.symbol() == SCROLLBAR_THUMB)
        })
    }

    #[test_case(20, true  ; "visible_when_content_overflows")]
    #[test_case(0,  false ; "hidden_when_content_fits")]
    fn scrollbar_visibility(extra_lines: usize, expect_visible: bool) {
        let mut input = InputBox::new(InputHistory::default());
        for _ in 0..extra_lines {
            input.buffer.add_line();
        }
        let terminal = render_input(&mut input, 40, MAX_INPUT_LINES + 2);
        assert_eq!(has_scrollbar_thumb(&terminal), expect_visible);
    }

    #[test]
    fn scroll_clamped_on_content_shrink() {
        let mut input = InputBox::new(InputHistory::default());
        for _ in 0..20 {
            input.buffer.add_line();
        }
        let area_height = 5_u16;
        let _ = render_input(&mut input, 40, area_height);
        let scroll_before = input.scroll_y;
        assert!(scroll_before > 0);

        input.buffer = TextBuffer::new("short".into());
        let _ = render_input(&mut input, 40, area_height);
        assert_eq!(input.scroll_y, 0);
    }

    fn border_fg(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Color {
        let buf = terminal.backend().buffer();
        buf.cell((0, 0)).unwrap().fg
    }

    #[test_case(false, Style::new().fg(theme::current().mode_plan), theme::current().mode_plan         ; "idle_uses_mode_color")]
    #[test_case(true,  theme::current().input_border,               theme::current().input_border.fg.unwrap()  ; "streaming_uses_default_border")]
    fn border_color_matches_mode(streaming: bool, border_style: Style, expected: Color) {
        let mut input = InputBox::new(InputHistory::default());
        let terminal = render_input_with(&mut input, 40, 5, streaming, border_style);
        assert_eq!(border_fg(&terminal), expected);
    }

    #[test]
    fn multibyte_input_renders_without_panic() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "● grep> hello");
        input.buffer.move_home();
        input.buffer.move_right();
        input.buffer.move_right();
        let _ = render_input(&mut input, 40, 5);
    }

    #[test_case("●\\", true  ; "after_multibyte")]
    #[test_case("●", false   ; "inside_multibyte_would_be_false")]
    fn char_before_cursor_backslash(input: &str, expected: bool) {
        let mut input_box = InputBox::new(InputHistory::default());
        type_text(&mut input_box, input);
        assert_eq!(input_box.char_before_cursor_is_backslash(), expected);
    }

    fn rendered_row(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        row: u16,
    ) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|col| buf.cell((col, row)).unwrap().symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn prefix_on_single_line() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello");
        let terminal = render_input(&mut input, 20, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with(CHEVRON), "row: {row:?}");
        assert!(row.contains("hello"));
    }

    #[test]
    fn prefix_on_multiline() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "aaa");
        input.buffer.add_line();
        type_text(&mut input, "bbb");
        let terminal = render_input(&mut input, 20, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with(CHEVRON), "row0: {row0:?}");
        assert!(row1.starts_with(NEWLINE_PAD), "row1: {row1:?}");
    }

    #[test]
    fn wrapped_line_gets_no_padding() {
        let mut input = InputBox::new(InputHistory::default());
        let ew = effective_width(14);
        type_text(&mut input, &"x".repeat(ew + 3));
        let terminal = render_input(&mut input, 14, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with(CHEVRON), "row0: {row0:?}");
        assert!(
            !row1.starts_with(CHEVRON) && !row1.starts_with(NEWLINE_PAD),
            "wrapped row should have no padding: {row1:?}"
        );
        assert!(
            row1.starts_with("x"),
            "wrapped row should start with content: {row1:?}"
        );
    }

    #[test]
    fn copy_text_includes_prefix() {
        let input = InputBox::new(InputHistory::default());
        assert_eq!(input.copy_text(), CHEVRON);

        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.copy_text(), "❯ line1\n  line2");
    }

    #[test]
    fn placeholder_has_prefix() {
        let mut input = InputBox::new(InputHistory::default());
        let terminal = render_input(&mut input, 40, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with(CHEVRON), "placeholder row: {row:?}");
    }

    fn test_image() -> ImageSource {
        use maki_providers::ImageMediaType;
        use std::sync::Arc;
        ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="))
    }

    #[test]
    fn submit_with_images() {
        let mut input = InputBox::new(InputHistory::default());

        input.attach_image(test_image());
        let sub = input.submit().unwrap();
        assert!(sub.text.is_empty());
        assert_eq!(sub.images.len(), 1);
        assert!(input.submit().is_none(), "images cleared after submit");

        type_text(&mut input, "describe this");
        input.attach_image(test_image());
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "describe this");
        assert_eq!(sub.images.len(), 1);
    }

    const IMAGE_LABEL: &str = "1 image";

    #[test]
    fn image_label_rendered() {
        let mut input = InputBox::new(InputHistory::default());
        input.attach_image(test_image());
        let h = input.height(40);
        let terminal = render_input(&mut input, 40, h);
        let found = (0..h).any(|row| rendered_row(&terminal, row).contains(IMAGE_LABEL));
        assert!(found, "image label not found in rendered output");
    }

    #[test]
    fn height_accounts_for_pending_images() {
        let mut input = InputBox::new(InputHistory::default());
        let base_height = input.height(TEST_WIDTH);
        input.attach_image(test_image());
        assert_eq!(input.height(TEST_WIDTH), base_height + 1);
    }
}
