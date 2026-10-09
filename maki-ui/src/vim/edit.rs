//! Operators, the regions they act on, and the one register they share. Like
//! the motions, these are pure functions over the value and byte offsets.

use std::ops::Range;

use super::motion::{self, Kind, Motion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operator {
    Delete,
    Change,
    Yank,
}

impl Operator {
    pub(super) fn from_char(c: char) -> Option<Self> {
        match c {
            'd' => Some(Self::Delete),
            'c' => Some(Self::Change),
            'y' => Some(Self::Yank),
            _ => None,
        }
    }

    /// The key that started it, which the half-typed command shows.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Delete => "d",
            Self::Change => "c",
            Self::Yank => "y",
        }
    }

    /// The same, waiting for the second `g` of `gg`.
    pub(super) const fn label_with_g(self) -> &'static str {
        match self {
            Self::Delete => "dg",
            Self::Change => "cg",
            Self::Yank => "yg",
        }
    }
}

/// The unnamed register: what the last delete, change or yank took.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Register {
    pub(super) text: String,
    /// Whole lines, which a put places on lines of their own.
    pub(super) linewise: bool,
}

/// The text an operator acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Region {
    Chars(Range<usize>),
    /// Whole lines, from the start of the first to the end of the last. The
    /// newlines around them are not part of it.
    Lines(Range<usize>),
}

impl Region {
    /// The lines {from} and {to} are on, and every line between.
    pub(super) fn lines(text: &str, from: usize, to: usize) -> Self {
        Self::Lines(motion::line_start(text, from)..motion::line_end(text, to))
    }

    /// What {op} covers over {motion} from {at} once the motion landed on
    /// {target}. A range that crosses lines is adjusted the way vim adjusts
    /// it:
    ///
    /// - An exclusive motion that ends at the start of a later line, such as
    ///   `b` from column 0, stops at the end of the line before. A range that
    ///   starts in the indent takes whole lines instead, so `db` takes the
    ///   line above rather than joining two lines and their indents.
    /// - A delete that would leave a line of blanks behind takes whole lines,
    ///   so `de` from an empty line removes that line too.
    pub(super) fn of_motion(
        text: &str,
        at: usize,
        motion: Motion,
        target: usize,
        op: Operator,
    ) -> Self {
        let (low, high) = (at.min(target), at.max(target));
        let range = match motion.kind() {
            Kind::Linewise => return Self::lines(text, low, high),
            Kind::Inclusive => low..motion::after(text, high),
            Kind::Exclusive if low < high && motion::line_start(text, high) == high => {
                if in_indent(text, low) {
                    return Self::lines(text, low, high - 1);
                }
                low..high - 1
            }
            Kind::Exclusive => low..high,
        };
        if op == Operator::Delete && leaves_blank_line(text, &range) {
            return Self::lines(text, range.start, range.end);
        }
        Self::Chars(range)
    }

    /// What an operator takes over `w`, which is not where `w` moves the
    /// cursor.
    ///
    /// It stops at the end of the line, so `dw` on the last word of a line
    /// leaves the newline alone. `cw` on a word changes only up to its end,
    /// the way `ce` does. On an empty line it takes the line itself, unless
    /// that is the last line, where vim finds nothing to take.
    pub(super) fn of_word(text: &str, at: usize, op: Operator) -> Self {
        let end = motion::line_end(text, at);
        if motion::line_start(text, at) == end {
            return if end < text.len() {
                Self::lines(text, at, at)
            } else {
                Self::Chars(at..at)
            };
        }
        let on_word = motion::char_at(text, at).is_some_and(|c| !c.is_whitespace());
        if op == Operator::Change && on_word {
            return Self::Chars(at..motion::after(text, motion::word_end(text, at, true)));
        }
        Self::Chars(at..motion::word_forward(text, at).min(end))
    }

    pub(super) fn is_empty(&self) -> bool {
        matches!(self, Self::Chars(range) if range.is_empty())
    }

    pub(super) fn yank(&self, text: &str) -> Register {
        let (range, linewise) = match self {
            Self::Chars(range) => (range, false),
            Self::Lines(range) => (range, true),
        };
        Register {
            text: text[range.clone()].to_owned(),
            linewise,
        }
    }

    /// The bytes a delete takes out. Whole lines take one newline with them:
    /// the one after the last line, or the one before the first when the
    /// lines run to the end of the text.
    pub(super) fn deleted(&self, text: &str) -> Range<usize> {
        match self {
            Self::Chars(range) => range.clone(),
            Self::Lines(range) if range.end < text.len() => range.start..range.end + 1,
            Self::Lines(range) => range.start.saturating_sub(1)..range.end,
        }
    }

    /// The bytes a change replaces. Whole lines keep their newlines, so one
    /// empty line is left to type on.
    pub(super) fn changed(&self) -> Range<usize> {
        match self {
            Self::Chars(range) | Self::Lines(range) => range.clone(),
        }
    }
}

/// Whether only blanks come before {at} on its line.
fn in_indent(text: &str, at: usize) -> bool {
    text[motion::line_start(text, at)..at]
        .chars()
        .all(char::is_whitespace)
}

/// Whether deleting {range}, which spans lines, leaves only blanks on the line
/// it ends up as.
fn leaves_blank_line(text: &str, range: &Range<usize>) -> bool {
    text[range.clone()].contains('\n')
        && in_indent(text, range.start)
        && text[range.end..motion::line_end(text, range.end)]
            .chars()
            .all(char::is_whitespace)
}

/// {text} with {range} replaced by {insert}. `None` when the range is not on
/// character boundaries, which only a bug in this module can cause, so it is
/// logged and the edit dropped.
pub(super) fn splice(text: &str, range: Range<usize>, insert: &str) -> Option<String> {
    let parts = (range.start <= range.end)
        .then(|| text.get(..range.start).zip(text.get(range.end..)))
        .flatten();
    let Some((head, tail)) = parts else {
        tracing::warn!(
            start = range.start,
            stop = range.end,
            len = text.len(),
            "vim edit range refused"
        );
        return None;
    };
    let mut out = String::with_capacity(head.len() + insert.len() + tail.len());
    out.push_str(head);
    out.push_str(insert);
    out.push_str(tail);
    Some(out)
}

/// {text} with {register} put after the cursor at {at}, or before it, and
/// where the cursor lands: on the last character put, the first one of
/// several lines put, or the first non-blank of whole lines.
///
/// `None` when the register holds nothing to put.
pub(super) fn put(
    text: &str,
    at: usize,
    register: &Register,
    after: bool,
) -> Option<(String, usize)> {
    if register.linewise {
        let (pos, insert) = if after {
            (motion::line_end(text, at), format!("\n{}", register.text))
        } else {
            (motion::line_start(text, at), format!("{}\n", register.text))
        };
        let next = splice(text, pos..pos, &insert)?;
        let first_line = if after { pos + 1 } else { pos };
        let cursor = motion::first_non_blank(&next, first_line);
        return Some((next, cursor));
    }
    if register.text.is_empty() {
        return None;
    }
    let pos = if after { motion::after(text, at) } else { at };
    let next = splice(text, pos..pos, &register.text)?;
    let cursor = if register.text.contains('\n') {
        pos
    } else {
        motion::prev(&next, pos + register.text.len()).unwrap_or(pos)
    };
    Some((next, cursor))
}

#[cfg(test)]
mod tests {
    use super::super::motion::tests::{mark, parse};
    use super::*;
    use test_case::test_case;

    fn region(marked: &str, motion: Motion, op: Operator) -> (String, Region) {
        let (text, at) = parse(marked);
        let target = motion.target(&text, at, motion::column(&text, at)).unwrap();
        let region = Region::of_motion(&text, at, motion, target, op);
        (text, region)
    }

    #[test_case("f|oo bar",   Motion::WordEnd,       "oo"       ; "e_is_inclusive")]
    #[test_case("foo |bar",   Motion::WordBackward,  "foo "     ; "b_is_exclusive")]
    #[test_case("a|bc\nd",    Motion::LineEnd,       "bc"       ; "dollar_takes_the_last_char")]
    #[test_case("ab|c",       Motion::Right,         "c"        ; "l_takes_the_last_char")]
    #[test_case("a|b\ncd\ne", Motion::Down,          "ab\ncd"   ; "j_takes_both_lines")]
    #[test_case("a\nb|c",     Motion::FirstLine,     "a\nbc"    ; "gg_takes_every_line_up")]
    fn a_motion_covers(start: &str, motion: Motion, expected: &str) {
        let (text, region) = region(start, motion, Operator::Yank);
        assert_eq!(region.yank(&text).text, expected);
    }

    #[test]
    fn dollar_on_an_empty_line_covers_nothing() {
        let (_, region) = region("a\n|\nb", Motion::LineEnd, Operator::Yank);
        assert!(region.is_empty());
    }

    /// The register shows which way a range crossing lines was adjusted: the
    /// yank text, and whether it holds whole lines.
    #[test_case("  abc\n|  def",       Motion::WordBackward, Operator::Yank,   "  abc", true  ; "b_from_column_0_to_an_indented_word_takes_the_line")]
    #[test_case("abc\n\n|def",         Motion::WordBackward, Operator::Yank,   "",      true  ; "b_from_column_0_onto_an_empty_line_takes_that_line")]
    #[test_case("foo bar\n|  baz",     Motion::WordBackward, Operator::Yank,   "bar",   false ; "b_from_column_0_past_the_indent_stops_at_the_line_end")]
    #[test_case("foo b|ar\n  baz",     Motion::WordBackward, Operator::Yank,   "b",     false ; "b_inside_one_line_is_left_alone")]
    #[test_case("abc\n|\ndef",         Motion::WordEnd,      Operator::Delete, "\ndef", true  ; "a_delete_leaving_only_blanks_takes_the_lines")]
    #[test_case("abc\n|\ndef",         Motion::WordEnd,      Operator::Change, "\ndef", false ; "a_change_never_does")]
    #[test_case("abc\n|\ndef x",       Motion::WordEnd,      Operator::Delete, "\ndef", false ; "a_delete_leaving_text_stays_charwise")]
    #[test_case("abc\nx|y\ndef",       Motion::WordEnd,      Operator::Delete, "y\ndef", false ; "a_delete_starting_after_text_stays_charwise")]
    fn a_range_across_lines(
        start: &str,
        motion: Motion,
        op: Operator,
        expected: &str,
        linewise: bool,
    ) {
        let (text, region) = region(start, motion, op);
        assert_eq!(region.yank(&text), register(expected, linewise));
    }

    #[test_case("foo b|ar\nbaz",   Operator::Delete, "ar"    ; "dw_stops_at_the_line_end")]
    #[test_case("|foo  bar",       Operator::Delete, "foo  " ; "dw_takes_the_blanks_after")]
    #[test_case("|foo  bar",       Operator::Change, "foo"   ; "cw_acts_like_ce")]
    #[test_case("fo|o bar",        Operator::Change, "o"     ; "cw_on_a_word_end_takes_one_char")]
    #[test_case("foo|  bar",       Operator::Change, "  "    ; "cw_on_a_blank_takes_the_blanks")]
    #[test_case("a\n|\nb",         Operator::Delete, ""      ; "dw_on_an_empty_line_takes_the_line")]
    fn an_operator_over_w_covers(start: &str, op: Operator, expected: &str) {
        let (text, at) = parse(start);
        assert_eq!(Region::of_word(&text, at, op).yank(&text).text, expected);
    }

    #[test]
    fn dw_on_an_empty_line_is_linewise() {
        let (text, at) = parse("a\n|\nb");
        assert!(
            Region::of_word(&text, at, Operator::Delete)
                .yank(&text)
                .linewise
        );
    }

    #[test]
    fn dw_on_an_empty_last_line_covers_nothing() {
        let (text, at) = parse("a\n|");
        assert!(Region::of_word(&text, at, Operator::Delete).is_empty());
    }

    #[test_case("a\n|b\nc", "a\nc" ; "a_middle_line_takes_the_newline_after")]
    #[test_case("a\n|b",    "a"    ; "the_last_line_takes_the_newline_before")]
    #[test_case("|a",       ""     ; "the_only_line_leaves_an_empty_text")]
    fn deleting_a_line_leaves(start: &str, expected: &str) {
        let (text, at) = parse(start);
        let range = Region::lines(&text, at, at).deleted(&text);
        assert_eq!(splice(&text, range, "").unwrap(), expected);
    }

    fn register(text: &str, linewise: bool) -> Register {
        Register {
            text: text.into(),
            linewise,
        }
    }

    #[test_case("a|bc",    "XY",    false, true,  "abX|Yc"      ; "p_puts_after_the_cursor")]
    #[test_case("a|bc",    "XY",    false, false, "aX|Ybc"      ; "capital_p_puts_before_it")]
    #[test_case("|",       "XY",    false, true,  "X|Y"         ; "p_on_an_empty_line")]
    #[test_case("a|bc",    "X\nY",  false, true,  "ab|X\nYc"    ; "several_lines_put_the_cursor_on_the_first")]
    #[test_case("a|b\nc",  "  X",   true,  true,  "ab\n  |X\nc" ; "lines_go_below")]
    #[test_case("a\nc|d",  "X",     true,  false, "a\n|X\ncd"   ; "lines_go_above")]
    #[test_case("|",       "X",     true,  true,  "\n|X"        ; "lines_below_an_empty_text")]
    #[test_case("a|b",     "",      true,  true,  "ab\n|"       ; "an_empty_line")]
    fn putting(start: &str, put_text: &str, linewise: bool, after: bool, expected: &str) {
        let (text, at) = parse(start);
        let (next, cursor) = put(&text, at, &register(put_text, linewise), after).unwrap();
        assert_eq!(mark(&next, cursor), expected);
    }

    #[test]
    fn an_empty_register_puts_nothing() {
        assert_eq!(put("abc", 1, &Register::default(), true), None);
    }

    #[test_case(2, 1 ; "inverted")]
    #[test_case(1, 2 ; "inside_a_char")]
    fn splice_refuses_a_bad_range(start: usize, stop: usize) {
        assert_eq!(splice("漢", start..stop, ""), None);
    }
}
