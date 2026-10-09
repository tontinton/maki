use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::highlight::TAB_SPACES;

pub fn is_newline_key(key: &KeyEvent) -> bool {
    (matches!(key.code, KeyCode::Enter)
        && key.modifiers.intersects(
            KeyModifiers::SHIFT
                .union(KeyModifiers::CONTROL)
                .union(KeyModifiers::ALT),
        ))
        || (key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditResult {
    Ignored,
    Moved,
    Changed,
}

pub struct TextBuffer {
    lines: Vec<String>,
    raw_x: usize,
    cursor_y: usize,
    version: u64,
    undo: Vec<Snapshot>,
    edit_group: Option<EditGroup>,
}

/// The buffer before one undoable step. Restoring also restores the cursor,
/// because an undo that leaves the caret somewhere else is a search for where
/// the text went.
#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    raw_x: usize,
    cursor_y: usize,
}

/// Which consecutive small edits share one snapshot. A paste, a word kill or
/// a plugin write is never a group: each is already a step the user meant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditGroup {
    Typing,
    Deleting,
}

/// Enough history for a long paste session typed over, and small enough that
/// the buffer cannot grow without bound on a held key.
const MAX_UNDO: usize = 64;

/// Refuses an offset that splits a character. Rounding it would move an edit
/// a plugin asked for onto text it never read.
fn check_boundary(value: &str, what: &str, idx: usize) -> Result<(), String> {
    if value.is_char_boundary(idx) {
        return Ok(());
    }
    Err(format!("{what} {idx} is inside a character"))
}

/// A tab is one column to the width math and [`TAB_SPACES`] wide to the
/// terminal, and a bare `\r` is a line break the wrapping never sees, so both
/// are spent here before they throw the caret cell off. Every other control
/// character goes the same way: the terminal draws it as nothing while the
/// width math counts it as one column, which shifts every later cell on the
/// row and lands a click on the wrong character. It would also reach the
/// model inside text nobody could see.
///
/// Text a plugin writes and text the user pastes go through this. Text the
/// user composed elsewhere, in `$EDITOR` or a restored draft, lands verbatim
/// through [`TextBuffer::set_value`]: expanding the tabs of a tab-indented
/// prompt on its way back in would send the model something the user never
/// wrote.
fn sanitize(text: &str) -> String {
    text.replace('\t', TAB_SPACES)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(|c: char| c.is_control() && c != '\n', "")
}

impl TextBuffer {
    pub fn new(input: String) -> Self {
        let lines: Vec<String> = input.split('\n').map(str::to_string).collect();
        Self {
            lines,
            raw_x: 0,
            cursor_y: 0,
            version: 0,
            undo: Vec::new(),
            edit_group: None,
        }
    }

    pub fn value(&self) -> String {
        self.lines.join("\n")
    }

    /// Replaces the whole value verbatim and parks the cursor at the start.
    ///
    /// The version keeps climbing across the swap, so a plugin holding a
    /// version from before a history entry was recalled cannot mistake the
    /// entry for the value it read.
    pub fn set_value(&mut self, value: String) {
        self.lines = value.split('\n').map(str::to_string).collect();
        self.raw_x = 0;
        self.cursor_y = 0;
        self.version += 1;
        self.clear_undo();
    }

    fn snapshot(&mut self) {
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            raw_x: self.raw_x,
            cursor_y: self.cursor_y,
        });
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.edit_group = None;
    }

    fn clear_undo(&mut self) {
        self.undo.clear();
        self.edit_group = None;
    }

    /// Undoes the last step: one paste, one typed word, one word of backspaces.
    /// Snapshots taken before an edit that turned out to be a no-op (killing
    /// a word at an empty prompt) are skipped, so one press always rewinds
    /// something the user can see. `false` when there is nothing left to undo.
    pub fn undo(&mut self) -> bool {
        while let Some(snap) = self.undo.pop() {
            let unchanged = snap.lines == self.lines
                && snap.cursor_y == self.cursor_y
                && snap.raw_x == self.raw_x.min(self.current_line_len());
            if unchanged {
                continue;
            }
            self.lines = snap.lines;
            self.raw_x = snap.raw_x;
            self.cursor_y = snap.cursor_y;
            self.version += 1;
            self.edit_group = None;
            return true;
        }
        false
    }

    /// Counts every change to the value, never a cursor move. `maki.ui.input`
    /// hands it to Lua so an edit planned against an older value fails instead
    /// of landing on text the user has typed since.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn x(&self) -> usize {
        self.raw_x.min(self.current_line_len())
    }

    pub fn y(&self) -> usize {
        self.cursor_y
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_y]
    }

    fn current_line_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map_or(s.len(), |(byte_idx, _)| byte_idx)
    }

    fn byte_x(&self) -> usize {
        Self::char_to_byte(self.current_line(), self.x())
    }

    pub fn push_char(&mut self, c: char) {
        // A typed word is one step, split at whitespace: undo after a run of
        // typing rewinds to the last space, not one keystroke at a time.
        let x = self.x();
        let boundary = x > 0
            && self
                .current_line()
                .chars()
                .nth(x - 1)
                .is_some_and(|prev| prev.is_whitespace());
        if self.edit_group != Some(EditGroup::Typing) || boundary {
            self.snapshot();
        }
        self.edit_group = Some(EditGroup::Typing);
        let bx = self.byte_x();
        self.lines[self.cursor_y].insert(bx, c);
        self.raw_x = self.x() + 1;
        self.version += 1;
    }

    pub fn insert_text(&mut self, text: &str) {
        let sanitized = sanitize(text);
        if sanitized.is_empty() {
            return;
        }
        // One paste, one step. The snapshot() clears the group so typing that
        // follows the paste undoes separately from the paste itself.
        self.snapshot();
        for (i, chunk) in sanitized.split('\n').enumerate() {
            if i > 0 {
                self.add_line();
            }
            if !chunk.is_empty() {
                let bx = self.byte_x();
                self.lines[self.cursor_y].insert_str(bx, chunk);
                self.raw_x = self.x() + chunk.chars().count();
                self.version += 1;
            }
        }
    }

    pub fn add_line(&mut self) {
        self.snapshot();
        self.edit_group = None;
        let bx = self.byte_x();
        let (left, right) = self.lines[self.cursor_y].split_at(bx);
        let (left, right) = (left.to_string(), right.to_string());
        self.lines[self.cursor_y] = left;
        self.lines.insert(self.cursor_y + 1, right);
        self.raw_x = 0;
        self.cursor_y += 1;
        self.version += 1;
    }

    pub fn remove_char(&mut self) {
        let x = self.x();
        // Backspaces merge into one step, but a space deletes on its own:
        // undo after mashing backspace should restore whole words.
        let boundary = if x > 0 {
            self.current_line()
                .chars()
                .nth(x - 1)
                .is_some_and(|c| c.is_whitespace())
        } else {
            false
        };
        if self.edit_group != Some(EditGroup::Deleting) || boundary {
            self.snapshot();
        }
        self.edit_group = Some(EditGroup::Deleting);
        if x == 0 {
            self.merge_with_previous_line();
        } else {
            let bx = Self::char_to_byte(self.current_line(), x - 1);
            self.lines[self.cursor_y].remove(bx);
            self.raw_x = x - 1;
            self.version += 1;
        }
    }

    pub fn delete_char(&mut self) {
        self.snapshot();
        self.edit_group = None;
        let x = self.x();
        if x == self.current_line_len() {
            self.merge_with_next_line();
        } else {
            let bx = self.byte_x();
            self.lines[self.cursor_y].remove(bx);
            self.version += 1;
        }
    }

    fn wrap_to_prev_line(&mut self) -> bool {
        if self.cursor_y > 0 {
            self.cursor_y -= 1;
            self.raw_x = self.current_line_len();
            true
        } else {
            false
        }
    }

    fn wrap_to_next_line(&mut self) -> bool {
        if self.cursor_y < self.lines.len().saturating_sub(1) {
            self.cursor_y += 1;
            self.raw_x = 0;
            true
        } else {
            false
        }
    }

    fn find_prev_word_boundary(&self, char_x: usize) -> usize {
        let chars: Vec<char> = self.current_line().chars().collect();
        let mut i = char_x;
        while i > 0 && chars[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        i
    }

    fn find_next_word_boundary(&self, char_x: usize) -> usize {
        let chars: Vec<char> = self.current_line().chars().collect();
        let len = chars.len();
        let mut i = char_x;
        while i < len && chars[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }

    pub fn delete_word_after_cursor(&mut self) {
        self.snapshot();
        self.edit_group = None;
        let x = self.x();
        if x == self.current_line_len() {
            self.merge_with_next_line();
            return;
        }
        let new_x = self.find_next_word_boundary(x);
        let byte_start = Self::char_to_byte(self.current_line(), x);
        let byte_end = Self::char_to_byte(self.current_line(), new_x);
        self.lines[self.cursor_y].replace_range(byte_start..byte_end, "");
        self.version += 1;
    }

    pub fn kill_to_end_of_line(&mut self) {
        self.snapshot();
        self.edit_group = None;
        let bx = self.byte_x();
        self.lines[self.cursor_y].truncate(bx);
        self.version += 1;
    }

    pub fn remove_word_before_cursor(&mut self) {
        self.snapshot();
        self.edit_group = None;
        let x = self.x();
        if x == 0 {
            self.merge_with_previous_line();
            return;
        }
        let new_x = self.find_prev_word_boundary(x);
        let line = self.current_line();
        let byte_start = Self::char_to_byte(line, new_x);
        let byte_end = Self::char_to_byte(line, x);
        self.lines[self.cursor_y].replace_range(byte_start..byte_end, "");
        self.raw_x = new_x;
        self.version += 1;
    }

    pub fn move_word_left(&mut self) {
        self.edit_group = None;
        let x = self.x();
        if x == 0 {
            self.wrap_to_prev_line();
            return;
        }
        self.raw_x = self.find_prev_word_boundary(x);
    }

    pub fn move_word_right(&mut self) {
        self.edit_group = None;
        let x = self.x();
        if x == self.current_line_len() {
            self.wrap_to_next_line();
            return;
        }
        self.raw_x = self.find_next_word_boundary(x);
    }

    pub fn move_left(&mut self) {
        self.edit_group = None;
        let x = self.x();
        if x > 0 {
            self.raw_x = x - 1;
        } else {
            self.wrap_to_prev_line();
        }
    }

    pub fn move_right(&mut self) {
        self.edit_group = None;
        let x = self.x();
        if x < self.current_line_len() {
            self.raw_x = x + 1;
        } else {
            self.wrap_to_next_line();
        }
    }

    pub fn move_up(&mut self) {
        self.edit_group = None;
        if self.cursor_y > 0 {
            self.cursor_y -= 1;
        }
    }

    pub fn move_down(&mut self) {
        self.edit_group = None;
        if self.cursor_y < self.lines.len().saturating_sub(1) {
            self.cursor_y += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.edit_group = None;
        self.raw_x = 0;
    }

    pub fn move_end(&mut self) {
        self.edit_group = None;
        self.raw_x = self.current_line_len();
    }

    /// Drops the value and the undo history with it: a submit, a swap or a
    /// reset is the user putting the text down, and an undo that reaches back
    /// past it would resurrect a prompt already sent.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.raw_x = 0;
        self.cursor_y = 0;
        self.version += 1;
        self.clear_undo();
    }

    /// Bytes in the whole buffer, newlines counted as one each. The unit
    /// `maki.ui.input` speaks, because Lua string functions index bytes, so a
    /// plugin slices the value it read with the offsets it was handed.
    pub fn byte_len(&self) -> usize {
        let bytes: usize = self.lines.iter().map(String::len).sum();
        bytes + self.lines.len().saturating_sub(1)
    }

    /// The cursor as a flat byte offset into [`Self::byte_len`].
    pub fn cursor_byte(&self) -> usize {
        let before: usize = self.lines[..self.cursor_y]
            .iter()
            .map(|l| l.len() + 1)
            .sum();
        before + Self::char_to_byte(self.current_line(), self.x())
    }

    /// Clamps past the end of the buffer, the way every other cursor move
    /// here does, and refuses an offset inside a character.
    pub fn set_cursor_byte(&mut self, idx: usize) -> Result<(), String> {
        self.edit_group = None;
        let mut left = idx.min(self.byte_len());
        for (y, line) in self.lines.iter().enumerate() {
            if left <= line.len() {
                check_boundary(line, "cursor", left)?;
                self.cursor_y = y;
                self.raw_x = line[..left].chars().count();
                return Ok(());
            }
            left -= line.len() + 1;
        }
        self.move_to_end();
        Ok(())
    }

    /// Replaces a flat byte range, leaving the cursor after the inserted text
    /// unless {cursor} names another offset.
    ///
    /// Errors instead of clamping: these ranges come from async plugin
    /// handlers, so a range that no longer fits means the buffer moved under
    /// the caller and the edit would land somewhere it was never meant to.
    /// The cursor is checked against the value the edit produces, so a bad one
    /// refuses the whole edit instead of leaving half applied.
    ///
    /// {text} is sanitized the way typed and pasted text is, so the default
    /// cursor lands at the end of what was really inserted.
    pub fn replace_byte_range(
        &mut self,
        start: usize,
        stop: usize,
        text: &str,
        cursor: Option<usize>,
    ) -> Result<(), String> {
        if start > stop {
            return Err(format!("start {start} is past stop {stop}"));
        }
        let len = self.byte_len();
        if stop > len {
            return Err(format!("stop {stop} is past the end of the input ({len})"));
        }
        let value = self.value();
        check_boundary(&value, "start", start)?;
        check_boundary(&value, "stop", stop)?;

        let text = sanitize(text);
        let mut next = String::with_capacity(value.len() - (stop - start) + text.len());
        next.push_str(&value[..start]);
        next.push_str(&text);
        next.push_str(&value[stop..]);

        let caret = cursor.unwrap_or(start + text.len()).min(next.len());
        check_boundary(&next, "cursor", caret)?;

        self.snapshot();
        self.lines = next.split('\n').map(str::to_string).collect();
        self.version += 1;
        self.set_cursor_byte(caret)
    }

    pub fn set_cursor(&mut self, y: usize, x: usize) {
        self.edit_group = None;
        self.cursor_y = y.min(self.lines.len().saturating_sub(1));
        self.raw_x = x.min(self.current_line_len());
    }

    pub fn move_to_end(&mut self) {
        self.edit_group = None;
        self.cursor_y = self.lines.len().saturating_sub(1);
        self.raw_x = self.current_line_len();
    }

    fn merge_with_next_line(&mut self) {
        if self.cursor_y + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_y + 1);
            self.lines[self.cursor_y].push_str(&next);
            self.version += 1;
        }
    }

    fn merge_with_previous_line(&mut self) {
        if self.cursor_y == 0 {
            return;
        }
        self.cursor_y -= 1;
        self.raw_x = self.current_line_len();
        self.merge_with_next_line();
    }

    pub fn kill_to_start_of_line(&mut self) {
        self.snapshot();
        self.edit_group = None;
        let byte_x = Self::char_to_byte(&self.lines[self.cursor_y], self.x());
        self.lines[self.cursor_y].drain(..byte_x);
        self.raw_x = 0;
        self.version += 1;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditResult {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT);
        let alt = m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL);
        let sup = m.contains(KeyModifiers::SUPER);

        if ctrl {
            return match key.code {
                KeyCode::Left => {
                    self.move_word_left();
                    EditResult::Moved
                }
                KeyCode::Right => {
                    self.move_word_right();
                    EditResult::Moved
                }
                KeyCode::Backspace | KeyCode::Char('w') => {
                    self.remove_word_before_cursor();
                    EditResult::Changed
                }
                KeyCode::Delete => {
                    self.delete_word_after_cursor();
                    EditResult::Changed
                }
                KeyCode::Char('k') => {
                    self.kill_to_end_of_line();
                    EditResult::Changed
                }
                KeyCode::Char('a') => {
                    self.move_home();
                    EditResult::Moved
                }
                KeyCode::Char('e') => {
                    self.move_end();
                    EditResult::Moved
                }
                _ => EditResult::Ignored,
            };
        }

        if alt {
            return match key.code {
                KeyCode::Left | KeyCode::Char('b') => {
                    self.move_word_left();
                    EditResult::Moved
                }
                KeyCode::Right | KeyCode::Char('f') => {
                    self.move_word_right();
                    EditResult::Moved
                }
                KeyCode::Backspace => {
                    self.remove_word_before_cursor();
                    EditResult::Changed
                }
                KeyCode::Delete | KeyCode::Char('d') => {
                    self.delete_word_after_cursor();
                    EditResult::Changed
                }
                _ => EditResult::Ignored,
            };
        }

        if sup {
            return match key.code {
                KeyCode::Left => {
                    self.move_home();
                    EditResult::Moved
                }
                KeyCode::Right => {
                    self.move_end();
                    EditResult::Moved
                }
                KeyCode::Backspace => {
                    self.kill_to_start_of_line();
                    EditResult::Changed
                }
                // Cmd+Z where the terminal delivers it (kitty-protocol).
                // Ctrl+Z is suspend and reserved; there is no notation for
                // SUPER, so no plugin can want this key for itself.
                KeyCode::Char('z') => {
                    if self.undo() {
                        EditResult::Changed
                    } else {
                        EditResult::Ignored
                    }
                }
                _ => EditResult::Ignored,
            };
        }

        match key.code {
            KeyCode::Char(c) => {
                self.push_char(c);
                EditResult::Changed
            }
            KeyCode::Backspace => {
                self.remove_char();
                EditResult::Changed
            }
            KeyCode::Delete => {
                self.delete_char();
                EditResult::Changed
            }
            KeyCode::Left => {
                self.move_left();
                EditResult::Moved
            }
            KeyCode::Right => {
                self.move_right();
                EditResult::Moved
            }
            KeyCode::Home => {
                self.move_home();
                EditResult::Moved
            }
            KeyCode::End => {
                self.move_end();
                EditResult::Moved
            }
            KeyCode::Up => {
                self.move_up();
                EditResult::Moved
            }
            KeyCode::Down => {
                self.move_down();
                EditResult::Moved
            }
            _ => EditResult::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EditResult, TAB_SPACES, TextBuffer, MAX_UNDO};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use test_case::test_case;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn insert_at_middle() {
        let mut buf = TextBuffer::new(String::new());
        buf.push_char('a');
        buf.push_char('c');
        buf.raw_x = 1;
        buf.push_char('b');
        assert_eq!(buf.value(), "abc");
    }

    #[test]
    fn split_then_merge_is_identity() {
        let mut buf = TextBuffer::new("abcd".into());
        buf.raw_x = 2;
        buf.add_line();
        assert_eq!(buf.lines(), &["ab", "cd"]);

        buf.remove_char();
        assert_eq!(buf.value(), "abcd");
    }

    #[test]
    fn delete_char_merges_lines() {
        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.raw_x = 2;
        buf.delete_char();
        assert_eq!(buf.value(), "abcd");
    }

    #[test]
    fn cursor_wraps_across_lines() {
        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.raw_x = 2;
        buf.move_right();
        assert_eq!((buf.y(), buf.x()), (1, 0));

        buf.move_left();
        assert_eq!((buf.y(), buf.x()), (0, 2));
    }

    #[test]
    fn insert_text_multiline() {
        let mut buf = TextBuffer::new(String::new());
        buf.insert_text("line1\nline2\nline3");
        assert_eq!(buf.lines(), &["line1", "line2", "line3"]);
        assert_eq!(buf.y(), 2);
        assert_eq!(buf.x(), 5);
    }

    #[test]
    fn insert_text_at_cursor_middle() {
        let mut buf = TextBuffer::new("abcd".into());
        buf.raw_x = 2;
        buf.insert_text("X\nY");
        assert_eq!(buf.lines(), &["abX", "Ycd"]);
    }

    #[test]
    fn insert_text_replaces_tabs_with_spaces() {
        let mut buf = TextBuffer::new(String::new());
        buf.insert_text("\tindented\n\t\tdouble");
        assert_eq!(buf.lines(), &["  indented", "    double"]);
    }

    /// The terminal gives a control character no cells while the input box
    /// counts it as one column wide, so one left in the value shifts every
    /// later cell of the row and a click lands on the wrong character. It is
    /// invisible in the box and still sent to the model, so both ways in
    /// spend it.
    #[test_case("\u{c}\u{c}abc",   "abc"      ; "form_feeds")]
    #[test_case("a\u{1b}[31mb",    "a[31mb"   ; "escape_sequence")]
    #[test_case("a\0b\u{7f}c",     "abc"      ; "nul_and_delete")]
    #[test_case("a\u{b}b",         "ab"       ; "vertical_tab")]
    fn pasted_and_plugin_text_lose_their_control_characters(text: &str, expected: &str) {
        let mut pasted = TextBuffer::new(String::new());
        pasted.insert_text(text);
        assert_eq!(pasted.value(), expected);

        let mut written = TextBuffer::new(String::new());
        written.replace_byte_range(0, 0, text, None).unwrap();
        assert_eq!(written.value(), expected);
        assert_eq!(
            written.cursor_byte(),
            expected.len(),
            "the default cursor counts what was really inserted"
        );
    }

    #[test]
    fn remove_word() {
        let mut buf = TextBuffer::new("hello world".into());
        buf.raw_x = 11;
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "hello ");

        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "");

        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.cursor_y = 1;
        buf.raw_x = 0;
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "abcd");

        let mut buf = TextBuffer::new("hello ●●●".into());
        buf.move_to_end();
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "hello ");
    }

    #[test]
    fn move_word_left() {
        let mut buf = TextBuffer::new("hello world".into());
        buf.move_to_end();
        buf.move_word_left();
        assert_eq!(buf.x(), 6);
        buf.move_word_left();
        assert_eq!(buf.x(), 0);

        let mut buf = TextBuffer::new("  hello".into());
        buf.move_to_end();
        buf.move_word_left();
        assert_eq!(buf.x(), 2);

        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.cursor_y = 1;
        buf.raw_x = 0;
        buf.move_word_left();
        assert_eq!((buf.y(), buf.x()), (0, 2));
    }

    #[test]
    fn move_word_right() {
        let mut buf = TextBuffer::new("hello world".into());
        buf.move_word_right();
        assert_eq!(buf.x(), 5);
        buf.move_word_right();
        assert_eq!(buf.x(), 11);

        let mut buf = TextBuffer::new("hello  ".into());
        buf.move_word_right();
        assert_eq!(buf.x(), 5);

        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.raw_x = 2;
        buf.move_word_right();
        assert_eq!((buf.y(), buf.x()), (1, 0));
    }

    #[test]
    fn multibyte_operations() {
        let mut buf = TextBuffer::new(String::new());
        buf.push_char('a');
        buf.push_char('●');
        buf.push_char('b');
        assert_eq!(buf.value(), "a●b");

        buf.remove_char();
        assert_eq!(buf.value(), "a●");
        buf.remove_char();
        assert_eq!(buf.value(), "a");

        let mut buf = TextBuffer::new("a●b".into());
        buf.move_to_end();
        buf.move_left();
        assert_eq!(buf.x(), 2);
        buf.move_left();
        assert_eq!(buf.x(), 1);
        buf.move_right();
        assert_eq!(buf.x(), 2);

        let mut buf = TextBuffer::new("a●b".into());
        buf.raw_x = 1;
        buf.delete_char();
        assert_eq!(buf.value(), "ab");

        let mut buf = TextBuffer::new("a●b".into());
        buf.raw_x = 2;
        buf.add_line();
        assert_eq!(buf.lines(), &["a●", "b"]);

        let mut buf = TextBuffer::new("a●b".into());
        buf.raw_x = 2;
        buf.insert_text("X");
        assert_eq!(buf.value(), "a●Xb");
    }

    #[test]
    fn sticky_x_with_multibyte() {
        let mut buf = TextBuffer::new("a●cd\nhi\na●cd".into());
        buf.raw_x = 4;
        buf.move_down();
        assert_eq!(buf.x(), 2);
        buf.move_down();
        assert_eq!(buf.x(), 4);
    }

    #[test]
    fn delete_word_after_cursor() {
        let mut buf = TextBuffer::new("hello world".into());
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), " world");

        let mut buf = TextBuffer::new("hello world".into());
        buf.raw_x = 6;
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), "hello ");

        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.raw_x = 2;
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), "abcd");

        let mut buf = TextBuffer::new("end".into());
        buf.raw_x = 3;
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), "end");
    }

    #[test]
    fn kill_to_end_of_line() {
        let mut buf = TextBuffer::new("hello world".into());
        buf.raw_x = 5;
        buf.kill_to_end_of_line();
        assert_eq!(buf.value(), "hello");

        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.kill_to_end_of_line();
        assert_eq!(buf.lines(), &["", "cd"]);

        let mut buf = TextBuffer::new("●text".into());
        buf.raw_x = 1;
        buf.kill_to_end_of_line();
        assert_eq!(buf.value(), "●");
    }

    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::ALT)
    }

    fn super_key(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::SUPER)
    }

    #[test_case(plain(KeyCode::Char('a')),      EditResult::Changed ; "plain_changed")]
    #[test_case(plain(KeyCode::Left),            EditResult::Moved   ; "plain_moved")]
    #[test_case(plain(KeyCode::F(1)),            EditResult::Ignored ; "plain_ignored")]
    #[test_case(ctrl(KeyCode::Char('e')),        EditResult::Moved   ; "ctrl_e_moved")]
    #[test_case(ctrl(KeyCode::Char('k')),        EditResult::Changed ; "ctrl_changed")]
    #[test_case(ctrl(KeyCode::Char('a')),        EditResult::Moved   ; "ctrl_moved")]
    #[test_case(ctrl(KeyCode::Char('z')),        EditResult::Ignored ; "ctrl_ignored")]
    #[test_case(alt(KeyCode::Backspace),         EditResult::Changed ; "alt_changed")]
    #[test_case(alt(KeyCode::Left),              EditResult::Moved   ; "alt_moved")]
    #[test_case(alt(KeyCode::Char('z')),         EditResult::Ignored ; "alt_ignored")]
    #[test_case(super_key(KeyCode::Backspace),   EditResult::Changed ; "super_changed")]
    #[test_case(super_key(KeyCode::Left),        EditResult::Moved   ; "super_moved")]
    #[test_case(super_key(KeyCode::Char('z')),   EditResult::Ignored ; "super_ignored")]
    #[test_case(plain(KeyCode::Up),              EditResult::Moved   ; "plain_up")]
    #[test_case(plain(KeyCode::Down),            EditResult::Moved   ; "plain_down")]
    fn handle_key_returns_correct_result(key: KeyEvent, expected: EditResult) {
        let mut buf = TextBuffer::new("hello world".into());
        buf.raw_x = 5;
        assert_eq!(buf.handle_key(key), expected);
    }

    #[test]
    fn kill_to_start_of_line() {
        let mut buf = TextBuffer::new("●hello world".into());
        buf.raw_x = 1;
        buf.kill_to_start_of_line();
        assert_eq!(buf.value(), "hello world");
        assert_eq!(buf.x(), 0);
    }

    #[test_case("",              0, 0, 0  ; "empty")]
    #[test_case("abc",           0, 2, 2  ; "single_line")]
    #[test_case("ab\ncd",        1, 1, 4  ; "newline_counts_as_one")]
    #[test_case("日本\n語",       1, 1, 10 ; "multi_byte")]
    #[test_case("a\n\nb",        2, 0, 3  ; "blank_line")]
    fn cursor_byte_counts_newlines(value: &str, y: usize, x: usize, expected: usize) {
        let mut buf = TextBuffer::new(value.into());
        buf.set_cursor(y, x);
        assert_eq!(buf.cursor_byte(), expected);
    }

    #[test_case("",        0 ; "empty")]
    #[test_case("abc",     3 ; "single_line")]
    #[test_case("ab\ncd",  5 ; "two_lines")]
    #[test_case("🦀\n🦀",  9 ; "emoji")]
    fn byte_len_matches_flat_length(value: &str, expected: usize) {
        assert_eq!(TextBuffer::new(value.into()).byte_len(), expected);
    }

    #[test_case(0, 0, 0 ; "start")]
    #[test_case(3, 1, 0 ; "line_start")]
    #[test_case(5, 1, 2 ; "end")]
    #[test_case(99, 1, 2 ; "past_end_clamps")]
    fn set_cursor_byte_round_trips(idx: usize, y: usize, x: usize) {
        let mut buf = TextBuffer::new("ab\ncd".into());
        buf.set_cursor_byte(idx).unwrap();
        assert_eq!((buf.y(), buf.x()), (y, x));
    }

    /// Rounding down to the character that offset is inside of would silently
    /// move the cursor somewhere the plugin never asked for.
    #[test_case(1 ; "inside_the_first_char")]
    #[test_case(8 ; "inside_a_char_on_the_second_line")]
    fn set_cursor_byte_refuses_an_offset_inside_a_char(idx: usize) {
        let mut buf = TextBuffer::new("日本\n語".into());
        assert!(buf.set_cursor_byte(idx).is_err());
        assert_eq!((buf.y(), buf.x()), (0, 0));
    }

    #[test]
    fn replace_byte_range_spans_lines() {
        let mut buf = TextBuffer::new("hello\nworld".into());
        buf.replace_byte_range(3, 8, "X", None).unwrap();
        assert_eq!(buf.value(), "helXrld");
        assert_eq!(buf.cursor_byte(), 4);
    }

    #[test]
    fn replace_byte_range_inserts_newlines() {
        let mut buf = TextBuffer::new("ab".into());
        buf.replace_byte_range(1, 1, "\nX\n", None).unwrap();
        assert_eq!(buf.lines(), &["a", "X", "b"]);
        assert_eq!((buf.y(), buf.x()), (2, 0));
    }

    #[test]
    fn replace_byte_range_is_byte_indexed_not_char_indexed() {
        let mut buf = TextBuffer::new("日本語".into());
        buf.replace_byte_range(3, 6, "🦀", None).unwrap();
        assert_eq!(buf.value(), "日🦀語");
        assert_eq!(buf.cursor_byte(), 7);
    }

    #[test]
    fn replace_byte_range_on_empty_buffer_inserts() {
        let mut buf = TextBuffer::new(String::new());
        buf.replace_byte_range(0, 0, "hi", None).unwrap();
        assert_eq!(buf.value(), "hi");
        assert_eq!(buf.cursor_byte(), 2);
    }

    #[test]
    fn replace_byte_range_honours_an_explicit_cursor() {
        let mut buf = TextBuffer::new("日本語".into());
        buf.replace_byte_range(0, 3, "🦀", Some(0)).unwrap();
        assert_eq!(buf.value(), "🦀本語");
        assert_eq!(buf.cursor_byte(), 0);
    }

    #[test_case(3, 2,  None    ; "inverted")]
    #[test_case(0, 99, None    ; "past_end")]
    #[test_case(1, 3,  None    ; "start_inside_a_char")]
    #[test_case(0, 4,  None    ; "stop_inside_a_char")]
    #[test_case(0, 3,  Some(2) ; "cursor_inside_a_char")]
    fn replace_byte_range_refuses_an_offset_it_cannot_honour(
        start: usize,
        stop: usize,
        cursor: Option<usize>,
    ) {
        let mut buf = TextBuffer::new("日本語".into());
        let before = buf.version();
        assert!(buf.replace_byte_range(start, stop, "x", cursor).is_err());
        assert_eq!(buf.value(), "日本語");
        assert_eq!(
            buf.version(),
            before,
            "a refused edit must not bump the version"
        );
    }

    /// `$EDITOR` output, a restored draft and a rewind prompt all come back
    /// through here, and none of them is a plugin write. A tab-indented
    /// prompt has to reach the model as the user wrote it.
    #[test_case("a\tb"    ; "tabs")]
    #[test_case("a\r\nb"  ; "crlf")]
    #[test_case("a\rb"    ; "lone_cr")]
    fn set_value_keeps_the_text_verbatim(value: &str) {
        let mut buf = TextBuffer::new(String::new());
        buf.set_value(value.into());
        assert_eq!(buf.value(), value);
    }

    /// The cursor defaults to the end of the inserted text, so it counts the
    /// expanded tab, not the one byte that was asked for.
    #[test]
    fn replace_byte_range_sanitizes_and_leaves_the_cursor_past_it() {
        let mut buf = TextBuffer::new("ab".into());
        buf.replace_byte_range(1, 1, "\t", None).unwrap();
        assert_eq!(buf.value(), format!("a{TAB_SPACES}b"));
        assert_eq!(buf.cursor_byte(), 1 + TAB_SPACES.len());
    }

    #[test]
    fn version_counts_value_changes_and_ignores_cursor_moves() {
        let mut buf = TextBuffer::new("ab".into());
        let start = buf.version();
        buf.move_home();
        buf.move_right();
        assert_eq!(buf.version(), start, "moving the cursor changes no value");

        buf.push_char('c');
        let typed = buf.version();
        assert!(typed > start);

        buf.set_value("recalled".into());
        assert!(
            buf.version() > typed,
            "a whole-value swap has to keep the counter climbing"
        );
    }

    #[test]
    fn undo_restores_a_paste_in_one_step() {
        let mut buf = TextBuffer::new("keep ".into());
        buf.insert_text("pasted text");
        assert!(buf.undo());
        assert_eq!(buf.value(), "keep ");
        assert!(!buf.undo(), "the fresh buffer has nothing older to give back");
    }

    #[test]
    fn undo_rewinds_typing_word_by_word() {
        let mut buf = TextBuffer::new(String::new());
        for c in "hello world".chars() {
            buf.push_char(c);
        }
        buf.undo();
        assert_eq!(buf.value(), "hello ");
        buf.undo();
        assert_eq!(buf.value(), "");
    }

    #[test]
    fn backspaces_undo_word_by_word() {
        let mut buf = TextBuffer::new("hello world".into());
        buf.move_to_end();
        for _ in 0..6 {
            buf.remove_char();
        }
        assert_eq!(buf.value(), "hello");
        buf.undo();
        assert_eq!(buf.value(), "hello ");
        buf.undo();
        assert_eq!(buf.value(), "hello world");
    }

    #[test]
    fn a_cursor_move_ends_the_typing_group() {
        let mut buf = TextBuffer::new("abc".into());
        buf.set_cursor(0, 1);
        buf.push_char('x');
        buf.push_char('y');
        assert_eq!(buf.value(), "axybc");
        buf.undo();
        assert_eq!(buf.value(), "abc");
        assert_eq!((buf.x(), buf.y()), (1, 0), "undo restores where the caret was");
    }

    #[test]
    fn a_plugin_write_undoes_in_one_step() {
        let mut buf = TextBuffer::new("pick a file".into());
        buf.replace_byte_range(7, 11, "/tmp/x.rs", None).unwrap();
        assert!(buf.undo());
        assert_eq!(buf.value(), "pick a file");
    }

    #[test]
    fn clear_forgets_the_history() {
        let mut buf = TextBuffer::new("sent".into());
        buf.insert_text(" again");
        buf.clear();
        assert!(!buf.undo(), "a submitted prompt does not come back");
    }

    #[test]
    fn no_op_steps_are_skipped() {
        let mut buf = TextBuffer::new("word".into());
        buf.move_to_end();
        buf.remove_word_before_cursor();
        buf.remove_word_before_cursor(); // snapshots a state that never changed
        assert!(buf.undo());
        assert_eq!(buf.value(), "word");
    }

    #[test]
    fn history_is_capped() {
        let mut buf = TextBuffer::new(String::new());
        for _ in 0..(MAX_UNDO + 10) {
            buf.insert_text("a ");
        }
        let mut undos = 0;
        while buf.undo() {
            undos += 1;
        }
        assert!(undos <= MAX_UNDO, "history must not grow without bound");
    }

    #[test]
    fn super_z_undoes_through_handle_key() {
        let mut buf = TextBuffer::new("hi".into());
        buf.insert_text(" there");
        let result = buf.handle_key(key(KeyCode::Char('z'), KeyModifiers::SUPER));
        assert_eq!(result, EditResult::Changed);
        assert_eq!(buf.value(), "hi");
    }
}
