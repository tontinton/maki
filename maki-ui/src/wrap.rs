//! Allocation-free line wrap measuring.
//!
//! Replays ratatui's `WordWrapper` (`ratatui-widgets/src/reflow.rs`) for the
//! `Wrap { trim: false }` case maki always uses, so row counts match what
//! `Paragraph` really draws without cloning a single line or span. With `trim`
//! pinned to false the machine shrinks to a few scalars plus the queue of
//! pending whitespace widths, the only scratch we keep.

use std::mem;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::Line;

/// Why a row ended: `Word` when the renderer swallowed whitespace at the
/// boundary, `Char` when the break cut through a word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Break {
    Word,
    Char,
}

/// Rows `lines` occupy at `width`. A zero width reports one row per line where
/// ratatui reports nothing, because that is what maki's layout expects.
///
/// Public so `benches/wrap.rs` can race it against `Paragraph::line_count`.
pub fn total_rows(lines: &[Line<'_>], width: u16) -> u16 {
    let mut measure = Measure::new(width);
    lines
        .iter()
        .fold(0, |rows, line| rows.saturating_add(measure.rows(line)))
}

/// Measures one line at a time, keeping the scan machine between them, so a
/// caller that walks a segment in pieces pays for measuring it once instead of
/// once per piece.
pub(crate) struct Measure {
    /// Absent at zero width, where every line counts as one row.
    scan: Option<Scan>,
}

impl Measure {
    pub(crate) fn new(width: u16) -> Self {
        Self {
            scan: (width > 0).then(|| Scan::new(u32::from(width))),
        }
    }

    pub(crate) fn rows(&mut self, line: &Line<'_>) -> u16 {
        self.scan
            .as_mut()
            .map_or(1, |scan| scan.run(line, &mut |_| {}))
    }
}

/// Calls `on_break` once per row boundary inside `line`, so exactly one less
/// than the rows the line draws as.
///
/// The scan reports one break too many when a row ends on whitespace the
/// renderer swallows: that break closes the line instead of opening a row, so a
/// caller walking rows would run ahead of what is drawn. Only the last break can
/// be spurious and the row count is what gives it away, so every break waits
/// here until the next one vouches for it. This stays out of [`Scan::run`]
/// because [`total_rows`] runs on every resize and pays nothing for it.
pub(crate) fn breaks(line: &Line<'_>, width: u16, mut on_break: impl FnMut(Break)) {
    if width == 0 {
        return;
    }
    let mut pending = None;
    let mut seen = 0;
    let rows = Scan::new(u32::from(width)).run(line, &mut |kind| {
        seen += 1;
        if let Some(prev) = pending.replace(kind) {
            on_break(prev);
        }
    });
    if let Some(kind) = pending
        && seen < rows
    {
        on_break(kind);
    }
}

/// The whitespace widths ratatui parks in a `VecDeque`, plus the scalars its
/// `process_input` keeps on the stack. The `_empty` flags stand in for
/// `Vec::is_empty` on ratatui's grapheme buffers, which is not the same as a
/// zero width: a zero width grapheme fills no cell yet still fills a buffer.
struct Scan {
    whitespace: Vec<u32>,
    whitespace_width: u32,
    word_width: u32,
    word_empty: bool,
    line_width: u32,
    line_empty: bool,
    prev_was_word: bool,
    max: u32,
}

impl Scan {
    fn new(max: u32) -> Self {
        Self {
            whitespace: Vec::new(),
            whitespace_width: 0,
            word_width: 0,
            word_empty: true,
            line_width: 0,
            line_empty: true,
            prev_was_word: false,
            max,
        }
    }

    /// Fresh machine for the next line, reusing the whitespace allocation.
    fn reset(&mut self) {
        let mut scratch = mem::take(&mut self.whitespace);
        scratch.clear();
        *self = Self {
            whitespace: scratch,
            ..Self::new(self.max)
        };
    }

    fn run(&mut self, line: &Line<'_>, on_break: &mut impl FnMut(Break)) -> u16 {
        self.reset();

        let mut rows = 0u16;
        let mut broke = |kind| {
            rows = rows.saturating_add(1);
            on_break(kind);
        };

        for span in &line.spans {
            let content = span.content.as_ref();
            // Grapheme segmentation is 80x a byte walk and buys nothing here:
            // every non control ASCII byte is one grapheme of width 1, and space
            // is the only ASCII whitespace left after the control filter. `\r\n`
            // is a single cluster, but it gets dropped either way.
            if content.is_ascii() {
                for &byte in content.as_bytes() {
                    if !byte.is_ascii_control() {
                        self.step(1, byte == b' ', &mut broke);
                    }
                }
            } else {
                for grapheme in span.styled_graphemes(Style::default()) {
                    let width = u32::from(grapheme.symbol.cell_width());
                    self.step(width, grapheme.is_whitespace(), &mut broke);
                }
            }
        }

        if !self.line_empty || !self.whitespace.is_empty() || !self.word_empty {
            rows = rows.saturating_add(1);
        }
        rows.max(1)
    }

    fn step(&mut self, symbol_width: u32, is_whitespace: bool, on_break: &mut impl FnMut(Break)) {
        if symbol_width > self.max {
            return;
        }

        let word_found = self.prev_was_word && is_whitespace;
        let untrimmed_overflow =
            self.line_empty && self.word_width + self.whitespace_width + symbol_width > self.max;
        if word_found || untrimmed_overflow {
            self.flush_segment();
        }

        let line_full = self.line_width >= self.max;
        let word_overflow = symbol_width > 0
            && self.line_width + self.whitespace_width + self.word_width >= self.max;
        if line_full || word_overflow {
            let mut remaining = self.max.saturating_sub(self.line_width);
            self.line_width = 0;
            self.line_empty = true;

            // Indexed on purpose: iterating `&self.whitespace` while writing
            // `self.whitespace_width` costs the whole scan 30%, because the
            // live borrow stops the other fields from staying in registers.
            let mut dropped = 0;
            while let Some(&width) = self.whitespace.get(dropped) {
                if width > remaining {
                    break;
                }
                self.whitespace_width -= width;
                remaining -= width;
                dropped += 1;
            }
            self.whitespace.drain(..dropped);

            // A row ending on whitespace swallows it, and swallows the grapheme
            // that forced the break too when that one is whitespace as well.
            if is_whitespace && self.whitespace.is_empty() {
                on_break(Break::Word);
                return;
            }
            on_break(if dropped > 0 {
                Break::Word
            } else {
                Break::Char
            });
        }

        if is_whitespace {
            self.whitespace_width += symbol_width;
            self.whitespace.push(symbol_width);
        } else {
            self.word_width += symbol_width;
            self.word_empty = false;
        }
        self.prev_was_word = !is_whitespace;
    }

    fn flush_segment(&mut self) {
        self.line_width += self.whitespace_width + self.word_width;
        self.line_empty &= self.whitespace.is_empty() && self.word_empty;
        self.whitespace.clear();
        self.whitespace_width = 0;
        self.word_width = 0;
        self.word_empty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{Break, breaks, total_rows};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Wrap};
    use std::slice;
    use test_case::test_case;

    const CORPUS: &[&str] = &[
        "",
        " ",
        "   ",
        "a",
        "🎉",
        "你",
        "hello world",
        "the quick brown fox jumps over the lazy dog",
        "supercalifragilisticexpialidociousandthensome",
        "   leading whitespace",
        "trailing whitespace   ",
        "many     spaces     between     words",
        "\ttabbed\tcolumns\there",
        "你好世界，这是一段中文文本",
        "a你b好c世d界",
        "emoji 🎉 party 👨‍👩‍👧‍👦 family",
        "nbsp\u{00a0}joined\u{00a0}words",
        "zwsp\u{200b}split\u{200b}here",
        "ideographic\u{3000}space\u{3000}wide",
        "mixed 漢字 and ascii words together",
        "xx xx xx xx xx xx xx xx",
        "control\u{7}chars\u{7f}stripped",
        "carriage\r\nreturn cluster",
    ];

    fn ratatui_rows(lines: &[Line<'_>], width: u16) -> u16 {
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }

    fn corpus_lines() -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = CORPUS.iter().copied().map(Line::from).collect();
        lines.push(Line::from(vec![
            Span::raw("super"),
            Span::raw("cali"),
            Span::raw("fragilistic "),
            Span::raw("expi"),
            Span::raw("alidocious and "),
            Span::raw("more"),
        ]));
        lines
    }

    /// Line by line pins the state machine, the whole corpus in one call pins
    /// that nothing leaks from a line into the next, and every break has to open
    /// a row, else [`crate::selection`] marks the wrong ones.
    #[test]
    fn matches_ratatui_wrapping() {
        let lines = corpus_lines();
        for width in (1..=40).chain([80, 200]) {
            for line in &lines {
                let rows = total_rows(slice::from_ref(line), width);
                assert_eq!(
                    rows,
                    ratatui_rows(slice::from_ref(line), width),
                    "line {line:?} at width {width}"
                );
                assert_eq!(
                    kinds(line, width).len() + 1,
                    rows as usize,
                    "breaks of {line:?} at width {width}"
                );
            }
            assert_eq!(
                total_rows(&lines, width),
                ratatui_rows(&lines, width),
                "corpus at width {width}"
            );
        }
    }

    /// Canaries for a ratatui change the differential test would happily follow,
    /// plus the zero width case where we differ on purpose.
    #[test_case(&["hello", "world"],  0, 2 ; "zero_width_returns_line_count")]
    #[test_case(&["", "a", ""],      40, 3 ; "empty_lines_count_as_one")]
    #[test_case(&[&"a".repeat(80)],  80, 1 ; "exactly_width_no_wrap")]
    #[test_case(&[&"a".repeat(81)],  80, 2 ; "one_over_width_wraps")]
    fn total_rows_cases(input: &[&str], width: u16, expected: u16) {
        let lines: Vec<Line<'_>> = input.iter().copied().map(Line::from).collect();
        assert_eq!(total_rows(&lines, width), expected);
    }

    #[test_case("hello world",    5, &[Break::Word] ; "space_swallowed_at_boundary")]
    #[test_case("helloworld",     5, &[Break::Char] ; "cut_through_word")]
    #[test_case("a    b",         4, &[Break::Word] ; "run_of_spaces_dropped")]
    #[test_case("   ",            2, &[]            ; "swallowed_trailing_space_opens_no_row")]
    #[test_case("ab\tcd",         3, &[Break::Char] ; "tab_is_stripped_as_control")]
    #[test_case("漢 字字",         3, &[Break::Word] ; "cjk_with_space_word_wrap")]
    #[test_case("漢字漢字",        3, &[Break::Char, Break::Char] ; "cjk_double_width_overflows")]
    #[test_case("hi worldaaaaaa", 5, &[Break::Word, Break::Char, Break::Char] ; "word_then_char")]
    fn break_kinds(input: &str, width: u16, expected: &[Break]) {
        assert_eq!(kinds(&Line::from(input), width), expected);
    }

    #[test]
    fn breaks_see_through_span_boundaries() {
        let line = Line::from(vec![Span::raw("hello "), Span::raw("world")]);
        assert_eq!(kinds(&line, 6), [Break::Word]);
    }

    fn kinds(line: &Line<'_>, width: u16) -> Vec<Break> {
        let mut kinds = Vec::new();
        breaks(line, width, |kind| kinds.push(kind));
        kinds
    }
}
