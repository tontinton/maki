//! Undo and redo for vim mode. A step is the whole value and the cursor from
//! before a change: prompts are small, and a copy is simpler than a diff that
//! has to agree with every way the buffer can be written.

use std::collections::VecDeque;

use crate::text_buffer::TextBuffer;

const MAX_UNDO_STEPS: usize = 64;
const MAX_UNDO_BYTES: usize = 1024 * 1024;

pub(super) struct Snapshot {
    text: String,
    cursor: usize,
}

impl Snapshot {
    pub(super) fn new(text: String, cursor: usize) -> Self {
        Self { text, cursor }
    }

    pub(super) fn of(buf: &TextBuffer) -> Self {
        Self::new(buf.value(), buf.cursor_byte())
    }

    pub(super) fn restore(self, buf: &mut TextBuffer) {
        buf.set_value(self.text);
        super::move_cursor(buf, self.cursor);
    }
}

#[derive(Default)]
pub(super) struct UndoHistory {
    undo: VecDeque<Snapshot>,
    redo: Vec<Snapshot>,
    /// Where the insert session in progress started, and the buffer version
    /// then. The session is one step, so only its end decides whether it
    /// changed anything.
    open: Option<(Snapshot, u64)>,
    /// Text bytes `undo` holds, so a push does not walk it to trim.
    undo_bytes: usize,
}

impl UndoHistory {
    /// Records a change. It forks history, so the steps redo held are gone.
    pub(super) fn push(&mut self, before: Snapshot) {
        self.redo.clear();
        self.record(before);
    }

    /// The oldest steps go first. The newest one stays even past the byte
    /// limit, because it is the one an accidental delete of a huge paste
    /// needs.
    fn record(&mut self, snapshot: Snapshot) {
        self.undo_bytes += snapshot.text.len();
        self.undo.push_back(snapshot);
        while self.undo.len() > MAX_UNDO_STEPS
            || (self.undo_bytes > MAX_UNDO_BYTES && self.undo.len() > 1)
        {
            if let Some(oldest) = self.undo.pop_front() {
                self.undo_bytes -= oldest.text.len();
            }
        }
    }

    pub(super) fn open(&mut self, before: Snapshot, version: u64) {
        self.open = Some((before, version));
    }

    /// Ends the insert session, keeping it as a step when the buffer changed
    /// since it opened.
    pub(super) fn close(&mut self, version: u64) {
        if let Some((before, opened)) = self.open.take()
            && opened != version
        {
            self.push(before);
        }
    }

    /// The state before the last change, handing {current} to redo.
    pub(super) fn undo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let previous = self.undo.pop_back()?;
        self.undo_bytes -= previous.text.len();
        self.redo.push(current);
        Some(previous)
    }

    /// The state the last undo left, handing {current} back to undo.
    pub(super) fn redo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let next = self.redo.pop()?;
        self.record(current);
        Some(next)
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(text: &str) -> Snapshot {
        Snapshot::new(text.into(), 0)
    }

    fn undo_text(history: &mut UndoHistory) -> Option<String> {
        history.undo(snap("current")).map(|s| s.text)
    }

    #[test]
    fn the_oldest_step_goes_past_the_step_limit() {
        let mut history = UndoHistory::default();
        for i in 0..=MAX_UNDO_STEPS {
            history.push(snap(&i.to_string()));
        }
        let mut left = Vec::new();
        while let Some(text) = undo_text(&mut history) {
            left.push(text);
        }
        assert_eq!(left.len(), MAX_UNDO_STEPS);
        assert_eq!(
            left.last().map(String::as_str),
            Some("1"),
            "step 0 was trimmed"
        );
    }

    #[test]
    fn the_oldest_step_goes_past_the_byte_limit() {
        let big = "a".repeat(MAX_UNDO_BYTES / 2 + 1);
        let mut history = UndoHistory::default();
        history.push(snap(&big));
        history.push(snap(&format!("{big}b")));
        assert_eq!(undo_text(&mut history), Some(format!("{big}b")));
        assert_eq!(undo_text(&mut history), None, "the older one did not fit");
    }

    #[test]
    fn a_new_change_clears_redo() {
        let mut history = UndoHistory::default();
        history.push(snap("a"));
        assert!(history.undo(snap("b")).is_some());
        history.push(snap("c"));
        assert!(history.redo(snap("d")).is_none());
    }

    #[test]
    fn an_insert_session_is_a_step_only_when_it_changed_something() {
        const OPENED: u64 = 3;
        let mut history = UndoHistory::default();
        history.open(snap("untouched"), OPENED);
        history.close(OPENED);
        assert_eq!(undo_text(&mut history), None);

        history.open(snap("typed in"), OPENED);
        history.close(OPENED + 1);
        assert_eq!(undo_text(&mut history).as_deref(), Some("typed in"));
    }
}
