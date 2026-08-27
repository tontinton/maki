use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

use crate::app::shell::parse_shell_prefix;
use crate::highlight;
use crate::text_buffer::{EditResult, TextBuffer, is_newline_key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_storage::input_history::InputHistory;
use std::mem;

use maki_providers::ImageSource;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::apply_scroll_delta;
use super::scrollbar::render_vertical_scrollbar;
use crate::selection::LineBreaks;

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
const QUEUE_PLACEHOLDER: &str = "Queue another prompt...";
const ASK_PREFIX: &str = "Ask maki to ";
const ASK_SUFFIX: &str = "...";
const BLANK_PLACEHOLDER: &str = " ";

#[derive(Clone, Copy)]
pub enum Placeholder {
    Suggestion,
    Blank,
    Queue,
}

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
    max_input_lines: u16,
    last_total_lines: u16,
    last_content_height: u16,
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
}

/// True when {c} sits flush against word content — a word char, `_`, or a
/// closer like `)]}>`. Shared by paste (add a space so the insert doesn't
/// glue on) and completion (a trigger only opens when the char before it is
/// NOT flush against a word).
pub(crate) fn is_word_adjacent(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || ")]}>".contains(c)
}

impl InputBox {
    /// Inserting a file path mid-word looks broken ("read/tmp/x" instead of
    /// "read /tmp/x"). This adds spaces around the paste only when needed.
    pub fn handle_paste_with_spaces(&mut self, text: &str) -> InputAction {
        let line = &self.buffer.lines()[self.buffer.y()];
        let bx = TextBuffer::char_to_byte(line, self.buffer.x());

        let char_before = line[..bx].chars().next_back();
        let char_after = line[bx..].chars().next();

        let needs_leading = char_before.is_some_and(is_word_adjacent) && !text.starts_with(' ');
        let needs_trailing = char_after.is_some_and(is_word_adjacent) && !text.ends_with(' ');

        if !needs_leading && !needs_trailing {
            return self.handle_paste(text);
        }

        let mut spaced = String::with_capacity(
            text.len() + usize::from(needs_leading) + usize::from(needs_trailing),
        );

        if needs_leading {
            spaced.push(' ');
        }
        spaced.push_str(text);
        if needs_trailing {
            spaced.push(' ');
        }

        self.handle_paste(&spaced)
    }

    pub fn new(history: InputHistory, max_input_lines: u32) -> Self {
        let max_input_lines = max_input_lines.clamp(1, u16::MAX as u32 - 2) as u16;
        Self {
            buffer: TextBuffer::new(String::new()),
            history,
            history_index: None,
            draft: String::new(),
            scroll_y: 0,
            follow_cursor: true,
            placeholder_hint: random_placeholder_hint(),
            pending_images: Vec::new(),
            max_input_lines,
            last_total_lines: 1,
            last_content_height: 1,
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

    /// Where the drawn rows start a new buffer line, for a selection copied out
    /// of the box. It walks the same greedy wrap the box draws with, cursor row
    /// included, since a count off by one row moves every later line start and
    /// scatters the newlines.
    pub fn line_breaks(&self, content_width: u16) -> LineBreaks {
        let ew = effective_width(content_width as usize);
        let cursor_y = self.buffer.y();
        LineBreaks::from_heights(
            self.buffer
                .lines()
                .iter()
                .enumerate()
                .map(|(i, line)| wrapped_row_count(line, ew, i == cursor_y) as u16),
        )
    }

    pub fn height(&self, width: u16) -> u16 {
        let ew = effective_width(width as usize);
        let mut visual_lines = total_visual_lines(&self.buffer, ew, true);
        if !self.pending_images.is_empty() {
            visual_lines += 1;
        }
        let capped = visual_lines.min(self.max_input_lines as usize);
        (capped + 2) as u16
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

    /// The row the cursor is drawn on, counted from the top of the buffer, so
    /// the scroll can bring exactly that row into view.
    fn visual_cursor_y(&self, ew: usize) -> u16 {
        let lines = self.buffer.lines();
        let lines_above: usize = lines
            .iter()
            .take(self.buffer.y())
            .map(|line| wrapped_row_count(line, ew, false))
            .sum();
        let row = lines_above + wrapped_cursor_row(&lines[self.buffer.y()], ew, self.buffer.x());
        u16::try_from(row).unwrap_or(u16::MAX)
    }

    /// Returns the screen cell it reversed for the cursor, if any.
    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        placeholder: Placeholder,
        border_style: Style,
        focused: bool,
        top_right_hint: Option<Line<'_>>,
    ) -> Option<Position> {
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
        self.last_total_lines = total_vl;
        self.last_content_height = content_height.max(1);
        let max_scroll = self.max_scroll();
        self.scroll_y = self.scroll_y.min(max_scroll);

        let is_empty = self.buffer.value().is_empty();
        let mut cursor_cell: Option<Position> = None;
        let mut styled_lines: Vec<Line> = if is_empty && self.pending_images.is_empty() {
            let base = theme::current().input_placeholder;
            let (head, tail) = match placeholder {
                Placeholder::Suggestion => (
                    ASK_PREFIX,
                    vec![
                        Span::styled(self.placeholder_hint, base.add_modifier(Modifier::ITALIC)),
                        Span::styled(ASK_SUFFIX, base),
                    ],
                ),
                Placeholder::Queue => (QUEUE_PLACEHOLDER, Vec::new()),
                Placeholder::Blank => (BLANK_PLACEHOLDER, Vec::new()),
            };
            let mut spans = vec![super::chevron_span()];
            let head = Span::styled(head, base);
            if focused {
                let (with_cursor, col) = overlay_cursor(vec![head], 0);
                cursor_cell = Some(Position::new(display_width(&spans) + col, 0));
                spans.extend(with_cursor);
            } else {
                spans.push(head);
            }
            spans.extend(tail);
            vec![Line::from(spans)]
        } else {
            let cursor_y = self.buffer.y();
            let cursor_x = self.buffer.x();
            let mut lines: Vec<Line> = Vec::new();
            for (i, line) in self.buffer.lines().iter().enumerate() {
                let is_cursor_line = i == cursor_y && focused;
                let shell_spans = if i == 0 {
                    shell_highlight_spans(line)
                } else {
                    None
                };
                let (rows, cursor) = wrap_line(
                    line,
                    ew,
                    is_cursor_line,
                    cursor_x,
                    i == 0,
                    shell_spans.as_deref(),
                );
                if let Some(cell) = cursor
                    && let Ok(row) = u16::try_from(lines.len() + usize::from(cell.y))
                {
                    cursor_cell = Some(Position::new(cell.x, row));
                }
                lines.extend(rows);
            }
            lines
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
            render_vertical_scrollbar(frame, inner, u32::from(total_vl), u32::from(self.scroll_y));
        }

        let cell = cursor_cell?;
        let y = cell.y.checked_sub(self.scroll_y)?;
        (y < content_height && cell.x < area.width)
            .then(|| Position::new(area.x + cell.x, area.y + 1 + y))
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines
            .saturating_sub(self.last_content_height)
    }

    pub fn scroll_y(&self) -> u16 {
        self.scroll_y
    }

    pub fn history(&self) -> &InputHistory {
        &self.history
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta).min(self.max_scroll());
        self.follow_cursor = false;
    }

    /// Move the text cursor to the position corresponding to a mouse click at
    /// the terminal coordinates (row, col) within the input content area.
    pub fn handle_click(&mut self, area: Rect, row: u16, col: u16, focused: bool) {
        let Some((y, x)) = self.click_position(area, row, col, focused) else {
            return;
        };
        self.buffer.set_cursor(y, x);
        self.follow_cursor = true;
    }

    /// Convert a mouse click at terminal (row, col) within the input content
    /// area into a (line_index, char_index) in the text buffer, accounting
    /// for scroll offset, word-wrap, and the chevron/padding prefix.
    fn click_position(
        &self,
        area: Rect,
        row: u16,
        col: u16,
        focused: bool,
    ) -> Option<(usize, usize)> {
        let content_y = row.checked_sub(area.y)?;
        let content_x = col.checked_sub(area.x)?;

        let ew = effective_width(area.width as usize);
        let visual_line = content_y as usize + self.scroll_y as usize;

        let cursor_line = self.buffer.y();
        let mut visual = 0usize;

        for (buf_line_idx, line) in self.buffer.lines().iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let widths: Vec<usize> = chars.iter().copied().map(char_width).collect();

            let is_cursor_line = buf_line_idx == cursor_line && focused;
            let ranges = wrap_ranges(&widths, ew, is_cursor_line);

            let n_visual_rows = ranges.len();

            if visual_line < visual + n_visual_rows {
                let wrap_row = visual_line - visual;
                let (row_char_start, row_char_end) = ranges[wrap_row];

                let row_display_width: usize = widths[row_char_start..row_char_end].iter().sum();

                // The first visual row of each buffer line has a 2-cell prefix
                // (chevron or continuation padding).  Wrapped rows have none.
                let text_col = if wrap_row == 0 {
                    (content_x as usize).saturating_sub(PREFIX_WIDTH as usize)
                } else {
                    content_x as usize
                };
                let text_col = text_col.min(row_display_width);

                // Walk character widths to find which char the column hits.
                let mut accum = 0;
                let mut char_idx = row_char_start;
                for &w in &widths[row_char_start..row_char_end] {
                    if accum + w > text_col {
                        break;
                    }
                    accum += w;
                    char_idx += 1;
                }

                return Some((buf_line_idx, char_idx));
            }
            visual += n_visual_rows;
        }

        None
    }
}

fn random_placeholder_hint() -> &'static str {
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as usize % PLACEHOLDER_SUGGESTIONS.len())
        .unwrap_or(0);
    PLACEHOLDER_SUGGESTIONS[idx]
}

const fn effective_width(content_width: usize) -> usize {
    content_width.saturating_sub(PREFIX_WIDTH as usize)
}

/// Wraps one buffer line into rendered rows. When the line holds the cursor it
/// also reports the cell it reversed, `y` rows down from the start of the line
/// and `x` columns into that row. Only this function knows where the cursor was
/// drawn, so a terminal cursor placed from it can never drift off that cell.
fn wrap_line(
    line: &str,
    ew: usize,
    is_cursor_line: bool,
    cursor_x: usize,
    is_first_line: bool,
    shell_spans: Option<&[Span<'static>]>,
) -> (Vec<Line<'static>>, Option<Position>) {
    let chars: Vec<char> = line.chars().collect();
    let widths: Vec<usize> = chars.iter().copied().map(char_width).collect();

    let ranges = wrap_ranges(&widths, ew, is_cursor_line);
    let row_count = ranges.len();
    let mut cursor_cell = None;
    let rows = ranges
        .into_iter()
        .enumerate()
        .map(|(row, (start, end))| {
            let prefix_span = if row == 0 && is_first_line {
                super::chevron_span()
            } else if row == 0 {
                Span::raw(NEWLINE_PAD)
            } else {
                Span::raw("")
            };
            let prefix_width = prefix_span.width() as u16;
            let mut spans = vec![prefix_span];

            let chunk_spans = if let Some(styled) = &shell_spans {
                slice_styled_spans(styled, start, end)
            } else {
                let chunk_text: String = chars[start..end].iter().collect();
                vec![Span::raw(chunk_text)]
            };

            // A cursor sitting on a wrap boundary belongs to the row that
            // starts there, otherwise both rows would draw it.
            let owns_cursor =
                is_cursor_line && cursor_x >= start && (cursor_x < end || row + 1 == row_count);
            if owns_cursor {
                let (with_cursor, col) = overlay_cursor(chunk_spans, cursor_x - start);
                cursor_cell = Some(Position::new(prefix_width + col, row as u16));
                spans.extend(with_cursor);
            } else {
                spans.extend(chunk_spans);
            }

            Line::from(spans)
        })
        .collect();
    (rows, cursor_cell)
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(1)
}

/// The one place that knows how a line breaks into rows: greedy wrapping, where
/// a char that no longer fits moves to the next row and leaves the gap behind.
/// Calls `emit` with the char range of every row, and appends an empty row when
/// the cursor sits just past a completely full last row.
///
/// Everything that needs row numbers goes through here. Dividing the cursor
/// column by the width instead looks close enough for plain text, but it cannot
/// see those gaps, so with wide chars it lands on the wrong row.
fn walk_wrap_rows(
    widths: impl IntoIterator<Item = usize>,
    ew: usize,
    is_cursor_line: bool,
    mut emit: impl FnMut(usize, usize),
) {
    let row_width = ew.max(1);
    let mut char_count = 0;
    let mut row_start = 0;
    let mut row_col = 0;
    for (i, w) in widths.into_iter().enumerate() {
        if row_col + w > row_width && row_col > 0 {
            emit(row_start, i);
            row_start = i;
            row_col = 0;
        }
        row_col += w;
        char_count = i + 1;
    }
    emit(row_start, char_count);
    if is_cursor_line && row_col + 1 > row_width {
        emit(char_count, char_count);
    }
}

/// Row ranges of char indices, for the callers that need to slice the text.
fn wrap_ranges(widths: &[usize], ew: usize, is_cursor_line: bool) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    walk_wrap_rows(widths.iter().copied(), ew, is_cursor_line, |start, end| {
        ranges.push((start, end));
    });
    ranges
}

/// How many rows a line takes once wrapped. Counting instead of collecting
/// keeps this allocation free, which matters because it runs per line per frame.
fn wrapped_row_count(line: &str, ew: usize, is_cursor_line: bool) -> usize {
    let mut rows = 0;
    walk_wrap_rows(line.chars().map(char_width), ew, is_cursor_line, |_, _| {
        rows += 1;
    });
    rows
}

/// Which row of the wrapped line holds `cursor_x`, that is the last row that
/// starts at or before it. Rows are contiguous, so this picks the same row the
/// renderer reverses a cell on, including on a wrap boundary.
fn wrapped_cursor_row(line: &str, ew: usize, cursor_x: usize) -> usize {
    let (mut row, mut owner) = (0, 0);
    walk_wrap_rows(line.chars().map(char_width), ew, true, |start, _| {
        if start <= cursor_x {
            owner = row;
        }
        row += 1;
    });
    owner
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
    let mut hl = maki_highlight::Highlighter::for_token("bash");
    for span in highlight::highlight_line(&mut hl, command) {
        spans.push(span);
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

fn display_width(spans: &[Span<'_>]) -> u16 {
    spans.iter().map(Span::width).sum::<usize>() as u16
}

/// Reverses the cell under the cursor and reports its display column, measured
/// from the spans actually emitted before it so wide chars cannot throw it off.
fn overlay_cursor(spans: Vec<Span<'static>>, cursor_char_pos: usize) -> (Vec<Span<'static>>, u16) {
    let mut result = Vec::new();
    let mut pos = 0;
    let mut cursor_col = None;
    for span in spans {
        let span_len = span.content.chars().count();
        if cursor_col.is_none() && cursor_char_pos >= pos && cursor_char_pos < pos + span_len {
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
            cursor_col = Some(display_width(&result));
            result.push(Span::styled(cursor_char.to_string(), span.style.reversed()));
            let rest: String = cs.collect();
            if !rest.is_empty() {
                result.push(Span::styled(rest.to_string(), span.style));
            }
        } else {
            result.push(span);
        }
        pos += span_len;
    }
    if let Some(col) = cursor_col {
        return (result, col);
    }
    let col = display_width(&result);
    result.push(Span::styled(" ", Style::new().reversed()));
    (result, col)
}

fn total_visual_lines(buffer: &TextBuffer, ew: usize, cursor_visible: bool) -> usize {
    let cursor_y = buffer.y();
    buffer
        .lines()
        .iter()
        .enumerate()
        .map(|(i, line)| wrapped_row_count(line, ew, cursor_visible && i == cursor_y))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use crate::selection::{ContentRegion, ScreenSelection, extract_selected_text};
    use ratatui::layout::{Position, Rect};
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
        let mut input = InputBox::new(InputHistory::default(), 20);
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
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "hello\\");
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["hello", ""]);

        let mut input = InputBox::new(InputHistory::default(), 20);
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
        let mut input = InputBox::new(InputHistory::default(), 20);
        let base = input.height(TEST_WIDTH);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        assert!(input.height(TEST_WIDTH) > base);
        assert!(input.height(TEST_WIDTH) <= 20 + 2);
    }

    #[test]
    fn height_respects_configured_max() {
        let mut input = InputBox::new(InputHistory::default(), 3);
        for _ in 0..10 {
            input.buffer.add_line();
        }
        assert_eq!(input.height(TEST_WIDTH), 3 + 2);
    }

    #[test]
    fn first_last_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
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
        let mut input = InputBox::new(InputHistory::default(), 20);

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

        let mut at_boundary = InputBox::new(InputHistory::default(), 20);
        type_text(&mut at_boundary, &"x".repeat(ew));

        let mut before_boundary = InputBox::new(InputHistory::default(), 20);
        type_text(&mut before_boundary, &"x".repeat(ew - 1));

        assert_eq!(
            at_boundary.height(width),
            before_boundary.height(width) + 1,
            "cursor at boundary should cause one extra visual line"
        );
    }

    fn draw_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
        placeholder: Placeholder,
        focused: bool,
    ) -> Rendered {
        let border_style = Style::new().fg(theme::current().mode_build);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut cursor = None;
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                cursor = input.view(frame, area, placeholder, border_style, focused, None);
            })
            .unwrap();
        Rendered { terminal, cursor }
    }

    fn render_input_with(
        input: &mut InputBox,
        width: u16,
        height: u16,
        placeholder: Placeholder,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        draw_input(input, width, height, placeholder, true).terminal
    }

    fn render_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        render_input_with(input, width, height, Placeholder::Suggestion)
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
        let mut input = InputBox::new(InputHistory::default(), 20);
        for _ in 0..extra_lines {
            input.buffer.add_line();
        }
        let terminal = render_input(&mut input, 40, 20 + 2);
        assert_eq!(has_scrollbar_thumb(&terminal), expect_visible);
    }

    #[test]
    fn scroll_clamped_on_content_shrink() {
        let mut input = InputBox::new(InputHistory::default(), 20);
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

    #[test]
    fn multibyte_input_renders_without_panic() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "● grep> hello");
        input.buffer.move_home();
        input.buffer.move_right();
        input.buffer.move_right();
        let _ = render_input(&mut input, 40, 5);
    }

    #[test_case("●\\", true  ; "after_multibyte")]
    #[test_case("●", false   ; "inside_multibyte_would_be_false")]
    fn char_before_cursor_backslash(input: &str, expected: bool) {
        let mut input_box = InputBox::new(InputHistory::default(), 20);
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
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "hello");
        let terminal = render_input(&mut input, 20, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with(CHEVRON), "row: {row:?}");
        assert!(row.contains("hello"));
    }

    #[test]
    fn prefix_on_multiline() {
        let mut input = InputBox::new(InputHistory::default(), 20);
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
        let mut input = InputBox::new(InputHistory::default(), 20);
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
        let input = InputBox::new(InputHistory::default(), 20);
        assert_eq!(input.copy_text(), CHEVRON);

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.copy_text(), "❯ line1\n  line2");
    }

    /// Eleven double width chars take three rows of an eleven column line, one
    /// more than dividing width by columns gives, because the char that no
    /// longer fits leaves a gap behind.
    #[test]
    fn a_partial_selection_of_a_wide_char_input_breaks_where_the_box_wrapped() {
        const WIDTH: u16 = 13;
        const HEIGHT: u16 = 8;
        const WIDE_LINE: &str = "一二三四五六七八九十百";
        const TAIL: &str = "tail";

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, WIDE_LINE);
        input.buffer.add_line();
        type_text(&mut input, TAIL);
        let terminal = render_input(&mut input, WIDTH, HEIGHT);

        let copy_text = input.copy_text();
        let regions = [ContentRegion {
            area: Rect::new(0, 1, WIDTH, HEIGHT - 2),
            raw_text: &copy_text,
            line_breaks: input.line_breaks(WIDTH),
        }];
        // Leaving the first drawn row out keeps the region partly selected, so
        // the copy walks cells and the row counting decides where it breaks.
        let selection = ScreenSelection {
            start_row: 2,
            start_col: 0,
            end_row: 4,
            end_col: WIDTH - 1,
        };

        let text = extract_selected_text(terminal.backend().buffer(), &selection, &regions);
        assert_eq!(text, format!("六七八九十百\n{NEWLINE_PAD}{TAIL}"));
    }

    #[test_case(Placeholder::Blank, "" ; "blank_shows_only_the_chevron")]
    #[test_case(Placeholder::Queue, QUEUE_PLACEHOLDER ; "queue_asks_for_another_prompt")]
    fn placeholder_row(placeholder: Placeholder, expected: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let terminal = render_input_with(&mut input, 40, 4, placeholder);
        assert_eq!(
            rendered_row(&terminal, 1),
            format!("{CHEVRON}{expected}").trim_end()
        );
    }

    #[test]
    fn suggestion_placeholder_shows_a_hint() {
        const HINT: &str = "fix a bug";
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.placeholder_hint = HINT;
        let terminal = render_input(&mut input, 40, 4);
        assert_eq!(
            rendered_row(&terminal, 1),
            format!("{CHEVRON}{ASK_PREFIX}{HINT}{ASK_SUFFIX}")
        );
    }

    fn test_image() -> ImageSource {
        use maki_providers::ImageMediaType;
        use std::sync::Arc;
        ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="))
    }

    #[test]
    fn submit_with_images() {
        let mut input = InputBox::new(InputHistory::default(), 20);

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
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.attach_image(test_image());
        let h = input.height(40);
        let terminal = render_input(&mut input, 40, h);
        let found = (0..h).any(|row| rendered_row(&terminal, row).contains(IMAGE_LABEL));
        assert!(found, "image label not found in rendered output");
    }

    #[test]
    fn height_accounts_for_pending_images() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let base_height = input.height(TEST_WIDTH);
        input.attach_image(test_image());
        assert_eq!(input.height(TEST_WIDTH), base_height + 1);
    }

    #[test_case("read", "src/main.rs", " src/main.rs" ; "leading_after_ascii")]
    #[test_case("打开", "src/main.rs", " src/main.rs" ; "leading_after_unicode")]
    #[test_case("", "src/main.rs", "src/main.rs" ; "no_leading_at_start")]
    #[test_case("read ", "src/main.rs", "src/main.rs" ; "no_leading_after_space")]
    #[test_case("--file=", "src/main.rs", "src/main.rs" ; "no_leading_after_equals")]
    #[test_case("/", "src/main.rs", "src/main.rs" ; "no_leading_after_slash")]
    #[test_case("\"", "src/main.rs", "src/main.rs" ; "no_leading_after_quote")]
    #[test_case("'", "src/main.rs", "src/main.rs" ; "no_leading_after_squote")]
    #[test_case("foo_", "src/main.rs", " src/main.rs" ; "leading_after_underscore")]
    #[test_case("$(cmd)", "src/main.rs", " src/main.rs" ; "leading_after_closing_paren")]
    #[test_case("arr[0]", "src/main.rs", " src/main.rs" ; "leading_after_closing_bracket")]
    fn paste_with_spaces_leading(before: &str, paste: &str, expected_suffix: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, before);
        input.handle_paste_with_spaces(paste);
        assert_eq!(input.buffer.value(), format!("{before}{expected_suffix}"));
    }

    #[test_case("file", 0, "/tmp/foo", "/tmp/foo file" ; "trailing_before_ascii")]
    #[test_case("を読む", 0, "/tmp/foo", "/tmp/foo を読む" ; "trailing_before_unicode")]
    #[test_case("foobar", 3, "src/main.rs", "foo src/main.rs bar" ; "both_sides_mid_word")]
    #[test_case("in  between", 3, "file.rs", "in file.rs between" ; "neither_side_between_spaces")]
    #[test_case("read ''", 6, "src/main.rs", "read 'src/main.rs'" ; "neither_side_between_quotes")]
    fn paste_with_spaces_at_cursor(before: &str, cursor_at: usize, paste: &str, expected: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, before);
        let back = before.chars().count() - cursor_at;
        for _ in 0..back {
            input.buffer.move_left();
        }
        input.handle_paste_with_spaces(paste);
        assert_eq!(input.buffer.value(), expected);
    }

    #[test]
    fn paste_with_spaces_empty_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "file.rs");
    }

    #[test]
    fn paste_with_spaces_text_has_leading_space() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "read");
        input.handle_paste_with_spaces(" file.rs");
        assert_eq!(input.buffer.value(), "read file.rs");
    }

    #[test]
    fn paste_with_spaces_text_has_trailing_space() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "file");
        for _ in 0..4 {
            input.buffer.move_left();
        }
        input.handle_paste_with_spaces("src/main.rs ");
        assert_eq!(input.buffer.value(), "src/main.rs file");
    }

    #[test]
    fn paste_with_spaces_multiline_buffer_cursor_on_second_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("first\nread");
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "first\nread file.rs");
    }

    #[test]
    fn paste_with_spaces_cursor_at_end_no_trailing() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "read");
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "read file.rs");
    }

    // ew = area.width - PREFIX_WIDTH (2); with width 10, row_width is 8.
    fn area(width: u16) -> Rect {
        Rect::new(0, 0, width, 10)
    }

    fn single_line(text: &str) -> InputBox {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, text);
        input
    }

    #[test_case("abc", (0, 0) => Some((0, 0)); "click in chevron prefix of first row")]
    #[test_case("abc", (0, 1) => Some((0, 0)); "click on trailing part of prefix")]
    #[test_case("abc", (0, 2) => Some((0, 0)); "click on first text column")]
    #[test_case("abc", (0, 9) => Some((0, 3)); "click past end of line clamps to line end")]
    #[test_case("abc", (1, 0) => None; "click below content returns None")]
    fn click_position_prefix_and_clamp(
        text: &str,
        (row, col): (u16, u16),
    ) -> Option<(usize, usize)> {
        single_line(text).click_position(area(10), row, col, true)
    }

    #[test_case((1, 0) => Some((1, 0)); "second line col 0 is its start")]
    #[test_case((1, 1) => Some((1, 0)); "second line prefix occupies its first two cols")]
    #[test_case((1, 3) => Some((1, 1)); "second line text starts after the prefix")]
    #[test_case((1, 4) => Some((1, 2)); "second line maps last column")]
    fn click_position_second_line_prefix((row, col): (u16, u16)) -> Option<(usize, usize)> {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "abc");
        input.buffer.add_line();
        type_text(&mut input, "def");
        input.click_position(area(10), row, col, true)
    }

    #[test_case((1, 0) => Some((0, 8)); "continuation row first col maps to wrapped chunk start")]
    #[test_case((1, 1) => Some((0, 9)); "continuation row has no prefix offset")]
    fn click_position_wrapped_line_has_no_prefix((row, col): (u16, u16)) -> Option<(usize, usize)> {
        // width 10 -> ew 8 -> "abcdefghij" wraps as [0,8) then [8,10).
        single_line("abcdefghij").click_position(area(10), row, col, true)
    }

    #[test_case(true, (1, 2) => Some((0, 8)); "focused full cursor line gets an extra row")]
    #[test_case(false, (1, 2) => Some((1, 0)); "unfocused full cursor line has no extra row")]
    fn click_position_cursor_extra_row(
        focused: bool,
        (row, col): (u16, u16),
    ) -> Option<(usize, usize)> {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "abcdefgh");
        input.buffer.add_line();
        type_text(&mut input, "xy");
        input.buffer.set_cursor(0, 8);
        input.click_position(area(10), row, col, focused)
    }

    // Width 12 leaves 10 text columns after the 2 cell prefix, height 6 leaves
    // 4 content rows between the borders.
    const CURSOR_WIDTH: u16 = 12;
    const CURSOR_HEIGHT: u16 = 6;
    const CURSOR_EW: usize = effective_width(CURSOR_WIDTH as usize);

    fn reversed_cells(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> Vec<Position> {
        let buf = terminal.backend().buffer();
        buf.area
            .positions()
            .filter(|&p| {
                buf.cell(p)
                    .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
            })
            .collect()
    }

    struct Rendered {
        terminal: ratatui::Terminal<ratatui::backend::TestBackend>,
        cursor: Option<Position>,
    }

    /// An IME anchors its preedit text to the terminal cursor, so the box has to
    /// report the very cell it reversed, and no other. The hardware cursor stays
    /// hidden: shown, it would invert that cell back to plain text.
    fn assert_cursor_at(rendered: &Rendered, expected: Option<Position>) {
        assert!(!rendered.terminal.backend().cursor_visible());
        assert_eq!(rendered.cursor, expected);
        assert_eq!(reversed_cells(&rendered.terminal), Vec::from_iter(expected));
    }

    fn render_cursor(input: &mut InputBox, width: u16, height: u16) -> Rendered {
        draw_input(input, width, height, Placeholder::Suggestion, true)
    }

    fn render_with_cursor_left(text: &str, left: usize) -> Rendered {
        let mut input = single_line(text);
        for _ in 0..left {
            input.buffer.move_left();
        }
        render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT)
    }

    #[test_case("hello", 0, Position::new(7, 1) ; "ascii_at_end_of_line")]
    #[test_case("hello", 2, Position::new(5, 1) ; "ascii_in_the_middle")]
    #[test_case("a漢b", 0, Position::new(6, 1)  ; "wide_char_advances_two_columns")]
    #[test_case("a漢b", 1, Position::new(5, 1)  ; "after_a_wide_char")]
    #[test_case("a漢b", 2, Position::new(3, 1)  ; "on_a_wide_char")]
    fn terminal_cursor_tracks_the_software_cursor(text: &str, left: usize, expected: Position) {
        assert_cursor_at(&render_with_cursor_left(text, left), Some(expected));
    }

    #[test_case(CURSOR_EW, 0, Position::new(0, 2)         ; "at_the_boundary_it_starts_the_next_row")]
    #[test_case(CURSOR_EW + 1, 0, Position::new(1, 2)     ; "past_the_boundary_it_trails_the_text")]
    #[test_case(CURSOR_EW + 2, 2, Position::new(0, 2)     ; "inside_the_text_at_the_boundary")]
    #[test_case(CURSOR_EW * 2 + 2, 2, Position::new(0, 3) ; "at_a_continuation_row_boundary")]
    fn terminal_cursor_at_wrap_boundary(chars: usize, left: usize, expected: Position) {
        assert_cursor_at(
            &render_with_cursor_left(&"x".repeat(chars), left),
            Some(expected),
        );
    }

    #[test]
    fn terminal_cursor_on_second_buffer_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("aaa\nbb");
        let rendered = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_cursor_at(&rendered, Some(Position::new(PREFIX_WIDTH + 2, 2)));
    }

    #[test]
    fn terminal_cursor_on_empty_input_sits_after_the_chevron() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let rendered = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_cursor_at(&rendered, Some(Position::new(PREFIX_WIDTH, 1)));
    }

    #[test]
    fn terminal_cursor_follows_vertical_scroll() {
        const LINES: usize = 10;
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste(&["a"; LINES].join("\n"));

        let rendered = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert!(input.scroll_y() > 0, "input should have scrolled");
        assert_cursor_at(
            &rendered,
            Some(Position::new(PREFIX_WIDTH + 1, CURSOR_HEIGHT - 2)),
        );

        input.scroll(LINES as i32);
        assert_eq!(input.scroll_y(), 0, "should be back at the top");
        assert_cursor_at(
            &render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT),
            None,
        );
    }

    // Issue #865 in miniature. `a` plus five wide chars is 11 columns, so the
    // last one does not fit in the 10 column row and starts a second row while
    // leaving a hole behind. With only one content row the viewport has to
    // scroll down to that second row, or the reversed cell and the IME with it
    // end up off screen.
    const WIDE_WRAP_LINE: &str = "a漢漢漢漢漢";
    const WIDE_WRAP_HEIGHT: u16 = 3;
    const WIDE_WRAP_SCROLL: u16 = 1;
    const SHOULD_FOLLOW_WIDE_WRAP: &str = "scroll should follow the cursor onto the wrapped row";

    #[test_case(0, Position::new(2, 1) ; "past_the_wide_char_that_wrapped")]
    #[test_case(1, Position::new(0, 1) ; "on_the_wide_char_that_wrapped")]
    fn terminal_cursor_follows_a_wide_char_onto_the_next_row(left: usize, expected: Position) {
        let mut input = single_line(WIDE_WRAP_LINE);
        for _ in 0..left {
            input.buffer.move_left();
        }
        let rendered = render_cursor(&mut input, CURSOR_WIDTH, WIDE_WRAP_HEIGHT);
        assert_eq!(
            input.scroll_y(),
            WIDE_WRAP_SCROLL,
            "{SHOULD_FOLLOW_WIDE_WRAP}"
        );
        assert_cursor_at(&rendered, Some(expected));
    }

    #[test]
    fn unfocused_input_leaves_the_terminal_cursor_alone() {
        let mut input = single_line("hello");
        let rendered = draw_input(
            &mut input,
            CURSOR_WIDTH,
            CURSOR_HEIGHT,
            Placeholder::Suggestion,
            false,
        );
        assert_cursor_at(&rendered, None);
    }
}
