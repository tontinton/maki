//! How the input's lines break into rows on screen. Drawing, the caret, clicks
//! and copying a selection all ask here, so they never disagree on where a row
//! starts.
//!
//! The row count of every line is kept for one width and caught up from the
//! buffer's change log, so a keystroke only recounts the line it touched.
//! Running totals over those counts find the line under any row without
//! walking the lines above it, so a frame only wraps the rows it draws.
//!
//! Inline virtual text from the marks takes columns too, so it is wrapped
//! along with the text. It never breaks across rows: it sticks to the
//! character after it, and a piece wider than the box is cut at the edge.
//!
//! A concealed range is one piece as wide as the text drawn over it. One
//! that hides a line break joins the lines on both sides into one display
//! line. Its rows are counted on its first line, and the lines it swallowed
//! count zero rows, which the running totals step over.

use std::iter::{self, repeat_n};
use std::mem;
use std::ops::Range;
use std::rc::Rc;

use maki_agent::SnapshotSpan;
use unicode_width::UnicodeWidthChar;

use crate::text_buffer::TextBuffer;

/// A drawn row: bytes {bytes} of the display line at {text}, the {index}th
/// row that line wraps into. {line} is the line it starts on. {shape} is the
/// one the line was wrapped with, so drawing and clicks walk the row with
/// the same pieces.
pub(super) struct Row {
    pub line: usize,
    pub index: usize,
    pub bytes: Range<usize>,
    pub last: bool,
    pub text: Range<usize>,
    pub shape: Rc<Shape>,
}

#[derive(Default)]
pub(super) struct Layout {
    /// Never 0 once built, so a default layout always rebuilds on its first sync.
    width: usize,
    version: u64,
    decor_epoch: u64,
    /// Rows each line takes as drawn, the cursor's extra row included. A line
    /// a conceal joined to the one above takes none.
    counts: Vec<u32>,
    sums: RowSums,
    cursor: usize,
    /// The conceals the cursor revealed, where they were at the last sync.
    revealed: Vec<Range<usize>>,
    /// The first line of the display line the caret is drawn on.
    cursor_line: usize,
    /// A caret past a completely full last row gets a row of its own, so the
    /// count of {cursor_line} carries one more row than its text wraps into.
    cursor_extra_row: bool,
}

impl Layout {
    /// Brings the counts up to date with {buffer} at {width} text columns.
    /// Cheap when nothing moved, so every reader calls it first.
    pub fn sync(&mut self, buffer: &TextBuffer, width: usize) {
        let width = width.max(1);
        let same_decor = buffer.decor_epoch() == self.decor_epoch;
        let cursor = buffer.cursor_byte();
        if width == self.width
            && buffer.version() == self.version
            && cursor == self.cursor
            && same_decor
        {
            return;
        }
        // Taken off while the counts still match the old text, because the
        // splice below can move or drop the line it sits on.
        self.set_cursor_extra_row(false);
        let revealed = buffer.revealed();
        if width != self.width || !same_decor || self.apply_changes(buffer, &revealed).is_none() {
            self.rebuild(buffer, width);
        }
        self.revealed = revealed;
        self.cursor = cursor;
        self.place_caret(buffer);
        debug_assert_eq!(
            self.counts.len(),
            buffer.line_count(),
            "a count for every line"
        );
    }

    /// Recounts the lines of bytes {at}, after marks changed how they are
    /// drawn while the text stayed the same. The cursor stayed too, so a
    /// conceal it reveals now or no longer is one of the marks in {at}.
    pub fn relayout(&mut self, buffer: &TextBuffer, at: &[Range<usize>]) {
        if self.width == 0 || at.is_empty() {
            return;
        }
        self.sync(buffer, self.width);
        self.set_cursor_extra_row(false);
        for bytes in at {
            self.recount(buffer, lines_of(buffer, bytes), true);
        }
        self.revealed = buffer.revealed();
        self.place_caret(buffer);
    }

    pub fn total_rows(&self) -> u32 {
        self.sums.before(self.counts.len())
    }

    /// The row the caret is drawn on, counted from the top of the buffer.
    pub fn cursor_row(&self, buffer: &TextBuffer) -> u32 {
        let caret = buffer.cursor_byte();
        let lines = buffer.display_lines(buffer.line_of(caret));
        let text = buffer.lines_bytes(lines.clone());
        let in_line = self
            .line_rows(buffer, lines.start, &text, &shape_of(buffer, &text))
            .iter()
            .rposition(|row| row.start <= caret - text.start)
            .unwrap_or(0);
        self.sums.before(lines.start) + in_line as u32
    }

    /// The {count} drawn rows from row {first} down. Only the lines they
    /// fall in are wrapped, and their marks are looked up in one go.
    pub fn rows<'a>(
        &'a self,
        buffer: &'a TextBuffer,
        first: u32,
        count: u32,
    ) -> impl Iterator<Item = Row> + 'a {
        let lines = self.counts.len();
        let (start, skip) = self.sums.locate(first).unwrap_or((lines, 0));
        let end = self
            .sums
            .locate(first + count.max(1) - 1)
            .map_or(lines, |(y, _)| buffer.display_lines(y).end);
        let shapes = Shape::new(buffer, start..end);
        let mut next = start;
        iter::from_fn(move || {
            (next < end).then(|| {
                let lines = buffer.display_lines(next);
                next = lines.end;
                lines
            })
        })
        .flat_map(move |lines| {
            let text = buffer.lines_bytes(lines.clone());
            let shape = Rc::new(shapes.cut(&text));
            let rows = self.line_rows(buffer, lines.start, &text, &shape);
            let count = rows.len();
            rows.into_iter().enumerate().map(move |(index, bytes)| Row {
                line: lines.start,
                index,
                bytes,
                last: index + 1 == count,
                text: text.clone(),
                shape: Rc::clone(&shape),
            })
        })
        .skip(skip as usize)
        .take(count as usize)
    }

    fn line_rows(
        &self,
        buffer: &TextBuffer,
        y: usize,
        text: &Range<usize>,
        shape: &Shape,
    ) -> Vec<Range<usize>> {
        debug_assert!(
            self.version == buffer.version() && self.cursor == buffer.cursor_byte(),
            "layout read before sync"
        );
        let line = &buffer.value()[text.clone()];
        let mut rows = Vec::new();
        wrap(line, shape, self.width, |row| rows.push(row));
        if self.cursor_extra_row && y == self.cursor_line {
            rows.push(line.len()..line.len());
        }
        rows
    }

    /// Walks the changes since the last sync in order. Each one swaps the
    /// counts of the lines it replaced for placeholders, one per line it left,
    /// and marks them dirty. Dirty lines below it shift with it, and the ones
    /// it overlaps join it. Then only the dirty lines are recounted, on the
    /// final text, so the untouched text between two edits keeps its counts.
    /// The lines of a conceal the cursor came into or left are recounted too.
    /// `None` when the log does not reach back far enough.
    ///
    /// When no line came or went, each recount is one Fenwick update.
    /// Otherwise the counts shift and the sums are built again, one pass over
    /// plain integers, like the buffer does for its own line starts.
    fn apply_changes(&mut self, buffer: &TextBuffer, revealed: &[Range<usize>]) -> Option<()> {
        // Sorted and disjoint, in the numbering of the text seen so far.
        let mut dirty: Vec<Range<usize>> = Vec::new();
        let mut shifted = false;
        let mut was_revealed = mem::take(&mut self.revealed);
        for change in buffer.changes_since(self.version)? {
            let old = change.start_line..change.old_end_line + 1;
            let new = change.start_line..change.new_end_line + 1;
            if old.end > self.counts.len() {
                return None;
            }
            if old.len() != new.len() {
                self.counts.splice(old.clone(), repeat_n(0, new.len()));
                shifted = true;
            }
            let mut joined = new.clone();
            dirty.retain_mut(|lines| {
                if lines.end <= old.start {
                    return true;
                }
                let end = lines.end.max(old.end) - old.end + new.end;
                if lines.start >= old.end {
                    *lines = lines.start - old.end + new.end..end;
                    return true;
                }
                joined = joined.start.min(lines.start)..joined.end.max(end);
                false
            });
            let at = dirty.partition_point(|lines| lines.end <= joined.start);
            dirty.insert(at, joined);
            for bytes in &mut was_revealed {
                *bytes = change.map_range(bytes.clone());
            }
        }
        if was_revealed != revealed {
            let flipped = was_revealed.iter().chain(revealed);
            dirty.extend(flipped.map(|bytes| lines_of(buffer, bytes)));
        }
        for lines in dirty {
            self.recount(buffer, lines, !shifted);
        }
        if shifted {
            self.sums = RowSums::new(&self.counts);
        }
        self.version = buffer.version();
        Some(())
    }

    fn rebuild(&mut self, buffer: &TextBuffer, width: usize) {
        self.width = width;
        self.version = buffer.version();
        self.decor_epoch = buffer.decor_epoch();
        let lines = buffer.line_count();
        self.counts = vec![0; lines];
        self.recount(buffer, 0..lines, false);
        self.sums = RowSums::new(&self.counts);
    }

    /// Counts the display lines {lines} fall in again, updating the running
    /// totals when {update_sums}.
    ///
    /// A conceal that came or went changes which lines join, so the recount
    /// spreads to whole display lines both as they were and as they are now.
    /// The old ones are read off the counts, where a joined line has zero
    /// rows, and the new ones off the buffer.
    fn recount(&mut self, buffer: &TextBuffer, lines: Range<usize>, update_sums: bool) {
        let total = self.counts.len();
        let mut start = lines.start;
        while start > 0 && self.counts[start] == 0 {
            start -= 1;
        }
        let start = buffer.display_lines(start).start;
        let mut end = lines.end;
        loop {
            end = buffer.display_lines(end - 1).end;
            if end == total || self.counts[end] != 0 {
                break;
            }
            end += 1;
        }
        let shapes = Shape::new(buffer, start..end);
        let mut y = start;
        while y < end {
            let joined = buffer.display_lines(y);
            let text = buffer.lines_bytes(joined.clone());
            let rows = count_rows(
                &buffer.value()[text.clone()],
                &shapes.cut(&text),
                self.width,
            );
            self.set_count(y, rows, update_sums);
            for hidden in joined.start + 1..joined.end {
                self.set_count(hidden, 0, update_sums);
            }
            y = joined.end;
        }
    }

    fn set_count(&mut self, y: usize, rows: u32, update_sums: bool) {
        if update_sums {
            self.sums.add(y, rows as i32 - self.counts[y] as i32);
        }
        self.counts[y] = rows;
    }

    fn place_caret(&mut self, buffer: &TextBuffer) {
        let lines = buffer.display_lines(buffer.line_of(buffer.cursor_byte()));
        let text = buffer.lines_bytes(lines.clone());
        self.cursor_line = lines.start;
        let line = &buffer.value()[text.clone()];
        let full = wrap(line, &shape_of(buffer, &text), self.width, |_| {});
        self.set_cursor_extra_row(full);
    }

    fn set_cursor_extra_row(&mut self, on: bool) {
        if on == self.cursor_extra_row {
            return;
        }
        self.cursor_extra_row = on;
        let delta = if on { 1 } else { -1 };
        self.counts[self.cursor_line] = self.counts[self.cursor_line].wrapping_add_signed(delta);
        self.sums.add(self.cursor_line, delta);
    }
}

fn lines_of(buffer: &TextBuffer, bytes: &Range<usize>) -> Range<usize> {
    buffer.line_of(bytes.start)..buffer.line_of(bytes.end) + 1
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(1)
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn virt_width(spans: &[SnapshotSpan]) -> usize {
    spans.iter().map(|span| text_width(&span.text)).sum()
}

/// What sits on display lines besides their own text.
#[derive(Default)]
pub(super) struct Shape {
    /// Concealed ranges and the columns of the text drawn over each, sorted
    /// and disjoint.
    conceals: Vec<(Range<usize>, usize)>,
    /// The columns of inline virtual text before a byte, sorted and one
    /// entry per byte.
    virts: Vec<(usize, usize)>,
}

/// What the wrapping never breaks: one char or one concealed range, and the
/// inline virtual text stuck before it.
pub(super) struct Piece {
    pub bytes: Range<usize>,
    pub virt: usize,
    pub width: usize,
    pub concealed: bool,
}

impl Shape {
    /// The shape of {lines}, which start and end on whole display lines, with
    /// offsets into the value. Looked up in one go and cut line by line with
    /// [`Self::cut`], since every lookup walks the marks.
    fn new(buffer: &TextBuffer, lines: Range<usize>) -> Self {
        if lines.is_empty() {
            return Self::default();
        }
        let bytes = buffer.lines_bytes(lines);
        let window = bytes.start..=bytes.end;
        let conceals = buffer
            .concealed(window.clone())
            .into_iter()
            .filter(|concealed| concealed.bytes.start >= bytes.start)
            .map(|concealed| (concealed.bytes, text_width(concealed.text)))
            .collect();
        let mut virts: Vec<(usize, usize)> = buffer
            .inline_virts(window)
            .filter(|mark| mark.bytes.start >= bytes.start)
            .map(|mark| (mark.bytes.start, virt_width(&mark.decoration.virt_text)))
            .collect();
        virts.sort_by_key(|&(at, _)| at);
        virts.dedup_by(|next, kept| {
            let same = next.0 == kept.0;
            if same {
                kept.1 += next.1;
            }
            same
        });
        Self { conceals, virts }
    }

    /// The part on the display line at bytes {text}, with offsets into it.
    fn cut(&self, text: &Range<usize>) -> Self {
        let first = self.conceals.partition_point(|(r, _)| r.start < text.start);
        let end = self.conceals.partition_point(|(r, _)| r.start < text.end);
        let conceals = self.conceals[first..end]
            .iter()
            .map(|(r, width)| (r.start - text.start..r.end - text.start, *width))
            .collect();
        let first = self.virts.partition_point(|&(at, _)| at < text.start);
        let end = self.virts.partition_point(|&(at, _)| at <= text.end);
        let virts = self.virts[first..end]
            .iter()
            .map(|&(at, width)| (at - text.start, width))
            .collect();
        Self { conceals, virts }
    }

    fn virt_before(&self, at: usize) -> usize {
        self.virts
            .binary_search_by_key(&at, |&(v, _)| v)
            .map_or(0, |i| self.virts[i].1)
    }

    /// The pieces of {line} in {bytes}, which starts where a piece does.
    /// Virtual text inside a concealed range is hidden with it. Wrapping,
    /// drawing and clicks all walk a row with this, so they never disagree
    /// on which row a piece is on.
    pub fn pieces<'a>(
        &'a self,
        line: &'a str,
        bytes: Range<usize>,
    ) -> impl Iterator<Item = Piece> + 'a {
        let mut at = bytes.start;
        iter::from_fn(move || {
            if at >= bytes.end {
                return None;
            }
            let start = at;
            let (width, concealed) = match self.conceals.binary_search_by_key(&at, |(r, _)| r.start)
            {
                Ok(i) => {
                    let (hidden, width) = &self.conceals[i];
                    at = hidden.end;
                    (*width, true)
                }
                Err(_) => {
                    let c = line[at..].chars().next()?;
                    at += c.len_utf8();
                    (char_width(c), false)
                }
            };
            Some(Piece {
                bytes: start..at,
                virt: self.virt_before(start),
                width,
                concealed,
            })
        })
    }
}

fn shape_of(buffer: &TextBuffer, text: &Range<usize>) -> Shape {
    Shape::new(buffer, lines_of(buffer, text)).cut(text)
}

/// The one place that knows how a line breaks into rows: greedy wrapping,
/// where a piece that no longer fits moves to the next row and leaves the gap
/// behind. Dividing the columns by the width looks close enough for plain
/// text, but it cannot see those gaps, so with wide chars it lands on the
/// wrong row. See [`Shape::pieces`] for what never breaks.
///
/// Calls {emit} with the byte range of every row, and returns whether the last
/// row is full, where a caret after it needs a row of its own. The caret sits
/// before virtual text at the end of the line, so then it never does.
fn wrap(line: &str, shape: &Shape, width: usize, mut emit: impl FnMut(Range<usize>)) -> bool {
    let mut row_start = 0;
    let mut row_col = 0;
    for piece in shape.pieces(line, 0..line.len()) {
        let w = piece.virt + piece.width;
        if row_col + w > width && row_col > 0 {
            emit(row_start..piece.bytes.start);
            row_start = piece.bytes.start;
            row_col = 0;
        }
        row_col += w;
    }
    let tail = shape.virt_before(line.len());
    if tail > 0 && row_col + tail > width && row_col > 0 {
        emit(row_start..line.len());
        row_start = line.len();
        row_col = 0;
    }
    emit(row_start..line.len());
    tail == 0 && row_col >= width
}

fn count_rows(line: &str, shape: &Shape, width: usize) -> u32 {
    let mut rows = 0;
    wrap(line, shape, width, |_| rows += 1);
    rows
}

/// Running row totals as a Fenwick tree, so both the row a line starts on and
/// the line a row falls in cost `O(log lines)` and a recounted line is one
/// `O(log lines)` update. Slot `i` holds the sum of the `i & -i` counts that end
/// at line `i - 1`, and slot 0 stays empty so that math works out.
struct RowSums(Vec<u32>);

impl Default for RowSums {
    fn default() -> Self {
        Self(vec![0])
    }
}

impl RowSums {
    fn new(counts: &[u32]) -> Self {
        let mut tree = Vec::with_capacity(counts.len() + 1);
        tree.push(0);
        tree.extend_from_slice(counts);
        for i in 1..tree.len() {
            let parent = i + i.isolate_lowest_one();
            if parent < tree.len() {
                tree[parent] += tree[i];
            }
        }
        Self(tree)
    }

    fn add(&mut self, line: usize, delta: i32) {
        let mut i = line + 1;
        while i < self.0.len() {
            self.0[i] = self.0[i].wrapping_add_signed(delta);
            i += i.isolate_lowest_one();
        }
    }

    /// Rows above {line}.
    fn before(&self, line: usize) -> u32 {
        let mut sum = 0;
        let mut i = line;
        while i > 0 {
            sum += self.0[i];
            i -= i.isolate_lowest_one();
        }
        sum
    }

    /// The line {row} falls in and how many of that line's rows come before
    /// it. A line with no rows is never the answer, and nothing is past the
    /// last row.
    fn locate(&self, row: u32) -> Option<(usize, u32)> {
        let len = self.0.len() - 1;
        let mut line = 0;
        let mut rest = row;
        let mut step = (len + 1).next_power_of_two() / 2;
        while step > 0 {
            let next = line + step;
            if next <= len && self.0[next] <= rest {
                line = next;
                rest -= self.0[next];
            }
            step /= 2;
        }
        (line < len).then_some((line, rest))
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::Arc;

    use maki_agent::{SnapshotSpan, SpanStyle};
    use maki_lua::{InputRule, PatternRule, SyntaxRule};
    use test_case::test_case;

    use super::{Layout, Row, RowSums, Shape, wrap};
    use crate::decor_worker;
    use crate::text_buffer::fuzz::{self, Driver, Odds};
    use crate::text_buffer::test_support::{settle_syntax, text_edit};
    use crate::text_buffer::{Decoration, Mark, Reveal, RuleSet, TextBuffer, VirtTextPos};

    const WIDTH: usize = 8;
    const MAX_WIDTH: usize = 20;
    const FUZZ_SEEDS: u64 = 4;
    const WIDTH_CHANGE_ODDS: usize = 40;
    const BURST_ODDS: usize = 150;
    const LAG_ODDS: usize = 3;
    const PARSE_ODDS: usize = 4;
    const POLL_ODDS: usize = 3;
    const RULE_CHIP: &str = r"@\w+";
    const RULE_GHOST: &str = r"#\d";
    const SYNTAX_LANG: &str = "markdown";
    const SYNTAX_INLINE_LANG: &str = "markdown_inline";
    /// Fences hidden while the cursor is off them, and each code block a chip
    /// with a badge before it.
    const SYNTAX_BLOCK: &str = "(fenced_code_block_delimiter) @hidden (fenced_code_block) @chip";
    const SYNTAX_INLINE: &str = "(strong_emphasis (emphasis_delimiter) @hidden)";
    /// At {WIDTH} the second line wraps into four rows, and the last line ends
    /// on a full row, so the caret starts out on a row of its own.
    const START: &str = "short\nlong enough to wrap twice\n\nend\nwide 一二三四五";

    type Seen = (usize, usize, Range<usize>, Range<usize>);
    type Observed = (Vec<u32>, u32, Vec<Seen>);

    fn seen(row: Row) -> Seen {
        (row.line, row.index, row.bytes, row.text)
    }

    /// Everything a reader gets out of a layout. Looking up every row on its
    /// own goes through the running totals, so a stale total shows up even
    /// when the per-line counts are right.
    fn observe(layout: &Layout, buffer: &TextBuffer) -> Observed {
        let rows = (0..layout.total_rows())
            .filter_map(|first| layout.rows(buffer, first, 1).next())
            .map(seen)
            .collect();
        (layout.counts.clone(), layout.cursor_row(buffer), rows)
    }

    /// Like [`observe`], but every row in one lookup and every running
    /// total read on its own, which stays cheap on a long buffer.
    fn observe_all(layout: &Layout, buffer: &TextBuffer) -> (Observed, Vec<u32>) {
        let rows = layout
            .rows(buffer, 0, layout.total_rows())
            .map(seen)
            .collect();
        let sums = (0..=layout.counts.len())
            .map(|line| layout.sums.before(line))
            .collect();
        (
            (layout.counts.clone(), layout.cursor_row(buffer), rows),
            sums,
        )
    }

    /// Swaps the first match of each `old` for its `new`, all as one version.
    fn swap_all(buffer: &mut TextBuffer, swaps: &[(&str, &str)]) {
        let edits = swaps
            .iter()
            .map(|&(old, new)| {
                let start = buffer.value().find(old).unwrap();
                text_edit(start, start + old.len(), new)
            })
            .collect();
        buffer
            .apply_edits(buffer.version(), edits, None, None, None)
            .unwrap();
    }

    fn synced(buffer: &TextBuffer, width: usize) -> Layout {
        let mut layout = Layout::default();
        layout.sync(buffer, width);
        layout
    }

    /// A few readable cases. `random_ops_catch_up_like_a_fresh_build` covers
    /// the rest, marks and widths included.
    #[test_case(|b| { b.set_cursor(0, 5); b.insert_text("wrap"); } ; "typing_that_wraps_a_line")]
    #[test_case(|b| swap_all(b, &[("ort", "\n"), ("twice", "and twice\n"), ("五", "五 more")]) ; "one_version_adding_lines_apart")]
    #[test_case(|b| { let seen = b.version(); b.set_cursor(1, 0); b.remove_char(); b.move_to_end(); while b.changes_since(seen).is_some() { b.push_char('x'); } } ; "more_edits_than_the_log_holds")]
    fn catching_up_matches_a_fresh_build(edit: fn(&mut TextBuffer)) {
        let mut buffer = TextBuffer::new(START.into());
        buffer.move_to_end();
        let mut layout = synced(&buffer, WIDTH);

        edit(&mut buffer);
        layout.sync(&buffer, WIDTH);

        assert_eq!(
            observe(&layout, &buffer),
            observe(&synced(&buffer, WIDTH), &buffer)
        );
    }

    /// Rows as `(start, end)` bytes, and whether a caret at the end needs a
    /// row of its own. Conceals are `(start, end, columns drawn)`.
    #[test_case("abcdef", &[(3, 2)], &[], 5 => (vec![(0, 3), (3, 6)], true) ; "virtual_text_moves_down_with_the_char_after_it")]
    #[test_case("abc", &[(3, 2)], &[], 4 => (vec![(0, 3), (3, 3)], false) ; "virtual_text_at_the_end_moves_down_alone")]
    #[test_case("ab", &[(2, 2)], &[], 4 => (vec![(0, 2)], false) ; "a_caret_before_virtual_text_on_a_full_row_needs_no_row")]
    #[test_case("abcd", &[], &[], 4 => (vec![(0, 4)], true) ; "a_caret_after_a_full_row_needs_a_row")]
    #[test_case("abcdefgh", &[], &[(2, 6, 1)], 4 => (vec![(0, 7), (7, 8)], false) ; "a_conceal_takes_the_columns_of_its_text")]
    #[test_case("abcdef", &[], &[(1, 5, 3)], 3 => (vec![(0, 1), (1, 5), (5, 6)], false) ; "a_conceal_moves_down_whole")]
    #[test_case("abcdef", &[], &[(0, 6, 0)], 4 => (vec![(0, 6)], false) ; "a_hidden_line_still_takes_a_row")]
    #[test_case("ab\ncd", &[], &[(1, 4, 1)], 4 => (vec![(0, 5)], false) ; "a_conceal_over_a_line_break_draws_one_line")]
    #[test_case("abcd", &[(2, 5)], &[(1, 3, 1)], 3 => (vec![(0, 4)], true) ; "virtual_text_inside_a_conceal_is_hidden_with_it")]
    fn wrapping(
        line: &str,
        virts: &[(usize, usize)],
        conceals: &[(usize, usize, usize)],
        width: usize,
    ) -> (Vec<(usize, usize)>, bool) {
        let shape = Shape {
            conceals: conceals
                .iter()
                .map(|&(start, end, width)| (start..end, width))
                .collect(),
            virts: virts.to_vec(),
        };
        let mut rows = Vec::new();
        let caret_row = wrap(line, &shape, width, |row| rows.push((row.start, row.end)));
        (rows, caret_row)
    }

    const PLUGIN: &str = "test";
    const NS: &str = "ghosts";

    fn inline_virt(text: &str) -> Decoration {
        Decoration {
            virt_text: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
            virt_text_pos: VirtTextPos::Inline,
            ..Decoration::default()
        }
    }

    /// Ghost text in the middle of a wrapping line, at the end of a line and
    /// before a wide char, plus a chip whose atomic range runs from line 1
    /// down to line 3. Returns where the virtual text sits.
    fn ghosts(buffer: &mut TextBuffer) -> Vec<Range<usize>> {
        let ghost = Arc::new(inline_virt("ghost"));
        let chip = Arc::new(Decoration {
            atomic: true,
            ..(*ghost).clone()
        });
        let value = buffer.value();
        let at = |needle: &str| value.find(needle).unwrap();
        let mark = |start, stop, decoration: &Arc<Decoration>| Mark {
            start,
            stop,
            decoration: Arc::clone(decoration),
        };
        let marks = vec![
            mark(at("enough"), None, &ghost),
            mark(at("\n\nend"), None, &ghost),
            mark(at("三"), None, &ghost),
            mark(at("to wrap"), Some(at("nd\n")), &chip),
        ];
        buffer
            .set_marks(buffer.version(), PLUGIN, NS, marks)
            .unwrap()
    }

    /// A fold hides "to wrap twice" down to the "e" of "end", which joins
    /// lines 1 to 3 into one. The "sh" of "short" is hidden for good, and
    /// "wide 一二" only while the cursor is not on it, which saves the last
    /// line a row.
    fn conceals(buffer: &mut TextBuffer) -> Vec<Range<usize>> {
        let conceal = |text: &str, reveal, atomic| {
            Arc::new(Decoration {
                conceal: Some(text.into()),
                reveal,
                atomic,
                ..Decoration::default()
            })
        };
        let value = buffer.value();
        let at = |needle: &str| value.find(needle).unwrap();
        let mark = |bytes: Range<usize>, decoration| Mark {
            start: bytes.start,
            stop: Some(bytes.end),
            decoration,
        };
        let wide = at("wide");
        let marks = vec![
            mark(
                at("to wrap")..at("nd\n"),
                conceal("[+3]", Reveal::Never, true),
            ),
            mark(0..2, conceal("", Reveal::Never, false)),
            mark(
                wide..wide + "wide 一二".len(),
                conceal("", Reveal::Cursor, false),
            ),
        ];
        buffer
            .set_marks(buffer.version(), PLUGIN, NS, marks)
            .unwrap()
    }

    #[test]
    fn a_fold_over_line_breaks_draws_its_lines_as_one() {
        let mut buffer = TextBuffer::new(START.into());
        conceals(&mut buffer);
        buffer.move_to_end();
        let layout = synced(&buffer, WIDTH);
        let mut display_lines: Vec<_> = layout
            .rows(&buffer, 0, layout.total_rows())
            .map(|row| (row.line, row.text))
            .collect();
        display_lines.dedup();
        let at = |needle: &str| START.find(needle).unwrap();
        let expected = vec![
            (0, 0..at("\nlong")),
            (1, at("long")..at("\nwide")),
            (4, at("wide")..START.len()),
        ];
        assert_eq!(display_lines, expected);
    }

    /// A fresh build that missed the reveal would agree with a catch-up that
    /// missed it too, so the rows must also change, and come back once the
    /// cursor leaves.
    #[test]
    fn the_cursor_moving_in_and_out_of_a_revealed_conceal_recounts_its_lines() {
        let mut buffer = TextBuffer::new(START.into());
        conceals(&mut buffer);
        buffer.move_to_end();
        let mut layout = synced(&buffer, WIDTH);
        let hidden = observe(&layout, &buffer);

        buffer.set_cursor(4, 0);
        layout.sync(&buffer, WIDTH);
        let revealed = observe(&synced(&buffer, WIDTH), &buffer);
        assert_ne!(revealed, hidden);
        assert_eq!(observe(&layout, &buffer), revealed);

        buffer.move_to_end();
        layout.sync(&buffer, WIDTH);
        assert_eq!(observe(&layout, &buffer), hidden);
    }

    /// Placing or clearing marks leaves the text and its version alone, so
    /// only an explicit recount can tell the layout.
    #[test_case(ghosts ; "virtual_text")]
    #[test_case(conceals ; "conceals")]
    fn placing_and_clearing_marks_recounts_their_lines(
        place: fn(&mut TextBuffer) -> Vec<Range<usize>>,
    ) {
        let mut buffer = TextBuffer::new(START.into());
        buffer.move_to_end();
        let mut layout = synced(&buffer, WIDTH);
        let plain = observe(&layout, &buffer);

        let placed = place(&mut buffer);
        layout.relayout(&buffer, &placed);
        let marked = observe(&synced(&buffer, WIDTH), &buffer);
        assert_ne!(marked, plain);
        assert_eq!(observe(&layout, &buffer), marked);

        let cleared = buffer
            .set_marks(buffer.version(), PLUGIN, NS, Vec::new())
            .unwrap();
        layout.relayout(&buffer, &cleared);
        assert_eq!(observe(&layout, &buffer), plain);
    }

    /// A line a conceal joined to the one above counts zero rows, and the
    /// totals have to step over it, or it would claim a row of its own.
    #[test]
    fn row_sums_match_a_plain_running_total() {
        let counts: [u32; _] = [0, 2, 1, 0, 3, 0];
        let sums = RowSums::new(&counts);
        let mut expected = counts
            .iter()
            .enumerate()
            .flat_map(|(line, &rows)| (0..rows).map(move |skip| (line, skip)));
        for row in 0..=counts.iter().sum::<u32>() {
            assert_eq!(sums.locate(row), expected.next());
        }
        for line in 0..=counts.len() {
            assert_eq!(sums.before(line), counts[..line].iter().sum::<u32>());
        }
    }

    /// A conceal and inline virtual text from rules, which are matched again
    /// from the text instead of moved with it.
    fn chip_rules() -> Arc<RuleSet> {
        let chip = Decoration {
            conceal: Some("@".into()),
            atomic: true,
            ..Decoration::default()
        };
        let ghost = inline_virt("ghost");
        let rule = |pattern, mark| InputRule::Pattern(PatternRule::new(pattern, mark).unwrap());
        RuleSet::with_plugins(&[rule(RULE_CHIP, chip), rule(RULE_GHOST, ghost)])
    }

    /// Marks from a tree-sitter parse, which lag behind the text until the
    /// worker is back and move like placed marks meanwhile.
    fn syntax_rules() -> Arc<RuleSet> {
        let hidden = Decoration {
            conceal: Some(String::new()),
            reveal: Reveal::Cursor,
            ..Decoration::default()
        };
        let chip = Decoration {
            atomic: true,
            ..inline_virt("code")
        };
        let rule = SyntaxRule::new(
            SYNTAX_LANG,
            vec![
                (SYNTAX_LANG.into(), SYNTAX_BLOCK.into()),
                (SYNTAX_INLINE_LANG.into(), SYNTAX_INLINE.into()),
            ],
            vec![("hidden".into(), hidden), ("chip".into(), chip)],
        )
        .unwrap();
        RuleSet::with_plugins(&[InputRule::Syntax(Arc::new(rule))])
    }

    /// Random edits, marks, undo, cursor moves and widths. The layout is
    /// left behind now and then, for a few ops or for about as many versions
    /// as the log holds, and however it caught up it has to read like one
    /// built fresh. Any window of rows reads like the same rows of the whole.
    #[test]
    fn random_ops_catch_up_like_a_fresh_build() {
        for seed in fuzz::seeds(FUZZ_SEEDS) {
            let mut driver = Driver::new(seed);
            decor_worker::manual();
            let rule_sets: [fn() -> Option<Arc<RuleSet>>; _] =
                [|| None, || Some(chip_rules()), || Some(syntax_rules())];
            if let Some(rules) = rule_sets[seed as usize % rule_sets.len()]() {
                driver.buffer.set_rules(rules);
            }
            let mut width = WIDTH;
            let mut layout = synced(&driver.buffer, width);
            for _ in 0..fuzz::OPS_PER_SEED {
                if driver.rng.one_in(WIDTH_CHANGE_ODDS) {
                    width = 1 + driver.rng.usize(..MAX_WIDTH);
                    driver.note(format!("width {width}"));
                }
                if driver.rng.one_in(BURST_ODDS) {
                    driver.note("burst".into());
                    driver.text_burst();
                }
                let moved = driver.step();
                layout.relayout(&driver.buffer, &moved);
                if driver.rng.one_in(POLL_ODDS) {
                    driver.note("poll_syntax".into());
                    if let Some(changed) = driver.buffer.poll_syntax() {
                        layout.relayout(&driver.buffer, &changed);
                    }
                }
                if driver.rng.one_in(PARSE_ODDS) {
                    driver.note("run_queued".into());
                    decor_worker::run_queued();
                }
                if driver.rng.one_in(LAG_ODDS) {
                    driver.note("lag".into());
                    continue;
                }
                layout.sync(&driver.buffer, width);
                let observed = observe_all(&layout, &driver.buffer);
                assert_eq!(
                    observed,
                    observe_all(&synced(&driver.buffer, width), &driver.buffer),
                    "width {width}, {}",
                    driver.context()
                );
                let ((_, _, rows), _) = &observed;
                let first = driver.rng.usize(..rows.len());
                let count = 1 + driver.rng.usize(..rows.len() - first);
                let window: Vec<_> = layout
                    .rows(&driver.buffer, first as u32, count as u32)
                    .map(seen)
                    .collect();
                assert_eq!(
                    window,
                    rows[first..first + count],
                    "rows {first}+{count}, width {width}, {}",
                    driver.context()
                );
            }
        }
    }

    /// A parse the worker has not replaced lags further behind with every
    /// edit. Once the log no longer reaches back to it, its marks can no
    /// longer be moved to the text and are not shown at all, so the lines
    /// they hid have to be counted again. A parse handed out before that
    /// and landing after hides them again, though it only says where its
    /// marks differ from the ones nobody saw.
    #[test_case(false ; "syntax_marks_falling_off_the_log_recount_their_lines")]
    #[test_case(true ; "a_parse_landing_after_the_last_one_fell_off_the_log_recounts_its_lines")]
    fn syntax_marks_past_the_log(parse_in_flight: bool) {
        const FENCED: &str = "```\na\n```\n\nb";
        const NARROW: usize = 2;
        decor_worker::manual();
        let mut buffer = TextBuffer::new(String::new());
        buffer.set_rules(syntax_rules());
        buffer.set_value(FENCED);
        settle_syntax(&mut buffer);
        buffer.move_to_end();
        let mut layout = synced(&buffer, NARROW);
        let parsed = buffer.version();
        buffer.push_char('x');
        if parse_in_flight {
            buffer.poll_syntax();
        }
        while buffer.changes_since(parsed).is_some() {
            buffer.push_char('x');
            layout.sync(&buffer, NARROW);
        }
        if parse_in_flight {
            decor_worker::run_queued();
            let changed = buffer.poll_syntax().unwrap();
            layout.relayout(&buffer, &changed);
            layout.sync(&buffer, NARROW);
        }
        assert_eq!(
            observe(&layout, &buffer),
            observe(&synced(&buffer, NARROW), &buffer)
        );
    }
}
