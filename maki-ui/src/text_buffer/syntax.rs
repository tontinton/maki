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
use crate::decor_worker::{self, ParseSlot};

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
///   sends the doc off with what changed and moves to `Parsing`. Called
///   while `Parsing`, it does nothing, and the next parse sends that text too.
/// - [`SyntaxMarks::receive`] takes in the [`Reply`], shows its marks and goes
///   back to `Idle` with the doc.
/// - A job that died drops its sender, so `receive` shows nothing and moves
///   to `Died`. Parsing the same text again would only die again, so the
///   whole text goes to the worker once it changes.
/// - New rules start over from an empty doc. A job still waiting for the old
///   rules gives its place in the [`ParseSlot`] to the next parse, and the
///   reply of one already running is dropped.
enum State {
    Idle(SyntaxDoc),
    Parsing {
        replies: Receiver<Reply>,
        version: u64,
    },
    /// The parse of the text at this version died and took the trees along.
    Died(u64),
}

pub(super) struct SyntaxMarks {
    rules: SyntaxRules,
    shown: Shown,
    state: State,
    slot: Arc<ParseSlot>,
}

impl SyntaxMarks {
    pub fn new(rules: SyntaxRules) -> Self {
        Self {
            rules,
            shown: Shown::default(),
            state: State::Idle(SyntaxDoc::default()),
            slot: Arc::default(),
        }
    }

    /// Setting any rule hands every input the whole list again, so rules
    /// that are the ones in use keep the trees and the marks shown.
    pub fn set_rules(&mut self, rules: SyntaxRules) {
        let same = self.rules.len() == rules.len()
            && self
                .rules
                .iter()
                .zip(rules.iter())
                .all(|(old, new)| old.0 == new.0 && Arc::ptr_eq(&old.1, &new.1));
        if !same {
            self.rules = rules;
            self.shown = Shown::default();
            self.state = State::Idle(SyntaxDoc::default());
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
        let idle = match self.state {
            State::Idle(_) => true,
            State::Parsing { .. } => false,
            State::Died(died) => died != version,
        };
        idle && !self.is_current(version)
    }

    /// Hands the worker what the text did since its last parse, which
    /// leaves the text at {version}.
    pub fn parse(&mut self, input: SyntaxInput, version: u64) {
        let (reply, replies) = flume::bounded(1);
        let mut doc = match mem::replace(&mut self.state, State::Parsing { replies, version }) {
            State::Idle(doc) => doc,
            State::Died(_) => SyntaxDoc::default(),
            parsing @ State::Parsing { .. } => {
                self.state = parsing;
                return;
            }
        };
        let rules = Arc::clone(&self.rules);
        decor_worker::spawn_parse(&self.slot, move || {
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
        let State::Parsing { replies, version } = &self.state else {
            return None;
        };
        let reply = match replies.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.state = State::Died(*version);
                self.shown = Shown::default();
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
    use maki_agent::SpanStyle;
    use maki_lua::{SyntaxRule, TextEdit};
    use test_case::test_case;

    use super::super::TextBuffer;
    use super::*;
    use crate::text_buffer::Decoration;

    const DIGITS: &str = "0123456789";
    const LANG: &str = "markdown";
    const TEXT: &str = "# title\n\nbody";
    const VERSION: u64 = 1;
    const CAPTURE: &str = "styled";
    const HEADING: &str = "(atx_heading) @styled";
    const PARAGRAPH: &str = "(paragraph) @styled";
    const STYLE: &str = "accent";

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
                .apply_edits(buffer.version(), vec![edit], None)
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

    fn rule(query: &str) -> Arc<SyntaxRule> {
        let styled = Decoration {
            hl: Some(SpanStyle::Named(STYLE.into())),
            ..Decoration::default()
        };
        let queries = vec![(LANG.into(), query.into())];
        Arc::new(SyntaxRule::new(LANG, queries, vec![(CAPTURE.into(), styled)]).unwrap())
    }

    /// Every rule set hands the input a new list, here holding {rule}.
    fn listed(rule: &Arc<SyntaxRule>) -> SyntaxRules {
        vec![(0, Arc::clone(rule))].into()
    }

    fn parse_text(marks: &mut SyntaxMarks) {
        marks.parse(SyntaxInput::Whole(TEXT.into()), VERSION);
    }

    /// A plugin may set rules on every keystroke, and on a big prompt each
    /// new set of rules is a whole parse, so only the latest one waits.
    #[test]
    fn rule_changes_leave_one_parse_waiting() {
        decor_worker::manual();
        let mut marks = SyntaxMarks::new(listed(&rule(HEADING)));
        parse_text(&mut marks);
        marks.set_rules(listed(&rule(PARAGRAPH)));
        parse_text(&mut marks);

        assert_eq!(decor_worker::run_queued(), 1);
        assert!(marks.receive().is_some());
        assert!(marks.is_current(VERSION));
    }

    /// Setting any rule hands the input every rule again.
    #[test]
    fn the_same_rules_set_again_keep_the_parse_done() {
        decor_worker::manual();
        let heading = rule(HEADING);
        let mut marks = SyntaxMarks::new(listed(&heading));
        parse_text(&mut marks);
        decor_worker::run_queued();
        marks.receive();

        marks.set_rules(listed(&heading));

        assert!(marks.is_current(VERSION));
        assert!(!marks.wants_parse(VERSION));
    }

    /// A parse that panics on some text would panic again on the same text,
    /// so it waits for the next edit.
    #[test_case(VERSION     => false ; "not_on_the_same_text")]
    #[test_case(VERSION + 1 => true  ; "once_the_text_changed")]
    fn a_parse_that_died_runs_again(version: u64) -> bool {
        let mut marks = SyntaxMarks::new(listed(&rule(HEADING)));
        let (_, replies) = flume::bounded(1);
        marks.state = State::Parsing {
            replies,
            version: VERSION,
        };
        assert_eq!(marks.receive(), Some(None));
        marks.wants_parse(version)
    }
}
