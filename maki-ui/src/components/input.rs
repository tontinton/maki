mod decor;
mod layout;

use std::cell::{RefCell, RefMut};
use std::time::{SystemTime, UNIX_EPOCH};

use self::decor::{LangCache, LineDecor};
use self::layout::{Layout, Piece, Row};
use crate::text_buffer::{EditResult, Mark, Marks, Prompt, TextBuffer, Writers, is_newline_key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_lua::{InputMarks, InputMode, TextEdit};
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
use crate::selection::ScreenSelection;

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

/// What opens the command palette at the start of [`sent_text`].
pub(crate) const COMMAND_PREFIX: char = '/';

/// The input as Enter reads it, trimmed. The command palette, the send and
/// history all read it so, which keeps a `/yolo ` from going out as a
/// message and coming back from history as a command.
pub(crate) fn sent_text(value: &str) -> &str {
    value.trim()
}

#[derive(Clone, Copy)]
pub enum Placeholder {
    Suggestion,
    Blank,
    Queue,
    SubagentQueue,
}

pub enum InputAction {
    /// Enter sends. The input is still there, so whoever sends it can look at
    /// it first.
    Submit,
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

/// What a chat keeps of the input while another chat has the box.
#[derive(Default)]
pub struct Draft {
    pub text: String,
    pub images: Vec<ImageSource>,
    marks: Marks,
    writers: Writers,
}

impl Draft {
    pub fn clear_plugin_marks(&mut self, plugin: &str) {
        self.marks.clear_plugin(plugin);
    }

    pub fn writers(&self) -> &Writers {
        &self.writers
    }
}

pub struct InputBox {
    pub(crate) buffer: TextBuffer,
    history: InputHistory,
    history_index: Option<usize>,
    /// What a history recall covers, marks and all, put back
    /// when the user walks past the newest entry.
    draft: Prompt,
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
    langs: LangCache,
}

impl InputBox {
    /// In normal mode a key never types or deletes. It still moves the
    /// cursor, walks the history and sends, and `Enter` always sends, since a
    /// `\` before the cursor only means a new line while typing.
    ///
    /// Shift with a motion selects in either mode, since selecting edits
    /// nothing. It runs before the history keys, so Shift+Up on the first
    /// line never swaps out the text being selected. `Esc` with a selection
    /// only drops it, so it never arms the rewind.
    pub fn handle_key(&mut self, key: KeyEvent, mode: InputMode) -> InputAction {
        self.follow_cursor = true;
        let insert = mode == InputMode::Insert;

        if key.modifiers.contains(KeyModifiers::SHIFT)
            && self.buffer.handle_motion_key(key, true) == EditResult::Moved
        {
            return InputAction::None;
        }
        match key.code {
            KeyCode::Esc if self.buffer.deselect() => return InputAction::None,
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
                if !insert {
                    return InputAction::None;
                }
                self.newline();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter if insert && self.char_before_cursor_is_backslash() => {
                self.continue_line();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter => return InputAction::Submit,
            _ => {}
        }

        let result = if insert {
            self.buffer.handle_key(key)
        } else {
            self.buffer.handle_motion_key(key, false)
        };
        match result {
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
    pub fn undo(&mut self) -> bool {
        self.follow_cursor = true;
        self.buffer.undo()
    }

    pub fn redo(&mut self) -> bool {
        self.follow_cursor = true;
        self.buffer.redo()
    }

    pub fn newline(&mut self) {
        self.follow_cursor = true;
        self.buffer.add_line();
    }

    pub fn new(history: InputHistory, max_input_lines: u32) -> Self {
        let max_input_lines = max_input_lines.clamp(1, u16::MAX as u32 - 2) as u16;
        Self {
            buffer: TextBuffer::new(String::new()),
            history,
            history_index: None,
            draft: Prompt::default(),
            scroll_y: 0,
            follow_cursor: true,
            placeholder_hint: random_placeholder_hint(),
            pending_images: Vec::new(),
            max_input_lines,
            last_total_lines: 1,
            last_content_height: 1,
            layout: RefCell::default(),
            langs: LangCache::default(),
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

    /// The selected source text, never what a conceal draws over it, and
    /// the selection goes with it.
    pub fn take_selected_text(&mut self) -> Option<String> {
        let selected = self.buffer.selection()?;
        self.buffer.deselect();
        Some(self.buffer.value()[selected].to_owned())
    }

    fn layout(&self, width: u16) -> RefMut<'_, Layout> {
        let mut layout = self.layout.borrow_mut();
        layout.sync(&self.buffer, effective_width(width.into()));
        layout
    }

    /// The source text under a selection dragged over the box at {area},
    /// laid out like [`Self::copy_text`]. Reading the cells instead would
    /// copy virtual text and the text drawn over conceals, which are not
    /// part of the prompt. A conceal or a char cut by either end is taken
    /// whole.
    pub fn selected_text(&self, area: Rect, selection: &ScreenSelection) -> String {
        if selection.covers_rect(area) {
            return self.copy_text();
        }
        let len = self.buffer.value().len();
        let (start_row, start_col) = if selection.start_row < area.y {
            (area.y, area.x)
        } else {
            (selection.start_row, selection.start_col)
        };
        let start_hit = self.hit(area, start_row, start_col);
        let prefix = match &start_hit {
            Some(hit) if matches!(hit.spot, Spot::Prefix) && hit.row.index == 0 => {
                if hit.row.line == 0 {
                    CHEVRON
                } else {
                    NEWLINE_PAD
                }
            }
            _ => "",
        };
        let start = start_hit.map_or(len, |hit| hit.byte(|piece, _| piece.bytes.start));
        let end = self
            .hit(area, selection.end_row, selection.end_col)
            .map_or(len, |hit| {
                hit.byte(|piece, col| {
                    if col < piece.virt {
                        piece.bytes.start
                    } else {
                        piece.bytes.end
                    }
                })
            });
        if start > end {
            return String::new();
        }
        let text = self.buffer.value()[start..end].replace('\n', &format!("\n{NEWLINE_PAD}"));
        format!("{prefix}{text}")
    }

    pub fn height(&self, width: u16) -> u16 {
        let rows = self.layout(width).total_rows() + u32::from(!self.pending_images.is_empty());
        let capped = rows.min(self.max_input_lines.into()) as u16;
        capped.saturating_add(BORDER_ROWS)
    }

    pub fn is_at_first_line(&self) -> bool {
        self.buffer.display_lines(self.buffer.y()).start == 0
    }

    pub fn is_at_last_line(&self) -> bool {
        self.buffer.display_lines(self.buffer.y()).end == self.buffer.line_count()
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        self.buffer.value()[..self.buffer.cursor_byte()].ends_with('\\')
    }

    pub fn continue_line(&mut self) {
        self.buffer.undoable(|buf| {
            buf.deselect();
            buf.remove_char();
            buf.add_line();
        });
    }

    /// A recall opens the command palette on a `/` text with nobody holding
    /// it to the plugins that wrote it, and the palette matches loosely, so
    /// a `/yol` a plugin wrote would come back as the user's `/yolo`. Such a
    /// text stays out of history.
    pub fn submit(&mut self) -> Option<Submission> {
        let text = sent_text(self.buffer.value()).to_string();
        let images = mem::take(&mut self.pending_images);
        if text.is_empty() && images.is_empty() {
            return None;
        }
        if !(text.starts_with(COMMAND_PREFIX) && !self.buffer.writers().is_empty()) {
            self.history.push(text.clone());
        }
        self.discard();
        Some(Submission { text, images })
    }

    /// The marks go with the text, so ghost text pushed for it never turns
    /// up in the next prompt.
    pub fn discard(&mut self) {
        self.pending_images.clear();
        self.history_index = None;
        self.draft = Prompt::default();
        self.buffer.clear();
        self.scroll_y = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.value().trim().is_empty() && !self.has_images()
    }

    pub fn has_images(&self) -> bool {
        !self.pending_images.is_empty()
    }

    /// The text the user is composing. Under an untouched history recall that
    /// is the draft it covers, since the recall is still in history. An edited
    /// recall is new work and wins over the draft, as it does on submit.
    pub fn draft_text(&self) -> String {
        if self.is_untouched_recall() {
            self.draft.text.clone()
        } else {
            self.buffer.value().to_owned()
        }
    }

    /// The plugins that wrote [`Self::draft_text`].
    pub fn draft_writers(&self) -> &Writers {
        if self.is_untouched_recall() {
            &self.draft.writers
        } else {
            self.buffer.writers()
        }
    }

    fn is_untouched_recall(&self) -> bool {
        self.history_index
            .and_then(|i| self.history.get(i))
            .is_some_and(|entry| entry == self.buffer.value())
    }

    /// The marks go with the text they were made for, so under an untouched
    /// history recall the draft it covers is parked with its own marks.
    pub fn swap_draft(&mut self, draft: Draft) -> Draft {
        let parks_the_draft = self.is_untouched_recall();
        let images = mem::replace(&mut self.pending_images, draft.images);
        let shown = self.buffer.swap_prompt(Prompt {
            text: draft.text,
            marks: draft.marks,
            writers: draft.writers,
        });
        let covered = mem::take(&mut self.draft);
        self.history_index = None;
        self.scroll_y = 0;
        let Prompt {
            text,
            marks,
            writers,
        } = if parks_the_draft { covered } else { shown };
        Draft {
            text,
            images,
            marks,
            writers,
        }
    }

    /// Marks leave the text and its version alone, so the layout would never
    /// notice a line growing or shrinking by its virtual text or conceals. It
    /// is told here. See [`TextBuffer::set_marks`] for {version}.
    pub fn set_marks(
        &mut self,
        version: u64,
        plugin: &str,
        ns: &str,
        marks: Vec<Mark>,
    ) -> Result<(), String> {
        let moved = self.buffer.set_marks(version, plugin, ns, marks)?;
        self.layout.get_mut().relayout(&self.buffer, &moved);
        Ok(())
    }

    pub fn clear_plugin_marks(&mut self, plugin: &str) {
        let moved = self.buffer.clear_plugin_marks(plugin);
        self.layout.get_mut().relayout(&self.buffer, &moved);
        self.draft.marks.clear_plugin(plugin);
    }

    /// Takes in what the worker finished for this input, and recounts the
    /// lines whose marks it changed. `true` when the frame has to show it.
    pub fn poll_decor(&mut self) -> bool {
        let syntax = self.buffer.poll_syntax();
        if let Some(changed) = &syntax {
            self.layout.get_mut().relayout(&self.buffer, changed);
        }
        self.langs.poll() | syntax.is_some()
    }

    pub fn attach_image(&mut self, source: ImageSource) {
        self.pending_images.push(source);
    }

    /// The way a plugin writes to the input. It carries the box's own state
    /// the way the paste path does: the view follows the cursor again, and a
    /// value the edits changed stops counting as a recalled history entry, so
    /// the next history key does not throw the edit away. A call that changes
    /// no text leaves the recall and the draft under it alone.
    ///
    /// See [`TextBuffer::apply_edits`] for what the offsets refuse.
    pub fn apply_edits(
        &mut self,
        version: u64,
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
        anchor: Option<usize>,
        marks: Option<(&str, InputMarks)>,
    ) -> Result<usize, String> {
        let before = self.buffer.version();
        let (caret, moved) = self
            .buffer
            .apply_edits(version, edits, cursor, anchor, marks)?;
        self.layout.get_mut().relayout(&self.buffer, &moved);
        self.follow_cursor = true;
        if self.buffer.version() != before {
            self.history_index = None;
            self.draft = Prompt::default();
        }
        Ok(caret)
    }

    /// A whole prompt no plugin wrote: a restored draft, a rewind prompt. The
    /// text lands verbatim, because the user composed it elsewhere and it is
    /// what the model would be sent. It is another prompt even when it reads
    /// the same, so the marks of the old one go, see
    /// [`TextBuffer::swap_prompt`].
    pub fn set_input(&mut self, s: &str) {
        self.buffer.set_value(s);
    }

    /// [`Self::set_input`] for text made from what the input holds: a
    /// completed command, what `$EDITOR` changed. The plugins that wrote the
    /// text it came from still count as its writers.
    pub fn rewrite_input(&mut self, s: &str) {
        let writers = self.buffer.writers().clone();
        self.buffer.swap_prompt(Prompt {
            text: s.to_owned(),
            writers,
            ..Prompt::default()
        });
    }

    /// `false` when there is no older entry to recall.
    pub fn history_up(&mut self) -> bool {
        self.follow_cursor = true;
        let Some(Some(new_index)) = self.recall_target(true) else {
            return false;
        };
        let entry = self.history_entry(new_index);
        let shown = self.buffer.swap_prompt(entry);
        if self.history_index.replace(new_index).is_none() {
            self.draft = shown;
        }
        self.buffer.move_to_end();
        true
    }

    /// History keeps text only, so a recalled entry comes in with no marks,
    /// and as the user's, who sent it. It holds only what went out, in the
    /// form Enter read it, so it reads the same way again, see
    /// [`sent_text`].
    fn history_entry(&self, index: usize) -> Prompt {
        Prompt {
            text: self.history.get(index).unwrap().to_owned(),
            ..Prompt::default()
        }
    }

    /// `false` when no entry is recalled, so there is nothing newer to go to.
    pub fn history_down(&mut self) -> bool {
        self.follow_cursor = true;
        let Some(target) = self.recall_target(false) else {
            return false;
        };
        self.history_index = target;
        let next = match target {
            Some(index) => self.history_entry(index),
            None => mem::take(&mut self.draft),
        };
        self.buffer.swap_prompt(next);
        true
    }

    /// Where a history key goes from here, older or newer: `Some` entry, or
    /// `None` for the draft a recall covers. The outer `None` when it goes
    /// nowhere.
    fn recall_target(&self, older: bool) -> Option<Option<usize>> {
        match (older, self.history_index) {
            (true, None) => self.history.len().checked_sub(1).map(Some),
            (true, Some(index)) => index.checked_sub(1).map(Some),
            (false, None) => None,
            (false, Some(index)) => Some(Some(index + 1).filter(|&next| next < self.history.len())),
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
        let layout = self.layout.get_mut();
        layout.sync(&self.buffer, effective_width(area.width.into()));
        let cursor_row = layout.cursor_row(&self.buffer);
        let text_rows = layout.total_rows();

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
                .rows(&self.buffer, self.scroll_y.into(), content_height.into())
                .collect();
            let window = match (rows.first(), rows.last()) {
                (Some(first), Some(last)) => first.text.start..=last.text.end,
                _ => 0..=0,
            };
            let marks = self.buffer.overlapping(window.clone());
            let conceals = self.buffer.concealed(window);
            self.langs.next_frame(&self.buffer);
            let caret = self.buffer.cursor_byte();
            let selected = self.buffer.selection();
            let mut lines = Vec::with_capacity(rows.len() + 1);
            for line_rows in rows.chunk_by(|a, b| a.line == b.line) {
                let text = line_rows[0].text.clone();
                let line = &self.buffer.value()[text.clone()];
                let decor = LineDecor::new(
                    self.buffer.value(),
                    text.clone(),
                    &marks,
                    &conceals,
                    selected.as_ref(),
                    &mut self.langs,
                );
                for row in line_rows {
                    let screen_row = lines.len() as u16;
                    let owns_caret = u32::from(self.scroll_y) + u32::from(screen_row) == cursor_row;
                    let caret = owns_caret.then(|| caret - text.start);
                    let (drawn, col) = draw_row(line, row, &decor, caret, show_cursor);
                    if let Some(col) = col {
                        cursor_cell = Some(Position::new(col, screen_row));
                    }
                    lines.push(drawn);
                }
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
    /// A drag from there is the screen's copy selection, never the input's,
    /// so the click drops the input's selection like any plain motion.
    pub fn handle_click(&mut self, area: Rect, row: u16, col: u16) {
        let Some((y, x)) = self.click_position(area, row, col) else {
            return;
        };
        self.buffer.set_cursor(y, x);
        self.buffer.deselect();
        self.follow_cursor = true;
    }

    /// Convert a mouse click at terminal (row, col) within the input content
    /// area into a (line_index, char_index) in the text buffer, accounting
    /// for scroll offset, word-wrap, and the chevron/padding prefix.
    ///
    /// A click on virtual text lands on the char it sits before, and one on
    /// the text drawn over a conceal on the nearer edge of the conceal.
    fn click_position(&self, area: Rect, row: u16, col: u16) -> Option<(usize, usize)> {
        let hit = self.hit(area, row, col)?;
        let at = hit.byte(|piece, col| {
            if piece.concealed && col >= piece.virt && (col - piece.virt) * 2 >= piece.width {
                piece.bytes.end
            } else {
                piece.bytes.start
            }
        });
        let y = self.buffer.line_of(at);
        let line_start = self.buffer.line_range(y).start;
        Some((y, self.buffer.value()[line_start..at].chars().count()))
    }

    /// The drawn row under terminal cell (row, col) of the content area, and
    /// what of it the cell shows.
    fn hit(&self, area: Rect, row: u16, col: u16) -> Option<Hit> {
        let content_y = row.checked_sub(area.y)?;
        let content_x = col.checked_sub(area.x)?;
        let row = self
            .layout(area.width)
            .rows(
                &self.buffer,
                u32::from(self.scroll_y) + u32::from(content_y),
                1,
            )
            .next()?;
        let prefix = if row.index == 0 { PREFIX_WIDTH } else { 0 };
        let Some(col) = content_x.checked_sub(prefix) else {
            return Some(Hit {
                row,
                spot: Spot::Prefix,
            });
        };
        let mut col = usize::from(col);
        let line = &self.buffer.value()[row.text.clone()];
        let mut spot = Spot::PastEnd;
        for found in row.shape.pieces(line, row.bytes.clone()) {
            let width = found.virt + found.width;
            if width > col {
                spot = Spot::Piece(found, col);
                break;
            }
            col -= width;
        }
        Some(Hit { row, spot })
    }
}

/// A screen cell of the box: the row it is on and what of the row it shows.
struct Hit {
    row: Row,
    spot: Spot,
}

enum Spot {
    /// The chevron or the pad before the text.
    Prefix,
    /// A piece of the row, and how many columns into it.
    Piece(Piece, usize),
    PastEnd,
}

impl Hit {
    /// The byte of the value it lands on: the row's start in the prefix, its
    /// end past it, and inside a piece what {pick} chooses.
    fn byte(&self, pick: impl FnOnce(&Piece, usize) -> usize) -> usize {
        let at = match &self.spot {
            Spot::Prefix => self.row.bytes.start,
            Spot::Piece(piece, col) => pick(piece, *col),
            Spot::PastEnd => self.row.bytes.end,
        };
        self.row.text.start + at
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
    decor: &LineDecor,
    caret: Option<usize>,
    reversed: bool,
) -> (Line<'static>, Option<u16>) {
    let mut spans = match row.index {
        0 if row.line == 0 => vec![super::chevron_span()],
        0 => vec![Span::raw(NEWLINE_PAD)],
        _ => Vec::new(),
    };
    let prefix_width = display_width(&spans);
    let (content, caret_chars) = decor.draw(line, row, caret);
    let Some(caret_chars) = caret_chars else {
        spans.extend(content);
        return (Line::from(spans), None);
    };
    let (with_cursor, col) = overlay_cursor(content, caret_chars, reversed);
    spans.extend(with_cursor);
    (Line::from(spans), Some(prefix_width + col))
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
    use crate::app::shell::parse_shell_prefix;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use crate::components::tool_display::resolve_span_style;
    use crate::decor_worker;
    use crate::text_buffer::test_support::{mark_ranges, text_edit};
    use crate::text_buffer::{Decoration, INPUT_REPLACED_ERR, Reveal, RuleSet, VirtTextPos};
    use maki_agent::types::InlineStyle;
    use maki_agent::{SnapshotSpan, SpanColor, SpanStyle};
    use maki_lua::{InputRule, PatternRule, SyntaxRule};
    use maki_providers::ImageMediaType;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::Color;
    use std::ops::Range;
    use std::sync::Arc;
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

    #[test_case("hello\\",  0, false => "hello\n"  ; "at_the_end")]
    #[test_case("asd\\asd", 3, false => "asd\nasd" ; "mid_line")]
    #[test_case("hello\\",  0, true  => "hello\n"  ; "over_a_selection")]
    fn backslash_continuation(text: &str, left: usize, select_line: bool) -> String {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, text);
        for _ in 0..left {
            input.buffer.move_left();
        }
        if select_line {
            input.handle_key(KeyCode::Home.into(), InputMode::Insert);
            input.handle_key(shift(KeyCode::End), InputMode::Insert);
        }
        assert!(input.char_before_cursor_is_backslash());
        input.handle_key(KeyCode::Enter.into(), InputMode::Insert);
        input.buffer.value().to_owned()
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

        let swapped = input.swap_draft(Draft::default());

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
        input
            .apply_edits(version, edits, cursor, None, None)
            .unwrap();

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

        // Leaving the first drawn row out keeps the box partly selected, so
        // the copy maps the cells back to the text.
        let selection = ScreenSelection {
            start_row: 2,
            start_col: 0,
            end_row: 4,
            end_col: WIDTH - 1,
        };

        let text = input.selected_text(Rect::new(0, 1, WIDTH, HEIGHT - 2), &selection);
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
        input
            .apply_edits(version, vec![edit], None, None, None)
            .unwrap();
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

    #[test_case(|input| { input.handle_key(KeyCode::Enter.into(), InputMode::Insert); } ; "backslash_continuation")]
    #[test_case(|input| plugin_write(input, 0..1, "plugin\nwrite")                   ; "plugin_write")]
    fn each_action_undoes_in_one_step(action: fn(&mut InputBox)) {
        const BEFORE: &str = "a\\";
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.set_input(BEFORE);
        input.buffer.move_to_end();
        action(&mut input);

        assert!(input.undo());
        assert_eq!(input.buffer.value(), BEFORE);
        assert_eq!(input.buffer.cursor_byte(), BEFORE.len());
        assert!(!input.undo());
    }

    type Rgb = (u8, u8, u8);

    const MARKS_PLUGIN: &str = "test";
    const MARKS_NS: &str = "marks";
    const RED: Rgb = (200, 0, 0);
    const BLUE: Rgb = (0, 0, 200);

    fn mark(bytes: Range<usize>, decoration: Decoration) -> Mark {
        Mark {
            start: bytes.start,
            stop: (!bytes.is_empty()).then_some(bytes.end),
            decoration: Arc::new(decoration),
        }
    }

    fn virt(text: &str, pos: VirtTextPos) -> Decoration {
        Decoration {
            virt_text: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
            virt_text_pos: pos,
            ..Decoration::default()
        }
    }

    fn hl(style: InlineStyle) -> Decoration {
        Decoration {
            hl: Some(SpanStyle::Inline(style)),
            ..Decoration::default()
        }
    }

    fn fg(rgb: Rgb) -> InlineStyle {
        InlineStyle {
            fg: Some(SpanColor::Rgb(rgb)),
            ..InlineStyle::default()
        }
    }

    /// The caret goes to the end, so it never covers a cell a test reads.
    fn marked(text: &str, marks: Vec<Mark>) -> InputBox {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.set_input(text);
        input.buffer.move_to_end();
        input
            .set_marks(input.buffer.version(), MARKS_PLUGIN, MARKS_NS, marks)
            .unwrap();
        input
    }

    /// The cell {x} columns into the text of screen row {row}, past the
    /// chevron.
    fn text_cell(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        x: u16,
        row: u16,
    ) -> ratatui::buffer::Cell {
        let col = PREFIX_WIDTH + x;
        terminal
            .backend()
            .buffer()
            .cell((col, row))
            .unwrap()
            .clone()
    }

    #[test]
    fn hl_paints_only_its_range() {
        let bold = InlineStyle {
            bold: true,
            ..InlineStyle::default()
        };
        let mut input = marked("plain bold", vec![mark(6..10, hl(bold))]);
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        let is_bold = |x| text_cell(&terminal, x, 1).modifier.contains(Modifier::BOLD);
        assert!(!is_bold(5));
        assert!((6..10).all(is_bold));
    }

    #[test_case(&[RED], &[BLUE] => BLUE ; "a_later_namespace_wins")]
    #[test_case(&[], &[RED, BLUE] => BLUE ; "the_later_mark_wins_within_a_namespace")]
    fn composition(first: &[Rgb], second: &[Rgb]) -> Rgb {
        let marks = |set: &[Rgb]| set.iter().map(|&rgb| mark(0..2, hl(fg(rgb)))).collect();
        let mut input = marked("xy", marks(first));
        input
            .set_marks(
                input.buffer.version(),
                MARKS_PLUGIN,
                "second",
                marks(second),
            )
            .unwrap();
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        let drawn = text_cell(&terminal, 0, 1).fg;
        *[RED, BLUE]
            .iter()
            .find(|&&rgb| resolve_span_style(&SpanStyle::Inline(fg(rgb))).fg == Some(drawn))
            .expect("one of the two colors is drawn")
    }

    /// Ten columns of text: the virtual text pushes `h` onto a row of its own.
    #[test]
    fn inline_virtual_text_takes_columns_and_wraps() {
        let mut input = marked(
            "abcdefgh",
            vec![mark(4..4, virt("XYZ", VirtTextPos::Inline))],
        );
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}abcdXYZefg"));
        assert_eq!(rendered_row(&terminal, 2), "h");
        assert_eq!(input.height(CURSOR_WIDTH), 2 + BORDER_ROWS);
    }

    #[test]
    fn eol_virtual_text_is_cut_rather_than_wrapped() {
        let mut input = marked("abcdefgh", vec![mark(0..0, virt("TAIL", VirtTextPos::Eol))]);
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}abcdefgh T"));
        assert_eq!(input.height(CURSOR_WIDTH), 1 + BORDER_ROWS);
    }

    /// Ghost text reads on from where the user types, so the caret sits on
    /// its first cell rather than past it.
    #[test]
    fn the_caret_comes_before_virtual_text_at_it() {
        let mut input = marked("ab", vec![mark(2..2, virt("cd", VirtTextPos::Inline))]);
        let rendered = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(
            rendered_row(&rendered.terminal, 1),
            format!("{CHEVRON}abcd")
        );
        assert_cursor_at(&rendered, Some(Position::new(PREFIX_WIDTH + 2, 1)));
    }

    #[test_case(PREFIX_WIDTH + 2 => Some((0, 2)) ; "on_the_virtual_text")]
    #[test_case(PREFIX_WIDTH + 4 => Some((0, 2)) ; "on_the_char_after_it")]
    #[test_case(PREFIX_WIDTH + 5 => Some((0, 3)) ; "past_it")]
    fn a_click_on_virtual_text_lands_on_the_char_after_it(col: u16) -> Option<(usize, usize)> {
        let input = marked("abcd", vec![mark(2..2, virt("XY", VirtTextPos::Inline))]);
        input.click_position(area(CURSOR_WIDTH), 0, col)
    }

    /// The same code sits on two lines and only the first is marked, so the
    /// second shows what the text looks like without the mark.
    #[test]
    fn lang_colors_its_range_as_code() {
        const CODE: &str = "fn x";
        crate::highlight::warmup();
        let rust = Decoration {
            lang: Some("rust".into()),
            ..Decoration::default()
        };
        let mut input = marked(&format!("{CODE}\n{CODE}"), vec![mark(0..CODE.len(), rust)]);
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_ne!(text_cell(&terminal, 0, 1).fg, text_cell(&terminal, 0, 2).fg);
    }

    #[test]
    fn marks_follow_the_draft_to_another_chat_and_back() {
        let mut input = marked("draft", vec![mark(0..5, hl(fg(RED)))]);

        let parked = input.swap_draft(Draft::default());
        assert!(mark_ranges(&input.buffer).is_empty());

        input.swap_draft(parked);
        assert_eq!(input.buffer.value(), "draft");
        assert_eq!(mark_ranges(&input.buffer), vec![0..5]);
    }

    /// Two chats with empty drafts read the same, yet an edit a plugin
    /// planned on the one left behind must not land in the one in front.
    #[test]
    fn a_stale_edit_is_refused_after_swapping_in_a_draft_that_reads_the_same() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let planned = input.buffer.version();

        input.swap_draft(Draft::default());

        assert_ne!(input.buffer.version(), planned, "a strict edit compares it");
        let refused = input
            .apply_edits(planned, vec![text_edit(0, 0, DRAFT)], None, None, None)
            .unwrap_err();
        assert!(refused.starts_with(INPUT_REPLACED_ERR), "{refused}");
        assert!(input.buffer.is_empty());
    }

    #[test_case(|input| { input.submit(); } ; "submit")]
    #[test_case(InputBox::discard           ; "discard")]
    fn ghost_text_goes_with_the_prompt(put_away: fn(&mut InputBox)) {
        let ghost = mark(DRAFT.len()..DRAFT.len(), virt("ghost", VirtTextPos::Inline));
        let mut input = marked(DRAFT, vec![ghost]);

        put_away(&mut input);
        type_text(&mut input, DRAFT);

        assert!(mark_ranges(&input.buffer).is_empty());
    }

    #[test_case(|input| { input.history_down(); } ; "history_down")]
    #[test_case(|input| { let parked = input.swap_draft(Draft::default()); input.swap_draft(parked); } ; "a_chat_switch_and_back")]
    fn the_draft_keeps_its_marks_under_a_history_recall(come_back: fn(&mut InputBox)) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, RECALLED);
        type_text(&mut input, DRAFT);
        let version = input.buffer.version();
        let marks = vec![mark(0..DRAFT.len(), hl(fg(RED)))];
        input
            .set_marks(version, MARKS_PLUGIN, MARKS_NS, marks)
            .unwrap();

        input.history_up();
        assert!(mark_ranges(&input.buffer).is_empty(), "an entry has none");
        come_back(&mut input);

        assert_eq!(input.buffer.value(), DRAFT);
        assert_eq!(mark_ranges(&input.buffer), vec![0..DRAFT.len()]);
    }

    /// `$s:go` is drawn as the four columns of `[go]`, so the row reads
    /// `a [go] b` and the chip takes text columns 2 to 5.
    const CHIP_TEXT: &str = "a $s:go b";
    const CHIP: Range<usize> = 2..7;
    const CHIP_DRAWN: &str = "a [go] b";

    fn chip(reveal: Reveal) -> InputBox {
        let decoration = Decoration {
            conceal: Some("[go]".into()),
            reveal,
            atomic: true,
            ..Decoration::default()
        };
        marked(CHIP_TEXT, vec![mark(CHIP, decoration)])
    }

    #[test]
    fn a_conceal_is_drawn_until_the_raw_text_is_asked_for() {
        let mut input = chip(Reveal::Never);
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}{CHIP_DRAWN}"));

        input.buffer.toggle_raw();
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}{CHIP_TEXT}"));
    }

    #[test]
    fn a_revealing_conceal_shows_its_text_while_the_caret_is_on_an_edge() {
        let mut input = chip(Reveal::Cursor);
        input.buffer.set_cursor(0, CHIP.end);
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}{CHIP_TEXT}"));

        input.buffer.move_to_end();
        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}{CHIP_DRAWN}"));
    }

    #[test_case(2, CHIP.end, 6 ; "left_onto_its_end")]
    #[test_case(3, CHIP.start, 2 ; "left_again_onto_its_start")]
    fn the_caret_steps_over_a_chip(lefts: usize, byte: usize, col: u16) {
        let mut input = chip(Reveal::Never);
        for _ in 0..lefts {
            input.handle_key(KeyCode::Left.into(), InputMode::Insert);
        }
        assert_eq!(input.buffer.cursor_byte(), byte);
        let rendered = render_cursor(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_cursor_at(&rendered, Some(Position::new(PREFIX_WIDTH + col, 1)));
    }

    #[test_case(PREFIX_WIDTH + 3 => CHIP.start ; "on_its_first_half")]
    #[test_case(PREFIX_WIDTH + 4 => CHIP.end ; "on_its_second_half")]
    #[test_case(PREFIX_WIDTH + 7 => CHIP_TEXT.find('b').unwrap() ; "on_the_char_after_it")]
    fn clicking_a_chip_row(col: u16) -> usize {
        let mut input = chip(Reveal::Never);
        input.handle_click(area(CURSOR_WIDTH), 0, col);
        input.buffer.cursor_byte()
    }

    /// Ghost text `XY` before the `b`: the cells copy as the text under them,
    /// never as what is drawn.
    #[test_case(3, 6 => " $s:go" ; "a_chip_cut_by_the_end_is_taken_whole")]
    #[test_case(5, 9 => "$s:go " ; "ending_on_virtual_text_leaves_it_out")]
    #[test_case(0, 11 => format!("{CHEVRON}{CHIP_TEXT}") ; "starting_on_the_prefix_takes_it")]
    fn copying_part_of_a_chip_row_copies_the_source(start_col: u16, end_col: u16) -> String {
        let mut input = chip(Reveal::Never);
        let ghost = mark(8..8, virt("XY", VirtTextPos::Inline));
        input
            .set_marks(input.buffer.version(), MARKS_PLUGIN, "ghost", vec![ghost])
            .unwrap();
        let selection = ScreenSelection {
            start_row: 1,
            start_col,
            end_row: 1,
            end_col,
        };
        input.selected_text(Rect::new(0, 1, CURSOR_WIDTH, CURSOR_HEIGHT - 2), &selection)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    /// Shift+Right over `a`, the space and the chip takes the chip whole. It
    /// still draws as `[go]`, now in the selection colors over the red of the
    /// mark, and copies as its source.
    #[test]
    fn a_selection_paints_over_marks_and_copies_the_source() {
        const SELECTED: &str = "a $s:go";
        const SELECTED_DRAWN: &str = "a [go]";
        const RIGHTS_OVER_THE_CHIP: usize = 3;
        let mut input = chip(Reveal::Never);
        let red = mark(0..CHIP_TEXT.len(), hl(fg(RED)));
        input
            .set_marks(input.buffer.version(), MARKS_PLUGIN, "red", vec![red])
            .unwrap();
        input.buffer.set_cursor(0, 0);
        for _ in 0..RIGHTS_OVER_THE_CHIP {
            input.handle_key(shift(KeyCode::Right), InputMode::Insert);
        }

        let terminal = render_input(&mut input, CURSOR_WIDTH, CURSOR_HEIGHT);
        assert_eq!(rendered_row(&terminal, 1), format!("{CHEVRON}{CHIP_DRAWN}"));
        let selected = theme::current().item_selected;
        let is_selected = |x| {
            let cell = text_cell(&terminal, x, 1);
            (cell.fg, cell.bg) == (selected.fg.unwrap(), selected.bg.unwrap())
        };
        let selected_end = SELECTED_DRAWN.len() as u16;
        assert!((0..selected_end).all(is_selected));
        assert!(!is_selected(selected_end));
        assert_eq!(input.take_selected_text().as_deref(), Some(SELECTED));
        assert_eq!(input.buffer.selection(), None);
    }

    /// Up on the first line has nothing to move over, so Shift+Up must not
    /// fall through to the history and swap out the text being selected.
    #[test]
    fn shift_selects_before_history_and_esc_drops_it_first() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, RECALLED);
        type_text(&mut input, DRAFT);
        input.handle_key(shift(KeyCode::Up), InputMode::Insert);
        input.handle_key(shift(KeyCode::Home), InputMode::Normal);
        assert_eq!(input.buffer.value(), DRAFT);
        assert_eq!(input.buffer.selection(), Some(0..DRAFT.len()));

        let esc = KeyCode::Esc.into();
        assert!(matches!(
            input.handle_key(esc, InputMode::Insert),
            InputAction::None
        ));
        assert_eq!(input.buffer.selection(), None);
        assert!(matches!(
            input.handle_key(esc, InputMode::Insert),
            InputAction::Passthrough(_)
        ));
    }

    const SHELL_WIDTH: u16 = 30;

    /// The `!` used to be painted by hand, and the two core rules took over.
    /// So the old look is rebuilt here from `parse_shell_prefix` and a bash
    /// mark, and every cell has to match it.
    #[test_case("!ls -la"       ; "bang")]
    #[test_case("!  ls"         ; "bang_with_extra_spaces")]
    #[test_case("!!x=1 && ls"   ; "double_bang")]
    #[test_case("!!!ls"         ; "triple_bang")]
    #[test_case("!"             ; "lone_bang")]
    #[test_case("!!"            ; "lone_double_bang")]
    #[test_case("!! "           ; "double_bang_and_space")]
    #[test_case("!\nls"         ; "bang_then_command_on_the_next_line")]
    #[test_case("ls\n!ls"       ; "bang_on_a_later_line")]
    fn a_shell_prompt_draws_like_parse_shell_prefix_reads_it(text: &str) {
        crate::highlight::warmup();
        let first_line = text.split('\n').next().unwrap();
        let prefix = parse_shell_prefix(first_line).map_or(0, |parsed| parsed.prefix_len);
        let bash = Decoration {
            lang: Some("bash".into()),
            ..Decoration::default()
        };
        let command = (prefix > 0).then(|| mark(prefix..first_line.len(), bash));
        let expected = render_input(
            &mut marked(text, command.into_iter().collect()),
            SHELL_WIDTH,
            CURSOR_HEIGHT,
        );
        let mut shell = marked(text, Vec::new());
        shell.buffer.set_rules(RuleSet::with_plugins(&[]));
        let shell = render_input(&mut shell, SHELL_WIDTH, CURSOR_HEIGHT);

        let prefix_cols = PREFIX_WIDTH..PREFIX_WIDTH + prefix as u16;
        let expected = expected.backend().buffer();
        for (i, cell) in shell.backend().buffer().content.iter().enumerate() {
            let (col, row) = expected.pos_of(i);
            let mut want = expected.content[i].clone();
            if row == 1 && prefix_cols.contains(&col) {
                want.set_style(theme::current().shell_prefix);
            }
            assert_eq!(cell, &want, "cell ({col}, {row})");
        }
    }

    fn chip_rule(conceal: &str) -> Arc<RuleSet> {
        let mark = Decoration {
            conceal: Some(conceal.into()),
            atomic: true,
            ..Decoration::default()
        };
        RuleSet::with_plugins(&[InputRule::Pattern(
            PatternRule::new(r"\$s:(\w+)", mark).unwrap(),
        )])
    }

    /// The half typed chip is drawn first, so its line is matched and kept
    /// before the chip exists. A stale cache would never show the chip.
    #[test]
    fn a_rule_chip_appears_once_typed_and_deletes_whole() {
        let mut input = ruled(chip_rule("[$1]"));
        let drawn_row = |input: &mut InputBox| {
            rendered_row(&render_input(input, CURSOR_WIDTH, CURSOR_HEIGHT), 1)
        };
        let (head, tail) = CHIP_TEXT.split_at(CHIP.start + 2);
        type_text(&mut input, head);
        assert_eq!(drawn_row(&mut input), format!("{CHEVRON}{head}"));

        type_text(&mut input, tail);
        assert_eq!(drawn_row(&mut input), format!("{CHEVRON}{CHIP_DRAWN}"));

        input.buffer.set_cursor(0, CHIP.end);
        input.handle_key(KeyCode::Backspace.into(), InputMode::Insert);
        assert_eq!(input.buffer.value(), "a  b");
    }

    /// Each line is two rows raw and one as a chip. Nothing is ever drawn, so
    /// the totals only add up if the layout matches lines on its own. The
    /// paste on top shifts every cached line down by one, which must not mix
    /// up whose marks are whose.
    #[test]
    fn rule_chips_count_rows_on_lines_never_drawn() {
        const LINES: u32 = 30;
        let line = format!("$s:{}", "w".repeat(CURSOR_EW - 2));
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.set_input(&vec![line.as_str(); LINES as usize].join("\n"));
        input.buffer.set_cursor(0, 0);
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), 2 * LINES);

        input.buffer.set_rules(chip_rule("$1"));
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), LINES);

        input.handle_paste(&format!("{line}\n"));
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), LINES + 1);
    }

    const PREVIEW_BLOCK: &str = r#"
        (fenced_code_block
          (info_string (language) @injection.language)
          (code_fence_content) @injection.content)
        (fenced_code_block_delimiter) @hidden
    "#;
    const PREVIEW_INLINE: &str = "(strong_emphasis (emphasis_delimiter) @hidden) @strong";

    /// Bold without its stars. The block query only puts the markdown
    /// grammar in the rule, and that is what runs the inline one under it.
    fn preview_rules() -> Arc<RuleSet> {
        let hidden = Decoration {
            conceal: Some(String::new()),
            reveal: Reveal::Cursor,
            ..Decoration::default()
        };
        let strong = hl(InlineStyle {
            bold: true,
            ..InlineStyle::default()
        });
        let rule = SyntaxRule::new(
            "markdown",
            vec![
                ("markdown".into(), PREVIEW_BLOCK.into()),
                ("markdown_inline".into(), PREVIEW_INLINE.into()),
            ],
            vec![("hidden".into(), hidden), ("strong".into(), strong)],
        )
        .unwrap();
        RuleSet::with_plugins(&[InputRule::Syntax(Arc::new(rule))])
    }

    fn ruled(rules: Arc<RuleSet>) -> InputBox {
        decor_worker::manual();
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.buffer.set_rules(rules);
        input
    }

    /// Runs the worker until it has nothing left, taking each reply in like
    /// the event loop's ticks would.
    fn settle(input: &mut InputBox) {
        input.poll_decor();
        while decor_worker::run_queued() > 0 {
            input.poll_decor();
        }
    }

    /// The worker has not seen the edits yet. A `z` typed inside the bold,
    /// right after its stars, and one typed inside the code grow those marks
    /// like placed ones grow, so neither flashes plain and no star comes
    /// back. A backtick typed inside a fence lands inside what it conceals,
    /// the one place the raw text shows until the worker is back.
    #[test]
    fn a_stale_result_grows_with_what_is_typed_inside_its_marks() {
        const DOC: &str = "**a** x\n```rust\nlet y\n```\n";
        const WIDTH: u16 = 30;
        const HEIGHT: u16 = 8;
        const FIRST_ROW: u16 = 1;
        const CODE_ROW: u16 = 3;
        const SETTLED: [&str; 4] = ["a x", "rust", "let y", ""];
        const STALE: [&str; 4] = ["za x", "````rust", "let zy", "````"];
        const EDITED: [&str; 4] = ["za x", "rust", "let zy", ""];
        let draw = |input: &mut InputBox| {
            let terminal = render_input(input, WIDTH, HEIGHT);
            let rows: Vec<String> = (FIRST_ROW..FIRST_ROW + SETTLED.len() as u16)
                .map(|row| {
                    let text = rendered_row(&terminal, row);
                    text.chars().skip(PREFIX_WIDTH.into()).collect()
                })
                .collect();
            (terminal, rows)
        };
        let insert = |start, text| text_edit(start, start, text);
        crate::highlight::warmup();
        let mut input = ruled(preview_rules());
        input.set_input(DOC);
        let x = DOC.find('x').unwrap();
        input.buffer.set_cursor(0, x);
        settle(&mut input);
        let (terminal, rows) = draw(&mut input);
        assert_eq!(rows, SETTLED);
        let code = text_cell(&terminal, 0, CODE_ROW).style();
        assert_ne!(code, text_cell(&terminal, 2, FIRST_ROW).style());

        let fences = DOC.match_indices("```").map(|(at, _)| insert(at + 1, "`"));
        let mut edits = vec![insert(2, "z"), insert(DOC.find('y').unwrap(), "z")];
        edits.extend(fences);
        edits.sort_by_key(|edit| edit.start);
        let cursor = Some(x + "z".len());
        input
            .apply_edits(input.buffer.version(), edits, cursor, None, None)
            .unwrap();
        let (terminal, rows) = draw(&mut input);
        assert_eq!(rows, STALE);
        let bold = text_cell(&terminal, 0, FIRST_ROW).modifier;
        assert!(bold.contains(Modifier::BOLD));
        assert_eq!(text_cell(&terminal, 0, CODE_ROW).style(), code);

        settle(&mut input);
        assert_eq!(draw(&mut input).1, EDITED);
    }

    /// Opening a fence turns every line under it into code, so their stars
    /// come back and each one wraps onto a second row. Those lines are far
    /// from the edit and never drawn, so only the result can tell the layout
    /// to count them again.
    #[test]
    fn a_result_recounts_lines_far_from_the_edit() {
        const LINES: u32 = 20;
        let mut input = ruled(preview_rules());
        let bold = format!("**{}**", "w".repeat(CURSOR_EW - 2));
        let text = format!("top\n{}", vec![bold.as_str(); LINES as usize].join("\n"));
        input.set_input(&text);
        input.buffer.set_cursor(0, 0);
        settle(&mut input);
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), 1 + LINES);

        let fence = text_edit("top\n".len(), "top\n".len(), "```\n");
        input
            .apply_edits(input.buffer.version(), vec![fence], None, None, None)
            .unwrap();
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), 2 + LINES);

        settle(&mut input);
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), 2 + 2 * LINES);
    }

    /// The block draws a badge wide enough to wrap the line it starts on.
    /// Typing inside the atomic block drops it before the worker is back,
    /// lines away from the edit, so that line has to be counted again.
    #[test]
    fn a_stale_mark_dropped_lines_away_recounts_them() {
        const DOC: &str = "```\na\nb\n```";
        const LINES: u32 = 4;
        let badge = Decoration {
            atomic: true,
            ..virt(&"b".repeat(CURSOR_EW), VirtTextPos::Inline)
        };
        let rule = SyntaxRule::new(
            "markdown",
            vec![("markdown".into(), "(fenced_code_block) @badge".into())],
            vec![("badge".into(), badge)],
        )
        .unwrap();
        let mut input = ruled(RuleSet::with_plugins(&[InputRule::Syntax(Arc::new(rule))]));
        input.set_input(DOC);
        input.buffer.set_cursor(0, 0);
        settle(&mut input);
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), LINES + 1);

        let b = DOC.find('b').unwrap();
        let edit = text_edit(b, b, "z");
        input
            .apply_edits(input.buffer.version(), vec![edit], None, None, None)
            .unwrap();
        assert_eq!(input.layout(CURSOR_WIDTH).total_rows(), LINES);
    }

    const ABOVE_CODE: &str = "top\n";
    const BLOCK_COMMENT_START: &str = "/*\nx\n";
    const BLOCK_COMMENT_END: &str = "*/";
    /// Past what a frame colors by itself, so the range goes to the worker.
    const LONG_FILLER: usize = 3000;

    /// A line of text, then a block comment marked as rust, made {filler}
    /// bytes longer.
    fn commented(filler: usize) -> InputBox {
        crate::highlight::warmup();
        decor_worker::manual();
        let code = format!(
            "{BLOCK_COMMENT_START}{}{BLOCK_COMMENT_END}",
            "y\n".repeat(filler / 2)
        );
        let rust = Decoration {
            lang: Some("rust".into()),
            ..Decoration::default()
        };
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.set_input(&format!("{ABOVE_CODE}{code}"));
        let range = ABOVE_CODE.len()..ABOVE_CODE.len() + code.len();
        input
            .set_marks(
                input.buffer.version(),
                MARKS_PLUGIN,
                MARKS_NS,
                vec![mark(range, rust)],
            )
            .unwrap();
        input.buffer.set_cursor(0, 0);
        input
    }

    /// Types at the top, or inside the comment under the line of `x`.
    fn type_in_commented(input: &mut InputBox, inside: bool) {
        let at = if inside {
            ABOVE_CODE.len() + BLOCK_COMMENT_START.len()
        } else {
            0
        };
        let edit = text_edit(at, at, "z");
        input
            .apply_edits(input.buffer.version(), vec![edit], None, None, None)
            .unwrap();
    }

    /// Whether `x` looks like the `*` of the line that opens its comment.
    /// Colored on its own, `x` would look like a plain identifier.
    fn colored_whole(input: &mut InputBox) -> bool {
        let terminal = render_input(input, CURSOR_WIDTH, CURSOR_HEIGHT);
        text_cell(&terminal, 1, 2).style() == text_cell(&terminal, 0, 3).style()
    }

    #[test]
    fn a_short_lang_range_is_colored_whole_in_the_frame() {
        let mut input = commented(0);
        assert!(colored_whole(&mut input));
        assert_eq!(decor_worker::run_queued(), 0);
    }

    /// Until the worker is back, each line is colored on its own, so the
    /// code never flashes plain. Once colored, a frame reads none of the
    /// range's text again until an edit lands inside it.
    #[test_case(false ; "an_edit_above_it_moves_its_colors")]
    #[test_case(true  ; "an_edit_inside_it_colors_it_again")]
    fn a_long_range_is_colored_again_only_when_an_edit_touches_it(inside: bool) {
        let mut input = commented(LONG_FILLER);
        assert!(!colored_whole(&mut input));
        assert_eq!(decor_worker::run_queued(), 1);
        assert!(input.poll_decor());
        assert!(colored_whole(&mut input));
        decor::take_bytes_read();

        type_in_commented(&mut input, inside);
        assert_eq!(colored_whole(&mut input), !inside);
        assert_eq!(decor::take_bytes_read() > 0, inside);
        assert_eq!(decor_worker::run_queued(), usize::from(inside));
    }

    /// Here the edit lands while the worker is still coloring. Its colors
    /// move along with an edit above the range. An edit inside makes them
    /// wrong, so they are thrown away and the range is colored again. Either
    /// way the result asks for a frame, or nothing would hand the range to
    /// the worker again until some other redraw.
    #[test_case(false ; "an_edit_above_it_moves_them")]
    #[test_case(true  ; "an_edit_inside_it_drops_them")]
    fn colors_from_the_worker_land_on_the_text_now(inside: bool) {
        let mut input = commented(LONG_FILLER);
        assert!(!colored_whole(&mut input));
        type_in_commented(&mut input, inside);
        assert!(!colored_whole(&mut input));
        assert_eq!(decor_worker::run_queued(), 1);
        assert!(input.poll_decor());
        assert_eq!(colored_whole(&mut input), !inside);

        assert_eq!(decor_worker::run_queued(), usize::from(inside));
        input.poll_decor();
        assert!(colored_whole(&mut input));
    }
}
