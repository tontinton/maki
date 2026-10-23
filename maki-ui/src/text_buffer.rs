mod marks;
mod rules;
mod syntax;
#[cfg(test)]
pub(crate) mod test_support;

use std::cell::Cell;
use std::collections::VecDeque;
use std::mem;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_lua::{InputMarks, PluginReach, SyntaxInput, TextEdit};

use crate::components::input::sent_text;
use crate::highlight::TAB_SPACES;

#[cfg(any(test, feature = "bench"))]
pub use self::marks::Decoration;
#[cfg(test)]
pub use self::marks::Reveal;
pub use self::marks::{Concealed, Mark, Marks, Placed, VirtTextPos};
use self::rules::RuleMarks;
pub use self::rules::RuleSet;
use self::syntax::{SyntaxMarks, edit_since};

/// How many versions back a reader can fall and still catch up through the
/// log. A reader that catches up every frame never falls far behind, and one
/// that falls off the end only pays for a rebuild from the whole value. A
/// plugin plans its edit a few keystrokes back at most, and a whole paste is
/// one version, so this leaves it plenty of room too.
const CHANGE_LOG_LEN: u64 = 256;

/// A step keeps the text its edit removed, so a long session of big deletes
/// would pile up forever without a cap. Nobody presses undo 64 times in a row.
const MAX_UNDO: usize = 64;

const NOTHING_TO_APPLY_ERR: &str = "nothing to apply: no edits, cursor or marks";
const PAST_END_ERR: &str = "is past the end of the input";
const INSIDE_CHAR_ERR: &str = "is inside a character";
const OVERLAPPING_EDITS_ERR: &str = "edits overlap or start at the same offset";
const VERSION_TOO_OLD_ERR: &str = "the input changed too many times to map the offsets";
pub(crate) const EDIT_CONFLICT_ERR: &str =
    "the input changed at or next to an edited range since the edit was planned";
pub(crate) const INPUT_REPLACED_ERR: &str =
    "the input was replaced (history, draft, $EDITOR or submit) since the offsets were read";
pub(crate) const CURSOR_CONFLICT_ERR: &str =
    "the cursor lands inside text changed since the edit was planned";
const UNCLEAN_TEXT_ERR: &str = "edit text has tabs, carriage returns or control characters \
                                that would shift its offsets, so clean the text first";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Typing, pasting and plugin writes. The text around the change did not
    /// move, so an offset outside it can simply be shifted past it.
    Edit,
    /// A whole prompt swapped in: history recall, a restored draft, `$EDITOR`,
    /// submit. Even when the diff is small or empty, an offset from before it
    /// points into a different prompt.
    Replace,
}

/// `start..old_end` of the value before it became `start..new_end`, and lines
/// `start_line..=old_end_line` became `start_line..=new_end_line`. Only
/// offsets, so a big paste costs the log as much as one keystroke.
///
/// A version that edits several ranges logs one change per range, all with
/// that version, in the order they were applied. Each one counts in the value
/// the ones before it left, so a position is mapped by walking them in order,
/// and the text between the ranges is never mistaken for changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    version: u64,
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
    pub start_line: usize,
    pub old_end_line: usize,
    pub new_end_line: usize,
    pub kind: ChangeKind,
}

impl Change {
    /// Where {pos} lands after this change, read as a delete followed by an
    /// insert: a position inside the removed text falls to where it started,
    /// and a position where text is inserted ends up after that text when
    /// {after}.
    pub fn map_pos(&self, pos: usize, after: bool) -> usize {
        if pos < self.start {
            pos
        } else if pos > self.old_end {
            pos - self.old_end + self.new_end
        } else if after {
            self.new_end
        } else {
            self.start
        }
    }

    /// Where {bytes} lands after this change, grown over text inserted at
    /// either end.
    pub fn map_range(&self, bytes: Range<usize>) -> Range<usize> {
        self.map_pos(bytes.start, false)..self.map_pos(bytes.end, true)
    }

    /// This change as it reads once {shrunk} bytes before it became {grown}
    /// bytes. Only its offsets move, since nothing maps a line through it.
    fn past(&self, grown: usize, shrunk: usize) -> Self {
        let at = |pos: usize| pos + grown - shrunk;
        Self {
            start: at(self.start),
            old_end: at(self.old_end),
            new_end: at(self.new_end),
            ..self.clone()
        }
    }
}

/// Which way the cursor leaves a range it may not stop inside.
#[derive(Clone, Copy)]
enum Toward {
    Start,
    End,
    Nearest,
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
    /// And who had written the text, so undoing past a clear brings a
    /// plugin's line back as the plugin's.
    writers: Writers,
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
    /// What undo took back, newest last, for redo. Undo and redo only move
    /// steps between the two, so together they never pass [`MAX_UNDO`].
    undone: Vec<UndoStep>,
    /// The step the action in progress records into.
    open: Option<UndoStep>,
}

impl UndoHistory {
    /// An action that changed nothing leaves no step, so every undo rewinds
    /// something the user can see. One that did change something forgets
    /// what redo could bring back, whose ranges point into the text from
    /// before it.
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
        self.undone.clear();
        if let Some(last) = self.steps.back_mut()
            && last.absorb(&step, text)
        {
            return;
        }
        self.keep(step);
    }

    fn keep(&mut self, step: UndoStep) {
        if self.steps.len() == MAX_UNDO {
            self.steps.pop_front();
        }
        self.steps.push_back(step);
    }
}

/// The plugins that changed a prompt's text, each held to the grants it had
/// in all of its changes. A user edit inside the text takes nobody out, so
/// retyping a character never makes a plugin's line the user's. Only a blank
/// text, a user edit over all of it or another prompt starts the set over.
#[derive(Clone, Default)]
pub struct Writers(Arc<[PluginReach]>);

impl Writers {
    /// One entry per plugin. A write with grants the entry lacks, as inside
    /// an action another plugin ran, keeps only what both hold.
    fn add(&mut self, writer: &PluginReach) {
        match self
            .0
            .iter()
            .position(|held| held.owner() == writer.owner())
        {
            Some(index) if writer.first_missing(&self.0[index]).is_none() => {}
            Some(index) => {
                let mut held = self.0.to_vec();
                held[index] = held[index].intersect(writer);
                self.0 = held.into();
            }
            None => self.0 = self.0.iter().cloned().chain([writer.clone()]).collect(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &PluginReach> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One prompt the input can hold: its text, the marks plugins placed on it
/// and the plugins that wrote it. They only make sense on that text, so a
/// prompt put away keeps them and brings them back when it returns.
#[derive(Default)]
pub struct Prompt {
    pub text: String,
    pub marks: Marks,
    pub writers: Writers,
}

pub struct TextBuffer {
    text: String,
    /// Byte offset where each line starts. An empty value is still one empty
    /// line, so this always holds at least the first 0 and
    /// `line_count() - 1` never underflows.
    line_starts: Vec<usize>,
    raw_x: usize,
    cursor_y: usize,
    /// [`Self::cursor_byte`] at `(cursor_y, raw_x)`, which every tick and
    /// frame asks for and which costs a scan of the line. A splice clears it.
    cursor_cache: Cell<Option<((usize, usize), usize)>>,
    version: u64,
    /// Oldest first, so `changes_since` can binary search for a version.
    changes: VecDeque<Change>,
    undo: UndoHistory,
    /// Mapped in [`Self::splice`], so they move with every edit. A prompt
    /// swapped in brings its own, see [`Self::swap_prompt`].
    marks: Marks,
    /// Never mapped like {marks}. [`Self::splice`] just forgets the lines it
    /// changed, and they are matched again from the text.
    rules: RuleMarks,
    /// Never mapped either: each lookup maps the worker's last result
    /// forward through {changes}.
    syntax: SyntaxMarks,
    /// Bumped when the marks change how wide a line is drawn in a way the
    /// change log cannot point at: a mark with inline virtual text or a
    /// conceal dropped far from the edit, another draft's marks swapped in,
    /// conceals turned on or off, new rules.
    decor_epoch: u64,
    /// Shows the text under every conceal, for checking what is really sent.
    raw: bool,
    /// A plugin's change adds to it, see [`Self::written_by`]. A prompt
    /// swapped in brings its own, and an undo step puts back the set it began
    /// with.
    writers: Writers,
}

/// Refuses an offset that splits a character. Rounding it would move an edit
/// a plugin asked for onto text it never read.
fn check_boundary(on_boundary: bool, what: &str, idx: usize) -> Result<(), String> {
    if on_boundary {
        return Ok(());
    }
    Err(format!("{what} {idx} {INSIDE_CHAR_ERR}"))
}

/// {edits} are sorted by start. Two ranges sharing an offset are fine, one
/// ending where the next starts, unless both start there: an insert next to
/// another edit at the same offset could go on either side of it.
fn check_ranges(edits: &[TextEdit]) -> Result<(), String> {
    if let Some(edit) = edits.iter().find(|edit| edit.start > edit.stop) {
        return Err(format!("start {} is past stop {}", edit.start, edit.stop));
    }
    match edits
        .windows(2)
        .find(|pair| pair[0].stop > pair[1].start || pair[0].start == pair[1].start)
    {
        Some([a, b]) => Err(format!(
            "{OVERLAPPING_EDITS_ERR}: {}..{} and {}..{}",
            a.start, a.stop, b.start, b.stop
        )),
        _ => Ok(()),
    }
}

/// An offset at an insertion lands after the inserted text, and one strictly
/// inside replaced text has nowhere to go, which answers `None`.
fn map_offset(changes: &[Change], mut at: usize) -> Option<usize> {
    for change in changes {
        if at >= change.old_end {
            at = at + change.new_end - change.old_end;
        } else if at > change.start {
            return None;
        }
    }
    Some(at)
}

/// Moves {marks} through {changes} the way the store moves its own, drops
/// the ones that would not have made it, and checks the rest fit a value
/// {len} bytes long whose char boundaries {on_boundary} answers. A reversed
/// range is refused first, since mapping would quietly drop it.
fn fit_marks(
    changes: &[Change],
    marks: &mut Vec<Mark>,
    len: usize,
    on_boundary: impl Fn(usize) -> bool,
) -> Result<(), String> {
    if let Some(mark) = marks.iter().find(|mark| mark.start > mark.end()) {
        return Err(format!("start {} is past stop {}", mark.start, mark.end()));
    }
    marks.retain_mut(|mark| changes.iter().all(|change| marks::remap_mark(mark, change)));
    for mark in marks.iter() {
        let stop = mark.end();
        if stop > len {
            return Err(format!("stop {stop} {PAST_END_ERR} ({len})"));
        }
        check_boundary(on_boundary(mark.start), "start", mark.start)?;
        check_boundary(on_boundary(stop), "stop", stop)?;
    }
    Ok(())
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
pub(crate) fn sanitize(text: &str) -> String {
    normalize_line_endings(text)
        .replace('\t', TAB_SPACES)
        .replace(|c: char| c.is_control() && c != '\n', "")
}

pub(crate) fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn hull(a: Range<usize>, b: Range<usize>) -> Range<usize> {
    a.start.min(b.start)..a.end.max(b.end)
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
        let line_starts: Vec<usize> = [0].into_iter().chain(line_starts_in(&text, 0)).collect();
        Self {
            rules: RuleMarks::new(Arc::default(), line_starts.len()),
            syntax: SyntaxMarks::new(Arc::default()),
            text,
            line_starts,
            raw_x: 0,
            cursor_y: 0,
            cursor_cache: Cell::default(),
            version: 0,
            changes: VecDeque::new(),
            undo: UndoHistory::default(),
            marks: Marks::default(),
            decor_epoch: 0,
            raw: false,
            writers: Writers::default(),
        }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Puts {prompt} in place of the one here and hands that one back, text
    /// and marks together. The text lands verbatim and the cursor goes to
    /// the start.
    ///
    /// It is always one new version, logged as a [`ChangeKind::Replace`],
    /// even when both prompts read the same. So an edit or marks planned on
    /// the prompt put away are refused instead of landing on the one that
    /// came in, and no mark is mapped across. Only the part of the text that
    /// differs is spliced, so the lines both prompts share keep their layout.
    ///
    /// The undo history goes with the old prompt. Walking back across a
    /// submit, a draft swap or a history recall would bring back a prompt the
    /// user already put away. A step that is still open goes too, because its
    /// ranges point into the old text.
    pub fn swap_prompt(&mut self, prompt: Prompt) -> Prompt {
        let old = Prompt {
            text: self.text.clone(),
            marks: mem::take(&mut self.marks),
            writers: mem::replace(&mut self.writers, prompt.writers),
        };
        let (start, old_end, new_end) = changed_span(&self.text, &prompt.text);
        self.splice(
            start..old_end,
            &prompt.text[start..new_end],
            ChangeKind::Replace,
        );
        self.commit();
        self.marks = prompt.marks;
        let shapes_rows = |marks: &Marks| marks.shaping(0..=usize::MAX).next().is_some();
        if shapes_rows(&old.marks) || shapes_rows(&self.marks) {
            self.decor_epoch += 1;
        }
        self.undo = UndoHistory::default();
        self.raw_x = 0;
        self.cursor_y = 0;
        old
    }

    /// [`Self::swap_prompt`] with a prompt no plugin marked yet, dropping the
    /// one put away.
    pub fn set_value(&mut self, value: &str) {
        self.swap_prompt(Prompt {
            text: value.to_owned(),
            ..Prompt::default()
        });
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
            writers: self.writers.clone(),
            group,
        });
        let out = action(self);
        // Gone when the action swapped the prompt, which drops it with the history.
        if let Some(step) = self.undo.open.take() {
            self.undo.push(step, &self.text);
        }
        out
    }

    /// `false` when there is nothing left to undo. The step comes back as one
    /// version, like the action that made it.
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
        let redo = self.revert(step);
        self.undo.undone.push(redo);
        true
    }

    /// Puts back what the last undo took, as one version. `false` when there
    /// is nothing to redo: no undo yet, or an edit made since.
    pub fn redo(&mut self) -> bool {
        let Some(step) = self.undo.undone.pop() else {
            return false;
        };
        let undo = self.revert(step);
        self.undo.keep(undo);
        true
    }

    /// Applies {step} and returns the step that reverts it in turn, which
    /// leaves the caret where it is now. Each inverse keeps the text it
    /// replaces, so undo and redo stay O(edit) like the edit itself.
    fn revert(&mut self, step: UndoStep) -> UndoStep {
        let cursor = self.cursor_byte();
        let writers = mem::replace(&mut self.writers, step.writers);
        let inverses = step
            .inverses
            .into_iter()
            .rev()
            .map(|Inverse { start, end, text }| {
                let removed = self.text[start..end].to_owned();
                self.splice(start..end, &text, ChangeKind::Edit);
                Inverse {
                    start,
                    end: start + text.len(),
                    text: removed,
                }
            })
            .collect();
        self.commit();
        self.place_cursor(step.cursor);
        self.settle_cursor();
        UndoStep {
            inverses,
            cursor,
            writers,
            group: None,
        }
    }

    /// Counts every change to the value, never a cursor move. `maki.ui.input`
    /// hands it to Lua so an edit planned against an older value fails instead
    /// of landing on text the user has typed since.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn writers(&self) -> &Writers {
        &self.writers
    }

    /// Adds {writer} when the text changed since version {since}, for a
    /// change made for a plugin rather than by the user.
    pub fn written_by(&mut self, writer: &PluginReach, since: u64) {
        if self.version != since {
            self.add_writer(writer);
        }
    }

    /// Adds {writer} unless the text is blank, whatever changed to get it
    /// here.
    pub fn add_writer(&mut self, writer: &PluginReach) {
        if !sent_text(&self.text).is_empty() {
            self.writers.add(writer);
        }
    }

    /// The changes that led from {version} to now, in the order they were
    /// applied. `None` when {version} is older than the log reaches back, or
    /// newer than the buffer.
    pub fn changes_since(
        &self,
        version: u64,
    ) -> Option<impl DoubleEndedIterator<Item = &Change> + Clone> {
        let oldest = self
            .changes
            .front()
            .map_or(self.version, |change| change.version - 1);
        if !(oldest..=self.version).contains(&version) {
            return None;
        }
        let skip = self
            .changes
            .partition_point(|change| change.version <= version);
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

    pub fn line_range(&self, y: usize) -> Range<usize> {
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

    /// Every change to the value goes through here, so the text, the line
    /// index and the log always agree. The change is logged under the next
    /// version, and [`Self::commit`] makes that version current, so all the
    /// splices of one transaction share it. An empty edit is dropped, so a
    /// no-op like `Ctrl+K` at the end of a line leaves the version alone. An
    /// empty replace is not, see [`Self::swap_prompt`].
    fn splice(&mut self, range: Range<usize>, text: &str, kind: ChangeKind) -> bool {
        if kind == ChangeKind::Edit && range.is_empty() && text.is_empty() {
            return false;
        }
        let Range {
            start,
            end: old_end,
        } = range;
        let new_end = start + text.len();
        self.text.replace_range(start..old_end, text);
        self.cursor_cache.set(None);

        // A line starting in `start + 1..=old_end` lost the newline before it.
        let start_line = self.line_of(start);
        let old_end_line = self.line_of(old_end);
        for line_start in &mut self.line_starts[old_end_line + 1..] {
            *line_start = *line_start - old_end + new_end;
        }
        self.line_starts.splice(
            start_line + 1..old_end_line + 1,
            line_starts_in(text, start),
        );

        let new_end_line = self.line_of(new_end);
        self.rules
            .splice(start_line..old_end_line + 1, new_end_line + 1 - start_line);
        let change = Change {
            version: self.version + 1,
            start,
            old_end,
            new_end,
            start_line,
            old_end_line,
            new_end_line,
            kind,
        };
        // A chip deleted whole sits on the lines the change already points
        // the layout at. Only a mark dropped lines away from them needs a
        // full recount, and that goes for the marks of the last parse too.
        let lines = start_line..=new_end_line;
        let syntax_dropped = self.syntax_dropped(&change);
        let dropped_away = self
            .marks
            .map(&change)
            .into_iter()
            .chain(syntax_dropped)
            .any(|bytes| {
                !lines.contains(&self.line_of(bytes.start))
                    || !lines.contains(&self.line_of(bytes.end))
            });
        if dropped_away {
            self.decor_epoch += 1;
        }
        self.changes.push_back(change);
        true
    }

    #[cfg(test)]
    pub fn marks(&self) -> &Marks {
        &self.marks
    }

    pub fn decor_epoch(&self) -> u64 {
        self.decor_epoch
    }

    /// Replaces what {plugin} has in namespace {ns}, with offsets into the
    /// value at {version}. Returns where marks that change how rows wrap
    /// came or went. Refused whole when one mark does not fit the text, like
    /// an edit is.
    ///
    /// Marks from an older version move through every change since, as if
    /// they had been placed back then, so a plugin racing the user's typing
    /// gets the same marks it would have had by being quick. Whatever that
    /// drops is dropped. Unlike an edit, nothing is refused for sitting next
    /// to the typing: that is where ghost text lives.
    pub fn set_marks(
        &mut self,
        version: u64,
        plugin: &str,
        ns: &str,
        mut marks: Vec<Mark>,
    ) -> Result<Vec<Range<usize>>, String> {
        let changes = self.rebase(version, &mut [])?;
        fit_marks(&changes, &mut marks, self.text.len(), |pos| {
            self.text.is_char_boundary(pos)
        })?;
        let moved = self.marks.set(plugin, ns, marks);
        self.settle_decor();
        Ok(moved)
    }

    /// Returns where marks that change how rows wrap went.
    pub fn clear_plugin_marks(&mut self, plugin: &str) -> Vec<Range<usize>> {
        self.marks.clear_plugin(plugin)
    }

    pub fn toggle_raw(&mut self) {
        self.raw = !self.raw;
        self.decor_epoch += 1;
        self.settle_decor();
    }

    pub fn set_rules(&mut self, rules: Arc<RuleSet>) {
        self.syntax.set_rules(Arc::clone(rules.syntax()));
        self.rules = RuleMarks::new(rules, self.line_count());
        self.decor_epoch += 1;
        self.settle_decor();
    }

    /// Takes in what the worker finished, and hands it the text when it lags
    /// behind. Returns where the tree-sitter marks changed in the text now,
    /// or `None` when no result came in. A result that cannot say where
    /// recounts every line, and so does one replacing a result the log no
    /// longer reaches back to: none of that one was shown, so even lines
    /// where both results agree changed on screen.
    pub fn poll_syntax(&mut self) -> Option<Vec<Range<usize>>> {
        let was_shown = self.syntax_changes().is_some();
        let changed = self.syntax.receive();
        if self.syntax.wants_parse(self.version) {
            let edit = self.syntax_changes().and_then(edit_since);
            let input = match edit {
                Some(edit) => SyntaxInput::Edit {
                    inserted: self.text[edit.start..edit.new_end].to_owned(),
                    start_row: self.line_of(edit.start),
                    edit,
                },
                None => SyntaxInput::Whole(self.text.clone()),
            };
            self.syntax.parse(input, self.version);
        }
        let changed = changed?;
        self.settle_decor();
        let now = changed.filter(|_| was_shown).and_then(|ranges| {
            let changes = self.syntax_changes()?;
            let moved = ranges.into_iter().map(|bytes| {
                changes
                    .clone()
                    .fold(bytes, |bytes, change| change.map_range(bytes))
            });
            Some(moved.collect())
        });
        Some(now.unwrap_or_else(|| {
            self.decor_epoch += 1;
            Vec::new()
        }))
    }

    /// Whether the worker has parsed the text as it is now, which a bench
    /// waits for to measure edits against a settled tree.
    #[cfg(feature = "bench")]
    pub fn syntax_is_current(&self) -> bool {
        self.syntax.is_current(self.version)
    }

    /// Where the tree-sitter marks that shape rows and that {change}, not
    /// logged yet, drops would have landed.
    fn syntax_dropped(&self, change: &Change) -> Vec<Range<usize>> {
        if !self.rules.shapes_rows() {
            return Vec::new();
        }
        self.syntax
            .touching(change.start..=change.old_end, self.syntax_changes())
            .filter(|mark| {
                mark.decoration.shapes_rows()
                    && marks::remap(mark.bytes.clone(), mark.decoration, change).is_none()
            })
            .map(|mark| change.map_range(mark.bytes))
            .collect()
    }

    fn derived(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Placed<'_>> {
        let (from, to) = (*bytes.start(), *bytes.end());
        let changes = self.syntax_changes();
        (self.line_of(from)..=self.line_of(to))
            .flat_map(move |y| self.rules.line(y, &self.text, self.line_range(y)))
            .filter(move |mark| mark.bytes.start <= to && mark.bytes.end >= from)
            .chain(self.syntax.touching(bytes, changes))
    }

    /// Placed and rule marks touching {bytes}, ends included, unsorted.
    fn touching(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Placed<'_>> {
        self.marks
            .touching(bytes.clone())
            .chain(self.derived(bytes))
    }

    /// The layout asks this for every line it counts, so when no rule can
    /// change how rows wrap, it skips matching altogether.
    fn shaping(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Placed<'_>> {
        let derived = self
            .rules
            .shapes_rows()
            .then(|| self.derived(bytes.clone()));
        self.marks.shaping(bytes).chain(
            derived
                .into_iter()
                .flatten()
                .filter(|mark| mark.decoration.shapes_rows()),
        )
    }

    /// The marks touching {bytes}, ends included, in the order they compose.
    pub fn overlapping(&self, bytes: RangeInclusive<usize>) -> Vec<Placed<'_>> {
        marks::composed(self.touching(bytes).collect())
    }

    pub fn inline_virts(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Placed<'_>> {
        self.shaping(bytes)
            .filter(|mark| mark.decoration.is_inline_virt())
    }

    /// The concealed ranges touching {bytes} as they are drawn now, sorted
    /// and disjoint. None while the raw text is shown, and none the cursor
    /// reveals.
    pub fn concealed(&self, bytes: RangeInclusive<usize>) -> Vec<Concealed<'_>> {
        if self.raw {
            return Vec::new();
        }
        marks::concealed(self.shaping(bytes), || self.cursor_byte())
    }

    /// The conceals the cursor shows the text of. Moving the cursor in or
    /// out of one changes how its lines wrap.
    pub fn revealed(&self) -> Vec<Range<usize>> {
        if self.raw {
            return Vec::new();
        }
        let cursor = self.cursor_byte();
        marks::revealed(self.shaping(cursor..=cursor), cursor)
    }

    fn hidden_at(&self, at: usize) -> Option<Range<usize>> {
        self.concealed(at..=at)
            .into_iter()
            .map(|concealed| concealed.bytes)
            .find(|bytes| bytes.contains(&at))
    }

    /// The lines drawn as one with line {y}. A concealed range over a line
    /// break hides it, which joins the lines on both sides, so a conceal
    /// spanning lines takes them all onto the line where it starts.
    pub fn display_lines(&self, y: usize) -> Range<usize> {
        let mut first = y;
        while first > 0
            && let Some(hidden) = self.hidden_at(self.line_starts[first] - 1)
        {
            first = self.line_of(hidden.start);
        }
        let mut last = y;
        while let Some(hidden) = self.hidden_at(self.line_range(last).end) {
            last = self.line_of(hidden.end);
        }
        first..last + 1
    }

    /// Where lines {lines} sit in the value, line breaks between them
    /// included.
    pub fn lines_bytes(&self, lines: Range<usize>) -> Range<usize> {
        self.line_starts[lines.start]..self.line_range(lines.end - 1).end
    }

    fn atomic(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Range<usize>> + '_ {
        self.touching(bytes)
            .filter(|mark| mark.decoration.atomic)
            .map(|mark| mark.bytes)
    }

    /// The range around byte {at} the cursor may not stop inside: an atomic
    /// range, or a concealed one, which shows nothing to stand on.
    fn blocked_at(&self, at: usize) -> Option<Range<usize>> {
        self.atomic(at..=at)
            .chain(self.hidden_at(at))
            .filter(|bytes| bytes.start < at && at < bytes.end)
            .reduce(hull)
    }

    /// Moves the cursor out of a range it may not stop inside. The way out
    /// is picked once, so ranges that overlap cannot bounce it back and forth.
    fn snap_cursor(&mut self, toward: Toward) {
        let mut at = self.cursor_byte();
        let Some(blocked) = self.blocked_at(at) else {
            return;
        };
        let forward = match toward {
            Toward::Start => false,
            Toward::End => true,
            Toward::Nearest => at - blocked.start > blocked.end - at,
        };
        let mut next = Some(blocked);
        while let Some(blocked) = next {
            at = if forward { blocked.end } else { blocked.start };
            next = self.blocked_at(at);
        }
        self.place_cursor(at);
    }

    /// Keeps the cursor out of every range it may not stop inside. A motion
    /// snaps the way it was going, and a key's edit as [`Self::edit`] says.
    /// An undo and a plugin edit that puts the cursor down end here.
    /// Otherwise the caret would be drawn at the edge of a conceal while the
    /// keys act on bytes inside it that the user cannot see.
    fn settle_cursor(&mut self) {
        self.snap_cursor(Toward::Nearest);
    }

    /// [`Self::settle_cursor`] after a change of what is drawn: new marks,
    /// rules or tree-sitter results, the raw text shown or hidden. The
    /// cursor only moves on past the range, so text typed next lands after
    /// what was typed, and a mark placed over the caret cannot send it back
    /// to the start of the input.
    fn settle_decor(&mut self) {
        self.snap_cursor(Toward::End);
    }

    /// Grows {range} over every atomic range it cuts into, so a delete takes
    /// a chip whole instead of leaving half of it behind.
    fn widen(&self, mut range: Range<usize>) -> Range<usize> {
        if range.is_empty() {
            return range;
        }
        loop {
            let grown = self
                .atomic(range.start..=range.end)
                .filter(|bytes| bytes.start < range.end && bytes.end > range.start)
                .fold(range.clone(), hull);
            if grown == range {
                return range;
            }
            range = grown;
        }
    }

    fn commit(&mut self) {
        let pending = self
            .changes
            .back()
            .is_some_and(|change| change.version > self.version);
        if !pending {
            return;
        }
        self.version += 1;
        if sent_text(&self.text).is_empty() {
            self.writers = Writers::default();
        }
        let syntax_shown = self.syntax_shapes_rows();
        while self
            .changes
            .front()
            .is_some_and(|change| change.version + CHANGE_LOG_LEN <= self.version)
        {
            self.changes.pop_front();
        }
        if syntax_shown && !self.syntax_shapes_rows() {
            self.decor_epoch += 1;
        }
    }

    /// Whether some rule can shape rows, which the rule set knows, and the log
    /// still reaches back to the last parse, so its marks are on screen. Once
    /// it no longer does, none of them can be moved to the text, so they all
    /// go at once, lines away from any change.
    fn syntax_shapes_rows(&self) -> bool {
        self.rules.shapes_rows() && self.syntax_changes().is_some()
    }

    /// What led from the text of the last parse to now, which its marks need
    /// to be shown at all.
    fn syntax_changes(&self) -> Option<impl DoubleEndedIterator<Item = &Change> + Clone> {
        self.changes_since(self.syntax.shown_version()?)
    }

    /// The edits the keys make. One that cuts into an atomic range takes all
    /// of it, which a plugin edit never does: a plugin asks for exact bytes.
    ///
    /// A caret that lands inside a range after an insert leaves past it, like
    /// the `$` that makes `$s:go` a chip, so typed text keeps its order even
    /// when a chip grows to take some of it in. After a delete it goes to the
    /// nearer edge, so a Backspace that joins two words into one chip does
    /// not jump past the chip.
    fn edit(&mut self, range: Range<usize>, text: &str, caret: usize) {
        let whole = self.widen(range.clone());
        let caret = if whole == range {
            caret
        } else {
            whole.start + text.len()
        };
        let toward = if text.is_empty() {
            Toward::Nearest
        } else {
            Toward::End
        };
        let replaces_all = whole == (0..self.text.len());
        self.undoable(|buf| {
            // No byte a plugin wrote is left, so the text is the user's now.
            // Done inside the step, so undo brings the plugin's text back as
            // the plugin's.
            if replaces_all {
                buf.writers = Writers::default();
            }
            buf.edit_all([(whole, text)], caret);
        });
        self.snap_cursor(toward);
    }

    /// Replaces sorted, disjoint ranges of the current value as one version,
    /// so a reader catching up never sees the value half edited.
    ///
    /// An edit made outside any action still becomes a step of its own. Left
    /// out, it would move the text under every older step and their ranges
    /// would point at the wrong place.
    fn edit_all<'a>(
        &mut self,
        edits: impl IntoIterator<Item = (Range<usize>, &'a str), IntoIter: DoubleEndedIterator>,
        caret: usize,
    ) {
        self.undoable(|buf| {
            // Back to front, so no range moves before its turn comes.
            for (range, text) in edits.into_iter().rev() {
                let start = range.start;
                let removed = buf.text[range.clone()].to_owned();
                if buf.splice(range, text, ChangeKind::Edit)
                    && let Some(step) = &mut buf.undo.open
                {
                    step.inverses.push(Inverse {
                        start,
                        end: start + text.len(),
                        text: removed,
                    });
                }
            }
            buf.commit();
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
        let end = self.lines_bytes(self.display_lines(self.cursor_y)).end;
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
        } else {
            self.raw_x = self.find_prev_word_boundary(x);
        }
        self.snap_cursor(Toward::Start);
    }

    pub fn move_word_right(&mut self) {
        let x = self.x();
        if x == self.current_line_len() {
            self.wrap_to_next_line();
        } else {
            self.raw_x = self.find_next_word_boundary(x);
        }
        self.snap_cursor(Toward::End);
    }

    pub fn move_left(&mut self) {
        let x = self.x();
        if x > 0 {
            self.raw_x = x - 1;
        } else {
            self.wrap_to_prev_line();
        }
        self.snap_cursor(Toward::Start);
    }

    pub fn move_right(&mut self) {
        let x = self.x();
        if x < self.current_line_len() {
            self.raw_x = x + 1;
        } else {
            self.wrap_to_next_line();
        }
        self.snap_cursor(Toward::End);
    }

    /// Lines joined by a conceal are one line on screen, so up goes to the
    /// first line of the one drawn above.
    pub fn move_up(&mut self) {
        let first = self.display_lines(self.cursor_y).start;
        if first > 0 {
            self.cursor_y = self.display_lines(first - 1).start;
            self.snap_cursor(Toward::Nearest);
        }
    }

    pub fn move_down(&mut self) {
        let end = self.display_lines(self.cursor_y).end;
        if end < self.line_count() {
            self.cursor_y = end;
            self.snap_cursor(Toward::Nearest);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_y = self.display_lines(self.cursor_y).start;
        self.raw_x = 0;
        self.snap_cursor(Toward::Start);
    }

    pub fn move_end(&mut self) {
        self.cursor_y = self.display_lines(self.cursor_y).end - 1;
        self.raw_x = self.current_line_len();
        self.snap_cursor(Toward::End);
    }

    pub fn clear(&mut self) {
        self.set_value("");
    }

    /// The cursor as a byte offset into [`Self::value`]. Bytes, not
    /// characters, because Lua string functions index bytes, so a plugin can
    /// slice the value it read with the offsets `maki.ui.input` hands it.
    pub fn cursor_byte(&self) -> usize {
        let at = (self.cursor_y, self.raw_x);
        if let Some((cached_at, byte)) = self.cursor_cache.get()
            && cached_at == at
        {
            return byte;
        }
        let byte = self.value_byte_at_column(self.x());
        self.cursor_cache.set(Some((at, byte)));
        byte
    }

    /// Applies {edits}, all planned against the value at {version}, as one
    /// version and one undo step, and returns where the cursor landed: at
    /// {cursor} if given, an offset into the value the edits produce, else at
    /// the end of the text of the edit with the highest start. A cursor past
    /// the end is clamped to it, and one inside a range it may not stop in
    /// moves to the nearer edge. Marks alone only move it on past the range,
    /// like any change of what is drawn, see [`Self::settle_decor`].
    ///
    /// Errors instead of clamping a range: these come from async plugin
    /// handlers, so a range that no longer fits means the buffer moved under
    /// the caller and the edit would land somewhere it was never meant to.
    /// Everything is checked before the first byte changes, so a refused
    /// transaction leaves nothing half applied.
    ///
    /// The text is sanitized the way typed and pasted text is, so the default
    /// cursor counts what was really inserted. With {cursor} or {marks}
    /// given, text sanitizing would change is refused instead: they count
    /// bytes of the text as sent, which would no longer be there.
    ///
    /// {marks}, when given, replace what the plugin named first has in their
    /// namespace. They count in the same value as {cursor} and land in the
    /// same change as the text, or refuse it with them. Where marks that
    /// change how rows wrap came or went is returned next to the cursor.
    ///
    /// No {edits} with a {cursor} or {marks} leaves the text alone, which
    /// makes neither a version nor an undo step. The cursor then stays where
    /// it is unless {cursor} moves it or {marks} hide it.
    pub fn apply_edits(
        &mut self,
        version: u64,
        mut edits: Vec<TextEdit>,
        cursor: Option<usize>,
        mut marks: Option<(&str, InputMarks)>,
    ) -> Result<(usize, Vec<Range<usize>>), String> {
        if edits.is_empty() && cursor.is_none() && marks.is_none() {
            return Err(NOTHING_TO_APPLY_ERR.into());
        }
        edits.sort_by_key(|edit| edit.start);
        check_ranges(&edits)?;
        let counts_offsets = cursor.is_some() || marks.is_some();
        for edit in &mut edits {
            let clean = sanitize(&edit.text);
            if counts_offsets && clean != edit.text {
                return Err(UNCLEAN_TEXT_ERR.into());
            }
            edit.text = clean;
        }
        let changes = self.rebase(version, &mut edits)?;
        let cursor = cursor
            .map(|caret| {
                map_offset(&changes, caret).ok_or_else(|| {
                    format!(
                        "{CURSOR_CONFLICT_ERR} (planned on version {version}, now {})",
                        self.version
                    )
                })
            })
            .transpose()?;

        let len = self.text.len();
        for &TextEdit { start, stop, .. } in &edits {
            if stop > len {
                return Err(format!("stop {stop} {PAST_END_ERR} ({len})"));
            }
            check_boundary(self.text.is_char_boundary(start), "start", start)?;
            check_boundary(self.text.is_char_boundary(stop), "stop", stop)?;
        }

        let grown: usize = edits.iter().map(|edit| edit.text.len()).sum();
        let shrunk: usize = edits.iter().map(|edit| edit.stop - edit.start).sum();
        let new_len = len + grown - shrunk;
        let caret = match (cursor, edits.last()) {
            (Some(at), _) => at,
            (None, Some(last)) => last.stop + grown - shrunk,
            (None, None) => self.cursor_byte(),
        }
        .min(new_len);
        check_boundary(self.is_boundary_after(&edits, caret), "cursor", caret)?;
        if let Some((_, pushed)) = &mut marks {
            fit_marks(&changes, &mut pushed.marks, new_len, |pos| {
                self.is_boundary_after(&edits, pos)
            })?;
        }
        let places_caret = cursor.is_some() || !edits.is_empty();
        self.edit_all(
            edits
                .iter()
                .map(|edit| (edit.start..edit.stop, edit.text.as_str())),
            caret,
        );
        let moved = marks.map_or_else(Vec::new, |(plugin, pushed)| {
            self.marks.set(plugin, &pushed.ns, pushed.marks)
        });
        if places_caret {
            self.settle_cursor();
        } else {
            self.settle_decor();
        }
        Ok((self.cursor_byte(), moved))
    }

    /// Moves {edits}, planned against {version}, past every change made
    /// since. Returns those changes as they read in the value the edits
    /// would have made of the old one, each shifted past the edits before
    /// it. That is the value a plugin counts its cursor and marks in, so
    /// they are mapped forward through these.
    ///
    /// A change that overlaps or touches a range refuses the lot: the user
    /// typed right where the edit lands, and there is no telling whether that
    /// text belongs before it, after it or under it. A whole-value replace
    /// refuses it too, because the offsets point into another prompt.
    fn rebase(&self, version: u64, edits: &mut [TextEdit]) -> Result<Vec<Change>, String> {
        let changes = self.changes_since(version).ok_or_else(|| {
            format!(
                "{VERSION_TOO_OLD_ERR} (planned on version {version}, now {})",
                self.version
            )
        })?;
        changes
            .map(|change| {
                if change.kind == ChangeKind::Replace {
                    return Err(INPUT_REPLACED_ERR.into());
                }
                let (mut grown, mut shrunk) = (0, 0);
                for edit in edits.iter_mut() {
                    if edit.stop < change.start {
                        grown += edit.text.len();
                        shrunk += edit.stop - edit.start;
                    } else if edit.start > change.old_end {
                        edit.start = edit.start + change.new_end - change.old_end;
                        edit.stop = edit.stop + change.new_end - change.old_end;
                    } else {
                        return Err(format!(
                            "{EDIT_CONFLICT_ERR} (planned on version {version}, now {})",
                            self.version
                        ));
                    }
                }
                Ok(change.past(grown, shrunk))
            })
            .collect()
    }

    /// Whether {caret}, an offset into the value {edits} would make of this
    /// one, falls on a character boundary there.
    fn is_boundary_after(&self, edits: &[TextEdit], caret: usize) -> bool {
        let (mut grown, mut shrunk) = (0, 0);
        for edit in edits {
            let start = edit.start + grown - shrunk;
            if caret < start {
                break;
            }
            if caret <= start + edit.text.len() {
                return edit.text.is_char_boundary(caret - start);
            }
            grown += edit.text.len();
            shrunk += edit.stop - edit.start;
        }
        self.text.is_char_boundary(caret + shrunk - grown)
    }

    #[cfg(test)]
    pub fn replace_byte_range(
        &mut self,
        start: usize,
        stop: usize,
        text: &str,
        cursor: Option<usize>,
    ) -> Result<(), String> {
        let edit = TextEdit {
            start,
            stop,
            text: text.into(),
        };
        self.apply_edits(self.version, vec![edit], cursor, None)
            .map(drop)
    }

    /// Puts the cursor on line {y}, column {x}, or at the nearer edge of a
    /// range it may not stop inside.
    pub fn set_cursor(&mut self, y: usize, x: usize) {
        self.cursor_y = y.min(self.line_count() - 1);
        self.raw_x = x.min(self.current_line_len());
        self.snap_cursor(Toward::Nearest);
    }

    pub fn move_to_end(&mut self) {
        self.cursor_y = self.line_count() - 1;
        self.raw_x = self.current_line_len();
    }

    pub fn kill_to_start_of_line(&mut self) {
        let from = self.line_starts[self.display_lines(self.cursor_y).start];
        let at = self.cursor_byte();
        self.edit(from..at, "", from);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditResult {
        if self.handle_motion_key(key) == EditResult::Moved {
            return EditResult::Moved;
        }
        let (ctrl, alt, sup) = modifiers(key);
        let group = match key.code {
            _ if ctrl || alt || sup => None,
            KeyCode::Char(_) => Some(EditGroup::Typing),
            KeyCode::Backspace => Some(EditGroup::Deleting),
            _ => None,
        };
        self.undo_step(group, |buf| buf.apply_edit_key(key.code, ctrl, alt, sup))
    }

    /// Only the keys of [`Self::handle_key`] that move the cursor. Normal mode
    /// uses this, so the text never changes under a vim plugin.
    pub fn handle_motion_key(&mut self, key: KeyEvent) -> EditResult {
        let (ctrl, alt, sup) = modifiers(key);
        let Some(motion) = motion(key.code, ctrl, alt, sup) else {
            return EditResult::Ignored;
        };
        motion(self);
        EditResult::Moved
    }

    fn apply_edit_key(&mut self, code: KeyCode, ctrl: bool, alt: bool, sup: bool) -> EditResult {
        match code {
            KeyCode::Backspace | KeyCode::Char('w') if ctrl => self.remove_word_before_cursor(),
            KeyCode::Delete if ctrl => self.delete_word_after_cursor(),
            KeyCode::Char('k') if ctrl => self.kill_to_end_of_line(),
            KeyCode::Backspace if alt => self.remove_word_before_cursor(),
            KeyCode::Delete | KeyCode::Char('d') if alt => self.delete_word_after_cursor(),
            _ if ctrl || alt => return EditResult::Ignored,
            KeyCode::Backspace if sup => self.kill_to_start_of_line(),
            // Cmd+Z only arrives on terminals that speak the kitty keyboard
            // protocol. Most never send it, which is why the app binds Alt+Z.
            KeyCode::Char('z') if sup => {
                return if self.undo() {
                    EditResult::Changed
                } else {
                    EditResult::Ignored
                };
            }
            _ if sup => return EditResult::Ignored,
            KeyCode::Char(c) => self.push_char(c),
            KeyCode::Backspace => self.remove_char(),
            KeyCode::Delete => self.delete_char(),
            _ => return EditResult::Ignored,
        }
        EditResult::Changed
    }
}

/// `(ctrl, alt, super)`, with Ctrl and Alt held together counting as neither.
fn modifiers(key: KeyEvent) -> (bool, bool, bool) {
    let m = key.modifiers;
    (
        m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT),
        m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL),
        m.contains(KeyModifiers::SUPER),
    )
}

/// A held modifier claims its key even when it has no motion for it, so
/// `Ctrl+Home` does nothing instead of acting like a plain `Home`.
fn motion(code: KeyCode, ctrl: bool, alt: bool, sup: bool) -> Option<fn(&mut TextBuffer)> {
    Some(match code {
        KeyCode::Left if ctrl || alt => TextBuffer::move_word_left,
        KeyCode::Right if ctrl || alt => TextBuffer::move_word_right,
        KeyCode::Char('a') if ctrl => TextBuffer::move_home,
        KeyCode::Char('e') if ctrl => TextBuffer::move_end,
        KeyCode::Char('b') if alt => TextBuffer::move_word_left,
        KeyCode::Char('f') if alt => TextBuffer::move_word_right,
        _ if ctrl || alt => return None,
        KeyCode::Left if sup => TextBuffer::move_home,
        KeyCode::Right if sup => TextBuffer::move_end,
        _ if sup => return None,
        KeyCode::Left => TextBuffer::move_left,
        KeyCode::Right => TextBuffer::move_right,
        KeyCode::Home => TextBuffer::move_home,
        KeyCode::End => TextBuffer::move_end,
        KeyCode::Up => TextBuffer::move_up,
        KeyCode::Down => TextBuffer::move_down,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::test_support::{mark_ranges, text_edit};
    use super::{
        CHANGE_LOG_LEN, CURSOR_CONFLICT_ERR, ChangeKind, EDIT_CONFLICT_ERR, EditResult,
        INPUT_REPLACED_ERR, INSIDE_CHAR_ERR, InputMarks, MAX_UNDO, NOTHING_TO_APPLY_ERR,
        OVERLAPPING_EDITS_ERR, PAST_END_ERR, Prompt, TAB_SPACES, TextBuffer, TextEdit,
        UNCLEAN_TEXT_ERR, VERSION_TOO_OLD_ERR,
    };
    use super::{Decoration, Mark, Reveal, RuleSet};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use maki_lua::{InputRule, PatternRule};
    use std::ops::Range;
    use std::sync::Arc;
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

    fn changes(buf: &TextBuffer, since: u64) -> Vec<(usize, usize, usize, ChangeKind)> {
        buf.changes_since(since)
            .expect("the log still reaches that far back")
            .map(|change| (change.start, change.old_end, change.new_end, change.kind))
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

    const PLANNED_ON: &str = "hello world";

    /// The cursor counts in the value the plugin meant to make, so it has to
    /// follow the user's change just like the ranges do. In that value the
    /// deleted `w` sits after both `**`, past cursor 7, even though its old
    /// offset was 6.
    #[test_case(vec![text_edit(0, 5, "hi")], None, |b| { b.move_to_end(); b.push_char('!') }, "hi world!", 2 ; "typed_after")]
    #[test_case(vec![text_edit(6, 6, "**"), text_edit(11, 11, "**")], Some(13), |b| { b.set_cursor(0, 0); b.insert_text("oh ") }, "oh hello **world**", 16 ; "typed_before_moves_the_cursor_too")]
    #[test_case(vec![text_edit(0, 0, "**"), text_edit(5, 5, "**")], Some(7), |b| { b.set_cursor(0, 7); b.remove_char() }, "**hello** orld", 7 ; "deleted_after_the_cursor")]
    #[test_case(vec![text_edit(5, 6, "_")], None, |b| { b.apply_edits(b.version(), vec![text_edit(0, 0, "**"), text_edit(11, 11, "**")], None, None).unwrap(); }, "**hello_world**", 8 ; "between_the_ranges_of_one_transaction")]
    fn a_stale_edit_moves_past_changes_that_do_not_touch_it(
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
        user: fn(&mut TextBuffer),
        expected: &str,
        expected_cursor: usize,
    ) {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        user(&mut buf);

        assert_eq!(
            buf.apply_edits(planned, edits, cursor, None)
                .map(|(caret, _)| caret),
            Ok(expected_cursor)
        );
        assert_eq!(buf.value(), expected);
        assert_eq!(buf.cursor_byte(), expected_cursor);
    }

    #[test_case(vec![text_edit(0, 5, "hi")], None, |b| { b.set_cursor(0, 5); b.push_char('!') }, EDIT_CONFLICT_ERR ; "typed_at_the_end")]
    #[test_case(vec![text_edit(6, 11, "there")], None, |b| { b.set_cursor(0, 6); b.remove_char() }, EDIT_CONFLICT_ERR ; "deleted_up_to_the_start")]
    #[test_case(vec![text_edit(0, 11, "")], None, |b| { b.set_cursor(0, 3); b.push_char('x') }, EDIT_CONFLICT_ERR ; "typed_inside")]
    #[test_case(vec![text_edit(5, 5, ",")], None, |b| { b.set_cursor(0, 5); b.push_char(';') }, EDIT_CONFLICT_ERR ; "inserted_at_the_same_offset")]
    #[test_case(vec![text_edit(0, 0, "> ")], Some(9), |b| b.replace_byte_range(6, 9, "", None).unwrap(), CURSOR_CONFLICT_ERR ; "cursor_inside_changed_text")]
    #[test_case(vec![text_edit(0, 0, "> ")], None, |b| b.set_value("hello world, again"), INPUT_REPLACED_ERR ; "whole_value_replaced")]
    #[test_case(vec![text_edit(0, 0, "> ")], None, |b| { b.move_to_end(); for _ in 0..=CHANGE_LOG_LEN { b.push_char('!') } }, VERSION_TOO_OLD_ERR ; "planned_before_the_log_reaches")]
    fn a_stale_edit_it_cannot_map_is_refused(
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
        user: fn(&mut TextBuffer),
        err: &str,
    ) {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        user(&mut buf);
        let (value, version) = (buf.value().to_owned(), buf.version());

        let refused = buf.apply_edits(planned, edits, cursor, None).unwrap_err();
        assert!(refused.starts_with(err), "{refused}");
        assert_eq!(buf.value(), value);
        assert_eq!(buf.version(), version);
    }

    /// Listed out of order on purpose, since a plugin should not have to sort.
    #[test]
    fn edits_land_as_one_version_and_one_undo_step() {
        const EDITED: &str = "> hello **world**";
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let before = buf.version();
        let edits = vec![
            text_edit(11, 11, "**"),
            text_edit(0, 0, "> "),
            text_edit(6, 6, "**"),
        ];

        assert_eq!(
            buf.apply_edits(before, edits, None, None)
                .map(|(caret, _)| caret),
            Ok(EDITED.len())
        );
        assert_eq!(buf.value(), EDITED);
        assert_eq!(buf.version(), before + 1);
        assert_eq!(undo_all(&mut buf), [PLANNED_ON]);
        assert_eq!(buf.version(), before + 2);
        assert!(buf.redo());
        assert_eq!((buf.value(), buf.version()), (EDITED, before + 3));
    }

    /// The cursor still counts in the value at {version}, so the user's typing
    /// since moves it along like it moves an edit.
    #[test_case(false, 3, 3 ; "current")]
    #[test_case(true,  3, 4 ; "planned_before_typing")]
    fn no_edits_with_a_cursor_only_move_it(typed_first: bool, cursor: usize, landed: usize) {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        if typed_first {
            buf.move_home();
            buf.push_char('!');
        }
        let before = buf.version();
        let steps = buf.undo.steps.len();

        let (caret, _) = buf
            .apply_edits(planned, vec![], Some(cursor), None)
            .unwrap();
        assert_eq!(caret, landed);
        assert_eq!(buf.cursor_byte(), landed);
        assert_eq!(buf.version(), before);
        assert_eq!(buf.undo.steps.len(), steps);
    }

    #[test_case(vec![],                                           NOTHING_TO_APPLY_ERR  ; "none")]
    #[test_case(vec![text_edit(0, 5, "a"), text_edit(3, 8, "b")], OVERLAPPING_EDITS_ERR ; "overlapping")]
    #[test_case(vec![text_edit(5, 5, "a"), text_edit(5, 5, "b")], OVERLAPPING_EDITS_ERR ; "two_inserts_at_one_offset")]
    #[test_case(vec![text_edit(5, 5, "a"), text_edit(5, 8, "b")], OVERLAPPING_EDITS_ERR ; "insert_where_a_range_starts")]
    fn an_edit_list_without_one_clear_order_is_refused(edits: Vec<TextEdit>, err: &str) {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let version = buf.version();
        let refused = buf.apply_edits(version, edits, None, None).unwrap_err();
        assert!(refused.starts_with(err), "{refused}");
        assert_eq!(buf.value(), PLANNED_ON);
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
    #[test_case("abc", "abc", 3, 3, 3 ; "the_same_text_still_logs_a_replace")]
    #[test_case("",    "",    0, 0, 0 ; "two_empty_prompts_still_log_a_replace")]
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
            [(start, old_end, new_end, ChangeKind::Replace)]
        );
    }

    #[test_case(|buf| { buf.set_cursor(0, 2); buf.kill_to_end_of_line() } ; "kill_at_end_of_line")]
    #[test_case(|buf| { buf.move_to_end(); buf.delete_char() }           ; "delete_at_end")]
    #[test_case(TextBuffer::remove_char                                  ; "backspace_at_start")]
    #[test_case(|buf| { buf.move_to_end(); buf.move_up() }              ; "cursor_moves")]
    fn an_edit_that_changes_nothing_keeps_the_version(edit: fn(&mut TextBuffer)) {
        let mut buf = TextBuffer::new("ab\ncd".into());
        let before = buf.version();
        edit(&mut buf);
        assert_eq!(buf.value(), "ab\ncd");
        assert_eq!(buf.version(), before);
        assert!(
            buf.changes_since(before + 1).is_none(),
            "a version the buffer never had"
        );
    }

    #[test]
    fn a_multi_line_paste_is_one_edit() {
        let mut buf = TextBuffer::new("ab".into());
        buf.set_cursor(0, 1);
        let since = buf.version();
        buf.insert_text("1\n2\n3");
        assert_eq!(changes(&buf, since), [(1, 1, 6, ChangeKind::Edit)]);
        let lines: Vec<_> = buf
            .changes_since(since)
            .unwrap()
            .map(|change| (change.start_line, change.old_end_line, change.new_end_line))
            .collect();
        assert_eq!(lines, [(0, 0, 2)]);
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
        assert_eq!(changes(&buf, since), [(4, 9, 4, ChangeKind::Edit)]);
    }

    #[test_case(|buf| buf.clear()                                          ; "after_an_action")]
    #[test_case(|buf| buf.undoable(|buf| { buf.push_char('!'); buf.clear() }) ; "inside_an_action")]
    fn a_whole_value_write_forgets_undo_and_redo(write: fn(&mut TextBuffer)) {
        let mut buf = TextBuffer::new(String::new());
        type_keys(&mut buf, "hello world");
        buf.undo();
        write(&mut buf);
        assert_eq!((buf.undo(), buf.redo()), (false, false));
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

    #[test]
    fn redo_walks_forward_until_the_next_edit() {
        let mut buf = TextBuffer::new(String::new());
        type_keys(&mut buf, "hello world");
        assert_eq!(undo_all(&mut buf), ["hello ", ""]);

        assert!(buf.redo());
        assert_eq!((buf.value(), buf.cursor_byte()), ("hello ", 6));
        assert!(buf.redo());
        assert_eq!((buf.value(), buf.cursor_byte()), ("hello world", 11));
        assert!(!buf.redo());
        assert_eq!(undo_all(&mut buf), ["hello ", ""]);

        buf.push_char('!');
        assert!(!buf.redo());
    }

    const PLUGIN: &str = "test";
    const NS: &str = "marks";

    #[derive(Clone, Copy)]
    enum Kind {
        Atomic,
        Hidden,
        Revealing,
        Raw,
    }

    fn marked(text: &str, bytes: Range<usize>, kind: Kind, cursor: usize) -> TextBuffer {
        let mut buf = TextBuffer::new(text.into());
        let decoration = Decoration {
            atomic: matches!(kind, Kind::Atomic),
            conceal: (!matches!(kind, Kind::Atomic)).then(|| "[chip]".into()),
            reveal: match kind {
                Kind::Revealing => Reveal::Cursor,
                _ => Reveal::Never,
            },
            ..Decoration::default()
        };
        let mark = Mark {
            start: bytes.start,
            stop: Some(bytes.end),
            decoration: Arc::new(decoration),
        };
        buf.set_marks(buf.version(), PLUGIN, NS, vec![mark])
            .unwrap();
        if matches!(kind, Kind::Raw) {
            buf.toggle_raw();
        }
        buf.place_cursor(cursor);
        buf
    }

    #[test_case("a one two b", 2..9, Kind::Atomic, 9, TextBuffer::move_left => 2 ; "left_steps_over_an_atomic_range")]
    #[test_case("a one two b", 2..9, Kind::Atomic, 2, TextBuffer::move_right => 9 ; "right_steps_over_an_atomic_range")]
    #[test_case("a one two b", 2..9, Kind::Atomic, 1, TextBuffer::move_word_right => 9 ; "word_right_stops_after_an_atomic_range")]
    #[test_case("a one two b", 2..9, Kind::Atomic, 9, TextBuffer::move_word_left => 2 ; "word_left_stops_before_an_atomic_range")]
    #[test_case("a one two b", 2..9, Kind::Hidden, 2, TextBuffer::move_right => 9 ; "right_steps_over_concealed_text")]
    #[test_case("a one two b", 2..9, Kind::Revealing, 2, TextBuffer::move_right => 3 ; "right_walks_into_text_the_cursor_reveals")]
    #[test_case("a one two b", 2..9, Kind::Raw, 2, TextBuffer::move_right => 3 ; "right_walks_into_raw_text")]
    #[test_case("abcdefgh\nab one two", 12..19, Kind::Atomic, 6, TextBuffer::move_down => 12 ; "down_lands_on_the_nearer_edge_before")]
    #[test_case("abcdefgh\nab one two", 12..19, Kind::Atomic, 7, TextBuffer::move_down => 19 ; "down_lands_on_the_nearer_edge_after")]
    #[test_case("a\nb\nc\nd", 1..4, Kind::Hidden, 0, TextBuffer::move_down => 6 ; "down_skips_the_lines_a_conceal_joined")]
    #[test_case("a\nb\nc\nd", 1..4, Kind::Hidden, 6, TextBuffer::move_up => 0 ; "up_skips_the_lines_a_conceal_joined")]
    #[test_case("a\nb\nc\nd", 1..4, Kind::Hidden, 5, TextBuffer::move_home => 0 ; "home_goes_to_the_start_of_joined_lines")]
    #[test_case("a\nb\nc\nd", 1..4, Kind::Hidden, 0, TextBuffer::move_end => 5 ; "end_goes_to_the_end_of_joined_lines")]
    #[test_case("a one two b", 2..9, Kind::Atomic, 0, |b| b.set_cursor(0, 3) => 2 ; "placing_inside_an_atomic_range_snaps_to_the_nearer_start")]
    #[test_case("a one two b", 2..9, Kind::Atomic, 0, |b| b.set_cursor(0, 7) => 9 ; "placing_inside_an_atomic_range_snaps_to_the_nearer_end")]
    fn cursor_motion(
        text: &str,
        bytes: Range<usize>,
        kind: Kind,
        from: usize,
        motion: fn(&mut TextBuffer),
    ) -> usize {
        let mut buf = marked(text, bytes, kind, from);
        motion(&mut buf);
        buf.cursor_byte()
    }

    /// Typing the `$` turns the word after the cursor into a chip around it.
    /// The caret goes past the chip, not back before the `$`, so the next
    /// key lands after it.
    #[test]
    fn a_key_that_makes_a_chip_leaves_the_caret_past_it() {
        let chip = Decoration {
            atomic: true,
            ..Decoration::default()
        };
        let rules = RuleSet::with_plugins(&[InputRule::Pattern(
            PatternRule::new(r"\$s:\w+", chip).unwrap(),
        )]);
        let mut buf = TextBuffer::new("fix s:go".into());
        buf.set_rules(rules);
        buf.place_cursor(4);

        type_keys(&mut buf, "$");
        assert_eq!(buf.cursor_byte(), "fix $s:go".len());
        type_keys(&mut buf, " ok");
        assert_eq!(buf.value(), "fix $s:go ok");
    }

    /// A delete that makes a chip sends the caret to its nearer edge, so the
    /// next Backspace does not take the whole chip. Typing always goes past
    /// it, so the keys land in the order they were pressed.
    #[test_case(r"@\S+", "@f longword", 3, TextBuffer::remove_char => ("@flongword".into(), 0) ; "a_backspace_that_joins_two_words")]
    #[test_case(r"\w+@\w+", "ab@cd", 0, |buf| type_keys(buf, "xy") => ("xab@cdy".into(), 7) ; "typing_in_front_of_a_chip_that_grows")]
    #[test_case(r"\$s:\w+", "fix s:go", 4, |buf| type_keys(buf, "$") => ("fix $s:go".into(), 9) ; "typing_that_begins_a_chip")]
    fn a_key_edit_inside_a_chip(
        pattern: &str,
        text: &str,
        from: usize,
        key: fn(&mut TextBuffer),
    ) -> (String, usize) {
        let chip = Decoration {
            atomic: true,
            ..Decoration::default()
        };
        let rules =
            RuleSet::with_plugins(&[InputRule::Pattern(PatternRule::new(pattern, chip).unwrap())]);
        let mut buf = TextBuffer::new(text.into());
        buf.set_rules(rules);
        buf.place_cursor(from);
        key(&mut buf);
        (buf.value().to_owned(), buf.cursor_byte())
    }

    /// The grown delete is still one splice, so one undo brings the whole
    /// chip back, caret and all.
    #[test_case("say CHIP now", 4..8, 8, TextBuffer::remove_char => ("say  now".into(), 4) ; "backspace_takes_a_chip_whole")]
    #[test_case("say CHIP now", 4..8, 4, TextBuffer::delete_char => ("say  now".into(), 4) ; "delete_takes_a_chip_whole")]
    #[test_case("say CHIP now", 2..6, 9, TextBuffer::remove_word_before_cursor => ("sanow".into(), 2) ; "a_word_kill_that_cuts_in_takes_the_chip")]
    #[test_case("ab CHIP\ncd ef", 5..10, 3, TextBuffer::kill_to_end_of_line => ("ab  ef".into(), 3) ; "a_line_kill_takes_a_chip_over_the_line_break")]
    fn deleting_into_an_atomic_range(
        text: &str,
        bytes: Range<usize>,
        from: usize,
        delete: fn(&mut TextBuffer),
    ) -> (String, usize) {
        let mut buf = marked(text, bytes, Kind::Atomic, from);
        delete(&mut buf);
        let deleted = (buf.value().to_owned(), buf.cursor_byte());
        assert!(buf.undo());
        assert_eq!((buf.value(), buf.cursor_byte()), (text, from));
        assert!(!buf.undo());
        deleted
    }

    fn point(start: usize) -> Mark {
        Mark {
            start,
            stop: None,
            decoration: Arc::new(Decoration::default()),
        }
    }

    fn highlight(bytes: Range<usize>) -> Mark {
        Mark {
            start: bytes.start,
            stop: Some(bytes.end),
            decoration: Arc::new(Decoration::default()),
        }
    }

    fn chip(bytes: Range<usize>) -> Mark {
        Mark {
            decoration: Arc::new(Decoration {
                atomic: true,
                ..Decoration::default()
            }),
            ..highlight(bytes)
        }
    }

    /// A plugin hears about the text a frame late. Its marks should end up
    /// where they would be had it pushed them before the user typed.
    #[test_case(vec![point(11)], |b| { b.move_to_end(); b.insert_text("!!") }, vec![13..13] ; "ghost_text_at_the_caret_follows_typing")]
    #[test_case(vec![chip(0..5), highlight(6..11)], |b| { b.set_cursor(0, 3); b.push_char('x') }, vec![7..12] ; "a_chip_typed_into_is_dropped_and_the_rest_still_placed")]
    fn stale_marks_move_like_placed_ones(
        marks: Vec<Mark>,
        user: fn(&mut TextBuffer),
        expected: Vec<Range<usize>>,
    ) {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        user(&mut buf);

        buf.set_marks(planned, PLUGIN, NS, marks).unwrap();
        assert_eq!(mark_ranges(&buf), expected);
    }

    fn chip_rule(buf: &mut TextBuffer) {
        let chip = Decoration {
            atomic: true,
            ..Decoration::default()
        };
        let rule = InputRule::Pattern(PatternRule::new("he", chip).unwrap());
        buf.set_rules(RuleSet::with_plugins(&[rule]));
    }

    /// A chip drawn over the caret moves it on past the chip, never back to
    /// the start, where a typed `!` would make a shell line. Marks and rules
    /// need no `input`, so they must not steer the user's next key.
    #[test_case(|b| { b.set_marks(b.version(), PLUGIN, NS, vec![chip(0..2)]).unwrap(); } => 2 ; "pushed_marks")]
    #[test_case(chip_rule => 2 ; "a_rule")]
    fn a_chip_drawn_over_the_caret_moves_it_on(decorate: fn(&mut TextBuffer)) -> usize {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        buf.place_cursor(1);
        decorate(&mut buf);
        buf.cursor_byte()
    }

    #[test_case(|b| b.set_value("another prompt"), INPUT_REPLACED_ERR ; "across_a_replace")]
    #[test_case(|b| { b.move_to_end(); for _ in 0..=CHANGE_LOG_LEN { b.push_char('!') } }, VERSION_TOO_OLD_ERR ; "past_the_log")]
    #[test_case(|b| { b.set_cursor(0, 0); b.push_char('!') }, PAST_END_ERR ; "past_the_end")]
    fn stale_marks_it_cannot_place_are_refused_whole(user: fn(&mut TextBuffer), err: &str) {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        buf.set_marks(planned, PLUGIN, NS, vec![point(0)]).unwrap();
        user(&mut buf);
        let kept = mark_ranges(&buf);

        let refused = buf
            .set_marks(
                planned,
                PLUGIN,
                NS,
                vec![highlight(0..PLANNED_ON.len() + 1)],
            )
            .unwrap_err();
        assert!(refused.contains(err), "{refused}");
        assert_eq!(mark_ranges(&buf), kept);
    }

    /// Ghost text pushed for the prompt put away never turns up in the next
    /// one, and comes back with its own prompt.
    #[test]
    fn marks_leave_and_come_back_with_their_prompt() {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let marks = vec![point(5), highlight(0..5)];
        buf.set_marks(buf.version(), PLUGIN, NS, marks).unwrap();

        let away = buf.swap_prompt(Prompt::default());
        buf.insert_text("typed");
        assert!(mark_ranges(&buf).is_empty());

        buf.swap_prompt(away);
        assert_eq!(buf.value(), PLANNED_ON);
        assert_eq!(mark_ranges(&buf), vec![0..5, 5..5]);
    }

    #[test]
    fn marks_of_a_stale_edit_land_on_its_text() {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        buf.set_cursor(0, 0);
        buf.insert_text("oh ");

        let marks = InputMarks {
            ns: NS.into(),
            marks: vec![chip(6..8)],
        };
        buf.apply_edits(
            planned,
            vec![text_edit(6, 6, "XY")],
            None,
            Some((PLUGIN, marks)),
        )
        .unwrap();
        assert_eq!(buf.value(), "oh hello XYworld");
        assert_eq!(mark_ranges(&buf), vec![9..11]);
    }

    #[test_case("XY", 0..3, PAST_END_ERR ; "past_the_end")]
    #[test_case("é", 0..1, INSIDE_CHAR_ERR ; "inside_an_inserted_char")]
    #[test_case("a\r\nb", 0..4, UNCLEAN_TEXT_ERR ; "over_text_cleaning_would_shrink")]
    fn a_mark_that_does_not_fit_refuses_its_edit(text: &str, bytes: Range<usize>, err: &str) {
        let mut buf = TextBuffer::new(String::new());
        let version = buf.version();
        let marks = InputMarks {
            ns: NS.into(),
            marks: vec![chip(bytes)],
        };
        let refused = buf
            .apply_edits(
                version,
                vec![text_edit(0, 0, text)],
                None,
                Some((PLUGIN, marks)),
            )
            .unwrap_err();
        assert!(refused.contains(err), "{refused}");
        assert_eq!((buf.value(), buf.version()), ("", version));
        assert!(mark_ranges(&buf).is_empty());
    }

    /// Offsets passed with the text count its bytes as sent, which cleaning
    /// would move under them. Even no marks at all is refused, so the rule
    /// is just whether any of them was given.
    #[test_case(Some(1), None ; "a_cursor")]
    #[test_case(None, Some(Vec::new()) ; "an_empty_mark_list")]
    fn unclean_text_next_to_offsets_is_refused(cursor: Option<usize>, marks: Option<Vec<Mark>>) {
        let mut buf = TextBuffer::new(String::new());
        let version = buf.version();
        let marks = marks.map(|marks| {
            (
                PLUGIN,
                InputMarks {
                    ns: NS.into(),
                    marks,
                },
            )
        });
        let refused = buf
            .apply_edits(version, vec![text_edit(0, 0, "a\tb")], cursor, marks)
            .unwrap_err();
        assert!(refused.contains(UNCLEAN_TEXT_ERR), "{refused}");
        assert_eq!((buf.value(), buf.version()), ("", version));
    }

    /// Planned before the user typed, so the mark moves past what was typed
    /// like the cursor of a cursor-only call does.
    #[test]
    fn no_edits_with_marks_place_them_and_leave_the_text_and_cursor() {
        let mut buf = TextBuffer::new(PLANNED_ON.into());
        let planned = buf.version();
        buf.set_cursor(0, 0);
        buf.insert_text("oh ");
        let (before, cursor, steps) = (buf.version(), buf.cursor_byte(), buf.undo.steps.len());

        let marks = InputMarks {
            ns: NS.into(),
            marks: vec![chip(6..11)],
        };
        let (caret, _) = buf
            .apply_edits(planned, vec![], None, Some((PLUGIN, marks)))
            .unwrap();

        assert_eq!(caret, cursor);
        assert_eq!(buf.cursor_byte(), cursor);
        assert_eq!(mark_ranges(&buf), vec![9..14]);
        assert_eq!(buf.version(), before);
        assert_eq!(buf.undo.steps.len(), steps);
    }
}
