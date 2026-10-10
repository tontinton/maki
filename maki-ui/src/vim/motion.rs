//! Motions as pure functions over the whole value and byte offsets into it.
//!
//! The value is the buffer's lines joined by `\n`. A `\n` sits where vim's
//! cursor would sit past a line's last character, and so does the end of the
//! text, so both count as blank. That one rule is what lets `w`, `b` and `e`
//! cross lines the way vim's do.

use crossterm::event::KeyCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Punctuation,
    Word,
}

fn class_at(text: &str, at: usize) -> Class {
    match char_at(text, at) {
        None => Class::Blank,
        Some(c) if c.is_whitespace() => Class::Blank,
        Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
        Some(_) => Class::Punctuation,
    }
}

pub(super) fn char_at(text: &str, at: usize) -> Option<char> {
    text.get(at..)?.chars().next()
}

pub(super) fn next(text: &str, at: usize) -> Option<usize> {
    char_at(text, at).map(|c| at + c.len_utf8())
}

pub(super) fn prev(text: &str, at: usize) -> Option<usize> {
    let c = text.get(..at)?.chars().next_back()?;
    Some(at - c.len_utf8())
}

pub(super) fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The offset of the `\n` that ends the line, or the end of the text.
pub(super) fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

fn is_empty_line(text: &str, at: usize) -> bool {
    line_start(text, at) == at && line_end(text, at) == at
}

/// Past the character at {at}, never past the end of its line. An inclusive
/// motion and a put after the cursor both end here.
pub(super) fn after(text: &str, at: usize) -> usize {
    match char_at(text, at) {
        Some(c) if c != '\n' => at + c.len_utf8(),
        _ => at,
    }
}

/// The last character of the line, or its start when it is empty. The
/// normal-mode cursor never goes further right than this.
pub(super) fn last_char(text: &str, at: usize) -> usize {
    let start = line_start(text, at);
    prev(text, line_end(text, at))
        .filter(|&p| p >= start)
        .unwrap_or(start)
}

/// `^`: the first non-blank character, or the last character of a line that
/// is all blanks.
pub(super) fn first_non_blank(text: &str, at: usize) -> usize {
    let start = line_start(text, at);
    text[start..line_end(text, at)]
        .find(|c: char| !c.is_whitespace())
        .map_or_else(|| last_char(text, at), |i| start + i)
}

/// Where `I` inserts: before the first non-blank, or at the end of a line that
/// is all blanks.
pub(super) fn indent_end(text: &str, at: usize) -> usize {
    let start = line_start(text, at);
    let end = line_end(text, at);
    text[start..end]
        .find(|c: char| !c.is_whitespace())
        .map_or(end, |i| start + i)
}

/// The cursor's column in characters, the unit `TextBuffer` counts in.
pub(super) fn column(text: &str, at: usize) -> usize {
    text[line_start(text, at)..at].chars().count()
}

/// The character {col} columns into the line starting at {start}, or the last
/// one when the line is shorter.
fn at_column(text: &str, start: usize, col: usize) -> usize {
    text[start..line_end(text, start)]
        .char_indices()
        .nth(col)
        .map_or_else(|| last_char(text, start), |(i, _)| start + i)
}

fn next_line_start(text: &str, at: usize) -> Option<usize> {
    let end = line_end(text, at);
    (end < text.len()).then_some(end + 1)
}

fn prev_line_start(text: &str, at: usize) -> Option<usize> {
    let start = line_start(text, at);
    (start > 0).then(|| line_start(text, start - 1))
}

/// `w`: the start of the next word, crossing lines, where an empty line counts
/// as a word. After the last word it stops at the end of the text.
pub(super) fn word_forward(text: &str, at: usize) -> usize {
    let class = class_at(text, at);
    let Some(mut p) = next(text, at) else {
        return at;
    };
    if class != Class::Blank {
        p = skip(text, p, class);
    }
    while class_at(text, p) == Class::Blank && !is_empty_line(text, p) {
        let Some(n) = next(text, p) else { break };
        p = n;
    }
    p
}

/// `b`: the start of the word the cursor is in, or of the one before it,
/// crossing lines, where an empty line counts as a word. `None` at the start
/// of the text, where vim counts `b` as failed.
pub(super) fn word_backward(text: &str, at: usize) -> Option<usize> {
    let mut p = prev(text, at)?;
    while class_at(text, p) == Class::Blank {
        if is_empty_line(text, p) {
            return Some(p);
        }
        let Some(q) = prev(text, p) else {
            return Some(p);
        };
        p = q;
    }
    let class = class_at(text, p);
    while let Some(q) = prev(text, p).filter(|&q| class_at(text, q) == class) {
        p = q;
    }
    Some(p)
}

/// `e`: the last character of the word the cursor is in, or of the next one,
/// crossing lines and the empty ones between them. With {stop}, a cursor
/// already on the last character of a word stays put, which is how far `cw`
/// changes.
pub(super) fn word_end(text: &str, at: usize, stop: bool) -> usize {
    let class = class_at(text, at);
    let Some(mut p) = next(text, at) else {
        return at;
    };
    if class != Class::Blank && class_at(text, p) == class {
        p = skip(text, p, class);
    } else if !stop || class == Class::Blank {
        while class_at(text, p) == Class::Blank {
            let Some(n) = next(text, p) else { return p };
            p = n;
        }
        p = skip(text, p, class_at(text, p));
    }
    prev(text, p).unwrap_or(p)
}

/// The first offset from {p} on that is not of {class}.
fn skip(text: &str, mut p: usize, class: Class) -> usize {
    while class_at(text, p) == class {
        let Some(n) = next(text, p) else { break };
        p = n;
    }
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    LineStart,
    FirstNonBlank,
    LineEnd,
    FirstLine,
    LastLine,
}

/// What an operator covers when it runs over a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// Up to the target, leaving the target out.
    Exclusive,
    /// Up to the target, taking it in.
    Inclusive,
    /// Every line from the cursor's to the target's.
    Linewise,
}

impl Motion {
    /// The motions one key names. `gg` takes two, so a half-typed `g` reads
    /// the second one instead.
    pub(super) fn from_key(code: KeyCode) -> Option<Self> {
        Some(match code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => Self::Left,
            KeyCode::Char('l') | KeyCode::Right => Self::Right,
            KeyCode::Char('k') | KeyCode::Up => Self::Up,
            KeyCode::Char('j') | KeyCode::Down => Self::Down,
            KeyCode::Char('w') => Self::WordForward,
            KeyCode::Char('b') => Self::WordBackward,
            KeyCode::Char('e') => Self::WordEnd,
            KeyCode::Char('0') | KeyCode::Home => Self::LineStart,
            KeyCode::Char('^') => Self::FirstNonBlank,
            KeyCode::Char('$') | KeyCode::End => Self::LineEnd,
            KeyCode::Char('G') => Self::LastLine,
            _ => return None,
        })
    }

    pub(super) fn kind(self) -> Kind {
        match self {
            Self::WordEnd | Self::LineEnd => Kind::Inclusive,
            Self::Up | Self::Down | Self::FirstLine | Self::LastLine => Kind::Linewise,
            _ => Kind::Exclusive,
        }
    }

    /// Where the motion lands from {at}, aiming `j` and `k` at {col}.
    ///
    /// `None` when vim counts the motion as failed: `k` on the first line, `j`
    /// on the last, and `b` at the start of the text. Vim beeps there and drops
    /// a pending operator. `h` at the line start is no failure but an empty
    /// motion, which `c` still changes.
    ///
    /// `l` lands on the end of the line from its last character, so an
    /// operator over it takes that character. A cursor is kept off that
    /// offset by the caller.
    pub(super) fn target(self, text: &str, at: usize, col: usize) -> Option<usize> {
        Some(match self {
            Self::Left => prev(text, at)
                .filter(|&p| p >= line_start(text, at))
                .unwrap_or(at),
            Self::Right => after(text, at),
            Self::Up => at_column(text, prev_line_start(text, at)?, col),
            Self::Down => at_column(text, next_line_start(text, at)?, col),
            Self::WordForward => word_forward(text, at),
            Self::WordBackward => word_backward(text, at)?,
            Self::WordEnd => word_end(text, at, false),
            Self::LineStart => line_start(text, at),
            Self::FirstNonBlank => first_non_blank(text, at),
            Self::LineEnd => last_char(text, at),
            Self::FirstLine => first_non_blank(text, 0),
            Self::LastLine => first_non_blank(text, line_start(text, text.len())),
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use test_case::test_case;

    pub(crate) const CURSOR: char = '|';

    /// Reads `hel|lo` as the text `hello` with the cursor before the second
    /// `l`, the notation every vim test here is written in.
    pub(crate) fn parse(marked: &str) -> (String, usize) {
        let at = marked.find(CURSOR).expect("the case marks its cursor");
        (marked.replacen(CURSOR, "", 1), at)
    }

    pub(crate) fn mark(text: &str, at: usize) -> String {
        format!("{}{CURSOR}{}", &text[..at], &text[at..])
    }

    const FAR_COL: usize = 99;

    fn land(marked: &str, motion: Motion) -> Option<String> {
        let (text, at) = parse(marked);
        let target = motion.target(&text, at, column(&text, at))?;
        Some(mark(&text, target))
    }

    #[test_case("hel|lo world",    "hello |world"   ; "to_the_next_word")]
    #[test_case("|foo.bar",        "foo|.bar"       ; "punctuation_is_its_own_word")]
    #[test_case("foo|...bar",      "foo...|bar"     ; "a_punctuation_run_is_one_word")]
    #[test_case("fo|o\n  bar",     "foo\n  |bar"    ; "across_a_line_break")]
    #[test_case("fo|o\n\nbar",     "foo\n|\nbar"    ; "stops_on_an_empty_line")]
    #[test_case("|\n\nbar",        "\n|\nbar"       ; "from_one_empty_line_to_the_next")]
    #[test_case("foo b|ar",        "foo bar|"       ; "past_the_last_word")]
    #[test_case("é|tê été",        "étê |été"       ; "accented_letters_are_word_characters")]
    #[test_case("|漢字 abc",        "漢字 |abc"       ; "ideographs_are_word_characters")]
    #[test_case("a|🦀b c",          "a🦀|b c"         ; "an_emoji_is_punctuation")]
    #[test_case("|",               "|"              ; "empty_text")]
    fn word_forward_lands(start: &str, expected: &str) {
        assert_eq!(land(start, Motion::WordForward).as_deref(), Some(expected));
    }

    #[test_case("hello |world",    "|hello world"   ; "to_the_previous_word")]
    #[test_case("hello wor|ld",    "hello |world"   ; "to_the_start_of_this_word")]
    #[test_case("foo  \n  |bar",   "|foo  \n  bar"  ; "across_a_line_break")]
    #[test_case("foo\n\n|bar",     "foo\n|\nbar"    ; "stops_on_an_empty_line")]
    #[test_case("foo.|bar",        "foo|.bar"       ; "punctuation_is_its_own_word")]
    #[test_case("  |foo",          "|  foo"         ; "blanks_at_the_start_of_the_text")]
    #[test_case("漢字 |été",        "|漢字 été"       ; "multibyte")]
    fn word_backward_lands(start: &str, expected: &str) {
        assert_eq!(land(start, Motion::WordBackward).as_deref(), Some(expected));
    }

    #[test_case("h|ello world",    "hell|o world"   ; "to_the_end_of_this_word")]
    #[test_case("hell|o world",    "hello worl|d"   ; "from_a_word_end_to_the_next")]
    #[test_case("fo|o\n\n  bar",   "foo\n\n  ba|r"  ; "across_empty_lines")]
    #[test_case("foo|...bar",      "foo..|.bar"     ; "a_punctuation_run")]
    #[test_case("foo ba|r   ",     "foo bar   |"    ; "past_the_last_word")]
    #[test_case("|été",            "ét|é"           ; "multibyte")]
    fn word_end_lands(start: &str, expected: &str) {
        assert_eq!(land(start, Motion::WordEnd).as_deref(), Some(expected));
    }

    /// How far `cw` changes: a word end stays where it is.
    #[test_case("fo|o bar",        "fo|o bar"       ; "on_a_word_end")]
    #[test_case("|foo bar",        "fo|o bar"       ; "inside_a_word")]
    fn word_end_with_stop_lands(start: &str, expected: &str) {
        let (text, at) = parse(start);
        assert_eq!(mark(&text, word_end(&text, at, true)), expected);
    }

    #[test_case("ab|c",    Motion::Left,          "a|bc"     ; "h_moves_left")]
    #[test_case("ab\n|c",  Motion::Left,          "ab\n|c"   ; "h_stops_at_the_line_start")]
    #[test_case("a|bc",    Motion::Right,         "ab|c"     ; "l_moves_right")]
    #[test_case("ab|c\nd", Motion::Right,         "abc|\nd"  ; "l_lands_on_the_line_end")]
    #[test_case("|\nab",   Motion::Right,         "|\nab"    ; "l_on_an_empty_line")]
    #[test_case("  a|b",   Motion::LineStart,     "|  ab"    ; "zero_goes_to_the_line_start")]
    #[test_case("  a|b",   Motion::FirstNonBlank, "  |ab"    ; "caret_skips_the_indent")]
    #[test_case("x\n |  ", Motion::FirstNonBlank, "x\n  | "  ; "caret_on_a_blank_line")]
    #[test_case("|ab\nc",  Motion::LineEnd,       "a|b\nc"   ; "dollar_goes_to_the_last_char")]
    #[test_case("a\n|\nc", Motion::LineEnd,       "a\n|\nc"  ; "dollar_on_an_empty_line")]
    #[test_case("a|漢字",   Motion::LineEnd,       "a漢|字"    ; "dollar_on_a_wide_char")]
    #[test_case("  a\nb|c", Motion::FirstLine,    "  |a\nbc" ; "gg_goes_to_the_first_non_blank")]
    #[test_case("|a\n  bc", Motion::LastLine,     "a\n  |bc" ; "capital_g_goes_to_the_last_line")]
    #[test_case("a\n|",    Motion::LastLine,      "a\n|"     ; "capital_g_on_an_empty_last_line")]
    fn line_motions_land(start: &str, motion: Motion, expected: &str) {
        assert_eq!(land(start, motion).as_deref(), Some(expected));
    }

    #[test_case("ab|cd\nxy",    Motion::Down, "abcd\nx|y"    ; "j_stops_on_the_last_char_of_a_shorter_line")]
    #[test_case("a|b\n\ncd",    Motion::Down, "ab\n|\ncd"    ; "j_onto_an_empty_line")]
    #[test_case("a漢|字\nabcd",  Motion::Down, "a漢字\nab|cd"  ; "j_counts_columns_in_characters")]
    #[test_case("abcd\nx|y",    Motion::Up,   "a|bcd\nxy"    ; "k_keeps_the_column")]
    fn vertical_motions_land(start: &str, motion: Motion, expected: &str) {
        assert_eq!(land(start, motion).as_deref(), Some(expected));
    }

    #[test_case("a|b",     Motion::Up           ; "k_on_the_first_line")]
    #[test_case("a\nb|c",  Motion::Down         ; "j_on_the_last_line")]
    #[test_case("|ab",     Motion::WordBackward ; "b_at_the_start_of_the_text")]
    fn a_motion_that_cannot_move_fails(start: &str, motion: Motion) {
        assert_eq!(land(start, motion), None);
    }

    #[test]
    fn a_vertical_move_aims_at_the_wanted_column() {
        let (text, at) = parse("|a\nb\nlonger");
        let middle = Motion::Down.target(&text, at, FAR_COL).unwrap();
        assert_eq!(mark(&text, middle), "a\n|b\nlonger");
        let bottom = Motion::Down.target(&text, middle, FAR_COL).unwrap();
        assert_eq!(mark(&text, bottom), "a\nb\nlonge|r");
    }
}
