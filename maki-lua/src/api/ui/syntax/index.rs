//! The marks of one rule as the input looks them up. The worker changes
//! them where a parse changed something and the input reads them while the
//! next parse runs, and both share everything the change did not touch.

use std::ops::Range;
#[cfg(test)]
use std::sync::Arc;

use super::SyntaxMark;
use crate::api::ui::marks::Decoration;
use crate::chunks::{Chunks, Spot, shifted};

/// A lookup finds marks up to this long by searching just this far before
/// the bytes it asks about. Longer ones, like a fenced code block, are few
/// and kept apart.
const SHORT_MARK: usize = 256;

/// Matches can make the same mark, like every star of a bold bringing the
/// bold along, and it is drawn once until the last of them is gone.
#[derive(Clone)]
struct Counted {
    mark: SyntaxMark,
    count: usize,
}

impl Spot for Counted {
    fn start(&self) -> usize {
        self.mark.bytes.start
    }

    fn end(&self) -> usize {
        self.mark.bytes.end
    }

    fn shift(&mut self, by: isize) {
        let bytes = &mut self.mark.bytes;
        *bytes = shifted(bytes.start, by)..shifted(bytes.end, by);
    }
}

pub(super) fn mark_key(mark: &SyntaxMark) -> (usize, usize, usize) {
    (mark.bytes.start, mark.bytes.end, mark.rank)
}

#[derive(Clone, Default)]
pub struct SyntaxIndex {
    short: Chunks<Counted>,
    long: Chunks<Counted>,
}

/// A mark where the index has it now.
pub struct MarkAt<'a> {
    pub bytes: Range<usize>,
    pub decoration: &'a Decoration,
    pub rank: usize,
}

impl SyntaxIndex {
    pub(super) fn new(mut marks: Vec<SyntaxMark>) -> Self {
        marks.sort_by_key(mark_key);
        let (mut short, mut long) = (Vec::new(), Vec::new());
        for mark in marks {
            let list: &mut Vec<Counted> = if is_long(&mark) {
                &mut long
            } else {
                &mut short
            };
            let same = list
                .iter_mut()
                .rev()
                .take_while(|counted| mark_key(&counted.mark) == mark_key(&mark))
                .find(|counted| counted.mark == mark);
            match same {
                Some(counted) => counted.count += 1,
                None => list.push(Counted { mark, count: 1 }),
            }
        }
        Self {
            short: Chunks::from_sorted(short),
            long: Chunks::from_sorted(long),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.short.is_empty() && self.long.is_empty()
    }

    pub fn touching(&self, from: usize, to: usize) -> impl Iterator<Item = MarkAt<'_>> {
        let short = self
            .short
            .starting_in(from.saturating_sub(SHORT_MARK), to)
            .filter(move |(shift, counted)| shifted(counted.end(), *shift) >= from);
        short
            .chain(self.long.touching(from, to))
            .map(|(shift, counted)| MarkAt {
                bytes: shifted(counted.start(), shift)..shifted(counted.end(), shift),
                decoration: &counted.mark.decoration,
                rank: counted.mark.rank,
            })
    }

    pub(super) fn add(&mut self, mark: &SyntaxMark) {
        let list = self.list(mark);
        let counted = |counted: &mut Counted| {
            counted.count += 1;
            true
        };
        if !list.update(mark.bytes.start, |other| other.mark == *mark, counted) {
            let mark = mark.clone();
            list.insert(Counted { mark, count: 1 }, |a, b| {
                mark_key(&a.mark) <= mark_key(&b.mark)
            });
        }
    }

    pub(super) fn remove(&mut self, mark: &SyntaxMark) {
        let uncounted = |counted: &mut Counted| {
            counted.count -= 1;
            counted.count > 0
        };
        let found =
            self.list(mark)
                .update(mark.bytes.start, |other| other.mark == *mark, uncounted);
        debug_assert!(found, "removed a mark the index never counted");
    }

    /// See [`Chunks::edit`].
    pub(super) fn edit(&mut self, old_end: usize, new_end: usize) {
        self.short.edit(old_end, new_end);
        self.long.edit(old_end, new_end);
    }

    fn list(&mut self, mark: &SyntaxMark) -> &mut Chunks<Counted> {
        if is_long(mark) {
            &mut self.long
        } else {
            &mut self.short
        }
    }

    #[cfg(test)]
    pub(super) fn marks(&self) -> Vec<SyntaxMark> {
        let mut marks: Vec<_> = self
            .touching(0, usize::MAX)
            .map(|at| SyntaxMark {
                bytes: at.bytes,
                decoration: Arc::new(at.decoration.clone()),
                rank: at.rank,
            })
            .collect();
        marks.sort_by_key(mark_key);
        marks
    }
}

fn is_long(mark: &SyntaxMark) -> bool {
    mark.bytes.len() > SHORT_MARK
}
