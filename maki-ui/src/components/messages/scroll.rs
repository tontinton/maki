use super::segment::SegmentCache;

/// One drawable part of the streaming tail, sitting where the segment that
/// replaces it will sit once the turn flushes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TailPart {
    Spacer,
    Thinking,
    Text,
}

/// Top of the viewport as a place in the document. `seg` indexes the segment
/// cache; indices past its end address the streaming tail, which `view` lays
/// out in the same order the cache will hold once it flushes.
///
/// Nothing here depends on the width, so a resize is not a scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScrollPos {
    pub seg: usize,
    pub row: u16,
}

/// One frame's document: cached segments followed by the streaming tail.
/// Every row walk goes through here, so both are counted the same way.
pub(super) struct Layout<'a> {
    cache: &'a SegmentCache,
    tail: &'a [(TailPart, u16)],
    width: u16,
}

impl<'a> Layout<'a> {
    pub fn new(cache: &'a SegmentCache, tail: &'a [(TailPart, u16)], width: u16) -> Self {
        Self { cache, tail, width }
    }

    fn len(&self) -> usize {
        self.cache.len() + self.tail.len()
    }

    fn height(&self, i: usize) -> u16 {
        match self.cache.get(i) {
            Some(seg) => seg.height(self.width),
            None => self.tail.get(i - self.cache.len()).map_or(0, |&(_, h)| h),
        }
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
            row: pos.row.min(self.height(pos.seg).saturating_sub(1)),
        }
    }

    /// Costs the number of segments crossed, not the number of rows, so a
    /// wheel tick stays cheap however tall the transcript is.
    pub fn advance(&self, mut pos: ScrollPos, mut rows: u32) -> ScrollPos {
        while pos.seg < self.len() {
            let left = u32::from(self.height(pos.seg).saturating_sub(pos.row));
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
            pos.row = self.height(pos.seg);
        }
        pos
    }

    /// The lowest position that still fills the viewport.
    pub fn bottom(&self, viewport: u16) -> ScrollPos {
        self.retreat(self.end(), u32::from(viewport))
    }

    /// Rows between two positions, or 0 when `to` is not below `from`. Only
    /// the segments in between are walked, so projecting a position into the
    /// viewport costs what is on screen.
    pub fn rows_from(&self, from: ScrollPos, to: ScrollPos) -> u32 {
        if to <= from {
            return 0;
        }
        (from.seg..to.seg.min(self.len()))
            .map(|i| u32::from(self.height(i)))
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

    fn cache(heights: &[u16]) -> SegmentCache {
        let mut cache = SegmentCache::new();
        for &h in heights {
            let lines = (0..h).map(|i| Line::raw(format!("l{i}"))).collect();
            cache.push(Segment::with_lines(lines, String::new(), None));
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
            Layout::new(&cache, &[], WIDTH).advance(from, rows),
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
            Layout::new(&cache, &[], WIDTH).retreat(from, rows),
            expected
        );
    }

    #[test]
    fn the_tail_extends_the_document_past_the_cache() {
        let cache = cache(&[3]);
        let layout = Layout::new(&cache, &[(TailPart::Spacer, 1), (TailPart::Text, 4)], WIDTH);
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
            Layout::new(&cache, &[], WIDTH).rows_from(from, to),
            expected
        );
    }
}
