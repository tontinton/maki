//! Width-aware markdown renderer. Theme-free: outputs semantic style
//! tokens that consumers (`maki-ui`, `maki-lua`) map to their own colours.
//!
//! Single source of truth for layout: tables, code bars, wrapping, blank
//! lines. Everyone consumes `Line` values from here.

use std::borrow::Cow;
use std::iter;
use std::mem;

use maki_highlight::CodeHighlighter;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    Block, BlockKind, Emphasis, InlineSpan, LineBlock, SpanKind, block_prefix, parse, parse_inline,
};

pub const CODE_BAR: &str = "│ ";
pub const CODE_BAR_WRAP: &str = "│";
/// Lines longer than this get truncated with `...` to protect the parser
/// and terminal from runaway output.
pub const TOOL_OUTPUT_MAX_LINE_BYTES: usize = 500;
const HR_CHAR: char = '─';
const MIN_COL_WIDTH: usize = 5;
const LONG_LINE_SUFFIX: &str = "...";

/// Semantic style token. Emphasis (bold/italic/strike/underline) lives on
/// the `Span`, not here, so they compose independently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StyleToken {
    Text,
    InlineCode,
    /// Syntax-highlighted token. Carries resolved rgb + modifiers so the
    /// consumer doesn't need to know the language.
    Highlight {
        fg: (u8, u8, u8),
        bold: bool,
        italic: bool,
        underline: bool,
    },
    CodeBar,
    Heading,
    ListMarker,
    TableBorder,
    HorizontalRule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: StyleToken,
    pub emphasis: Emphasis,
}

impl Span {
    pub fn new(text: impl Into<String>, style: StyleToken) -> Self {
        Self {
            text: text.into(),
            style,
            emphasis: Emphasis::default(),
        }
    }

    pub fn with_emphasis(text: impl Into<String>, style: StyleToken, emphasis: Emphasis) -> Self {
        Self {
            text: text.into(),
            style,
            emphasis,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineKind {
    Paragraph,
    Heading,
    ListItem,
    Code,
    TableBorder,
    TableRow,
    HorizontalRule,
    Blank,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Line {
    pub kind: LineKind,
    pub spans: Vec<Span>,
}

impl Line {
    pub fn blank() -> Self {
        Self {
            kind: LineKind::Blank,
            spans: Vec::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| s.text.width()).sum()
    }

    pub fn is_blank(&self) -> bool {
        self.spans.is_empty() || self.spans.iter().all(|s| s.text.is_empty())
    }
}

pub fn render(text: &str, width: u16) -> Vec<Line> {
    Renderer::new().render(text, width, 0)
}

/// Reuses highlighter and table-width caches across calls so streaming
/// (successive prefixes of a growing message) doesn't re-highlight completed
/// code lines. Bump `theme_gen` to flush caches after a theme change.
pub struct Renderer {
    highlighters: Vec<CodeHighlighter>,
    table_col_widths: Vec<Vec<usize>>,
    theme_gen: u64,
    wrap_paragraphs: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            highlighters: Vec::new(),
            table_col_widths: Vec::new(),
            theme_gen: 0,
            wrap_paragraphs: true,
        }
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Skip paragraph/heading/list wrapping (ratatui re-wraps those at
    /// paint time). Code blocks and tables still wrap.
    pub fn unwrapped() -> Self {
        Self {
            wrap_paragraphs: false,
            ..Self::default()
        }
    }

    pub fn render(&mut self, text: &str, width: u16, theme_gen: u64) -> Vec<Line> {
        if theme_gen != self.theme_gen {
            self.highlighters.clear();
            self.theme_gen = theme_gen;
        }
        let text = text.trim_start_matches('\n');
        let blocks = parse(text);
        let mut lines: Vec<Line> = Vec::new();
        let mut state = RenderState {
            code_idx: 0,
            table_idx: 0,
            highlighters: &mut self.highlighters,
            table_col_widths: &mut self.table_col_widths,
        };
        let ctx = RenderCtx {
            width,
            wrap_paragraphs: self.wrap_paragraphs,
        };

        for block in &blocks {
            render_block(block, &mut lines, &mut state, &ctx);
        }

        state.highlighters.truncate(state.code_idx);
        state.table_col_widths.truncate(state.table_idx);
        finalize_lines(&mut lines);
        lines
    }
}

struct RenderCtx {
    width: u16,
    wrap_paragraphs: bool,
}

struct RenderState<'a> {
    code_idx: usize,
    table_idx: usize,
    highlighters: &'a mut Vec<CodeHighlighter>,
    table_col_widths: &'a mut Vec<Vec<usize>>,
}

/// Streaming can split tokens differently than a oneshot render because the
/// highlighter sees partial input. Merging identical neighbours keeps the
/// span shape stable.
fn coalesce_adjacent_spans(spans: &mut Vec<Span>) {
    if spans.len() < 2 {
        return;
    }
    let mut write = 0;
    for read in 1..spans.len() {
        if spans[write].style == spans[read].style && spans[write].emphasis == spans[read].emphasis
        {
            let tail = mem::take(&mut spans[read].text);
            spans[write].text.push_str(&tail);
        } else {
            write += 1;
            if write != read {
                spans.swap(write, read);
            }
        }
    }
    spans.truncate(write + 1);
}

fn render_block(
    block: &Block,
    lines: &mut Vec<Line>,
    state: &mut RenderState<'_>,
    ctx: &RenderCtx,
) {
    match block {
        Block::Lines(line_blocks) => {
            for lb in line_blocks {
                render_line_block(lb, lines, ctx);
            }
        }
        Block::Code { lang, code } => {
            ensure_blank_line(lines);
            if state.code_idx >= state.highlighters.len() {
                state.highlighters.push(CodeHighlighter::new(lang));
            }
            let segments: Vec<_> = state.highlighters[state.code_idx].update(code).to_vec();
            let start = lines.len();
            for segs in segments {
                let mut spans = vec![Span::new(CODE_BAR, StyleToken::CodeBar)];
                for seg in segs {
                    spans.push(Span::new(
                        seg.text,
                        StyleToken::Highlight {
                            fg: seg.fg,
                            bold: seg.bold,
                            italic: seg.italic,
                            underline: seg.underline,
                        },
                    ));
                }
                coalesce_adjacent_spans(&mut spans);
                lines.push(Line {
                    kind: LineKind::Code,
                    spans,
                });
            }
            wrap_code_lines(lines, start, ctx.width);
            ensure_blank_line(lines);
            state.code_idx += 1;
        }
        Block::Table { rows, header_end } => {
            ensure_blank_line(lines);
            if state.table_idx >= state.table_col_widths.len() {
                state
                    .table_col_widths
                    .resize_with(state.table_idx + 1, Vec::new);
            }
            let pw = &mut state.table_col_widths[state.table_idx];
            lines.extend(render_table(rows, *header_end, ctx.width, pw));
            ensure_blank_line(lines);
            state.table_idx += 1;
        }
    }
}

fn render_line_block(lb: &LineBlock, lines: &mut Vec<Line>, ctx: &RenderCtx) {
    if matches!(lb.kind, BlockKind::HorizontalRule) {
        lines.push(Line {
            kind: LineKind::HorizontalRule,
            spans: vec![Span::new(hr_text(ctx.width), StyleToken::HorizontalRule)],
        });
        return;
    }

    let marker = block_prefix(&lb.kind).map(|p| Span::new(p, StyleToken::ListMarker));

    let is_heading = matches!(lb.kind, BlockKind::Heading(_));
    let kind = match &lb.kind {
        BlockKind::Heading(_) => LineKind::Heading,
        BlockKind::UnorderedListItem { .. } | BlockKind::OrderedListItem { .. } => {
            LineKind::ListItem
        }
        _ => LineKind::Paragraph,
    };

    let mut content_spans: Vec<Span> = Vec::new();
    for InlineSpan {
        text,
        kind: sk,
        emphasis,
    } in parse_inline(&lb.inline)
    {
        // Code keeps its own token inside headings so consumers can layer
        // code colours on top. The Lua bridge collapses to one name per span.
        let style = if sk == SpanKind::Code {
            StyleToken::InlineCode
        } else if is_heading {
            StyleToken::Heading
        } else {
            StyleToken::Text
        };
        content_spans.push(Span::with_emphasis(text, style, emphasis));
    }

    let marker_width = marker.as_ref().map_or(0, |m| m.text.width());
    let width = ctx.width as usize;

    if width == 0 || !ctx.wrap_paragraphs {
        let mut spans = Vec::new();
        if let Some(m) = marker {
            spans.push(m);
        }
        spans.extend(content_spans);
        lines.push(Line { kind, spans });
        return;
    }

    // If the marker is wider than the line, it gets its own row.
    // Otherwise it shares row 1 and continuations indent to align.
    let (first_row_marker, cont_indent, content_width) = if marker_width >= width {
        if let Some(mut mk) = marker {
            mk.text = mk.text.trim_start_matches(' ').to_owned();
            lines.push(Line {
                kind: kind.clone(),
                spans: vec![mk],
            });
        }
        (None, None, width)
    } else {
        let indent = marker
            .as_ref()
            .map(|_| Span::new(" ".repeat(marker_width), StyleToken::ListMarker));
        (marker, indent, width - marker_width)
    };

    let wrapped = wrap_spans(content_spans, content_width);

    if wrapped.is_empty() {
        if let Some(m) = first_row_marker {
            lines.push(Line {
                kind,
                spans: vec![m],
            });
        }
        return;
    }

    let mut first_row_marker = first_row_marker;
    for (i, row) in wrapped.into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            if let Some(m) = first_row_marker.take() {
                spans.push(m);
            }
        } else if let Some(ref ind) = cont_indent {
            spans.push(ind.clone());
        }
        spans.extend(row);
        lines.push(Line {
            kind: kind.clone(),
            spans,
        });
    }
}

fn finalize_lines(lines: &mut Vec<Line>) {
    let mut write = 0;
    let mut prev_blank = false;
    for read in 0..lines.len() {
        let blank = lines[read].is_blank();
        if blank && prev_blank {
            continue;
        }
        if write != read {
            lines.swap(write, read);
        }
        write += 1;
        prev_blank = blank;
    }
    lines.truncate(write);
    while lines.last().is_some_and(Line::is_blank) {
        lines.pop();
    }
}

fn ensure_blank_line(lines: &mut Vec<Line>) {
    if !lines.last().is_some_and(Line::is_blank) {
        lines.push(Line::blank());
    }
}

fn fit_width(text: &str, max_width: usize) -> usize {
    let mut width = 0;
    for (i, ch) in text.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return i;
        }
        width += cw;
    }
    text.len()
}

fn wrap_code_lines(lines: &mut Vec<Line>, start: usize, width: u16) {
    let width = width as usize;
    if width == 0 {
        return;
    }
    let tail = lines.split_off(start);
    for line in tail {
        if line.width() <= width {
            lines.push(line);
        } else {
            lines.extend(split_line_with_bar(line, width));
        }
    }
}

fn split_line_with_bar(line: Line, width: usize) -> Vec<Line> {
    if line.spans.is_empty() {
        return vec![line];
    }

    let bar_span = line.spans[0].clone();
    let content_spans = &line.spans[1..];
    let first_avail = width.saturating_sub(CODE_BAR.width());
    let cont_avail = width.saturating_sub(CODE_BAR_WRAP.width());

    let mut result: Vec<Line> = Vec::new();
    let mut current_spans: Vec<Span> = vec![bar_span];
    let mut remaining = first_avail;

    for span in content_spans {
        let mut text = span.text.as_str();
        let style = span.style.clone();
        let emphasis = span.emphasis;

        while !text.is_empty() {
            let fits = fit_width(text, remaining);
            if fits == 0 {
                if current_spans.len() > 1 {
                    result.push(Line {
                        kind: LineKind::Code,
                        spans: mem::take(&mut current_spans),
                    });
                    current_spans = vec![Span::new(CODE_BAR_WRAP, StyleToken::CodeBar)];
                    remaining = cont_avail;
                    continue;
                }
                let ch_len = text.chars().next().map_or(1, char::len_utf8);
                current_spans.push(Span::with_emphasis(
                    text[..ch_len].to_owned(),
                    style.clone(),
                    emphasis,
                ));
                text = &text[ch_len..];
                result.push(Line {
                    kind: LineKind::Code,
                    spans: mem::take(&mut current_spans),
                });
                current_spans = vec![Span::new(CODE_BAR_WRAP, StyleToken::CodeBar)];
                remaining = cont_avail;
                continue;
            }
            current_spans.push(Span::with_emphasis(
                text[..fits].to_owned(),
                style.clone(),
                emphasis,
            ));
            remaining -= text[..fits].width();
            text = &text[fits..];
            if !text.is_empty() {
                result.push(Line {
                    kind: LineKind::Code,
                    spans: mem::take(&mut current_spans),
                });
                current_spans = vec![Span::new(CODE_BAR_WRAP, StyleToken::CodeBar)];
                remaining = cont_avail;
            }
        }
    }

    if current_spans.len() > 1 || result.is_empty() {
        result.push(Line {
            kind: LineKind::Code,
            spans: current_spans,
        });
    }

    result
}

fn cell_display_width(cell: &str) -> usize {
    parse_inline(cell).iter().map(|s| s.text.width()).sum()
}

fn constrain_col_widths(col_widths: &mut [usize], available: usize) {
    let total: usize = col_widths.iter().sum();
    if total <= available {
        return;
    }
    for w in col_widths.iter_mut() {
        *w = (*w * available / total).max(MIN_COL_WIDTH).min(*w);
    }
    let mut excess = col_widths.iter().sum::<usize>().saturating_sub(available);
    while excess > 0 {
        let max_w = col_widths.iter().copied().max().unwrap_or(0);
        if max_w <= MIN_COL_WIDTH {
            break;
        }
        for w in col_widths.iter_mut() {
            if excess == 0 {
                break;
            }
            if *w == max_w && *w > MIN_COL_WIDTH {
                *w -= 1;
                excess -= 1;
            }
        }
    }
}

/// Soft-break on spaces, hard-break on char boundaries for long runs.
fn wrap_spans(spans: Vec<Span>, max_width: usize) -> Vec<Vec<Span>> {
    if max_width == 0 {
        return vec![spans];
    }
    let mut result: Vec<Vec<Span>> = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut remaining = max_width;

    for span in spans {
        let mut text = span.text.as_str();
        let style = span.style.clone();
        let emphasis = span.emphasis;

        while !text.is_empty() {
            let fits = fit_width(text, remaining);
            if fits == 0 {
                if current.is_empty() {
                    let ch_len = text.chars().next().map_or(1, char::len_utf8);
                    current.push(Span::with_emphasis(
                        text[..ch_len].to_owned(),
                        style.clone(),
                        emphasis,
                    ));
                    text = &text[ch_len..];
                }
                result.push(mem::take(&mut current));
                remaining = max_width;
                text = text.strip_prefix(' ').unwrap_or(text);
                continue;
            }
            let (take, skip) = if fits < text.len() {
                match text[..fits].rfind(' ') {
                    Some(sp) if sp > 0 => (sp, sp + 1),
                    _ => (fits, fits),
                }
            } else {
                (fits, fits)
            };
            current.push(Span::with_emphasis(
                text[..take].to_owned(),
                style.clone(),
                emphasis,
            ));
            remaining -= text[..take].width();
            text = &text[skip..];
            if take < fits && !text.is_empty() {
                result.push(mem::take(&mut current));
                remaining = max_width;
            }
        }
    }
    if !current.is_empty() || result.is_empty() {
        result.push(current);
    }
    result
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.width()).sum()
}

fn cell_spans(cell: &str, header: bool) -> Vec<Span> {
    parse_inline(cell)
        .into_iter()
        .map(
            |InlineSpan {
                 text,
                 kind,
                 emphasis,
             }| {
                let mut emphasis = emphasis;
                if header {
                    emphasis.bold = true;
                }
                let style = if kind == SpanKind::Code {
                    StyleToken::InlineCode
                } else {
                    StyleToken::Text
                };
                Span::with_emphasis(text, style, emphasis)
            },
        )
        .collect()
}

fn render_table(
    rows: &[Vec<String>],
    header_end: usize,
    width: u16,
    persistent_widths: &mut Vec<usize>,
) -> Vec<Line> {
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let overhead = col_count * 3 + 1;
    let min_box_width = overhead + col_count * MIN_COL_WIDTH;
    if (width as usize) < min_box_width {
        return render_table_compact(rows, header_end, width);
    }

    let mut col_widths = vec![0usize; col_count];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(cell_display_width(cell));
        }
    }

    let available = (width as usize) - overhead;

    persistent_widths.resize(persistent_widths.len().max(col_count), 0);
    for (i, w) in col_widths.iter_mut().enumerate() {
        persistent_widths[i] = persistent_widths[i].max(*w);
        *w = persistent_widths[i];
    }

    constrain_col_widths(&mut col_widths, available);

    let mut lines = Vec::new();

    let border = |left: &str, mid: &str, right: &str, fill: &str| -> Line {
        let mut spans = vec![Span::new(left, StyleToken::TableBorder)];
        for (i, &w) in col_widths.iter().enumerate() {
            spans.push(Span::new(fill.repeat(w + 2), StyleToken::TableBorder));
            if i < col_count - 1 {
                spans.push(Span::new(mid, StyleToken::TableBorder));
            }
        }
        spans.push(Span::new(right, StyleToken::TableBorder));
        Line {
            kind: LineKind::TableBorder,
            spans,
        }
    };

    lines.push(border("╭", "┬", "╮", "─"));

    for (ri, row) in rows.iter().enumerate() {
        let header = ri < header_end;

        let wrapped_cells: Vec<Vec<Vec<Span>>> = (0..col_count)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                wrap_spans(cell_spans(cell, header), col_widths[c])
            })
            .collect();

        let row_height = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
        let row_emphasis = if header {
            Emphasis::BOLD
        } else {
            Emphasis::default()
        };

        for line_idx in 0..row_height {
            let mut spans = vec![Span::new("│ ", StyleToken::TableBorder)];
            for (c, &w) in col_widths.iter().enumerate() {
                let sub_line = wrapped_cells[c].get(line_idx);
                let content_width = sub_line.map_or(0, |sl| spans_width(sl));

                let pad = w.saturating_sub(content_width);

                if let Some(sl) = sub_line {
                    spans.extend(sl.iter().cloned());
                }
                spans.push(Span::with_emphasis(
                    " ".repeat(pad + 1),
                    StyleToken::Text,
                    row_emphasis,
                ));
                if c < col_count - 1 {
                    spans.push(Span::new("│ ", StyleToken::TableBorder));
                } else {
                    spans.push(Span::new("│", StyleToken::TableBorder));
                }
            }
            lines.push(Line {
                kind: LineKind::TableRow,
                spans,
            });
        }

        if ri + 1 < rows.len() {
            lines.push(border("├", "┼", "┤", "─"));
        }
    }

    lines.push(border("╰", "┴", "╯", "─"));

    lines
}

pub fn hr_text(width: u16) -> String {
    iter::repeat_n(HR_CHAR, width as usize).collect()
}

pub fn truncate_long_lines(text: &str) -> Cow<'_, str> {
    truncate_long_lines_at(text, TOOL_OUTPUT_MAX_LINE_BYTES)
}

pub fn truncate_long_lines_at(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if !text.lines().any(|l| l.len() > max_bytes) {
        return Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if line.len() > max_bytes {
            let mut boundary = max_bytes;
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            result.push_str(&line[..boundary]);
            result.push_str(LONG_LINE_SUFFIX);
        } else {
            result.push_str(line);
        }
    }
    if text.ends_with('\n') {
        result.push('\n');
    }
    Cow::Owned(result)
}

/// Fallback when the terminal is too narrow for box-drawing borders.
fn render_table_compact(rows: &[Vec<String>], header_end: usize, width: u16) -> Vec<Line> {
    const CELL_SEP: &str = " | ";
    let mut lines = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let header = ri < header_end;
        let mut spans: Vec<Span> = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            if c > 0 {
                spans.push(Span::new(CELL_SEP, StyleToken::TableBorder));
            }
            spans.extend(cell_spans(cell, header));
        }
        for row_spans in wrap_spans(spans, width as usize) {
            lines.push(Line {
                kind: LineKind::TableRow,
                spans: row_spans,
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TEST_WIDTH: u16 = 80;

    fn lines_text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    fn find_span<'a>(lines: &'a [Line], text: &str) -> Option<&'a Span> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.text.trim() == text)
    }

    #[test]
    fn render_empty_input_yields_no_lines() {
        assert!(render("", TEST_WIDTH).is_empty());
    }

    #[test]
    fn render_bold_emits_text_with_bold_emphasis() {
        let lines = render("**bold**", TEST_WIDTH);
        let span = find_span(&lines, "bold").expect("bold span");
        assert_eq!(span.style, StyleToken::Text);
        assert_eq!(span.emphasis, Emphasis::BOLD);
    }

    #[test]
    fn render_inline_code_emits_inline_code_token() {
        let lines = render("a `b` c", TEST_WIDTH);
        let code = find_span(&lines, "b").expect("code span");
        assert_eq!(code.style, StyleToken::InlineCode);
    }

    #[test_case(1; "h1")]
    #[test_case(3; "h3")]
    #[test_case(6; "h6")]
    fn render_heading_emits_heading_kind_and_token(level: u8) {
        let input = format!("{} hello", "#".repeat(level as usize));
        let lines = render(&input, TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::Heading);
        let hello = lines[0]
            .spans
            .iter()
            .find(|s| s.text == "hello")
            .expect("hello span");
        assert_eq!(hello.style, StyleToken::Heading);
    }

    #[test]
    fn render_heading_preserves_inline_styles() {
        let lines = render("## **bold** and `code`", TEST_WIDTH);
        assert_eq!(
            find_span(&lines, "code").unwrap().style,
            StyleToken::InlineCode
        );
        assert_eq!(
            find_span(&lines, "bold").unwrap().style,
            StyleToken::Heading
        );
    }

    #[test]
    fn render_horizontal_rule_emits_hr_token() {
        let lines = render("---", TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::HorizontalRule);
        assert_eq!(lines[0].spans[0].style, StyleToken::HorizontalRule);
    }

    #[test]
    fn render_unordered_list_marker_then_content() {
        let lines = render("- item", TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::ListItem);
        assert_eq!(lines[0].spans[0].text, "• ");
        assert_eq!(lines[0].spans[0].style, StyleToken::ListMarker);
        assert_eq!(lines[0].spans[1].text, "item");
    }

    #[test]
    fn render_code_block_emits_code_bar_then_highlight_tokens() {
        let lines = render("```rust\nfn x() {}\n```", TEST_WIDTH);
        let code_lines: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
        assert!(!code_lines.is_empty());
        assert_eq!(code_lines[0].spans[0].style, StyleToken::CodeBar);
        assert!(
            code_lines[0]
                .spans
                .iter()
                .skip(1)
                .all(|s| matches!(s.style, StyleToken::Highlight { .. })),
            "code line content spans must be highlight tokens"
        );
    }

    #[test]
    fn render_code_block_wraps_long_lines_with_continuation_bar() {
        let code = "a".repeat(40);
        let input = format!("```\n{code}\n```");
        let lines = render(&input, 15);
        let code_lines: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
        assert!(code_lines.len() > 1, "long code line should wrap");
        for line in &code_lines {
            assert!(line.width() <= 15);
            assert_eq!(line.spans[0].style, StyleToken::CodeBar);
        }
        let bar_text: Vec<_> = code_lines
            .iter()
            .map(|l| l.spans[0].text.as_str())
            .collect();
        assert_eq!(bar_text[0], CODE_BAR);
        assert_eq!(bar_text[1], CODE_BAR_WRAP);
    }

    #[test]
    fn render_code_block_narrow_width_does_not_loop() {
        let input = "```\n\u{4e16}\u{754c}\n```";
        for w in 1..=3 {
            let lines = render(input, w);
            let code_lines: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
            assert!(
                !code_lines.is_empty(),
                "width={w} should produce code lines"
            );
        }
    }

    #[test]
    fn render_table_emits_borders_and_rows() {
        let lines = render("| H |\n| --- |\n| d |", TEST_WIDTH);
        assert!(
            lines
                .iter()
                .filter(|l| l.kind == LineKind::TableBorder)
                .count()
                >= 2
        );
        assert!(lines.iter().any(|l| l.kind == LineKind::TableRow));
    }

    #[test]
    fn render_table_wraps_overflowing_cells_within_width() {
        let long = "x".repeat(60);
        let input = format!("| Col1 | Col2 |\n| --- | --- |\n| short | {long} |");
        let width: u16 = 40;
        let lines = render(&input, width);
        assert!(
            lines.iter().all(|l| l.width() <= width as usize),
            "line overflow"
        );
        let x_count: usize = lines_text(&lines).join("").matches('x').count();
        assert_eq!(x_count, 60, "wrap must preserve content");
    }

    #[test]
    fn render_never_emits_consecutive_blanks() {
        let lines = render("before\n```\ncode\n```\nafter", TEST_WIDTH);
        let consecutive = lines.windows(2).any(|w| w[0].is_blank() && w[1].is_blank());
        assert!(!consecutive, "should never have two consecutive blanks");
    }

    #[test]
    fn renderer_caches_table_column_widths_across_calls() {
        let mut r = Renderer::new();
        r.render("| A | B |\n| --- | --- |\n| hi | there |", 120, 0);
        let widths_before = r.table_col_widths[0].clone();
        r.render(
            "| A | B |\n| --- | --- |\n| hi | there |\n| longer | x |",
            120,
            0,
        );
        for (i, (&old, &new)) in widths_before.iter().zip(&r.table_col_widths[0]).enumerate() {
            assert!(new >= old, "table width shrank at col {i}: {old} -> {new}");
        }
    }

    #[test]
    fn render_width_zero_does_not_panic() {
        let _ = render("```\nhello\n```", 0);
    }

    #[test]
    fn render_table_header_row_cells_are_bold() {
        let lines = render("| Header |\n| --- |\n| Data |", TEST_WIDTH);
        let header = find_span(&lines, "Header").expect("header span");
        assert!(header.emphasis.bold, "header cells must be bold");
    }

    #[test]
    fn truncate_long_lines_behavior() {
        let max = TOOL_OUTPUT_MAX_LINE_BYTES;

        assert_eq!(&*truncate_long_lines("short\nlines\n"), "short\nlines\n");
        assert_eq!(&*truncate_long_lines(&"a".repeat(max)), "a".repeat(max));
        assert_eq!(
            &*truncate_long_lines(&"a".repeat(max + 1)),
            format!("{}{LONG_LINE_SUFFIX}", "a".repeat(max))
        );

        let mut multibyte = "a".repeat(max - 1);
        multibyte.push('\u{00e9}');
        let result = truncate_long_lines(&multibyte);
        assert!(result.ends_with(LONG_LINE_SUFFIX));
        assert!(!result.contains('\u{00e9}'));

        let with_nl = format!("{}\n", "z".repeat(max + 10));
        assert!(truncate_long_lines(&with_nl).ends_with('\n'));
        let without_nl = "z".repeat(max + 10);
        assert!(!truncate_long_lines(&without_nl).ends_with('\n'));
    }

    #[test]
    fn streaming_matches_oneshot() {
        const CORPUS: &[&str] = &[
            "hello world\n# heading\n\npara",
            "```rust\nfn main() {}\n```",
            "| H1 | H2 |\n| --- | --- |\n| a | b |\n| c | d |",
            "## title with `code`\n\n- one\n- two\n- three\n\n```py\nx=1\ny=2\n```\nend",
        ];
        const WIDTHS: &[u16] = &[20, 40, 80];
        for text in CORPUS {
            for &w in WIDTHS {
                let oneshot = Renderer::new().render(text, w, 0);
                let mut streamer = Renderer::new();
                for end in 1..text.len() {
                    if !text.is_char_boundary(end) {
                        continue;
                    }
                    let _ = streamer.render(&text[..end], w, 0);
                }
                let final_streamed = streamer.render(text, w, 0);
                assert_eq!(final_streamed, oneshot, "mismatch text={text:?} width={w}");
            }
        }
    }

    #[test]
    fn finalize_lines_collapses_consecutive_blanks() {
        let para = |t| Line {
            kind: LineKind::Paragraph,
            spans: vec![Span::new(t, StyleToken::Text)],
        };
        let mut lines = vec![
            para("a"),
            Line::blank(),
            Line::blank(),
            Line::blank(),
            para("b"),
            Line::blank(),
            para("c"),
            Line::blank(),
            Line::blank(),
        ];
        finalize_lines(&mut lines);
        assert!(!lines.last().is_some_and(Line::is_blank));
        assert!(!lines.windows(2).any(|w| w[0].is_blank() && w[1].is_blank()));
        assert_eq!(lines_text(&lines), vec!["a", "", "b", "", "c"]);
    }

    #[test]
    fn theme_gen_change_clears_highlighter_cache() {
        let mut r = Renderer::new();
        let code = "```rust\nlet x = 42;\n```";
        r.render(code, TEST_WIDTH, 0);
        assert_eq!(r.highlighters.len(), 1);
        r.render(code, TEST_WIDTH, 1);
        assert_eq!(r.theme_gen, 1);
        assert_eq!(r.highlighters.len(), 1);
    }

    #[test]
    fn unwrapped_mode_skips_paragraph_wrap_but_wraps_code() {
        let long_para = "word ".repeat(50);
        let mut r = Renderer::unwrapped();
        let para_lines = r.render(long_para.trim(), 30, 0);
        assert_eq!(
            para_lines
                .iter()
                .filter(|l| l.kind == LineKind::Paragraph)
                .count(),
            1
        );

        let long_code = "a".repeat(60);
        let input = format!("```\n{long_code}\n```");
        let code_lines = r.render(&input, 20, 0);
        assert!(
            code_lines
                .iter()
                .filter(|l| l.kind == LineKind::Code)
                .count()
                > 1
        );
    }

    #[test]
    fn table_compact_fallback_at_small_width() {
        let lines = render("| aa | bb |\n| --- | --- |\n| cc | dd |", 10);
        assert!(
            !lines.iter().any(|l| l.kind == LineKind::TableBorder),
            "compact: no borders"
        );
        assert!(
            lines.iter().any(|l| l.kind == LineKind::TableRow),
            "compact: has rows"
        );
    }

    #[test]
    fn multiple_code_blocks_get_separate_highlighters() {
        let mut r = Renderer::new();
        r.render(
            "```rust\nfn a() {}\n```\n\n```python\nx = 1\n```",
            TEST_WIDTH,
            0,
        );
        assert_eq!(r.highlighters.len(), 2);
        r.render("```rust\nfn a() {}\n```", TEST_WIDTH, 0);
        assert_eq!(r.highlighters.len(), 1);
    }

    #[test]
    fn paragraph_wrapping_preserves_all_content() {
        const INPUT: &str = "The **quick** brown _fox_ jumps over the `lazy` dog repeatedly";
        let lines = render(INPUT, 20);
        let rendered: String = lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.text.as_str())
            .collect();
        let expected = INPUT.replace("**", "").replace(['_', '`'], "");
        assert_eq!(
            rendered, expected,
            "wrapped output must preserve all visible text"
        );
    }

    #[test]
    fn coalesce_merges_same_style_splits_different() {
        let mut spans = vec![
            Span::new("aa", StyleToken::Text),
            Span::new("bb", StyleToken::Text),
            Span::new("cc", StyleToken::InlineCode),
            Span::new("dd", StyleToken::InlineCode),
            Span::new("plain", StyleToken::Text),
            Span::with_emphasis("bold", StyleToken::Text, Emphasis::BOLD),
        ];
        coalesce_adjacent_spans(&mut spans);
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].text, "aabb");
        assert_eq!(spans[1].text, "ccdd");
        assert_eq!(spans[2].text, "plain");
        assert_eq!(spans[3].text, "bold");
    }

    #[test_case(10 ; "narrow")]
    #[test_case(40 ; "medium")]
    #[test_case(120 ; "wide")]
    fn table_with_empty_cell_does_not_panic(width: u16) {
        let lines = render("| a | | c |\n| --- | --- | --- |\n| d | | f |", width);
        assert!(
            lines
                .iter()
                .filter(|l| l.kind == LineKind::TableRow)
                .count()
                >= 2
        );
    }

    #[test]
    fn ordered_list_emits_correct_marker() {
        let lines = render("1. first", TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::ListItem);
        assert_eq!(lines[0].spans[0].style, StyleToken::ListMarker);
        assert!(find_span(&lines, "first").is_some());
    }

    #[test]
    fn wrap_spans_hard_breaks_unbreakable_run() {
        let long_word = "x".repeat(30);
        let wrapped = wrap_spans(vec![Span::new(long_word.clone(), StyleToken::Text)], 10);
        assert!(wrapped.len() >= 3);
        let reassembled: String = wrapped
            .iter()
            .flat_map(|row| row.iter())
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(reassembled, long_word);
        assert!(wrapped.iter().all(|row| spans_width(row) <= 10));
    }

    #[test]
    fn render_leading_newlines_are_stripped() {
        let lines = render("\n\n\nhello", TEST_WIDTH);
        assert!(!lines.is_empty());
        assert_eq!(find_span(&lines, "hello").unwrap().text, "hello");
    }

    #[test]
    fn truncate_long_line_preserves_table_detection() {
        let long_cell = "A".repeat(500);
        let text = format!("| What | Why |\n| --- | --- |\n| Short cell | {long_cell} |");

        let full = Renderer::unwrapped().render(&text, 120, 0);
        assert!(
            full.iter().any(|l| l.kind == LineKind::TableBorder),
            "full text must parse as a table"
        );

        let truncated = truncate_long_lines_at(&text, 500);
        assert_ne!(truncated.as_ref(), text, "truncation must modify the text");

        let trunc = Renderer::unwrapped().render(&truncated, 120, 0);
        assert!(
            trunc.iter().any(|l| l.kind == LineKind::TableBorder),
            "table detection survives truncation (parser is lenient about missing closing |)"
        );
    }
}
