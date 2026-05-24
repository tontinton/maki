//! Markdown parser and width-aware renderer.
//!
//! The parser separates two orthogonal axes: `SpanKind` (text vs code) and
//! `Emphasis` (bold, italic, strike). They compose freely, so `***x***` is
//! bold+italic, and code inside bold keeps both.

pub mod render;

const BULLET: &str = "• ";
const LIST_INDENT_STEP: usize = 2;
const MAX_HEADING_LEVEL: u8 = 6;
const FENCE_MIN: usize = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
}

impl Emphasis {
    pub const BOLD: Self = Self {
        bold: true,
        italic: false,
        strike: false,
        underline: false,
    };
    pub const ITALIC: Self = Self {
        bold: false,
        italic: true,
        strike: false,
        underline: false,
    };
    pub const BOLD_ITALIC: Self = Self {
        bold: true,
        italic: true,
        strike: false,
        underline: false,
    };
    pub const STRIKE: Self = Self {
        bold: false,
        italic: false,
        strike: true,
        underline: false,
    };
    pub const UNDERLINE: Self = Self {
        bold: false,
        italic: false,
        strike: false,
        underline: true,
    };

    pub fn merge(self, other: Self) -> Self {
        Self {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            strike: self.strike || other.strike,
            underline: self.underline || other.underline,
        }
    }

    pub fn is_empty(self) -> bool {
        !self.bold && !self.italic && !self.strike && !self.underline
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanKind {
    Text,
    Code,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineSpan {
    pub text: String,
    pub kind: SpanKind,
    pub emphasis: Emphasis,
}

impl InlineSpan {
    pub fn text(text: impl Into<String>, emphasis: Emphasis) -> Self {
        Self {
            text: text.into(),
            kind: SpanKind::Text,
            emphasis,
        }
    }

    pub fn code(text: impl Into<String>, emphasis: Emphasis) -> Self {
        Self {
            text: text.into(),
            kind: SpanKind::Code,
            emphasis,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockKind {
    Paragraph,
    Heading(u8),
    UnorderedListItem { depth: usize },
    OrderedListItem { depth: usize, marker: String },
    HorizontalRule,
}

/// Inline delimiters are kept intact here. Emphasis and code parsing is
/// deferred to `parse_inline` so callers can wrap before deciding styles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineBlock {
    pub kind: BlockKind,
    pub inline: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Block {
    Lines(Vec<LineBlock>),
    Code {
        lang: String,
        code: String,
    },
    Table {
        rows: Vec<Vec<String>>,
        header_end: usize,
    },
}

pub fn parse(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(fence) = find_code_fence(rest) {
        let before = rest[..fence.before_end].trim_end_matches('\n');
        if !before.is_empty() {
            blocks.extend(split_normal_blocks(before));
        }
        blocks.push(Block::Code {
            lang: fence.lang.to_owned(),
            code: fence.code.to_owned(),
        });
        let skip = fence.block_end + rest[fence.block_end..].len()
            - rest[fence.block_end..].trim_start_matches('\n').len();
        rest = &rest[skip..];
    }
    if !rest.is_empty() {
        blocks.extend(split_normal_blocks(rest));
    }
    blocks
}

fn split_normal_blocks(text: &str) -> Vec<Block> {
    let mut lines_with_offsets: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        lines_with_offsets.push((offset, line));
        offset += line.len() + 1;
    }

    let mut blocks: Vec<Block> = Vec::new();
    let mut normal_start: Option<usize> = None;
    let mut i = 0;

    while i < lines_with_offsets.len() {
        let (_, line) = lines_with_offsets[i];
        if is_table_row(line) {
            let table_start = i;
            let header_cols = parse_table_cells(line).len();
            let mut sep_idx = None;
            let mut j = i;
            while j < lines_with_offsets.len() && is_table_row(lines_with_offsets[j].1) {
                if sep_idx.is_none()
                    && is_separator_row(lines_with_offsets[j].1)
                    && parse_table_cells(lines_with_offsets[j].1).len() >= header_cols
                {
                    sep_idx = Some(j - table_start);
                }
                j += 1;
            }
            if let Some(si) = sep_idx
                && j - table_start >= 2
            {
                if let Some(ns) = normal_start.take() {
                    let start = lines_with_offsets[ns].0;
                    let end = lines_with_offsets[table_start].0;
                    let slice = text[start..end].trim_matches('\n');
                    if !slice.is_empty() {
                        blocks.push(Block::Lines(lines_to_blocks(slice)));
                    }
                }

                let table_end = if j < lines_with_offsets.len()
                    && j == lines_with_offsets.len() - 1
                    && lines_with_offsets[j].1.trim_start().starts_with('|')
                {
                    j + 1
                } else {
                    j
                };

                let mut rows = Vec::new();
                for (k, &(_, line)) in lines_with_offsets[table_start..table_end]
                    .iter()
                    .enumerate()
                {
                    if k != si {
                        rows.push(parse_table_cells(line));
                    }
                }
                blocks.push(Block::Table {
                    rows,
                    header_end: si,
                });
                i = table_end;
                continue;
            }
        }

        if normal_start.is_none() {
            normal_start = Some(i);
        }
        i += 1;
    }

    if let Some(ns) = normal_start {
        let start = lines_with_offsets[ns].0;
        let content = text[start..].trim_start_matches('\n');
        if !content.is_empty() {
            blocks.push(Block::Lines(lines_to_blocks(content)));
        }
    }

    if blocks.is_empty() {
        blocks.push(Block::Lines(lines_to_blocks(text)));
    }

    blocks
}

fn lines_to_blocks(text: &str) -> Vec<LineBlock> {
    text.split('\n').map(classify_line).collect()
}

fn classify_line(line: &str) -> LineBlock {
    if is_horizontal_rule(line) {
        return LineBlock {
            kind: BlockKind::HorizontalRule,
            inline: String::new(),
        };
    }
    if let Some((level, content)) = parse_heading(line) {
        return LineBlock {
            kind: BlockKind::Heading(level),
            inline: content.to_owned(),
        };
    }
    if let Some((indent_spaces, rest)) = parse_unordered_marker(line) {
        return LineBlock {
            kind: BlockKind::UnorderedListItem {
                depth: indent_spaces / LIST_INDENT_STEP,
            },
            inline: rest.to_owned(),
        };
    }
    if let Some((indent_spaces, marker, rest)) = parse_ordered_marker(line) {
        return LineBlock {
            kind: BlockKind::OrderedListItem {
                depth: indent_spaces / LIST_INDENT_STEP,
                marker: marker.to_owned(),
            },
            inline: rest.to_owned(),
        };
    }
    LineBlock {
        kind: BlockKind::Paragraph,
        inline: line.to_owned(),
    }
}

pub fn block_prefix(kind: &BlockKind) -> Option<String> {
    match kind {
        BlockKind::UnorderedListItem { depth } => {
            Some(format!("{}{BULLET}", " ".repeat(depth * LIST_INDENT_STEP)))
        }
        BlockKind::OrderedListItem { depth, marker } => {
            Some(format!("{}{marker} ", " ".repeat(depth * LIST_INDENT_STEP)))
        }
        BlockKind::Paragraph | BlockKind::Heading(_) | BlockKind::HorizontalRule => None,
    }
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > MAX_HEADING_LEVEL as usize {
        return None;
    }
    let rest = &line[hashes..];
    let level = hashes as u8;
    if let Some(stripped) = rest.strip_prefix(' ') {
        Some((level, stripped.trim_end()))
    } else if rest.is_empty() {
        Some((level, ""))
    } else {
        None
    }
}

fn parse_unordered_marker(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[indent..];
    let marker = rest.as_bytes().first()?;
    if !matches!(marker, b'-' | b'*' | b'+') {
        return None;
    }
    let after = &rest[1..];
    let stripped = after.strip_prefix(' ')?;
    Some((indent, stripped))
}

fn parse_ordered_marker(line: &str) -> Option<(usize, &str, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[indent..];
    let digits_end = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits_end == 0 {
        return None;
    }
    let after_digits = &rest[digits_end..];
    if !after_digits.starts_with(". ") {
        return None;
    }
    Some((indent, &rest[..=digits_end], &after_digits[2..]))
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let first = match trimmed.as_bytes().first() {
        Some(b'-' | b'*' | b'_') => trimmed.as_bytes()[0],
        _ => return false,
    };
    trimmed.bytes().all(|b| b == first || b == b' ')
        && trimmed.bytes().filter(|&b| b == first).count() >= 3
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.matches('|').count() >= 2
}

fn is_separator_row(line: &str) -> bool {
    if !is_table_row(line) {
        return false;
    }
    parse_table_cells(line)
        .iter()
        .all(|cell| cell.bytes().all(|b| matches!(b, b'-' | b':')) && cell.contains('-'))
}

fn parse_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);

    let bytes = inner.as_bytes();
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run_len = count_backtick_run(bytes, i);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, i, run_len) {
                current.push_str(&inner[i..close_end]);
                i = close_end;
            } else {
                current.push_str(&inner[i..]);
                i = bytes.len();
            }
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            current.push('|');
            i += 2;
        } else if bytes[i] == b'|' {
            cells.push(current.trim().to_owned());
            current = String::new();
            i += 1;
        } else {
            let ch = inner[i..].chars().next().unwrap();
            current.push(ch);
            i += ch.len_utf8();
        }
    }

    cells.push(current.trim().to_owned());
    cells
}

struct CodeFence<'a> {
    before_end: usize,
    lang: &'a str,
    code: &'a str,
    block_end: usize,
}

fn find_code_fence(text: &str) -> Option<CodeFence<'_>> {
    let bytes = text.as_bytes();
    let mut search_from = 0;
    while search_from < bytes.len() {
        let pos = text[search_from..].find("```")?;
        let abs = search_from + pos;
        if abs != 0 && bytes[abs - 1] != b'\n' {
            search_from = abs + FENCE_MIN;
            continue;
        }
        let fence_len = FENCE_MIN
            + bytes[abs + FENCE_MIN..]
                .iter()
                .take_while(|&&b| b == b'`')
                .count();
        let after_ticks = abs + fence_len;
        let Some(nl) = text[after_ticks..].find('\n') else {
            search_from = abs + fence_len;
            continue;
        };
        let info = &text[after_ticks..after_ticks + nl];
        if info.contains('`') {
            search_from = abs + fence_len;
            continue;
        }
        let lang = info.trim();
        let code_start = after_ticks + nl + 1;
        let fence_str = "`".repeat(fence_len);
        let mut offset = 0;
        let mut close: Option<(usize, usize)> = None;
        for line in text[code_start..].split('\n') {
            let trimmed = line.trim_end();
            if trimmed.len() >= fence_len
                && trimmed.starts_with(&fence_str)
                && !trimmed[fence_len..].starts_with('`')
            {
                close = Some((offset, line.len()));
                break;
            }
            offset += line.len() + 1;
        }
        let (code, block_end) = if let Some((close_off, close_line_len)) = close {
            let raw_end = code_start + close_off;
            let code_end = if raw_end > code_start && bytes[raw_end - 1] == b'\n' {
                raw_end - 1
            } else {
                raw_end
            };
            let trailing_start = code_start + close_off + fence_len;
            let trailing_end = code_start + close_off + close_line_len;
            let block_end = if text[trailing_start..trailing_end].trim().is_empty() {
                trailing_end
            } else {
                trailing_start
            };
            (&text[code_start..code_end], block_end)
        } else {
            (&text[code_start..], text.len())
        };
        return Some(CodeFence {
            before_end: abs,
            lang,
            code,
            block_end,
        });
    }
    None
}

/// Emphasis composes additively. Code spans are atomic and carry the
/// surrounding emphasis as a separate modifier.
pub fn parse_inline(text: &str) -> Vec<InlineSpan> {
    parse_inline_impl(text, Emphasis::default(), ParseMode::WithCode)
}

/// `EmphasisOnly` is for rescanning a region the outer pass already split on
/// code, so we don't re-recognize backticks we've already consumed.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ParseMode {
    WithCode,
    EmphasisOnly,
}

fn parse_inline_impl(text: &str, base: Emphasis, mode: ParseMode) -> Vec<InlineSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut pos = 0;
    let mut plain_start = 0;

    let flush_plain =
        |spans: &mut Vec<InlineSpan>, plain: &str, base: Emphasis, mode: ParseMode| {
            if plain.is_empty() {
                return;
            }
            match mode {
                ParseMode::WithCode => {
                    spans.extend(parse_inline_impl(plain, base, ParseMode::EmphasisOnly))
                }
                ParseMode::EmphasisOnly => spans.push(InlineSpan::text(plain.to_owned(), base)),
            }
        };

    while pos < bytes.len() {
        if mode == ParseMode::WithCode && bytes[pos] == b'`' {
            let run_len = count_backtick_run(bytes, pos);
            if let Some((cs, ce, close_end)) = find_code_span_close(bytes, pos, run_len)
                && ce > cs
            {
                flush_plain(&mut spans, &text[plain_start..pos], base, mode);
                spans.push(InlineSpan::code(text[cs..ce].to_owned(), base));
                pos = close_end;
                plain_start = pos;
                continue;
            }
            pos += run_len;
            continue;
        }

        let outcome = match bytes[pos] {
            b'*' => try_star_emphasis(bytes, pos),
            b'~' => try_strike_emphasis(bytes, pos),
            b'_' => try_underscore_emphasis(bytes, pos),
            _ => InlineMatch::None,
        };

        match outcome {
            InlineMatch::Found {
                emphasis,
                content_start,
                close,
                delim_len,
            } => {
                flush_plain(&mut spans, &text[plain_start..pos], base, mode);
                let inner = base.merge(emphasis);
                spans.extend(parse_inline_impl(&text[content_start..close], inner, mode));
                pos = close + delim_len;
                plain_start = pos;
            }
            InlineMatch::Skip(n) => pos += n,
            InlineMatch::None => pos += 1,
        }
    }

    if plain_start < bytes.len() {
        flush_plain(&mut spans, &text[plain_start..], base, mode);
    }
    spans
}

enum InlineMatch {
    Found {
        emphasis: Emphasis,
        content_start: usize,
        close: usize,
        delim_len: usize,
    },
    /// `**` with no closer: skip past the whole open run, not just one byte,
    /// otherwise the second `*` would re-trigger a match.
    Skip(usize),
    None,
}

fn count_run(bytes: &[u8], pos: usize, ch: u8) -> usize {
    bytes[pos..].iter().take_while(|&&b| b == ch).count()
}

fn count_backtick_run(bytes: &[u8], pos: usize) -> usize {
    count_run(bytes, pos, b'`')
}

fn find_code_span_close(bytes: &[u8], pos: usize, run_len: usize) -> Option<(usize, usize, usize)> {
    let content_start = pos + run_len;
    let mut i = content_start;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let close_run = count_backtick_run(bytes, i);
            if close_run == run_len {
                return Some((content_start, i, i + run_len));
            }
            i += close_run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_emphasis_close(bytes: &[u8], start: usize, delim: &[u8]) -> Option<usize> {
    let mut pos = start;
    while pos + delim.len() <= bytes.len() {
        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, pos, run) {
                pos = close_end;
            } else {
                pos += run;
            }
            continue;
        }
        if bytes[pos..].starts_with(delim) {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_italic_close(bytes: &[u8], start: usize, ch: u8) -> Option<usize> {
    let mut pos = start;
    while pos < bytes.len() {
        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, pos, run) {
                pos = close_end;
            } else {
                pos += run;
            }
            continue;
        }
        if bytes[pos] == ch {
            if (ch == b'*' && pos + 1 < bytes.len() && bytes[pos + 1] == b'*')
                || (pos > 0 && bytes[pos - 1] == ch)
            {
                pos += 1;
                continue;
            }
            if pos > start && !bytes[pos - 1].is_ascii_whitespace() {
                if ch == b'_' && pos + 1 < bytes.len() && is_word_char(bytes[pos + 1]) {
                    pos += 1;
                    continue;
                }
                return Some(pos);
            }
        }
        pos += 1;
    }
    None
}

fn is_valid_italic_open(bytes: &[u8], pos: usize) -> bool {
    if pos + 1 >= bytes.len() || bytes[pos + 1].is_ascii_whitespace() {
        return false;
    }
    let ch = bytes[pos];
    if ch == b'*' {
        if bytes[pos + 1] == b'*' {
            return false;
        }
        if pos > 0 && bytes[pos - 1] == b'*' {
            return false;
        }
        if pos > 0 && is_word_char(bytes[pos - 1]) {
            return false;
        }
    }
    if ch == b'_' && pos > 0 && is_word_char(bytes[pos - 1]) {
        return false;
    }
    true
}

fn is_valid_strike_open(bytes: &[u8], pos: usize) -> bool {
    if pos + 2 >= bytes.len() {
        return false;
    }
    if bytes[pos + 2] == b'~' {
        return false;
    }
    if pos > 0 && bytes[pos - 1] == b'~' {
        return false;
    }
    !bytes[pos + 2].is_ascii_whitespace()
}

fn find_strike_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while pos + 1 < bytes.len() {
        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, pos, run) {
                pos = close_end;
            } else {
                pos += run;
            }
            continue;
        }
        if bytes[pos] == b'~' && bytes[pos + 1] == b'~' {
            if pos + 2 < bytes.len() && bytes[pos + 2] == b'~' {
                pos += 1;
                continue;
            }
            if pos > start && bytes[pos - 1] == b'~' {
                pos += 1;
                continue;
            }
            if pos > start && !bytes[pos - 1].is_ascii_whitespace() {
                return Some(pos);
            }
        }
        pos += 1;
    }
    None
}

fn try_star_emphasis(bytes: &[u8], pos: usize) -> InlineMatch {
    let run = count_run(bytes, pos, b'*');
    if run >= 3
        && let Some(close) = find_emphasis_close(bytes, pos + 3, b"***")
        && close > pos + 3
    {
        return InlineMatch::Found {
            emphasis: Emphasis::BOLD_ITALIC,
            content_start: pos + 3,
            close,
            delim_len: 3,
        };
    }
    if run >= 2 {
        if let Some(close) = find_emphasis_close(bytes, pos + 2, b"**")
            && close > pos + 2
        {
            return InlineMatch::Found {
                emphasis: Emphasis::BOLD,
                content_start: pos + 2,
                close,
                delim_len: 2,
            };
        }
        return InlineMatch::Skip(2);
    }
    if is_valid_italic_open(bytes, pos)
        && let Some(close) = find_italic_close(bytes, pos + 1, b'*')
        && close > pos + 1
    {
        return InlineMatch::Found {
            emphasis: Emphasis::ITALIC,
            content_start: pos + 1,
            close,
            delim_len: 1,
        };
    }
    InlineMatch::Skip(1)
}

fn try_strike_emphasis(bytes: &[u8], pos: usize) -> InlineMatch {
    if pos + 1 >= bytes.len() || bytes[pos + 1] != b'~' {
        return InlineMatch::None;
    }
    if is_valid_strike_open(bytes, pos)
        && let Some(close) = find_strike_close(bytes, pos + 2)
        && close > pos + 2
    {
        return InlineMatch::Found {
            emphasis: Emphasis::STRIKE,
            content_start: pos + 2,
            close,
            delim_len: 2,
        };
    }
    InlineMatch::Skip(2)
}

fn try_underscore_emphasis(bytes: &[u8], pos: usize) -> InlineMatch {
    if !is_valid_italic_open(bytes, pos) {
        return InlineMatch::None;
    }
    if let Some(close) = find_italic_close(bytes, pos + 1, b'_')
        && close > pos + 1
    {
        return InlineMatch::Found {
            emphasis: Emphasis::ITALIC,
            content_start: pos + 1,
            close,
            delim_len: 1,
        };
    }
    InlineMatch::Skip(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn span_text(spans: &[InlineSpan]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn parse_inline_plain_text_yields_single_text_span() {
        let spans = parse_inline("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert_eq!(spans[0].kind, SpanKind::Text);
        assert!(spans[0].emphasis.is_empty());
    }

    #[test]
    fn parse_inline_bold_emits_bold_span_and_strips_delimiters() {
        let spans = parse_inline("a **b** c");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "a ");
        assert!(spans[0].emphasis.is_empty());
        assert_eq!(spans[1].text, "b");
        assert_eq!(spans[1].emphasis, Emphasis::BOLD);
        assert_eq!(spans[2].text, " c");
    }

    #[test_case("*x*"; "star")]
    #[test_case("_y_"; "underscore")]
    fn parse_inline_italic_variants(input: &str) {
        let spans = parse_inline(input);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].emphasis, Emphasis::ITALIC);
    }

    #[test]
    fn parse_inline_triple_star_is_bold_italic() {
        let spans = parse_inline("***hi***");
        assert_eq!(spans[0].emphasis, Emphasis::BOLD_ITALIC);
        assert_eq!(spans[0].text, "hi");
    }

    #[test]
    fn parse_inline_code_span_keeps_kind() {
        let spans = parse_inline("a `b()` c");
        assert_eq!(spans[1].kind, SpanKind::Code);
        assert_eq!(spans[1].text, "b()");
        assert!(spans[1].emphasis.is_empty());
    }

    #[test]
    fn parse_inline_strikethrough() {
        let spans = parse_inline("~~gone~~");
        assert_eq!(spans[0].emphasis, Emphasis::STRIKE);
        assert_eq!(spans[0].text, "gone");
    }

    #[test]
    fn parse_inline_code_inside_bold_preserves_both_axes() {
        let spans = parse_inline("**bold `code` bold**");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "bold ");
        assert_eq!(spans[0].emphasis, Emphasis::BOLD);
        assert_eq!(spans[0].kind, SpanKind::Text);
        assert_eq!(spans[1].text, "code");
        assert_eq!(spans[1].emphasis, Emphasis::BOLD);
        assert_eq!(spans[1].kind, SpanKind::Code);
        assert_eq!(spans[2].text, " bold");
        assert_eq!(spans[2].emphasis, Emphasis::BOLD);
    }

    #[test]
    fn parse_inline_bold_inside_code_treats_code_as_atomic() {
        let spans = parse_inline("`code **bold** code`");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "code **bold** code");
        assert_eq!(spans[0].kind, SpanKind::Code);
    }

    #[test]
    fn parse_inline_nested_bold_inside_italic_becomes_bold_italic() {
        let spans = parse_inline("*a **b** c*");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].emphasis, Emphasis::ITALIC);
        assert_eq!(spans[1].emphasis, Emphasis::BOLD_ITALIC);
        assert_eq!(spans[2].emphasis, Emphasis::ITALIC);
    }

    #[test_case(1, "# h1"; "level_1")]
    #[test_case(2, "## h2"; "level_2")]
    #[test_case(3, "### h3"; "level_3")]
    #[test_case(4, "#### h4"; "level_4")]
    #[test_case(5, "##### h5"; "level_5")]
    #[test_case(6, "###### h6"; "level_6")]
    fn parse_heading_levels_1_through_6(level: u8, input: &str) {
        let (got, content) = parse_heading(input).expect("heading parses");
        assert_eq!(got, level);
        assert_eq!(content, format!("h{level}"));
    }

    #[test]
    fn parse_seven_hashes_is_not_heading() {
        assert!(parse_heading("####### nope").is_none());
    }

    #[test]
    fn parse_horizontal_rule_classifies_as_hr() {
        let blocks = parse("---");
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(lines[0].kind, BlockKind::HorizontalRule);
    }

    #[test]
    fn parse_unordered_list_records_depth() {
        let blocks = parse("- item\n  - nested");
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(lines[0].kind, BlockKind::UnorderedListItem { depth: 0 });
        assert_eq!(lines[1].kind, BlockKind::UnorderedListItem { depth: 1 });
    }

    #[test]
    fn parse_preserves_user_newlines_one_logical_line_each() {
        let blocks = parse("a\nb\nc");
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].inline, "a");
        assert_eq!(lines[2].inline, "c");
    }

    #[test]
    fn parse_fenced_code_block_emits_code_block() {
        let blocks = parse("```rust\nfn x() {}\nlet y;\n```");
        assert_eq!(blocks.len(), 1);
        let Block::Code { lang, code } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(lang, "rust");
        assert_eq!(code, "fn x() {}\nlet y;");
    }

    #[test]
    fn parse_table_emits_table_block_with_header_separator_dropped() {
        let blocks = parse("| Name | Value |\n| --- | --- |\n| foo | 42 |");
        assert_eq!(blocks.len(), 1);
        let Block::Table { rows, header_end } = &blocks[0] else {
            panic!("expected Table")
        };
        assert_eq!(*header_end, 1);
        assert_eq!(rows, &[vec!["Name", "Value"], vec!["foo", "42"]]);
    }

    #[test]
    fn parse_inline_never_panics_on_arbitrary_unicode() {
        let mut rng = fastrand::Rng::with_seed(0xC0FFEE);
        for _ in 0..500 {
            let n = rng.usize(0..200);
            let mut s = String::with_capacity(n);
            for _ in 0..n {
                s.push(rng.char(..));
            }
            let spans = parse_inline(&s);
            let total: usize = spans.iter().map(|sp| sp.text.len()).sum();
            assert!(total <= s.len(), "fabrication: {} > {}", total, s.len());
        }
    }

    #[test]
    fn parse_inline_visible_text_invariant() {
        let cases = [
            "plain text",
            "a **b** c",
            "a *b* c",
            "a `code` b",
            "a ~~strike~~ b",
        ];
        for input in cases {
            let spans = parse_inline(input);
            let visible = span_text(&spans);
            let strip = |s: &str| -> String {
                s.chars()
                    .filter(|c| !matches!(c, '`' | '*' | '~' | '_'))
                    .collect()
            };
            assert_eq!(strip(&visible), strip(input), "input: {input:?}");
        }
    }

    #[test]
    fn parse_inline_empty_input_returns_empty() {
        assert!(parse_inline("").is_empty());
    }

    #[test]
    fn parse_inline_unmatched_star_passes_through_as_plain() {
        let spans = parse_inline("a*b");
        assert!(
            spans
                .iter()
                .all(|s| s.kind == SpanKind::Text && s.emphasis.is_empty())
        );
        assert_eq!(span_text(&spans), "a*b");
    }

    #[test_case("- item", Some("• "); "unordered_depth_zero")]
    #[test_case("  - item", Some("  • "); "unordered_depth_one")]
    fn block_prefix_unordered_list_depth(input: &str, expected: Option<&str>) {
        let blocks = parse(input);
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(block_prefix(&lines[0].kind).as_deref(), expected);
    }

    const STRIP_DELIMS: &[char] = &['`', '*', '~', '_'];

    fn first_lines(blocks: &[Block]) -> &[LineBlock] {
        match &blocks[0] {
            Block::Lines(l) => l,
            other => panic!("expected Lines, got {other:?}"),
        }
    }

    #[test]
    fn emphasis_merge_ors_fields_and_default_is_identity() {
        assert_eq!(
            Emphasis::BOLD.merge(Emphasis::ITALIC),
            Emphasis::BOLD_ITALIC
        );
        let e = Emphasis::BOLD_ITALIC;
        assert_eq!(e.merge(Emphasis::default()), e);
        assert!(Emphasis::default().is_empty());
        assert!(!Emphasis::BOLD.is_empty());
    }

    #[test]
    fn ordered_list_depth_and_marker_preserved() {
        let lines = first_lines(&parse("1. a\n   2. nested\n10. ten")).to_vec();
        assert_eq!(
            lines[0].kind,
            BlockKind::OrderedListItem {
                depth: 0,
                marker: "1.".to_owned()
            }
        );
        // 3 spaces over a 2-space step rounds down to depth 1.
        assert_eq!(
            lines[1].kind,
            BlockKind::OrderedListItem {
                depth: 1,
                marker: "2.".to_owned()
            }
        );
        assert_eq!(
            lines[2].kind,
            BlockKind::OrderedListItem {
                depth: 0,
                marker: "10.".to_owned()
            }
        );
    }

    #[test_case(0, "1.", "1. "; "depth_zero")]
    #[test_case(2, "42.", "    42. "; "depth_two_two_digits")]
    fn block_prefix_ordered_list_depths(depth: usize, marker: &str, expected: &str) {
        let kind = BlockKind::OrderedListItem {
            depth,
            marker: marker.to_owned(),
        };
        assert_eq!(block_prefix(&kind).as_deref(), Some(expected));
    }

    #[test_case("---"; "three_dashes")]
    #[test_case("***"; "three_stars")]
    #[test_case("___"; "three_unders")]
    #[test_case("- - -"; "spaced_dashes")]
    #[test_case("* * *"; "spaced_stars")]
    #[test_case("-- -"; "two_dashes_space_dash_still_three_dashes_total")]
    fn horizontal_rule_accepted_variants(input: &str) {
        assert_eq!(
            first_lines(&parse(input))[0].kind,
            BlockKind::HorizontalRule
        );
    }

    #[test_case("--"; "two_dashes")]
    #[test_case("-a-"; "letter_between")]
    #[test_case("**a"; "two_stars_letter")]
    #[test_case("---x"; "trailing_non_marker")]
    fn horizontal_rule_rejected_variants(input: &str) {
        assert_ne!(
            first_lines(&parse(input))[0].kind,
            BlockKind::HorizontalRule
        );
    }

    #[test]
    fn heading_requires_space_or_empty_after_hashes() {
        assert!(parse_heading("#nospace").is_none());
    }

    #[test_case("#"; "bare_hash")]
    #[test_case("# "; "hash_space")]
    fn bare_hash_is_heading_level_1_with_empty_inline(input: &str) {
        let blocks = parse(input);
        let lb = &first_lines(&blocks)[0];
        assert_eq!(lb.kind, BlockKind::Heading(1));
        assert_eq!(lb.inline, "");
    }

    const FOUR_BACKTICK_FENCE: &str = "````\n```\ninner\n```\n````";

    #[test]
    fn four_backtick_fence_wraps_inner_three_backtick_block() {
        let blocks = parse(FOUR_BACKTICK_FENCE);
        let Block::Code { lang, code } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(lang, "");
        assert_eq!(code, "```\ninner\n```");
    }

    #[test]
    fn unclosed_code_fence_runs_to_eof() {
        let blocks = parse("```\nfoo\nbar");
        let Block::Code { code, .. } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(code, "foo\nbar");
    }

    #[test]
    fn mid_line_backticks_do_not_open_fence() {
        // A run of backticks only opens a fence at the start of a line.
        let blocks = parse("text ``` text");
        let lb = &first_lines(&blocks)[0];
        assert_eq!(lb.kind, BlockKind::Paragraph);
        assert_eq!(lb.inline, "text ``` text");
    }

    #[test]
    fn code_fence_language_is_optional() {
        let blocks = parse("```\ncode\n```");
        let Block::Code { lang, code } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(lang, "");
        assert_eq!(code, "code");
    }

    #[test]
    fn underscore_italic_does_not_match_intraword() {
        let spans = parse_inline("foo_bar_baz");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "foo_bar_baz");
        assert!(spans[0].emphasis.is_empty());
    }

    #[test]
    fn star_italic_intraword_does_not_open_when_preceded_by_word_char() {
        // A word char before `*` blocks italic from opening, so `a*b*c`
        // stays plain. Worth a test because file names like `a*b*c` are real.
        let spans = parse_inline("a*b*c");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "a*b*c");
        assert!(spans[0].emphasis.is_empty());
    }

    #[test_case("~not~"; "single_tildes")]
    #[test_case("~~foo"; "unclosed_double")]
    fn strikethrough_non_match_stays_plain(input: &str) {
        let spans = parse_inline(input);
        assert!(
            spans
                .iter()
                .all(|s| s.emphasis.is_empty() && s.kind == SpanKind::Text)
        );
        assert_eq!(span_text(&spans), input);
    }

    #[test]
    fn code_span_with_pipes_and_emphasis_chars_is_atomic() {
        let spans = parse_inline("a `x|y*z` b");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].kind, SpanKind::Code);
        assert_eq!(spans[1].text, "x|y*z");
        assert!(spans[1].emphasis.is_empty());
    }

    #[test]
    fn table_cell_backslash_pipe_is_literal_pipe() {
        let blocks = parse("| a | b\\|c | d |\n| --- | --- | --- |\n| 1 | 2 | 3 |");
        let Block::Table { rows, header_end } = &blocks[0] else {
            panic!("expected Table")
        };
        assert_eq!(*header_end, 1);
        assert_eq!(rows[0], vec!["a", "b|c", "d"]);
    }

    #[test]
    fn table_cell_backticked_pipe_stays_inside_cell() {
        let blocks = parse("| `x|y` | z |\n|---|---|");
        let Block::Table { rows, .. } = &blocks[0] else {
            panic!("expected Table")
        };
        assert_eq!(rows[0], vec!["`x|y`", "z"]);
    }

    #[test]
    fn underscore_italic_with_nested_bold_becomes_bold_italic() {
        let spans = parse_inline("_a **b** c_");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].emphasis, Emphasis::ITALIC);
        assert_eq!(spans[1].emphasis, Emphasis::BOLD_ITALIC);
        assert_eq!(spans[1].text, "b");
        assert_eq!(spans[2].emphasis, Emphasis::ITALIC);
    }

    #[test]
    fn triple_star_mismatched_close_preserves_visible_text() {
        let input = "***bold only**";
        let spans = parse_inline(input);
        let visible = span_text(&spans);
        let strip =
            |s: &str| -> String { s.chars().filter(|c| !STRIP_DELIMS.contains(c)).collect() };
        assert_eq!(strip(&visible), strip(input));
    }
}
