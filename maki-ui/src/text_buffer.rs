use std::collections::VecDeque;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::highlight::TAB_SPACES;

/// How far a reader can fall behind and still catch up through the log. A
/// reader that catches up every frame never falls far behind, and one that
/// falls off the end only pays for a rebuild from the whole value.
const CHANGE_LOG_LEN: usize = 256;

/// A step keeps the text its edit removed, so a long session of big deletes
/// would pile up forever without a cap. Nobody presses undo 64 times in a row.
const MAX_UNDO: usize = 64;

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

/// `start..old_end` of the old value became `start..new_end` of the new one.
/// Only offsets, so a big paste costs the log as much as one keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
}

/// Plain typing and plain backspaces run together into one step per word. A
/// paste, a word kill or a plugin write never joins a run, because each one is
/// already a step the user meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditGroup {
    Typing,
    Deleting,
}

/// Puts {text} back over `start..end` to undo one splice. It keeps only what
/// the splice removed, so typing and pasting store no text at all and a step
/// never costs the whole prompt.
struct Inverse {
    start: usize,
    end: usize,
    text: String,
}

impl Inverse {
    /// Whether {text} already reads what this puts back, as after writing
    /// the same text over itself, which is no change at all.
    fn restores(&self, text: &str) -> bool {
        text[self.start..self.end] == self.text
    }
}

struct UndoStep {
    /// Oldest first. Undo walks them back to front, because each range points
    /// into the text the edits before it left behind.
    inverses: Vec<Inverse>,
    /// Undo puts the caret back here, or the user goes looking for the text
    /// that just changed.
    cursor: usize,
    group: Option<EditGroup>,
}

impl UndoStep {
    /// Folds {next} into this step when it continues the same run, a character
    /// typed where the last one ended or a backspace right before the last one.
    /// Only position counts, so moving the cursor away and back keeps the run.
    /// A run breaks after whitespace, which makes undo go a word at a time.
    fn absorb(&mut self, next: &UndoStep, text: &str) -> bool {
        let (Some(group), [last], [new]) = (self.group, &mut self.inverses[..], &next.inverses[..])
        else {
            return false;
        };
        if next.group != Some(group) {
            return false;
        }
        match group {
            EditGroup::Typing => {
                if new.start != last.end || text[..new.start].ends_with(char::is_whitespace) {
                    return false;
                }
                last.end = new.end;
            }
            EditGroup::Deleting => {
                if new.start + new.text.len() != last.start
                    || new.text.starts_with(char::is_whitespace)
                {
                    return false;
                }
                last.start = new.start;
                last.end = new.end;
                last.text.insert_str(0, &new.text);
            }
        }
        true
    }
}

#[derive(Default)]
struct UndoHistory {
    steps: VecDeque<UndoStep>,
    /// The step the action in progress records into.
    open: Option<UndoStep>,
}

impl UndoHistory {
    /// An action that changed nothing leaves no step, so every undo rewinds
    /// something the user can see.
    fn push(&mut self, step: UndoStep, text: &str) {
        // Only the newest inverse is sure to point into {text}. When it holds,
        // the text before it was {text} too, so the one before can be checked
        // next, and all of them holding means the step changed nothing.
        if step
            .inverses
            .iter()
            .rev()
            .all(|inverse| inverse.restores(text))
        {
            return;
        }
        if let Some(last) = self.steps.back_mut()
            && last.absorb(&step, text)
        {
            return;
        }
        if self.steps.len() == MAX_UNDO {
            self.steps.pop_front();
        }
        self.steps.push_back(step);
    }
}

pub struct TextBuffer {
    text: String,
    /// Byte offset where each line starts. An empty value is still one empty
    /// line, so this always holds at least the first 0 and
    /// `line_count() - 1` never underflows.
    line_starts: Vec<usize>,
    raw_x: usize,
    cursor_y: usize,
    version: u64,
    /// One entry per version, newest at the back, so `changes_since` finds a
    /// version by counting back from the end.
    changes: VecDeque<Change>,
    undo: UndoHistory,
}

/// Refuses an offset that splits a character. Rounding it would move an edit
/// a plugin asked for onto text it never read.
fn check_boundary(on_boundary: bool, what: &str, idx: usize) -> Result<(), String> {
    if on_boundary {
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

fn line_starts_in(text: &str, offset: usize) -> impl Iterator<Item = usize> {
    text.match_indices('\n').map(move |(i, _)| offset + i + 1)
}

/// The smallest span where {old} and {new} differ, as
/// `(start, old_end, new_end)`. Two different characters can share their
/// first or last byte, so both ends are pushed out to a character boundary
/// before the span is sliced.
fn changed_span(old: &str, new: &str) -> (usize, usize, usize) {
    let prefix = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b);
    let start = old.floor_char_boundary(prefix.count());
    let max_suffix = old.len().min(new.len()) - start;
    let suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let old_end = old.ceil_char_boundary(old.len() - suffix);
    (start, old_end, new.len() - (old.len() - old_end))
}

impl TextBuffer {
    pub fn new(text: String) -> Self {
        let line_starts = [0].into_iter().chain(line_starts_in(&text, 0)).collect();
        Self {
            text,
            line_starts,
            raw_x: 0,
            cursor_y: 0,
            version: 0,
            changes: VecDeque::new(),
            undo: UndoHistory::default(),
        }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the whole value verbatim and parks the cursor at the start.
    ///
    /// A recalled history entry often shares most of its text with the
    /// prompt, so only the part that differs is spliced.
    ///
    /// The undo history goes with the old value. Walking back across a submit,
    /// a draft swap or a history recall would bring back a prompt the user
    /// already put away. A step that is still open goes too, because its
    /// ranges point into the old value.
    pub fn set_value(&mut self, value: &str) {
        let (start, old_end, new_end) = changed_span(&self.text, value);
        self.splice(start..old_end, &value[start..new_end]);
        self.undo = UndoHistory::default();
        self.raw_x = 0;
        self.cursor_y = 0;
    }

    /// Runs one action as one undo step, however many edits it makes. Called
    /// inside another action, it joins that action's step.
    pub fn undoable<R>(&mut self, action: impl FnOnce(&mut Self) -> R) -> R {
        self.undo_step(None, action)
    }

    fn undo_step<R>(&mut self, group: Option<EditGroup>, action: impl FnOnce(&mut Self) -> R) -> R {
        if self.undo.open.is_some() {
            return action(self);
        }
        self.undo.open = Some(UndoStep {
            inverses: Vec::new(),
            cursor: self.cursor_byte(),
            group,
        });
        let out = action(self);
        // Gone when the action called `set_value`, which drops it with the history.
        if let Some(step) = self.undo.open.take() {
            self.undo.push(step, &self.text);
        }
        out
    }

    /// `false` when there is nothing left to undo.
    pub fn undo(&mut self) -> bool {
        debug_assert!(
            self.undo
                .open
                .as_ref()
                .is_none_or(|s| s.inverses.is_empty()),
            "undo inside an action that already edited"
        );
        let Some(step) = self.undo.steps.pop_back() else {
            return false;
        };
        for Inverse { start, end, text } in step.inverses.into_iter().rev() {
            self.splice(start..end, &text);
        }
        self.place_cursor(step.cursor);
        true
    }

    /// Counts every change to the value, never a cursor move. `maki.ui.input`
    /// hands it to Lua so an edit planned against an older value fails instead
    /// of landing on text the user has typed since.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The changes that led from {version} to now, oldest first. `None` when
    /// {version} is older than the log reaches back, or newer than the buffer.
    pub fn changes_since(&self, version: u64) -> Option<impl Iterator<Item = &Change>> {
        let behind = usize::try_from(self.version.checked_sub(version)?).ok()?;
        let skip = self.changes.len().checked_sub(behind)?;
        Some(self.changes.range(skip..))
    }

    pub fn line(&self, y: usize) -> &str {
        &self.text[self.line_range(y)]
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    /// The line that byte {idx} of the value sits on.
    pub fn line_of(&self, idx: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= idx) - 1
    }

    fn line_range(&self, y: usize) -> Range<usize> {
        let end = self
            .line_starts
            .get(y + 1)
            .map_or(self.text.len(), |next| next - 1);
        self.line_starts[y]..end
    }

    pub fn x(&self) -> usize {
        self.raw_x.min(self.current_line_len())
    }

    pub fn y(&self) -> usize {
        self.cursor_y
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    fn current_line(&self) -> &str {
        self.line(self.cursor_y)
    }

    fn current_line_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map_or(s.len(), |(byte_idx, _)| byte_idx)
    }

    fn value_byte_at_column(&self, char_x: usize) -> usize {
        self.line_starts[self.cursor_y] + Self::char_to_byte(self.current_line(), char_x)
    }

    /// The cursor as a byte offset into its own line.
    pub fn cursor_byte_in_line(&self) -> usize {
        Self::char_to_byte(self.current_line(), self.x())
    }

    /// Every change to the value goes through here, so the text, the line
    /// index, the version and the log always agree. An empty splice is
    /// dropped, so a no-op like `Ctrl+K` at the end of a line leaves the
    /// version alone.
    fn splice(&mut self, range: Range<usize>, text: &str) -> bool {
        if range.is_empty() && text.is_empty() {
            return false;
        }
        let Range {
            start,
            end: old_end,
        } = range;
        let new_end = start + text.len();
        self.text.replace_range(start..old_end, text);

        // A line starting in `start + 1..=old_end` lost the newline before it.
        let first_gone = self.line_starts.partition_point(|&s| s <= start);
        let first_kept = self.line_starts.partition_point(|&s| s <= old_end);
        for line_start in &mut self.line_starts[first_kept..] {
            *line_start = *line_start - old_end + new_end;
        }
        self.line_starts
            .splice(first_gone..first_kept, line_starts_in(text, start));

        self.version += 1;
        if self.changes.len() == CHANGE_LOG_LEN {
            self.changes.pop_front();
        }
        self.changes.push_back(Change {
            start,
            old_end,
            new_end,
        });
        true
    }

    /// An edit made outside any action still becomes a step of its own. Left
    /// out, it would move the text under every older step and their ranges
    /// would point at the wrong place.
    fn edit(&mut self, range: Range<usize>, text: &str, caret: usize) {
        self.undoable(|buf| {
            let start = range.start;
            let removed = buf.text[range.clone()].to_owned();
            if buf.splice(range, text)
                && let Some(step) = &mut buf.undo.open
            {
                step.inverses.push(Inverse {
                    start,
                    end: start + text.len(),
                    text: removed,
                });
            }
            buf.place_cursor(caret);
        });
    }

    fn insert(&mut self, text: &str) {
        let at = self.cursor_byte();
        self.edit(at..at, text, at + text.len());
    }

    /// {idx} must be a char boundary within the value.
    fn place_cursor(&mut self, idx: usize) {
        self.cursor_y = self.line_of(idx);
        self.raw_x = self.text[self.line_starts[self.cursor_y]..idx]
            .chars()
            .count();
    }

    pub fn push_char(&mut self, c: char) {
        self.insert(c.encode_utf8(&mut [0; 4]));
    }

    pub fn insert_text(&mut self, text: &str) {
        self.insert(&sanitize(text));
    }

    pub fn add_line(&mut self) {
        self.insert("\n");
    }

    /// At the start of a line the character before the cursor is the newline,
    /// so removing it joins the two lines with no special case.
    pub fn remove_char(&mut self) {
        let at = self.cursor_byte();
        if let Some(c) = self.text[..at].chars().next_back() {
            let from = at - c.len_utf8();
            self.edit(from..at, "", from);
        }
    }

    /// At the end of a line the character after the cursor is the newline, so
    /// deleting it pulls the next line up.
    pub fn delete_char(&mut self) {
        let at = self.cursor_byte();
        if let Some(c) = self.text[at..].chars().next() {
            self.edit(at..at + c.len_utf8(), "", at);
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
        if self.cursor_y < self.line_count() - 1 {
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
        let x = self.x();
        if x == self.current_line_len() {
            self.delete_char();
            return;
        }
        let at = self.cursor_byte();
        let end = self.value_byte_at_column(self.find_next_word_boundary(x));
        self.edit(at..end, "", at);
    }

    pub fn kill_to_end_of_line(&mut self) {
        let at = self.cursor_byte();
        let end = self.line_range(self.cursor_y).end;
        self.edit(at..end, "", at);
    }

    pub fn remove_word_before_cursor(&mut self) {
        let x = self.x();
        if x == 0 {
            self.remove_char();
            return;
        }
        let from = self.value_byte_at_column(self.find_prev_word_boundary(x));
        let at = self.cursor_byte();
        self.edit(from..at, "", from);
    }

    pub fn move_word_left(&mut self) {
        let x = self.x();
        if x == 0 {
            self.wrap_to_prev_line();
            return;
        }
        self.raw_x = self.find_prev_word_boundary(x);
    }

    pub fn move_word_right(&mut self) {
        let x = self.x();
        if x == self.current_line_len() {
            self.wrap_to_next_line();
            return;
        }
        self.raw_x = self.find_next_word_boundary(x);
    }

    pub fn move_left(&mut self) {
        let x = self.x();
        if x > 0 {
            self.raw_x = x - 1;
        } else {
            self.wrap_to_prev_line();
        }
    }

    pub fn move_right(&mut self) {
        let x = self.x();
        if x < self.current_line_len() {
            self.raw_x = x + 1;
        } else {
            self.wrap_to_next_line();
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_y > 0 {
            self.cursor_y -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_y < self.line_count() - 1 {
            self.cursor_y += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.raw_x = 0;
    }

    pub fn move_end(&mut self) {
        self.raw_x = self.current_line_len();
    }

    pub fn clear(&mut self) {
        self.set_value("");
    }

    /// The cursor as a byte offset into [`Self::value`]. Bytes, not
    /// characters, because Lua string functions index bytes, so a plugin can
    /// slice the value it read with the offsets `maki.ui.input` hands it.
    pub fn cursor_byte(&self) -> usize {
        self.value_byte_at_column(self.x())
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
        let len = self.text.len();
        if stop > len {
            return Err(format!("stop {stop} is past the end of the input ({len})"));
        }
        check_boundary(self.text.is_char_boundary(start), "start", start)?;
        check_boundary(self.text.is_char_boundary(stop), "stop", stop)?;

        let text = sanitize(text);
        let inserted_end = start + text.len();
        let caret = cursor
            .unwrap_or(inserted_end)
            .min(inserted_end + len - stop);
        let on_boundary = if caret <= start {
            self.text.is_char_boundary(caret)
        } else if caret <= inserted_end {
            text.is_char_boundary(caret - start)
        } else {
            self.text.is_char_boundary(caret - inserted_end + stop)
        };
        check_boundary(on_boundary, "cursor", caret)?;

        self.edit(start..stop, &text, caret);
        Ok(())
    }

    pub fn set_cursor(&mut self, y: usize, x: usize) {
        self.cursor_y = y.min(self.line_count() - 1);
        self.raw_x = x.min(self.current_line_len());
    }

    pub fn move_to_end(&mut self) {
        self.cursor_y = self.line_count() - 1;
        self.raw_x = self.current_line_len();
    }

    pub fn kill_to_start_of_line(&mut self) {
        let from = self.line_starts[self.cursor_y];
        let at = self.cursor_byte();
        self.edit(from..at, "", from);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditResult {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT);
        let alt = m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL);
        let sup = m.contains(KeyModifiers::SUPER);
        let group = match key.code {
            _ if ctrl || alt || sup => None,
            KeyCode::Char(_) => Some(EditGroup::Typing),
            KeyCode::Backspace => Some(EditGroup::Deleting),
            _ => None,
        };
        self.undo_step(group, |buf| buf.apply_key(key.code, ctrl, alt, sup))
    }

    fn apply_key(&mut self, code: KeyCode, ctrl: bool, alt: bool, sup: bool) -> EditResult {
        if ctrl {
            return match code {
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
            return match code {
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
            return match code {
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
                // Cmd+Z only arrives on terminals that speak the kitty keyboard
                // protocol. Most never send it, which is why the app binds Alt+Z.
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

        match code {
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
    use super::{CHANGE_LOG_LEN, Change, EditResult, MAX_UNDO, TAB_SPACES, TextBuffer};
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

    /// Reads the lines through the line index and checks them against the
    /// text, so every test that looks at lines also catches a drifted index.
    fn lines(buf: &TextBuffer) -> Vec<&str> {
        let indexed: Vec<&str> = (0..buf.line_count()).map(|y| buf.line(y)).collect();
        assert_eq!(
            indexed,
            buf.value().split('\n').collect::<Vec<_>>(),
            "the line index drifted from the text"
        );
        indexed
    }

    fn changes(buf: &TextBuffer, since: u64) -> Vec<Change> {
        buf.changes_since(since)
            .expect("the log still reaches that far back")
            .cloned()
            .collect()
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
        assert_eq!(lines(&buf), ["ab", "cd"]);

        buf.remove_char();
        assert_eq!(buf.value(), "abcd");
    }

    /// The column remembered from the longer line above must not outlive the
    /// join, or the cursor jumps to the end of the joined line.
    #[test]
    fn delete_char_merges_lines_and_keeps_the_cursor_put() {
        let mut buf = TextBuffer::new("longer\nab\ncd".into());
        buf.set_cursor(0, 6);
        buf.move_down();
        buf.delete_char();
        assert_eq!(lines(&buf), ["longer", "abcd"]);
        assert_eq!((buf.y(), buf.x()), (1, 2));
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
        assert_eq!(lines(&buf), ["line1", "line2", "line3"]);
        assert_eq!(buf.y(), 2);
        assert_eq!(buf.x(), 5);
    }

    #[test]
    fn insert_text_at_cursor_middle() {
        let mut buf = TextBuffer::new("abcd".into());
        buf.raw_x = 2;
        buf.insert_text("X\nY");
        assert_eq!(lines(&buf), ["abX", "Ycd"]);
    }

    #[test]
    fn insert_text_replaces_tabs_with_spaces() {
        let mut buf = TextBuffer::new(String::new());
        buf.insert_text("\tindented\n\t\tdouble");
        assert_eq!(lines(&buf), ["  indented", "    double"]);
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
        assert_eq!(lines(&buf), ["a●", "b"]);

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
        assert_eq!(lines(&buf), ["", "cd"]);

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

    #[test_case(0, 0, 0 ; "start")]
    #[test_case(3, 1, 0 ; "line_start")]
    #[test_case(5, 1, 2 ; "end")]
    #[test_case(99, 1, 2 ; "past_end_clamps")]
    fn an_empty_edit_moves_only_the_cursor(idx: usize, y: usize, x: usize) {
        let mut buf = TextBuffer::new("ab\ncd".into());
        let before = buf.version();
        buf.replace_byte_range(0, 0, "", Some(idx)).unwrap();
        assert_eq!((buf.y(), buf.x()), (y, x));
        assert_eq!(buf.version(), before);
    }

    /// Rounding down to the character that offset is inside of would silently
    /// move the cursor somewhere the plugin never asked for.
    #[test_case(1 ; "inside_the_first_char")]
    #[test_case(8 ; "inside_a_char_on_the_second_line")]
    fn a_cursor_inside_a_char_is_refused(idx: usize) {
        let mut buf = TextBuffer::new("日本\n語".into());
        assert!(buf.replace_byte_range(0, 0, "", Some(idx)).is_err());
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
        assert_eq!(lines(&buf), ["a", "X", "b"]);
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
        buf.set_value(value);
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

        buf.set_value("recalled");
        assert!(
            buf.version() > typed,
            "a whole-value swap has to keep the counter climbing"
        );
    }

    /// Lines are slices of one string, cut where the index says each one
    /// starts. Every splice has to drop, add and shift those starts just
    /// right, and `lines` catches the first one that does not.
    #[test]
    fn line_index_follows_every_kind_of_splice() {
        let mut buf = TextBuffer::new("one\ntwo\nthree".into());
        buf.replace_byte_range(2, 9, "X\nY\nZ", None).unwrap();
        assert_eq!(lines(&buf), ["onX", "Y", "Zhree"]);
        buf.replace_byte_range(0, 0, "\n\n", None).unwrap();
        assert_eq!(lines(&buf), ["", "", "onX", "Y", "Zhree"]);
        buf.move_to_end();
        buf.insert_text("a\nb");
        assert_eq!(lines(&buf), ["", "", "onX", "Y", "Zhreea", "b"]);
        buf.move_home();
        buf.remove_char();
        assert_eq!(lines(&buf), ["", "", "onX", "Y", "Zhreeab"]);
        buf.set_value("one\nthree\n");
        assert_eq!(lines(&buf), ["one", "three", ""]);
        buf.clear();
        assert_eq!(lines(&buf), [""]);
    }

    #[test_case("abc", "aXc", 1, 2, 2 ; "middle")]
    #[test_case("abc", "",    0, 3, 0 ; "cleared")]
    #[test_case("aa",  "aaa", 2, 2, 3 ; "repeated_char_is_not_counted_twice")]
    #[test_case("é",   "ê",   0, 2, 2 ; "shared_lead_byte_is_not_split")]
    #[test_case("xé",  "xè",  1, 3, 3 ; "shared_lead_byte_after_a_prefix")]
    #[test_case("é",   "ũ",   0, 2, 2 ; "shared_trail_byte_is_not_split")]
    #[test_case("fix the parser\nthen ship", "fix the lexer\nthen ship", 8, 12, 11 ; "one_word_of_many_lines")]
    fn a_whole_value_write_logs_only_what_differs(
        old: &str,
        new: &str,
        start: usize,
        old_end: usize,
        new_end: usize,
    ) {
        let mut buf = TextBuffer::new(old.into());
        let since = buf.version();
        buf.set_value(new);
        assert_eq!(lines(&buf), new.split('\n').collect::<Vec<_>>());
        assert_eq!(
            changes(&buf, since),
            [Change {
                start,
                old_end,
                new_end,
            }]
        );
    }

    #[test_case(|buf| buf.set_value("ab\ncd")                            ; "same_value")]
    #[test_case(|buf| { buf.set_cursor(0, 2); buf.kill_to_end_of_line() } ; "kill_at_end_of_line")]
    #[test_case(|buf| { buf.move_to_end(); buf.delete_char() }           ; "delete_at_end")]
    #[test_case(TextBuffer::remove_char                                  ; "backspace_at_start")]
    fn an_edit_that_changes_nothing_keeps_the_version(edit: fn(&mut TextBuffer)) {
        let mut buf = TextBuffer::new("ab\ncd".into());
        let before = buf.version();
        edit(&mut buf);
        assert_eq!(buf.value(), "ab\ncd");
        assert_eq!(buf.version(), before);
    }

    #[test]
    fn a_multi_line_paste_is_one_edit() {
        let mut buf = TextBuffer::new("ab".into());
        buf.set_cursor(0, 1);
        let since = buf.version();
        buf.insert_text("1\n2\n3");
        assert_eq!(
            changes(&buf, since),
            [Change {
                start: 1,
                old_end: 1,
                new_end: 6,
            }]
        );
    }

    #[test]
    fn the_log_forgets_its_oldest_changes() {
        let mut buf = TextBuffer::new(String::new());
        let start = buf.version();
        for _ in 0..=CHANGE_LOG_LEN {
            buf.push_char('a');
        }
        let now = buf.version();

        assert!(
            buf.changes_since(start).is_none(),
            "the first change was dropped"
        );
        assert_eq!(changes(&buf, start + 1).len(), CHANGE_LOG_LEN);
        assert_eq!(
            changes(&buf, now - 1),
            [Change {
                start: CHANGE_LOG_LEN,
                old_end: CHANGE_LOG_LEN,
                new_end: CHANGE_LOG_LEN + 1,
            }],
            "counting back from the end lands on the newest change"
        );
        assert!(changes(&buf, now).is_empty());
        assert!(
            buf.changes_since(now + 1).is_none(),
            "a version the buffer never had"
        );
    }

    fn type_keys(buf: &mut TextBuffer, text: &str) {
        for c in text.chars() {
            buf.handle_key(plain(KeyCode::Char(c)));
        }
    }

    fn undo_all(buf: &mut TextBuffer) -> Vec<String> {
        let mut seen = Vec::new();
        while buf.undo() {
            seen.push(buf.value().to_owned());
        }
        seen
    }

    #[test]
    fn typing_and_backspaces_undo_word_by_word() {
        let mut buf = TextBuffer::new(String::new());
        type_keys(&mut buf, "hello world");
        for _ in 0.." world".len() {
            buf.handle_key(plain(KeyCode::Backspace));
        }
        assert_eq!(undo_all(&mut buf), ["hello ", "hello world", "hello ", ""]);
    }

    #[test]
    fn only_typing_elsewhere_breaks_a_run() {
        let mut buf = TextBuffer::new("abc".into());
        buf.move_to_end();
        type_keys(&mut buf, "d");
        buf.handle_key(plain(KeyCode::Left));
        buf.handle_key(plain(KeyCode::Right));
        type_keys(&mut buf, "e");
        buf.set_cursor(0, 0);
        type_keys(&mut buf, "x");

        assert!(buf.undo());
        assert_eq!((buf.value(), buf.x()), ("abcde", 0));
        assert!(buf.undo());
        assert_eq!((buf.value(), buf.x()), ("abc", 3));
    }

    #[test_case(|buf| { buf.handle_key(ctrl(KeyCode::Char('w'))); buf.handle_key(ctrl(KeyCode::Char('w'))); }, &["word"] ; "a_word_kill_with_nothing_left")]
    #[test_case(|buf| { buf.replace_byte_range(0, 4, "word", None).unwrap(); }, &[] ; "rewriting_the_same_text")]
    #[test_case(|buf| { type_keys(buf, "s"); buf.replace_byte_range(0, 5, "words", None).unwrap(); }, &["word"] ; "rewriting_the_same_text_after_typing")]
    fn an_action_that_changes_nothing_leaves_no_step(act: fn(&mut TextBuffer), undone: &[&str]) {
        let mut buf = TextBuffer::new("word".into());
        buf.move_to_end();
        act(&mut buf);
        assert_eq!(undo_all(&mut buf), undone);
    }

    #[test]
    fn a_step_keeps_only_the_text_its_edit_removed() {
        let mut buf = TextBuffer::new("pick a file".into());
        buf.replace_byte_range(7, 11, "fn main() {}\n", None)
            .unwrap();
        let kept: Vec<&str> = buf
            .undo
            .steps
            .iter()
            .flat_map(|step| &step.inverses)
            .map(|inverse| inverse.text.as_str())
            .collect();
        assert_eq!(kept, ["file"]);
        assert_eq!(undo_all(&mut buf), ["pick a file"]);
    }

    #[test]
    fn an_edit_outside_any_action_is_its_own_step() {
        let mut buf = TextBuffer::new(String::new());
        buf.push_char('a');
        type_keys(&mut buf, "b");
        buf.push_char('c');
        assert_eq!(undo_all(&mut buf), ["ab", "a", ""]);
    }

    #[test]
    fn undo_is_logged_as_an_edit() {
        let mut buf = TextBuffer::new("keep".into());
        buf.move_to_end();
        buf.insert_text(" this");
        let since = buf.version();
        buf.undo();
        assert_eq!(
            changes(&buf, since),
            [Change {
                start: 4,
                old_end: 9,
                new_end: 4,
            }]
        );
    }

    #[test_case(|buf| buf.clear()                                          ; "after_an_action")]
    #[test_case(|buf| buf.undoable(|buf| { buf.push_char('!'); buf.clear() }) ; "inside_an_action")]
    fn a_whole_value_write_forgets_the_history(write: fn(&mut TextBuffer)) {
        let mut buf = TextBuffer::new(String::new());
        type_keys(&mut buf, "sent");
        write(&mut buf);
        assert!(!buf.undo());
    }

    #[test]
    fn the_history_keeps_only_the_newest_steps() {
        let mut buf = TextBuffer::new(String::new());
        for _ in 0..MAX_UNDO + 1 {
            buf.insert_text("a");
        }
        assert_eq!(undo_all(&mut buf).len(), MAX_UNDO);
        assert_eq!(buf.value(), "a");
    }
}
