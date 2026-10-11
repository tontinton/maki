mod layout;

use std::cell::{RefCell, RefMut};
use std::time::{SystemTime, UNIX_EPOCH};

use self::layout::{Layout, Row, char_width};
use crate::app::shell::parse_shell_prefix;
use crate::highlight;
use crate::text_buffer::{EditResult, TextBuffer, is_newline_key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::TextEdit;
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
/// The box's top and bottom border. A box shorter than this plus one row
/// shows the user none of what it holds.
pub(crate) const BORDER_ROWS: u16 = 2;
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
const SUBAGENT_QUEUE_PLACEHOLDER: &str = "Queue a message for this subagent...";
const ASK_PREFIX: &str = "Ask maki to ";
const ASK_SUFFIX: &str = "...";
const BLANK_PLACEHOLDER: &str = " ";

#[derive(Clone, Copy)]
pub enum Placeholder {
    Suggestion,
    Blank,
    Queue,
    SubagentQueue,
}

pub enum InputAction {
    Submit(Submission),
    ContinueLine,
    Changed,
    Passthrough(KeyEvent),
    None,
}

#[derive(Default)]
pub struct Submission {
    pub text: String,
    pub images: Vec<ImageSource>,
}

impl Submission {
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
    /// `height` only gets `&self`, yet it has to bring this cache up to date
    /// too, hence the `RefCell`.
    layout: RefCell<Layout>,
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
                    None => InputAction::Submit(Submission::default()),
                };
            }
            _ => {}
        }

        match self.buffer.handle_key(key) {
            EditResult::Changed => InputAction::Changed,
            EditResult::Moved | EditResult::Ignored => InputAction::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> InputAction {
        self.follow_cursor = true;
        self.buffer.insert_text(text);
        InputAction::Changed
    }

    /// Inserting a file path mid-word looks broken ("read/tmp/x" instead of
    /// "read /tmp/x"). This adds spaces around the paste only when needed.
    pub fn handle_paste_with_spaces(&mut self, text: &str) -> InputAction {
        let value = self.buffer.value();
        let at = self.buffer.cursor_byte();

        let char_before = value[..at].chars().next_back();
        let char_after = value[at..].chars().next();

        let is_word_boundary =
            |c: char| -> bool { c.is_alphanumeric() || c == '_' || ")]}>".contains(c) };

        let needs_leading = char_before.is_some_and(is_word_boundary) && !text.starts_with(' ');
        let needs_trailing = char_after.is_some_and(is_word_boundary) && !text.ends_with(' ');

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

    /// Rewinds one editing step. The paste a mistimed `Cmd+V` drops in the
    /// middle of a sentence is one step back; a run of typing rewinds word
    /// by word. Cmd+Z is also answered inside the buffer itself, for the
    /// terminals that deliver it; Alt+Z is the portable spelling.
    pub fn undo(&mut self) -> InputAction {
        self.follow_cursor = true;
        if self.buffer.undo() {
            InputAction::Changed
        } else {
            InputAction::None
        }
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
            layout: RefCell::default(),
        }
    }

    pub fn copy_text(&self) -> String {
        self.buffer
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { CHEVRON } else { NEWLINE_PAD };
                format!("{prefix}{l}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn layout(&self, width: u16) -> RefMut<'_, Layout> {
        let mut layout = self.layout.borrow_mut();
        layout.sync(&self.buffer, effective_width(width.into()));
        layout
    }

    /// Where the drawn rows start a new buffer line, for a selection copied out
    /// of the box. It reads the row counts the box draws with, cursor row
    /// included, since a count off by one row moves every later line start and
    /// scatters the newlines.
    pub fn line_breaks(&self, content_width: u16) -> LineBreaks {
        LineBreaks::from_heights(
            self.layout(content_width)
                .heights()
                .map(|rows| u16::try_from(rows).unwrap_or(u16::MAX)),
        )
    }

    pub fn height(&self, width: u16) -> u16 {
        let rows = self.layout(width).total_rows() + u32::from(!self.pending_images.is_empty());
        let capped = rows.min(self.max_input_lines.into()) as u16;
        capped.saturating_add(BORDER_ROWS)
    }

    pub fn is_at_first_line(&self) -> bool {
        self.buffer.y() == 0
    }

    pub fn is_at_last_line(&self) -> bool {
        self.buffer.y() == self.buffer.line_count().saturating_sub(1)
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        self.buffer.value()[..self.buffer.cursor_byte()].ends_with('\\')
    }

    pub fn continue_line(&mut self) {
        self.buffer.undoable(|buf| {
            buf.remove_char();
            buf.add_line();
        });
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

    /// The text the user is composing. Under an untouched history recall that
    /// is the draft it covers, since the recall is still in history. An edited
    /// recall is new work and wins over the draft, as it does on submit.
    pub fn draft_text(&self) -> String {
        let text = self.buffer.value();
        let untouched_recall = self
            .history_index
            .and_then(|i| self.history.get(i))
            .is_some_and(|entry| entry == text);
        if untouched_recall {
            self.draft.clone()
        } else {
            text.to_owned()
        }
    }

    pub fn swap_draft(&mut self, draft: Submission) -> Submission {
        let text = self.draft_text();
        let images = mem::take(&mut self.pending_images);
        self.discard();
        self.set_input(&draft.text);
        self.pending_images = draft.images;
        Submission { text, images }
    }

    pub fn attach_image(&mut self, source: ImageSource) {
        self.pending_images.push(source);
    }

    /// The way a plugin writes to the input. It carries the box's own state
    /// the way the paste path does: the view follows the cursor again, and a
    /// value the edits changed stops counting as a recalled history entry, so
    /// the next history key does not throw the edit away. A call that only
    /// moves the cursor leaves the recall and the draft under it alone.
    ///
    /// See [`TextBuffer::apply_edits`] for what the offsets refuse.
    pub fn apply_edits(
        &mut self,
        version: u64,
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
    ) -> Result<usize, String> {
        let before = self.buffer.version();
        let caret = self.buffer.apply_edits(version, edits, cursor)?;
        self.follow_cursor = true;
        if self.buffer.version() != before {
            self.history_index = None;
            self.draft.clear();
        }
        Ok(caret)
    }

    /// Whole-value writes that are not plugin writes: a history entry, a
    /// restored draft, a rewind prompt, what came back from `$EDITOR`. The
    /// text lands verbatim, because the user composed it elsewhere and a
    /// rewrite here is what the model would be sent.
    pub fn set_input(&mut self, s: &str) {
        self.buffer.set_value(s);
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_index = match self.history_index {
            None => {
                self.draft = self.buffer.value().to_owned();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(new_index);
        self.buffer.set_value(self.history.get(new_index).unwrap());
        self.buffer.move_to_end();
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_index = Some(i + 1);
            self.buffer.set_value(self.history.get(i + 1).unwrap());
        } else {
            self.history_index = None;
            self.buffer.set_value(&mem::take(&mut self.draft));
        }
    }

    /// Returns the screen cell the caret was drawn in, reversed or not.
    ///
    /// {show_cursor} only decides whether that cell is painted as a block
    /// cursor, which an overlay owning the keyboard turns off. The cell is
    /// reported either way, so a caret-anchored window stays on it while a
    /// float holds focus to read keys.
    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        placeholder: Placeholder,
        border_style: Style,
        show_cursor: bool,
        top_right_hint: Option<Line<'_>>,
    ) -> Option<Position> {
        let content_height = area.height.saturating_sub(BORDER_ROWS);
        let (cursor_row, text_rows) = {
            let layout = self.layout(area.width);
            (layout.cursor_row(&self.buffer), layout.total_rows())
        };

        if self.follow_cursor {
            let cursor_row = u16::try_from(cursor_row).unwrap_or(u16::MAX);
            if cursor_row < self.scroll_y {
                self.scroll_y = cursor_row;
            } else if cursor_row >= self.scroll_y.saturating_add(content_height) {
                self.scroll_y = cursor_row.saturating_sub(content_height.saturating_sub(1));
            }
        }

        let total_vl = u16::try_from(text_rows + u32::from(!self.pending_images.is_empty()))
            .unwrap_or(u16::MAX);
        self.last_total_lines = total_vl;
        self.last_content_height = content_height.max(1);
        let max_scroll = self.max_scroll();
        self.scroll_y = self.scroll_y.min(max_scroll);
        let is_empty = self.buffer.is_empty();
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
                Placeholder::SubagentQueue => (SUBAGENT_QUEUE_PLACEHOLDER, Vec::new()),
                Placeholder::Blank => (BLANK_PLACEHOLDER, Vec::new()),
            };
            let mut spans = vec![super::chevron_span()];
            let head = Span::styled(head, base);
            let (with_cursor, col) = overlay_cursor(vec![head], 0, show_cursor);
            cursor_cell = Some(Position::new(display_width(&spans) + col, 0));
            spans.extend(with_cursor);
            spans.extend(tail);
            vec![Line::from(spans)]
        } else {
            let rows: Vec<Row> = self
                .layout
                .get_mut()
                .rows_from(&self.buffer, self.scroll_y.into())
                .take(content_height.into())
                .collect();
            let shell_spans = rows
                .first()
                .filter(|row| row.line == 0)
                .and_then(|_| shell_highlight_spans(self.buffer.line(0)));
            let caret = self.buffer.cursor_byte_in_line();
            let mut lines = Vec::with_capacity(rows.len() + 1);
            for (screen_row, row) in (0..).zip(rows) {
                let owns_caret = u32::from(self.scroll_y) + u32::from(screen_row) == cursor_row;
                let (drawn, col) = draw_row(
                    self.buffer.line(row.line),
                    &row,
                    shell_spans.as_deref().filter(|_| row.line == 0),
                    owns_caret.then_some(caret),
                    show_cursor,
                );
                if let Some(col) = col {
                    cursor_cell = Some(Position::new(col, screen_row));
                }
                lines.push(drawn);
            }
            lines
        };

        // Only the visible rows were drawn, so the label, which sits right
        // after the text, fits only when the text ends inside the box.
        if !self.pending_images.is_empty() && styled_lines.len() < usize::from(content_height) {
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
            .block(block);
        frame.render_widget(paragraph, area);

        if max_scroll > 0 {
            let inner = area.inner(ratatui::layout::Margin::new(0, 1));
            render_vertical_scrollbar(frame, inner, u32::from(total_vl), u32::from(self.scroll_y));
        }

        let cell = cursor_cell?;
        (cell.y < content_height && cell.x < area.width)
            .then(|| Position::new(area.x + cell.x, area.y + 1 + cell.y))
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

    pub fn set_history(&mut self, history: InputHistory) {
        self.history = history;
        self.history_index = None;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta).min(self.max_scroll());
        self.follow_cursor = false;
    }

    /// Move the text cursor to the position corresponding to a mouse click at
    /// the terminal coordinates (row, col) within the input content area.
    pub fn handle_click(&mut self, area: Rect, row: u16, col: u16) {
        let Some((y, x)) = self.click_position(area, row, col) else {
            return;
        };
        self.buffer.set_cursor(y, x);
        self.follow_cursor = true;
    }

    /// Convert a mouse click at terminal (row, col) within the input content
    /// area into a (line_index, char_index) in the text buffer, accounting
    /// for scroll offset, word-wrap, and the chevron/padding prefix.
    fn click_position(&self, area: Rect, row: u16, col: u16) -> Option<(usize, usize)> {
        let content_y = row.checked_sub(area.y)?;
        let content_x = col.checked_sub(area.x)?;

        let layout = self.layout(area.width);
        let row = layout
            .rows_from(
                &self.buffer,
                u32::from(self.scroll_y) + u32::from(content_y),
            )
            .next()?;
        let line = self.buffer.line(row.line);

        let prefix = if row.index == 0 { PREFIX_WIDTH } else { 0 };
        let mut col = usize::from(content_x.saturating_sub(prefix));
        let mut hit = row.bytes.end;
        for (i, c) in line[row.bytes.clone()].char_indices() {
            let w = char_width(c);
            if w > col {
                hit = row.bytes.start + i;
                break;
            }
            col -= w;
        }
        Some((row.line, line[..hit].chars().count()))
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

/// Draws one row of {line}. When the row owns the caret, {caret} is its byte in
/// the line and the column it lands in comes back too. Only this function
/// knows where the caret was drawn, so a terminal cursor placed from that
/// column can never drift off the cell. {reversed} paints it as a block
/// cursor, and the column is reported either way.
fn draw_row(
    line: &str,
    row: &Row,
    shell_spans: Option<&[Span<'static>]>,
    caret: Option<usize>,
    reversed: bool,
) -> (Line<'static>, Option<u16>) {
    let mut spans = match row.index {
        0 if row.line == 0 => vec![super::chevron_span()],
        0 => vec![Span::raw(NEWLINE_PAD)],
        _ => Vec::new(),
    };
    let prefix_width = display_width(&spans);
    let text = &line[row.bytes.clone()];
    let chunk = match shell_spans {
        Some(styled) => {
            let start = line[..row.bytes.start].chars().count();
            slice_styled_spans(styled, start, start + text.chars().count())
        }
        None => vec![Span::raw(text.to_owned())],
    };
    let Some(caret) = caret else {
        spans.extend(chunk);
        return (Line::from(spans), None);
    };
    let (with_cursor, col) = overlay_cursor(
        chunk,
        line[row.bytes.start..caret].chars().count(),
        reversed,
    );
    spans.extend(with_cursor);
    (Line::from(spans), Some(prefix_width + col))
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

/// Reports the display column of the cell under the cursor, measured from the
/// spans actually emitted before it so wide chars cannot throw it off, and
/// reverses that cell when {reversed}. The spans are split around it either
/// way, so the geometry is the same whoever owns the keyboard.
fn overlay_cursor(
    spans: Vec<Span<'static>>,
    cursor_char_pos: usize,
    reversed: bool,
) -> (Vec<Span<'static>>, u16) {
    let cursor_style = |style: Style| {
        if reversed {
            style.patch(theme::cursor_style())
        } else {
            style
        }
    };
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
            result.push(Span::styled(
                cursor_char.to_string(),
                cursor_style(span.style),
            ));
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
    result.push(Span::styled(" ", cursor_style(Style::new())));
    (result, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use crate::selection::{ContentRegion, ScreenSelection, extract_selected_text};
    use crate::text_buffer::test_support::text_edit;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::Color;
    use std::ops::Range;
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
        assert_eq!(input.buffer.value(), "hello\n");

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "asd\\asd");
        for _ in 0..3 {
            input.buffer.move_left();
        }
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.value(), "asd\nasd");
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

        input.set_input("alpha\nbeta");
        input.submit();
        input.set_input("gamma\ndelta");
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

    const DRAFT: &str = "draft";
    const RECALLED: &str = "recalled";
    const EDITED_RECALL: &str = "edited";

    #[test_case(None, DRAFT ; "untouched_recall_keeps_the_draft")]
    #[test_case(Some(EDITED_RECALL), EDITED_RECALL ; "edited_recall_keeps_the_edit")]
    fn draft_while_browsing_history(edit: Option<&str>, parked: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, RECALLED);
        type_text(&mut input, DRAFT);
        input.history_up();
        if let Some(edit) = edit {
            input.buffer.clear();
            type_text(&mut input, edit);
        }
        assert_eq!(input.draft_text(), parked);

        let swapped = input.swap_draft(Submission {
            text: String::new(),
            images: Vec::new(),
        });

        assert_eq!(swapped.text, parked);
    }

    #[test_case(Vec::new(), Some(1), DRAFT ; "a_cursor_move_keeps_the_recall")]
    #[test_case(vec![text_edit(0, RECALLED.len(), EDITED_RECALL)], None, EDITED_RECALL ; "an_edit_is_new_work")]
    fn plugin_write_over_a_history_recall(
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
        parked: &str,
    ) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, RECALLED);
        type_text(&mut input, DRAFT);
        input.history_up();

        let version = input.buffer.version();
        input.apply_edits(version, edits, cursor).unwrap();

        assert_eq!(input.draft_text(), parked);
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
        show_cursor: bool,
    ) -> Rendered {
        let border_style = Style::new().fg(theme::current().mode_build);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut cursor = None;
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                cursor = input.view(frame, area, placeholder, border_style, show_cursor, None);
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

        input.set_input("short");
        let _ = render_input(&mut input, 40, area_height);
        assert_eq!(input.scroll_y, 0);
    }

    /// Past `u16::MAX` rows, the scroll and the height of a box that grew
    /// under a view scrolled to the end no longer add up in a `u16`.
    #[test]
    fn a_tall_input_scrolled_to_its_end_renders_as_the_box_grows() {
        const GROWN_ROWS: u16 = 20;
        let mut input = InputBox::new(InputHistory::default(), u32::MAX);
        input.set_input(&"\n".repeat(usize::from(u16::MAX) + 1));
        input.buffer.move_to_end();
        let _ = render_input(&mut input, 40, BORDER_ROWS + 1);
        let _ = render_input(&mut input, 40, BORDER_ROWS + GROWN_ROWS);
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
    #[test_case(Placeholder::SubagentQueue, SUBAGENT_QUEUE_PLACEHOLDER ; "subagent_queue_names_the_subagent")]
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
        single_line(text).click_position(area(10), row, col)
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
        input.click_position(area(10), row, col)
    }

    #[test_case((1, 0) => Some((0, 8)); "continuation row first col maps to wrapped chunk start")]
    #[test_case((1, 1) => Some((0, 9)); "continuation row has no prefix offset")]
    fn click_position_wrapped_line_has_no_prefix((row, col): (u16, u16)) -> Option<(usize, usize)> {
        // width 10 -> ew 8 -> "abcdefghij" wraps as [0,8) then [8,10).
        single_line("abcdefghij").click_position(area(10), row, col)
    }

    /// The cursor sitting just past a full row is drawn on a row of its own,
    /// whoever owns the keyboard, so a click has to count that row too or
    /// every line below it maps one row out.
    #[test_case((1, 2) => Some((0, 8)); "the extra cursor row is its own row")]
    #[test_case((2, 2) => Some((1, 0)); "the next buffer line comes after it")]
    fn click_position_cursor_extra_row((row, col): (u16, u16)) -> Option<(usize, usize)> {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "abcdefgh");
        input.buffer.add_line();
        type_text(&mut input, "xy");
        input.buffer.set_cursor(0, 8);
        input.click_position(area(10), row, col)
    }

    // Width 12 leaves 10 text columns after the 2 cell prefix, height 6 leaves
    // 4 content rows between the borders.
    const CURSOR_WIDTH: u16 = 12;
    const CURSOR_HEIGHT: u16 = 6;
    const CURSOR_EW: usize = effective_width(CURSOR_WIDTH as usize);
    const CURSOR_STAYS_HIDDEN: &str = "the hardware cursor must never be shown";
    const NO_BLOCK_CURSOR_UNFOCUSED: &str =
        "an overlay owns the keyboard, so nothing may be reversed";
    const CARET_FG: Color = Color::Rgb(0x12, 0x34, 0x56);
    const CARET_BG: Color = Color::Rgb(0x65, 0x43, 0x21);
    const CARET_UNFOCUSED_FG: Color = Color::Rgb(0x0f, 0x0f, 0x0f);
    const CARET_THEME_WITH_CURSOR: &str = r##"
[ui]
"cursor" = { fg = "#123456", bg = "#654321" }
"cursor_unfocused" = { fg = "#0f0f0f" }
"##;
    const CARET_THEME_WITHOUT_CURSOR: &str = "";

    /// Finds the cells painted as the caret, whatever the loaded theme is.
    fn caret_cells(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<Position> {
        let buf = terminal.backend().buffer();
        buf.area
            .positions()
            .filter(|&p| buf.cell(p).is_some_and(theme::is_caret_cell))
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
        assert_eq!(caret_cells(&rendered.terminal), Vec::from_iter(expected));
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

    /// The caret carries whatever paint the theme gives it: the explicit
    /// `[ui] cursor` colors when set, the reversed fallback when not.
    fn painted_caret() -> ratatui::buffer::Cell {
        render_with_cursor_left("abc", 1)
            .terminal
            .backend()
            .buffer()
            .cell(Position::new(4, 1))
            .expect("caret cell must be on screen")
            .clone()
    }

    /// The caret carries whatever paint the theme gives it: the explicit
    /// `[ui] cursor` colors when set, the reversed fallback when not, and
    /// `[ui] cursor_unfocused` once the terminal reports lost focus.
    #[test]
    fn caret_honors_the_theme_cursor_styles() {
        theme::set(theme::Theme::from_toml(CARET_THEME_WITHOUT_CURSOR).unwrap());
        assert!(painted_caret().modifier.contains(Modifier::REVERSED));

        theme::set(theme::Theme::from_toml(CARET_THEME_WITH_CURSOR).unwrap());
        let cell = painted_caret();
        assert_eq!(cell.fg, CARET_FG);
        assert_eq!(cell.bg, CARET_BG);

        theme::set_cursor_focused(false);
        assert_eq!(painted_caret().fg, CARET_UNFOCUSED_FG);
        theme::set_cursor_focused(true);
        theme::set(theme::load_by_name("dracula").unwrap());
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
    const CURSOR_ON_THE_EDIT: &str = "the cursor the edit moved has to be on screen";
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

    /// An overlay owning the keyboard takes the block cursor away, but not the
    /// caret: a caret-anchored window is placed from the cell reported here,
    /// and reporting None teleports it to the middle of the screen.
    #[test]
    fn an_unfocused_input_reports_its_caret_without_reversing_it() {
        let mut input = single_line("hello");
        let focused = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        let unfocused = draw_input(
            &mut input,
            CURSOR_WIDTH,
            CURSOR_HEIGHT,
            Placeholder::Suggestion,
            false,
        );
        assert_eq!(
            unfocused.cursor, focused.cursor,
            "the caret cell does not move when the keyboard goes elsewhere"
        );
        assert!(
            !unfocused.terminal.backend().cursor_visible(),
            "{CURSOR_STAYS_HIDDEN}"
        );
        assert_eq!(
            caret_cells(&unfocused.terminal),
            Vec::new(),
            "{NO_BLOCK_CURSOR_UNFOCUSED}"
        );
    }

    fn plugin_write(input: &mut InputBox, range: Range<usize>, text: &str) {
        let edit = text_edit(range.start, range.end, text);
        let version = input.buffer.version();
        input.apply_edits(version, vec![edit], None).unwrap();
    }

    /// A plugin edit lands on a recalled history entry, so the value is the
    /// user's own text again. Left in the browse state, the next history key
    /// would put the entry back over the edit.
    #[test]
    fn a_plugin_edit_leaves_history_browsing() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, "recalled");
        input.history_up();
        assert_eq!(input.buffer.value(), "recalled");

        plugin_write(&mut input, 0.."recalled".len(), "written");
        input.history_down();
        assert_eq!(input.buffer.value(), "written");
    }

    /// A wheel scroll pins the view, so an edit has to bring the cursor back
    /// into it the way typing does.
    #[test]
    fn a_plugin_edit_brings_the_view_back_to_the_cursor() {
        const LINES: usize = 10;
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste(&["a"; LINES].join("\n"));
        let _ = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);

        input.scroll(LINES as i32);
        assert_eq!(input.scroll_y(), 0, "the wheel scrolled back to the top");

        let end = input.buffer.value().len();
        plugin_write(&mut input, end - 1..end, "b");
        let rendered = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert!(
            input.scroll_y() > 0,
            "the edit put the cursor on the last row, so the view has to follow"
        );
        assert!(rendered.cursor.is_some(), "{}", CURSOR_ON_THE_EDIT);
    }

    #[test_case(|input| { input.handle_key(KeyCode::Enter.into()); }                  ; "backslash_continuation")]
    #[test_case(|input| plugin_write(input, 0..1, "plugin\nwrite")                   ; "plugin_write")]
    fn each_action_undoes_in_one_step(action: fn(&mut InputBox)) {
        const BEFORE: &str = "a\\";
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.set_input(BEFORE);
        input.buffer.move_to_end();
        action(&mut input);

        assert!(matches!(input.undo(), InputAction::Changed));
        assert_eq!(input.buffer.value(), BEFORE);
        assert_eq!(input.buffer.cursor_byte(), BEFORE.len());
        assert!(matches!(input.undo(), InputAction::None));
    }
}
