//! Marks decorate the input without being part of its text: a highlight, a
//! language to color a range in, virtual text that is drawn but never sent.
//! They move with the text on every change, so a plugin that placed them once
//! does not have to place them again after every keystroke.
//!
//! A namespace sorts its marks by start once and cuts them into chunks. Each
//! chunk knows the span its marks cover and a shift it still owes all of them.
//! An edit remaps the marks of the chunks whose span it touches, and a chunk
//! wholly after the edit only adds to its shift. So a keystroke costs one step
//! per chunk plus the marks of the chunk under it, never one step per mark.
//!
//! Remapping can swap two marks that sat inside the edited text, so a chunk is
//! only roughly sorted after a while. Nothing depends on the order: an edit
//! and a lookup only read each chunk's span, which is always exact.
//!
//! Marks with inline virtual text and marks that conceal share chunks of their
//! own. They change how a line wraps, so the layout asks for them often, and
//! it should not have to step over every highlight to find the few chips.

use std::cmp::Reverse;
use std::iter;
use std::mem;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

pub use maki_lua::{Decoration, Reveal, VirtTextPos};

use super::Change;

/// A hundred thousand marks make a few hundred chunks to step over per edit,
/// and remapping the one chunk under the edit stays a few hundred marks.
const CHUNK_LEN: usize = 256;
/// A rule mark uses its rule's place in the list as its rank. Ranks of
/// placed marks start above every such place, so at the same priority a mark
/// a plugin placed by hand draws over one a rule made.
const LAST_RULE_RANK: u64 = u32::MAX as u64;

#[derive(Debug, Clone)]
pub struct Mark {
    pub start: usize,
    /// `None` makes a point mark.
    pub stop: Option<usize>,
    /// Shared, so a rule stamping one look over many ranges stores it once.
    pub decoration: Arc<Decoration>,
}

/// A mark where it sits now. A point mark has an empty range.
pub struct Placed<'a> {
    pub bytes: Range<usize>,
    pub decoration: &'a Decoration,
    rank: u64,
}

/// Text drawn over a range instead of it. Conceals that overlap are merged
/// into one, drawn as the one that starts first.
pub struct Concealed<'a> {
    pub bytes: Range<usize>,
    pub text: &'a str,
}

/// Every namespace's marks over the text of one input. A namespace belongs to
/// one plugin. None of it is saved: marks are a cache, rebuilt from the text
/// or by the plugin that placed them.
pub struct Marks {
    namespaces: Vec<Namespace>,
    /// Given to every mark set, so a later mark ranks after an earlier one.
    next_rank: u64,
}

impl Default for Marks {
    fn default() -> Self {
        Self {
            namespaces: Vec::new(),
            next_rank: LAST_RULE_RANK,
        }
    }
}

impl Marks {
    /// Replaces what {plugin} has in namespace {name}. Returns where marks
    /// that change how rows wrap came or went.
    pub fn set(&mut self, plugin: &str, name: &str, marks: Vec<Mark>) -> Vec<Range<usize>> {
        let mut moved = Vec::new();
        if let Some(at) = self
            .namespaces
            .iter()
            .position(|ns| ns.plugin == plugin && ns.name == name)
        {
            moved.extend(self.namespaces.swap_remove(at).shaping_ranges());
        }
        let ns = Namespace::new(plugin, name, marks, &mut self.next_rank);
        if !ns.is_empty() {
            moved.extend(ns.shaping_ranges());
            self.namespaces.push(ns);
        }
        moved
    }

    /// Moves every mark through {change}. Returns where the dropped marks
    /// that change how rows wrap would have landed, since that can be lines
    /// away from where the text changed.
    pub(super) fn map(&mut self, change: &Change) -> Vec<Range<usize>> {
        let mut dropped = Vec::new();
        for ns in &mut self.namespaces {
            map_chunks(&mut ns.plain, change, &mut Vec::new());
            map_chunks(&mut ns.shaping, change, &mut dropped);
        }
        self.namespaces.retain(|ns| !ns.is_empty());
        dropped
    }

    /// The marks touching {bytes}, ends included, in no particular order.
    pub fn touching(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Placed<'_>> {
        self.namespaces.iter().flat_map(move |ns| {
            touching(&ns.plain, bytes.clone()).chain(touching(&ns.shaping, bytes.clone()))
        })
    }

    /// Like [`Self::touching`] for the marks that change how rows wrap only:
    /// conceals and inline virtual text. They sit apart from the highlights,
    /// so the layout, which asks for them line by line, never walks those.
    pub(super) fn shaping(&self, bytes: RangeInclusive<usize>) -> impl Iterator<Item = Placed<'_>> {
        self.namespaces
            .iter()
            .flat_map(move |ns| touching(&ns.shaping, bytes.clone()))
    }
}

/// {marks} in the order they compose: higher priority later, and at equal
/// priority the mark set later.
pub(super) fn composed(mut marks: Vec<Placed<'_>>) -> Vec<Placed<'_>> {
    marks.sort_by_key(|mark| (mark.decoration.priority, mark.rank));
    marks
}

/// The ranges {marks} hide with the cursor at {cursor}, sorted and disjoint.
/// At one start the mark that composes on top wins. {cursor} is only asked
/// for when a conceal is found, since finding the cursor's byte walks its
/// line.
///
/// Overlapping conceals merge, so whether a byte is hidden never depends on
/// which bytes were asked about. A lookup over whole lines therefore agrees
/// with one over a single byte of them, which is what lets the layout and
/// the cursor work on different lookups.
pub(super) fn concealed<'a>(
    marks: impl Iterator<Item = Placed<'a>>,
    cursor: impl FnOnce() -> usize,
) -> Vec<Concealed<'a>> {
    let mut found: Vec<Placed> = marks.filter(Placed::conceals).collect();
    if found.is_empty() {
        return Vec::new();
    }
    let cursor = cursor();
    found.retain(|mark| !mark.reveals_at(cursor));
    found.sort_by_key(|mark| {
        let order = (mark.decoration.priority, mark.rank);
        (mark.bytes.start, Reverse(order))
    });
    let mut merged: Vec<Concealed> = Vec::new();
    for mark in found {
        match merged.last_mut() {
            Some(last) if mark.bytes.start < last.bytes.end => {
                last.bytes.end = last.bytes.end.max(mark.bytes.end);
            }
            _ => merged.push(Concealed {
                bytes: mark.bytes,
                text: mark.decoration.conceal.as_deref().unwrap_or_default(),
            }),
        }
    }
    merged
}

/// The conceals among {marks} the cursor at {cursor} shows the text of,
/// sorted.
pub(super) fn revealed<'a>(
    marks: impl Iterator<Item = Placed<'a>>,
    cursor: usize,
) -> Vec<Range<usize>> {
    let mut found: Vec<Range<usize>> = marks
        .filter(|mark| mark.conceals() && mark.reveals_at(cursor))
        .map(|mark| mark.bytes)
        .collect();
    found.sort_by_key(|bytes| (bytes.start, bytes.end));
    found
}

impl<'a> Placed<'a> {
    pub(super) fn derived(bytes: Range<usize>, decoration: &'a Decoration, rank: usize) -> Self {
        Self {
            bytes,
            decoration,
            rank: rank as u64,
        }
    }

    /// A point mark hides nothing.
    fn conceals(&self) -> bool {
        self.decoration.conceal.is_some() && !self.bytes.is_empty()
    }

    fn reveals_at(&self, cursor: usize) -> bool {
        self.decoration.reveal == Reveal::Cursor
            && (self.bytes.start..=self.bytes.end).contains(&cursor)
    }
}

struct Namespace {
    plugin: String,
    name: String,
    plain: Vec<Chunk>,
    /// The marks that change how rows wrap: inline virtual text and conceals.
    shaping: Vec<Chunk>,
}

impl Namespace {
    /// An empty range is dropped right away, like one an edit emptied.
    fn new(plugin: &str, name: &str, marks: Vec<Mark>, next_rank: &mut u64) -> Self {
        let (shaping, plain): (Vec<Entry>, Vec<Entry>) = marks
            .into_iter()
            .filter(|mark| mark.stop != Some(mark.start))
            .map(|mark| {
                *next_rank += 1;
                Entry {
                    mark,
                    rank: *next_rank,
                }
            })
            .partition(|entry| entry.mark.decoration.shapes_rows());
        Self {
            plugin: plugin.to_owned(),
            name: name.to_owned(),
            plain: chunked(plain),
            shaping: chunked(shaping),
        }
    }

    fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.shaping.is_empty()
    }

    fn shaping_ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        touching(&self.shaping, 0..=usize::MAX).map(|mark| mark.bytes)
    }
}

fn chunked(mut entries: Vec<Entry>) -> Vec<Chunk> {
    entries.sort_by_key(|entry| entry.mark.start);
    let mut entries = entries.into_iter();
    iter::from_fn(|| {
        let chunk: Vec<Entry> = entries.by_ref().take(CHUNK_LEN).collect();
        (!chunk.is_empty()).then(|| Chunk {
            span: span(&chunk),
            entries: chunk,
            shift: 0,
        })
    })
    .collect()
}

/// Adds where each mark it drops would have landed to {dropped}.
fn map_chunks(chunks: &mut Vec<Chunk>, change: &Change, dropped: &mut Vec<Range<usize>>) {
    let delta = change.new_end.cast_signed() - change.old_end.cast_signed();
    for chunk in chunks.iter_mut() {
        if chunk.first() > change.old_end {
            chunk.shift += delta;
        } else if chunk.last() >= change.start {
            chunk.remap(change, dropped);
        }
    }
    chunks.retain(|chunk| !chunk.entries.is_empty());
}

fn touching<'a>(
    chunks: &'a [Chunk],
    bytes: RangeInclusive<usize>,
) -> impl Iterator<Item = Placed<'a>> {
    let (from, to) = bytes.into_inner();
    chunks
        .iter()
        .filter(move |chunk| chunk.first() <= to && chunk.last() >= from)
        .flat_map(|chunk| chunk.entries.iter().map(|entry| chunk.place(entry)))
        .filter(move |mark| mark.bytes.start <= to && mark.bytes.end >= from)
}

/// Offsets in a chunk are stored without its {shift}, which every read adds.
struct Chunk {
    entries: Vec<Entry>,
    shift: isize,
    /// The lowest start and the highest end of its marks.
    span: (usize, usize),
}

impl Chunk {
    fn first(&self) -> usize {
        shifted(self.span.0, self.shift)
    }

    fn last(&self) -> usize {
        shifted(self.span.1, self.shift)
    }

    fn place<'a>(&self, entry: &'a Entry) -> Placed<'a> {
        let at = |pos: usize| shifted(pos, self.shift);
        Placed {
            bytes: at(entry.mark.start)..at(entry.mark.end()),
            decoration: &entry.mark.decoration,
            rank: entry.rank,
        }
    }

    /// Pays the shift owed, then moves each mark on its own.
    fn remap(&mut self, change: &Change, dropped: &mut Vec<Range<usize>>) {
        let shift = mem::take(&mut self.shift);
        self.entries.retain_mut(|Entry { mark, .. }| {
            mark.start = shifted(mark.start, shift);
            mark.stop = mark.stop.map(|stop| shifted(stop, shift));
            let bytes = mark.start..mark.end();
            let kept = mark.remap(change);
            if !kept {
                dropped.push(change.map_range(bytes));
            }
            kept
        });
        self.span = span(&self.entries);
    }
}

/// {pos} moved by {by}. A mark never moves past either end of the text.
fn shifted(pos: usize, by: isize) -> usize {
    let (moved, overflowed) = pos.overflowing_add_signed(by);
    debug_assert!(!overflowed, "{pos} shifted by {by} leaves the text");
    moved
}

fn span(entries: &[Entry]) -> (usize, usize) {
    let first = entries.iter().map(|entry| entry.mark.start).min();
    let last = entries.iter().map(|entry| entry.mark.end()).max();
    (first.unwrap_or(0), last.unwrap_or(0))
}

struct Entry {
    mark: Mark,
    rank: u64,
}

impl Mark {
    fn end(&self) -> usize {
        self.stop.unwrap_or(self.start)
    }

    /// [`remap`] for a placed mark, `false` when it has to go.
    fn remap(&mut self, change: &Change) -> bool {
        let Some(bytes) = remap(self.start..self.end(), &self.decoration, change) else {
            return false;
        };
        self.start = bytes.start;
        self.stop = self.stop.map(|_| bytes.end);
        true
    }
}

/// Where a mark with {decoration} over {bytes} sits after {change}, `None`
/// when it has to go. Placed marks and the marks of a parse that has not
/// caught up both move this way. An empty range is a point mark, which stays
/// after text typed at it, so ghost text keeps following the caret. A range
/// only grows at an edge when it is inclusive, and goes when it is emptied or
/// when the change lands inside it and it conceals or is atomic.
pub(super) fn remap(
    bytes: Range<usize>,
    decoration: &Decoration,
    change: &Change,
) -> Option<Range<usize>> {
    if bytes.is_empty() {
        let at = change.map_pos(bytes.start, true);
        return Some(at..at);
    }
    let edited_inside = change.start < bytes.end && change.old_end > bytes.start;
    if edited_inside && (decoration.conceal.is_some() || decoration.atomic) {
        return None;
    }
    let start = change.map_pos(bytes.start, !decoration.inclusive);
    let end = change.map_pos(bytes.end, decoration.inclusive);
    (start < end).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_buffer::TextBuffer;
    use maki_lua::TextEdit;
    use test_case::test_case;

    const PLUGIN: &str = "test";
    const NS: &str = "marks";
    const TEXT: &str = "0123456789";
    const LCG_MUL: u64 = 6364136223846793005;
    const LCG_INC: u64 = 1442695040888963407;

    #[derive(Clone, Copy)]
    enum Kind {
        Plain,
        Inclusive,
        Conceal,
        Atomic,
    }

    fn decoration(kind: Kind) -> Arc<Decoration> {
        Arc::new(Decoration {
            inclusive: matches!(kind, Kind::Inclusive),
            conceal: matches!(kind, Kind::Conceal).then(String::new),
            atomic: matches!(kind, Kind::Atomic),
            ..Decoration::default()
        })
    }

    fn placed(marks: &Marks) -> Vec<(u64, Range<usize>)> {
        let mut all: Vec<_> = marks
            .touching(0..=usize::MAX)
            .map(|mark| (mark.rank, mark.bytes))
            .collect();
        all.sort_by_key(|(rank, _)| *rank);
        all
    }

    fn ranges(marks: &Marks) -> Vec<Range<usize>> {
        placed(marks).into_iter().map(|(_, bytes)| bytes).collect()
    }

    #[test_case(5, None, Kind::Plain, (5, 5, "ab") => Some(7..7) ; "point_stays_after_text_typed_at_it")]
    #[test_case(5, None, Kind::Plain, (2, 2, "ab") => Some(7..7) ; "point_moves_with_text_typed_before_it")]
    #[test_case(5, None, Kind::Plain, (6, 6, "ab") => Some(5..5) ; "point_ignores_text_typed_after_it")]
    #[test_case(5, None, Kind::Plain, (3, 7, "") => Some(3..3) ; "point_in_deleted_text_falls_to_its_start")]
    #[test_case(5, None, Kind::Plain, (3, 7, "xy") => Some(5..5) ; "point_in_replaced_text_lands_after_the_new_text")]
    #[test_case(2, Some(5), Kind::Plain, (2, 2, "ab") => Some(4..7) ; "range_does_not_grow_at_its_start")]
    #[test_case(2, Some(5), Kind::Plain, (5, 5, "ab") => Some(2..5) ; "range_does_not_grow_at_its_stop")]
    #[test_case(2, Some(5), Kind::Plain, (3, 3, "ab") => Some(2..7) ; "range_grows_with_text_typed_inside")]
    #[test_case(2, Some(5), Kind::Inclusive, (2, 2, "ab") => Some(2..7) ; "inclusive_range_grows_at_its_start")]
    #[test_case(2, Some(5), Kind::Inclusive, (5, 5, "ab") => Some(2..7) ; "inclusive_range_grows_at_its_stop")]
    #[test_case(2, Some(5), Kind::Plain, (4, 7, "") => Some(2..4) ; "range_loses_its_deleted_tail")]
    #[test_case(2, Some(5), Kind::Plain, (1, 6, "") => None ; "range_emptied_is_dropped")]
    #[test_case(2, Some(5), Kind::Plain, (2, 5, "abc") => None ; "range_whose_text_is_replaced_is_dropped")]
    #[test_case(2, Some(5), Kind::Inclusive, (2, 5, "abc") => Some(2..5) ; "inclusive_range_keeps_the_text_replacing_its_own")]
    #[test_case(2, Some(5), Kind::Conceal, (3, 3, "a") => None ; "typing_inside_a_conceal_drops_it")]
    #[test_case(2, Some(5), Kind::Atomic, (4, 6, "") => None ; "deleting_into_an_atomic_range_drops_it")]
    #[test_case(2, Some(5), Kind::Atomic, (5, 5, "a") => Some(2..5) ; "typing_at_an_atomic_edge_keeps_it")]
    #[test_case(2, Some(5), Kind::Conceal, (0, 2, "") => Some(0..3) ; "deleting_up_to_a_conceal_keeps_it")]
    #[test_case(2, Some(2), Kind::Plain, (9, 9, "a") => None ; "empty_range_is_dropped_when_set")]
    fn mapping(
        start: usize,
        stop: Option<usize>,
        kind: Kind,
        (from, to, text): (usize, usize, &str),
    ) -> Option<Range<usize>> {
        let mut buffer = TextBuffer::new(TEXT.into());
        let mark = Mark {
            start,
            stop,
            decoration: decoration(kind),
        };
        buffer.set_marks(PLUGIN, NS, vec![mark]).unwrap();
        buffer.replace_byte_range(from, to, text, None).unwrap();
        let mut mapped = ranges(buffer.marks());
        assert!(mapped.len() <= 1);
        mapped.pop()
    }

    /// Both `**` land in one transaction. Folded into one change, the edit
    /// would cover the whole word and drop the mark, so this only holds when
    /// each change is mapped on its own.
    #[test]
    fn wrapping_a_marked_word_keeps_the_mark_on_the_word() {
        let mut buffer = TextBuffer::new("say word now".into());
        let mark = Mark {
            start: 4,
            stop: Some(8),
            decoration: decoration(Kind::Plain),
        };
        buffer.set_marks(PLUGIN, NS, vec![mark]).unwrap();
        let wrap = |at| TextEdit {
            start: at,
            stop: at,
            text: "**".into(),
        };
        buffer
            .apply_edits(buffer.version(), vec![wrap(4), wrap(8)], None)
            .unwrap();
        assert_eq!(ranges(buffer.marks()), vec![6..10]);
    }

    /// The chunks skip and shift marks in bulk, which is easy to get subtly
    /// wrong. So the same marks also go one by one through every edit, and
    /// both must agree after each one. Half the edits start right on a
    /// chunk's edge, where an off by one hides, since a random offset almost
    /// never lands there. The seed is fixed, so a failure replays the same
    /// way every time.
    #[test]
    fn chunks_map_like_marks_mapped_one_by_one() {
        const MARKS: usize = CHUNK_LEN * 6;
        const EDITS: usize = 300;
        const MAX_MARK_LEN: usize = 8;
        const POINT_ODDS: usize = 5;
        const LINE: &str = "abcdefghij\n";
        let mut seed: u64 = 7;
        let mut roll = |below: usize| {
            seed = seed.wrapping_mul(LCG_MUL).wrapping_add(LCG_INC);
            (seed >> 33) as usize % below
        };

        let mut buffer = TextBuffer::new(LINE.repeat(MARKS / 4));
        let kinds = [Kind::Plain, Kind::Inclusive, Kind::Conceal, Kind::Atomic];
        let mut marks = Vec::with_capacity(MARKS);
        for _ in 0..MARKS {
            let start = roll(buffer.value().len() - MAX_MARK_LEN);
            let stop = (roll(POINT_ODDS) > 0).then(|| start + 1 + roll(MAX_MARK_LEN - 1));
            marks.push(Mark {
                start,
                stop,
                decoration: decoration(kinds[roll(kinds.len())]),
            });
        }
        buffer.set_marks(PLUGIN, NS, marks.clone()).unwrap();
        let first_rank = placed(buffer.marks())[0].0;
        let mut expected: Vec<(u64, Mark)> = (first_rank..).zip(marks).collect();

        for _ in 0..EDITS {
            let len = buffer.value().len();
            let edges: Vec<usize> = buffer
                .marks()
                .namespaces
                .iter()
                .flat_map(|ns| &ns.plain)
                .flat_map(|chunk| [chunk.first(), chunk.last()])
                .collect();
            let start = match roll(2) {
                0 if !edges.is_empty() => edges[roll(edges.len())],
                _ => roll(len + 1),
            };
            let stop = (start + roll(4) * roll(3)).min(len);
            let text = ["", "x", "yz\n", "long insert"][roll(4)];
            let seen = buffer.version();
            buffer.replace_byte_range(start, stop, text, None).unwrap();
            for change in buffer.changes_since(seen).unwrap() {
                expected.retain_mut(|(_, mark)| mark.remap(change));
            }
            let expected: Vec<_> = expected
                .iter()
                .map(|(rank, mark)| (*rank, mark.start..mark.end()))
                .collect();
            assert_eq!(placed(buffer.marks()), expected);
        }
    }
}
