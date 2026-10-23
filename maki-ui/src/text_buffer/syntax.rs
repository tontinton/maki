//! Marks from tree-sitter rules. The worker keeps its own copy of the text
//! and its trees, gets only what changed since its last parse, and runs the
//! queries again only where the tree changed. The input keeps the latest
//! result as it was for the text the worker saw. The UI never parses and
//! never waits for a parse.
//!
//! A lookup maps that result forward through the change log to the text now,
//! by the rules placed marks move by. So while the worker catches up, a bold
//! or a code block grows with the text typed inside it instead of flashing
//! plain, and only a conceal or an atomic range an edit lands inside is gone.
//! The next result replaces them all.
//!
//! What a line looks like can change when another line changes, like when a
//! code fence opens above it. So each result says where its marks differ
//! from the last one, and the layout recounts just those lines.

use std::mem;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use flume::{Receiver, TryRecvError};
use maki_lua::{SyntaxDoc, SyntaxEdit, SyntaxIndex, SyntaxInput};

use super::Change;
use super::marks::{Placed, remap};
use super::rules::SyntaxRules;
use crate::decor_worker;

/// The marks of every rule for the text at {version}, `None` before the
/// first parse.
#[derive(Default)]
struct Shown {
    version: Option<u64>,
    marks: Vec<SyntaxIndex>,
}

struct Reply {
    /// Goes to the worker with a job and comes back here, so one parse runs
    /// at a time and nothing is shared between the threads. It always holds
    /// the text of the shown result.
    doc: SyntaxDoc,
    shown: Shown,
    /// Where the marks differ from the last result, in the text of this one.
    /// `None` when there was nothing to compare with.
    changed: Option<Vec<Range<usize>>>,
}

/// The worker's side, one parse at a time:
/// - `Idle(doc)` holds the trees of the shown result. [`SyntaxMarks::parse`]
///   sends the doc off with what changed and moves to `Parsing(rx)`. Called
///   while `Parsing`, it does nothing, and the next parse sends that text too.
/// - [`SyntaxMarks::receive`] takes in the [`Reply`], shows its marks and goes
///   back to `Idle` with the doc.
/// - A job that died drops its sender, so `receive` starts over from an empty
///   doc with nothing shown, and the parse after it sends the whole text.
/// - New rules build a new [`SyntaxMarks`], which replaces all of this, a job
///   still running for the old rules included.
enum State {
    Idle(SyntaxDoc),
    Parsing(Receiver<Reply>),
}

pub(super) struct SyntaxMarks {
    rules: SyntaxRules,
    shown: Shown,
    state: State,
}

impl SyntaxMarks {
    pub fn new(rules: SyntaxRules) -> Self {
        Self {
            rules,
            shown: Shown::default(),
            state: State::Idle(SyntaxDoc::default()),
        }
    }

    pub fn shown_version(&self) -> Option<u64> {
        self.shown.version
    }

    /// Whether the shown result is for the text at {version}. Without
    /// rules there is nothing to wait for.
    pub fn is_current(&self, version: u64) -> bool {
        self.rules.is_empty() || self.shown.version == Some(version)
    }

    /// Whether the worker is idle and the text moved past its last parse.
    pub fn wants_parse(&self, version: u64) -> bool {
        matches!(self.state, State::Idle(_)) && !self.is_current(version)
    }

    /// Hands the worker what the text did since its last parse, which
    /// leaves the text at {version}.
    pub fn parse(&mut self, input: SyntaxInput, version: u64) {
        let (reply, replies) = flume::bounded(1);
        let mut doc = match mem::replace(&mut self.state, State::Parsing(replies)) {
            State::Idle(doc) => doc,
            parsing => {
                self.state = parsing;
                return;
            }
        };
        let rules = Arc::clone(&self.rules);
        decor_worker::spawn_parse(move || {
            let changed = doc.update(&rules, &input);
            let shown = Shown {
                version: Some(version),
                marks: doc.marks(),
            };
            let _ = reply.send(Reply {
                doc,
                shown,
                changed,
            });
        });
    }

    /// Takes in a finished parse. `Some` with where the marks changed, in
    /// the text of the new result, see [`Reply::changed`].
    pub fn receive(&mut self) -> Option<Option<Vec<Range<usize>>>> {
        let State::Parsing(replies) = &self.state else {
            return None;
        };
        let reply = match replies.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return None,
            // The job died and took the trees along, so the next parse
            // starts over.
            Err(TryRecvError::Disconnected) => {
                *self = Self::new(Arc::clone(&self.rules));
                return Some(None);
            }
        };
        self.shown = reply.shown;
        self.state = State::Idle(reply.doc);
        Some(reply.changed)
    }

    /// The marks touching {bytes} of the text now. {changes} lead from the
    /// text of the shown result to it, and without them nothing is shown.
    pub fn touching<'a>(
        &'a self,
        bytes: RangeInclusive<usize>,
        changes: Option<impl DoubleEndedIterator<Item = &'a Change> + Clone + 'a>,
    ) -> impl Iterator<Item = Placed<'a>> + 'a {
        let (from, to) = bytes.into_inner();
        let shown = &self.shown;
        changes
            .filter(|_| shown.marks.iter().any(|marks| !marks.is_empty()))
            .map(move |changes| {
                let old_from = map_back(from, changes.clone(), false);
                let old_to = map_back(to, changes.clone(), true);
                shown
                    .marks
                    .iter()
                    .flat_map(move |marks| marks.touching(old_from, old_to))
                    .filter_map(move |mark| {
                        let bytes = changes.clone().try_fold(mark.bytes, |bytes, change| {
                            remap(bytes, mark.decoration, change)
                        })?;
                        (bytes.start <= to && bytes.end >= from)
                            .then(|| Placed::derived(bytes, mark.decoration, mark.rank))
                    })
            })
            .into_iter()
            .flatten()
    }
}

/// {changes} as one edit of the text before the first of them.
pub(super) fn edit_since<'a>(changes: impl Iterator<Item = &'a Change>) -> Option<SyntaxEdit> {
    changes
        .map(|change| SyntaxEdit {
            start: change.start,
            old_end: change.old_end,
            new_end: change.new_end,
        })
        .reduce(|edit, next| SyntaxEdit {
            start: edit.start.min(next.start),
            old_end: edit.old_end + next.old_end.saturating_sub(edit.new_end),
            new_end: edit.new_end.max(next.old_end) - next.old_end + next.new_end,
        })
}

/// Where {pos} of the text now was before {changes}. A position inside text
/// they inserted widens to the edge of what was there, toward the end when
/// {toward_end}.
fn map_back<'a>(
    pos: usize,
    changes: impl DoubleEndedIterator<Item = &'a Change>,
    toward_end: bool,
) -> usize {
    changes.rev().fold(pos, |pos, change| {
        if pos < change.start {
            pos
        } else if pos > change.new_end {
            pos - change.new_end + change.old_end
        } else if toward_end {
            change.old_end
        } else {
            change.start
        }
    })
}

#[cfg(test)]
mod tests {
    use maki_lua::TextEdit;
    use test_case::test_case;

    use super::super::TextBuffer;
    use super::*;

    const DIGITS: &str = "0123456789";

    /// The worker gets one edit for everything since its last parse, so
    /// splicing that edit's span of the new text into the old text has to
    /// give the new text back.
    #[test_case(&[(2, 2, "x"), (8, 8, "y")]   ; "an_insert_then_one_after_it")]
    #[test_case(&[(8, 8, "y"), (2, 2, "x")]   ; "an_insert_then_one_before_it")]
    #[test_case(&[(2, 6, ""), (1, 3, "zz")]   ; "a_delete_then_an_edit_over_its_edge")]
    #[test_case(&[(4, 4, "abc"), (5, 6, "")]  ; "a_delete_inside_an_insert")]
    fn changes_since_a_version_read_as_one_edit(edits: &[(usize, usize, &str)]) {
        let mut buffer = TextBuffer::new(DIGITS.into());
        for &(start, stop, text) in edits {
            let edit = TextEdit {
                start,
                stop,
                text: text.into(),
            };
            buffer
                .apply_edits(buffer.version(), vec![edit], None, None, None)
                .unwrap();
        }
        let edit = edit_since(buffer.changes_since(0).unwrap()).unwrap();
        let new = buffer.value();
        let spliced = format!(
            "{}{}{}",
            &DIGITS[..edit.start],
            &new[edit.start..edit.new_end],
            &DIGITS[edit.old_end..]
        );
        assert_eq!(spliced, new);
    }
}
