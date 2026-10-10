use super::segment::SegmentCache;
use std::cell::OnceCell;

/// Top of the viewport as a place in the document. `seg` indexes the segment
/// cache, the one ordered list every row walk shares.
///
/// Nothing here depends on the width, so a resize is not a scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScrollPos {
    pub seg: usize,
    pub row: u16,
}

/// One frame's document: the segment list, every row walk goes through here.
pub(super) struct Layout<'a> {
    cache: &'a SegmentCache,
    width: u16,
    /// Rows the document may show, counted from the top. A part shows the rows
    /// the cursor has reached and no more, so a part below the frontier cannot
    /// appear ahead of one above it. A `u32` because a transcript can pass
    /// `u16::MAX` rows.
    revealed: u32,
    /// First index of the live streaming segments. They are paced by the
    /// typewriter, not the cursor, so they are never capped: capping them would
    /// hold typed rows back behind the cursor's rate.
    live_start: usize,
    /// `starts[i]` is the total full height of the parts before `i`, so a part
    /// gets `revealed - starts[i]` rows without rescanning the document. Built
    /// on the first cursored lookup: the walkers that only need full heights,
    /// which is most of them, never pay for it.
    starts: OnceCell<Vec<u32>>,
}

impl<'a> Layout<'a> {
    pub fn new(cache: &'a SegmentCache, width: u16, revealed: u32, live_start: usize) -> Self {
        Self {
            cache,
            width,
            revealed,
            live_start,
            starts: OnceCell::new(),
        }
    }

    fn starts(&self) -> &[u32] {
        self.starts.get_or_init(|| {
            let n = self.cache.len();
            let mut starts = Vec::with_capacity(n);
            let mut acc: u32 = 0;
            for i in 0..n {
                starts.push(acc);
                acc = acc.saturating_add(u32::from(self.full_height(i)));
            }
            starts
        })
    }

    fn len(&self) -> usize {
        self.cache.len()
    }

    fn full_height(&self, i: usize) -> u16 {
        self.cache.get(i).map_or(0, |seg| seg.height(self.width))
    }

    /// Rows segment `i` shows when drawn, bounded by how much of the cursor is
    /// left after the segments above it have taken their share, which spreads a
    /// block that arrived whole over several frames.
    ///
    /// Drawing only. The walkers that move and clamp a scroll position use
    /// [`Self::full_height`], because the cursor paces growth, not navigation:
    /// the reader can scroll anywhere in the document the moment it exists.
    pub(super) fn height(&self, i: usize) -> u16 {
        if i >= self.live_start {
            return self.full_height(i);
        }
        let before = self.starts().get(i).copied().unwrap_or(u32::MAX);
        let remaining = self.revealed.saturating_sub(before);
        self.full_height(i)
            .min(remaining.min(u32::from(u16::MAX)) as u16)
    }

    /// Total rows the cursored document shows, the number a draw produces.
    /// Used by the jank harness to measure growth.
    #[cfg(test)]
    pub(super) fn drawn_total(&self) -> u32 {
        (0..self.len()).map(|i| u32::from(self.height(i))).sum()
    }

    /// One past the last addressable row, so `retreat` from here is "the last
    /// N rows of the document".
    fn end(&self) -> ScrollPos {
        ScrollPos {
            seg: self.len(),
            row: 0,
        }
    }

    /// Pulls `row` back inside its segment. A segment can shrink under a
    /// stored position, and the walkers here read a row past its end as
    /// "nothing left" while the renderer carries the excess into the segments
    /// below, so the two only agree while the row is in range.
    pub fn clamp(&self, pos: ScrollPos) -> ScrollPos {
        ScrollPos {
            seg: pos.seg,
            row: pos.row.min(self.full_height(pos.seg).saturating_sub(1)),
        }
    }

    /// Costs the number of segments crossed, not the number of rows, so a
    /// wheel tick stays cheap however tall the transcript is.
    pub fn advance(&self, mut pos: ScrollPos, mut rows: u32) -> ScrollPos {
        while pos.seg < self.len() {
            let left = u32::from(self.full_height(pos.seg).saturating_sub(pos.row));
            if rows < left {
                pos.row += rows as u16;
                return pos;
            }
            rows -= left;
            pos = ScrollPos {
                seg: pos.seg + 1,
                row: 0,
            };
        }
        self.end()
    }

    pub fn retreat(&self, mut pos: ScrollPos, mut rows: u32) -> ScrollPos {
        while rows > 0 {
            if u32::from(pos.row) >= rows {
                pos.row -= rows as u16;
                return pos;
            }
            rows -= u32::from(pos.row);
            if pos.seg == 0 {
                return ScrollPos::default();
            }
            pos.seg -= 1;
            pos.row = self.full_height(pos.seg);
        }
        pos
    }

    /// The lowest drawn position that still fills the viewport: the pin every
    /// frame aims at. Walks the *cursored* heights, so it lands on the last row
    /// that is actually drawn rather than on the end of a document still being
    /// revealed, which would scroll the reader past the reveal.
    pub fn bottom(&self, viewport: u16) -> ScrollPos {
        let mut pos = ScrollPos {
            seg: self.len(),
            row: 0,
        };
        let mut rows = u32::from(viewport);
        while rows > 0 {
            if u32::from(pos.row) >= rows {
                pos.row -= rows as u16;
                return pos;
            }
            rows -= u32::from(pos.row);
            if pos.seg == 0 {
                return ScrollPos::default();
            }
            pos.seg -= 1;
            pos.row = self.height(pos.seg);
        }
        pos
    }

    /// Rows between two positions, or 0 when `to` is not below `from`. Only
    /// the segments in between are walked, so projecting a position into the
    /// viewport costs what is on screen.
    pub fn rows_from(&self, from: ScrollPos, to: ScrollPos) -> u32 {
        if to <= from {
            return 0;
        }
        (from.seg..to.seg.min(self.len()))
            .map(|i| u32::from(self.full_height(i)))
            .fold(u32::from(to.row), u32::saturating_add)
            .saturating_sub(u32::from(from.row))
    }

    /// O(transcript): only the scrollbar and `winsaveview` need a document
    /// row, and both read cached heights rather than re-wrapping.
    pub fn doc_row(&self, pos: ScrollPos) -> u32 {
        self.rows_from(ScrollPos::default(), pos)
    }

    pub fn total_rows(&self) -> u32 {
        self.doc_row(self.end())
    }

    /// The inverse of [`Self::doc_row`], needed only by `winrestview`.
    pub fn at_row(&self, doc_row: u32) -> ScrollPos {
        self.advance(ScrollPos::default(), doc_row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::messages::segment::Segment;
    use ratatui::text::Line;
    use test_case::test_case;

    const WIDTH: u16 = 80;

    /// A layout that hides nothing, since these walk the document rather than
    /// the reveal. No segment is live, so every one takes its full height.
    fn layout<'a>(cache: &'a SegmentCache) -> Layout<'a> {
        Layout::new(cache, WIDTH, u32::MAX, cache.len())
    }

    fn cache(heights: &[u16]) -> SegmentCache {
        let mut cache = SegmentCache::new();
        for &h in heights {
            let lines = (0..h).map(|i| Line::raw(format!("l{i}"))).collect();
            cache.push(Segment::with_lines(lines, None));
        }
        cache
    }

    fn pos(seg: usize, row: u16) -> ScrollPos {
        ScrollPos { seg, row }
    }

    #[test_case(pos(0, 0), 0, pos(0, 0)  ; "zero_rows_stays")]
    #[test_case(pos(0, 0), 2, pos(0, 2)  ; "inside_first_segment")]
    #[test_case(pos(0, 0), 3, pos(1, 0)  ; "boundary_lands_on_next_start")]
    #[test_case(pos(0, 1), 4, pos(2, 1)  ; "crosses_two_segments")]
    #[test_case(pos(1, 0), 99, pos(3, 0) ; "clamps_at_the_end")]
    fn advance_walks_rows(from: ScrollPos, rows: u32, expected: ScrollPos) {
        let cache = cache(&[3, 1, 2]);
        assert_eq!(
            layout(&cache).advance(from, rows),
            expected
        );
    }

    #[test_case(pos(2, 1), 1, pos(2, 0) ; "inside_a_segment")]
    #[test_case(pos(2, 0), 1, pos(1, 0) ; "into_the_previous_segment")]
    #[test_case(pos(2, 0), 2, pos(0, 2) ; "across_a_one_row_segment")]
    #[test_case(pos(1, 0), 99, pos(0, 0) ; "clamps_at_the_start")]
    fn retreat_walks_rows(from: ScrollPos, rows: u32, expected: ScrollPos) {
        let cache = cache(&[3, 1, 2]);
        assert_eq!(
            layout(&cache).retreat(from, rows),
            expected
        );
    }

    #[test]
    fn the_document_is_the_segment_list() {
        // 3 + 1 + 4 = 8 rows across three segments, addressed the same way the
        // tail used to be.
        let cache = cache(&[3, 1, 4]);
        let layout = layout(&cache);
        assert_eq!(layout.total_rows(), 8);
        assert_eq!(layout.at_row(4), pos(2, 0));
        assert_eq!(layout.doc_row(pos(2, 3)), 7);
        assert_eq!(layout.bottom(2), pos(2, 2));
    }

    #[test_case(pos(0, 2), pos(1, 1), 2 ; "counts_rows_between")]
    #[test_case(pos(1, 1), pos(0, 2), 0 ; "target_above_never_underflows")]
    fn rows_from_counts_down(from: ScrollPos, to: ScrollPos, expected: u32) {
        let cache = cache(&[3, 2]);
        assert_eq!(
            layout(&cache).rows_from(from, to),
            expected
        );
    }
}
