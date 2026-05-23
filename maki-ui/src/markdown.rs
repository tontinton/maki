use std::borrow::Cow;
use std::iter;
use std::mem;

use crate::highlight;
use crate::highlight::CodeHighlighter;
use crate::theme;
use maki_markdown::{Block, BlockKind, Emphasis, InlineSpan, LineBlock, SpanKind, parse};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub(crate) const CODE_BAR: &str = "│ ";
pub(crate) const CODE_BAR_WRAP: &str = "│";
pub const TRUNCATION_PREFIX: &str = "...";
const MIN_TRUNCATABLE_LINES: usize = 2;
const HR_CHAR: char = '─';
const MIN_COL_WIDTH: usize = 5;
const MAX_LINE_CHARS: usize = 500;

/// Add `over`'s modifiers on top of `base`, keeping `base`'s colors.
/// Used for emphasis (bold, italic, strike): so `## **bold**` keeps the
/// heading color and just picks up the bold flag.
fn apply_modifiers(base: Style, over: Style) -> Style {
    base.add_modifier(over.add_modifier)
        .remove_modifier(over.sub_modifier)
}

/// Recolor `base` with `over`'s colors and OR the modifiers. Used for code
/// spans, where the color carries meaning, and for body text where emphasis
/// supplies its own theme color.
fn overlay_style(base: Style, over: Style) -> Style {
    let mut out = base;
    if let Some(fg) = over.fg {
        out.fg = Some(fg);
    }
    if let Some(bg) = over.bg {
        out.bg = Some(bg);
    }
    apply_modifiers(out, over)
}

/// Emphasis normally recolors with its theme style so `**bold**` stands out
/// in body text. When `preserve_base_color` is set (headings), emphasis
/// only adds modifiers so the heading color survives. Code always overlays
/// its color, because "this is code" is semantic and beats context.
fn style_for(kind: SpanKind, emphasis: Emphasis, base: Style, preserve_base_color: bool) -> Style {
    let t = theme::current();
    let emph_style = match (emphasis.bold, emphasis.italic) {
        (true, true) => Some(t.bold_italic),
        (true, false) => Some(t.bold),
        (false, true) => Some(t.italic),
        (false, false) => None,
    };
    let combine = |s: Style, over: Style| {
        if preserve_base_color {
            apply_modifiers(s, over)
        } else {
            overlay_style(s, over)
        }
    };
    let mut style = base;
    if let Some(es) = emph_style {
        style = combine(style, es);
    }
    if emphasis.strike {
        style = combine(style, t.strikethrough);
    }
    if kind == SpanKind::Code {
        style = overlay_style(style, t.inline_code);
    }
    style
}

pub fn parse_inline_markdown(text: &str, base_style: Style) -> Vec<Span<'static>> {
    parse_inline_with_base(text, base_style, false)
}

fn parse_inline_with_base(
    text: &str,
    base_style: Style,
    preserve_base_color: bool,
) -> Vec<Span<'static>> {
    maki_markdown::parse_inline(text)
        .into_iter()
        .map(
            |InlineSpan {
                 text,
                 kind,
                 emphasis,
             }| {
                Span::styled(
                    text,
                    style_for(kind, emphasis, base_style, preserve_base_color),
                )
            },
        )
        .collect()
}

fn fit_width(text: &str, max_width: usize) -> usize {
    let mut width = 0;
    for (i, ch) in text.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return i;
        }
        width += cw;
    }
    text.len()
}

fn prepend_code_bar(line: &mut Line<'static>) {
    line.spans
        .insert(0, Span::styled(CODE_BAR, theme::current().code_bar));
}

pub(crate) fn wrap_code_lines(lines: &mut Vec<Line<'static>>, start: usize, width: u16) {
    let width = width as usize;
    if width == 0 {
        return;
    }
    let tail = lines.split_off(start);
    for line in tail {
        let line_width: usize = line.spans.iter().map(|s| s.content.width()).sum();
        if line_width <= width {
            lines.push(line);
        } else {
            lines.extend(split_line_with_bar(line, width));
        }
    }
}

fn split_line_with_bar(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() {
        return vec![line];
    }

    let bar_span = line.spans[0].clone();
    let content_spans = &line.spans[1..];
    let first_avail = width.saturating_sub(CODE_BAR.width());
    let cont_avail = width.saturating_sub(CODE_BAR_WRAP.width());

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = vec![bar_span];
    let mut remaining = first_avail;

    for span in content_spans {
        let mut text = span.content.as_ref();
        let style = span.style;

        while !text.is_empty() {
            let fits = fit_width(text, remaining);
            if fits == 0 && remaining == 0 {
                break;
            }
            if fits > 0 {
                current_spans.push(Span::styled(text[..fits].to_owned(), style));
                remaining -= text[..fits].width();
                text = &text[fits..];
            }
            if !text.is_empty() {
                result.push(Line::from(current_spans));
                current_spans = vec![Span::styled(CODE_BAR_WRAP, theme::current().code_bar)];
                remaining = cont_avail;
            }
        }
    }

    if current_spans.len() > 1 || result.is_empty() {
        result.push(Line::from(current_spans));
    }

    result
}

fn highlight_code(lang: &str, code: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = highlight::highlight_code_plain(lang, code);
    for line in &mut lines {
        prepend_code_bar(line);
    }
    wrap_code_lines(&mut lines, 0, width);
    lines
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Keep {
    Head,
    Tail,
}

pub fn should_truncate(hidden: usize) -> bool {
    hidden >= MIN_TRUNCATABLE_LINES
}

pub fn truncation_notice(count: usize) -> String {
    debug_assert!(
        should_truncate(count),
        "truncation_notice called with count={count} below threshold"
    );
    format!("{TRUNCATION_PREFIX} ({count} lines) click to expand")
}

pub struct Truncated<'a> {
    pub kept: &'a str,
    pub skipped: usize,
    keep: Keep,
}

impl Truncated<'_> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn notice_line(&self) -> Option<Line<'static>> {
        if should_truncate(self.skipped) {
            let text = truncation_notice(self.skipped);
            Some(Line::from(Span::styled(text, theme::current().tool_dim)))
        } else {
            None
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_string(self) -> String {
        if !should_truncate(self.skipped) {
            return self.kept.to_owned();
        }
        let notice = truncation_notice(self.skipped);
        match self.keep {
            Keep::Head => format!("{}\n{notice}", self.kept),
            Keep::Tail => format!("{notice}\n{}", self.kept),
        }
    }
}

pub(crate) fn hr_line(width: u16, style: Style) -> Line<'static> {
    let hr: String = iter::repeat_n(HR_CHAR, width as usize).collect();
    Line::from(Span::styled(hr, style))
}

fn cell_display_width(cell: &str) -> usize {
    parse_inline_markdown(cell, Style::default())
        .iter()
        .map(|s| s.content.width())
        .sum()
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

fn wrap_cell_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 {
        return vec![spans];
    }
    let mut result: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut remaining = max_width;

    for span in spans {
        let mut text = span.content.as_ref();
        let style = span.style;

        while !text.is_empty() {
            let fits = fit_width(text, remaining);
            if fits == 0 {
                if current.is_empty() {
                    let ch_len = text.chars().next().map_or(1, char::len_utf8);
                    current.push(Span::styled(text[..ch_len].to_owned(), style));
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
            current.push(Span::styled(text[..take].to_owned(), style));
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

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn render_table(
    rows: &[Vec<String>],
    header_end: usize,
    text_style: Style,
    width: u16,
    persistent_widths: Option<&mut Vec<usize>>,
) -> Vec<Line<'static>> {
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let mut col_widths = vec![0usize; col_count];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(cell_display_width(cell));
        }
    }

    let overhead = col_count * 3 + 1;
    let available = (width as usize).saturating_sub(overhead);

    if let Some(pw) = persistent_widths {
        pw.resize(pw.len().max(col_count), 0);
        for (i, w) in col_widths.iter_mut().enumerate() {
            pw[i] = pw[i].max(*w);
            *w = pw[i];
        }
    }

    constrain_col_widths(&mut col_widths, available);

    let mut lines = Vec::new();

    let border = |left: &str, mid: &str, right: &str, fill: &str| -> Line<'static> {
        let tbs = theme::current().table_border;
        let mut spans = vec![Span::styled(left.to_owned(), tbs)];
        for (i, &w) in col_widths.iter().enumerate() {
            spans.push(Span::styled(fill.repeat(w + 2), tbs));
            if i < col_count - 1 {
                spans.push(Span::styled(mid.to_owned(), tbs));
            }
        }
        spans.push(Span::styled(right.to_owned(), tbs));
        Line::from(spans)
    };

    lines.push(border("╭", "┬", "╮", "─"));

    for (ri, row) in rows.iter().enumerate() {
        let base = if ri < header_end {
            overlay_style(text_style, theme::current().bold)
        } else {
            text_style
        };

        let wrapped_cells: Vec<Vec<Vec<Span<'static>>>> = (0..col_count)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                let cell_spans = parse_inline_markdown(cell, base);
                wrap_cell_spans(cell_spans, col_widths[c])
            })
            .collect();

        let row_height = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);

        for line_idx in 0..row_height {
            let mut spans = vec![Span::styled("│ ".to_owned(), theme::current().table_border)];
            for (c, &w) in col_widths.iter().enumerate() {
                let sub_line = wrapped_cells[c].get(line_idx);
                let content_width = sub_line.map_or(0, |sl| spans_width(sl));
                let pad = w.saturating_sub(content_width);

                if let Some(sl) = sub_line {
                    spans.extend(sl.iter().cloned());
                }
                spans.push(Span::styled(" ".repeat(pad + 1), base));
                if c < col_count - 1 {
                    spans.push(Span::styled("│ ".to_owned(), theme::current().table_border));
                } else {
                    spans.push(Span::styled("│".to_owned(), theme::current().table_border));
                }
            }
            lines.push(Line::from(spans));
        }

        if ri + 1 < rows.len() {
            lines.push(border("├", "┼", "┤", "─"));
        }
    }

    lines.push(border("╰", "┴", "╯", "─"));

    lines
}

fn is_blank_line(line: &Line<'_>) -> bool {
    line.spans.is_empty() || line.spans.iter().all(|s| s.content.is_empty())
}

fn ensure_blank_line(lines: &mut Vec<Line<'static>>) {
    if !lines.last().is_some_and(is_blank_line) {
        lines.push(Line::default());
    }
}

fn prefix_span(prefix: &str, style: Style) -> Span<'static> {
    Span::styled(prefix.to_owned(), style.add_modifier(Modifier::BOLD))
}

/// Skip the prefix when it's empty, otherwise we'd push a styled empty span
/// that shows up as a phantom bold marker on every first line.
fn push_prefix(spans: &mut Vec<Span<'static>>, prefix: &str, style: Style) {
    if !prefix.is_empty() {
        spans.push(prefix_span(prefix, style));
    }
}

/// Returns `Line::default()` when the prefix is empty, so callers can use it
/// as a blank first line without an empty styled span sneaking in.
fn prefix_line(prefix: &str, style: Style) -> Line<'static> {
    if prefix.is_empty() {
        Line::default()
    } else {
        Line::from(prefix_span(prefix, style))
    }
}

pub fn plain_lines(
    text: &str,
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let text = text.trim_start_matches('\n');
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut first_line = true;

    for line in text.split('\n') {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if first_line {
            push_prefix(&mut spans, prefix, prefix_style);
            first_line = false;
        }
        spans.push(Span::styled(line.to_owned(), text_style));
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(prefix_line(prefix, prefix_style));
    }

    lines
}

pub(crate) struct RenderState<'a> {
    pub code_idx: usize,
    pub table_idx: usize,
    pub highlighters: Option<&'a mut Vec<CodeHighlighter>>,
    pub table_col_widths: Option<&'a mut Vec<Vec<usize>>>,
}

impl<'a> RenderState<'a> {
    pub fn new() -> Self {
        Self {
            code_idx: 0,
            table_idx: 0,
            highlighters: None,
            table_col_widths: None,
        }
    }
}

pub(crate) struct RenderCtx<'a> {
    pub prefix: &'a str,
    pub text_style: Style,
    pub prefix_style: Style,
    pub width: u16,
}

fn emit_first_line_prefix(lines: &mut Vec<Line<'static>>, ctx: &RenderCtx<'_>, standalone: bool) {
    if !lines.is_empty() {
        return;
    }
    if !standalone || !ctx.prefix.is_empty() {
        lines.push(prefix_line(ctx.prefix, ctx.prefix_style));
    }
}

fn render_line_block(lb: &LineBlock, lines: &mut Vec<Line<'static>>, ctx: &RenderCtx<'_>) {
    if matches!(lb.kind, BlockKind::HorizontalRule) {
        emit_first_line_prefix(lines, ctx, true);
        lines.push(hr_line(ctx.width, theme::current().horizontal_rule));
        return;
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    if lines.is_empty() {
        push_prefix(&mut spans, ctx.prefix, ctx.prefix_style);
    }

    if let Some(p) = maki_markdown::block_prefix(&lb.kind) {
        spans.push(Span::styled(p, theme::current().list_marker));
    }

    let is_heading = matches!(lb.kind, BlockKind::Heading(_));
    let base = if is_heading {
        theme::current().heading
    } else {
        ctx.text_style
    };

    for InlineSpan {
        text,
        kind,
        emphasis,
    } in maki_markdown::parse_inline(&lb.inline)
    {
        spans.push(Span::styled(
            text,
            style_for(kind, emphasis, base, is_heading),
        ));
    }

    lines.push(Line::from(spans));
}

pub(crate) fn render_block(
    block: &Block,
    lines: &mut Vec<Line<'static>>,
    state: &mut RenderState<'_>,
    ctx: &RenderCtx<'_>,
) {
    match block {
        Block::Lines(line_blocks) => {
            for lb in line_blocks {
                render_line_block(lb, lines, ctx);
            }
        }
        Block::Code { lang, code } => {
            if lines.is_empty() {
                lines.push(prefix_line(ctx.prefix, ctx.prefix_style));
            }
            ensure_blank_line(lines);
            if let Some(hl) = state.highlighters.as_deref_mut() {
                if state.code_idx >= hl.len() {
                    hl.push(CodeHighlighter::new(lang));
                }
                let unwrapped = hl[state.code_idx].update(code);
                let start = lines.len();
                for src_line in unwrapped {
                    let mut line = src_line.clone();
                    prepend_code_bar(&mut line);
                    lines.push(line);
                }
                wrap_code_lines(lines, start, ctx.width);
            } else {
                lines.extend(highlight_code(lang, code, ctx.width));
            }
            ensure_blank_line(lines);
            state.code_idx += 1;
        }
        Block::Table { rows, header_end } => {
            emit_first_line_prefix(lines, ctx, true);
            ensure_blank_line(lines);
            let pw = state.table_col_widths.as_deref_mut().map(|all| {
                if state.table_idx >= all.len() {
                    all.resize_with(state.table_idx + 1, Vec::new);
                }
                &mut all[state.table_idx]
            });
            lines.extend(render_table(
                rows,
                *header_end,
                ctx.text_style,
                ctx.width,
                pw,
            ));
            ensure_blank_line(lines);
            state.table_idx += 1;
        }
    }
}

pub(crate) fn finalize_lines(lines: &mut Vec<Line<'static>>, prefix: &str, prefix_style: Style) {
    while lines.last().is_some_and(is_blank_line) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(prefix_line(prefix, prefix_style));
    }
}

pub fn text_to_lines<'a>(
    text: &str,
    prefix: &'a str,
    text_style: Style,
    prefix_style: Style,
    highlighters: Option<&'a mut Vec<CodeHighlighter>>,
    width: u16,
) -> Vec<Line<'static>> {
    let text = text.trim_start_matches('\n');
    let blocks = parse(text);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut state = RenderState {
        highlighters,
        ..RenderState::new()
    };
    let ctx = RenderCtx {
        prefix,
        text_style,
        prefix_style,
        width,
    };

    for block in &blocks {
        render_block(block, &mut lines, &mut state, &ctx);
    }

    if let Some(hl) = state.highlighters.as_deref_mut() {
        hl.truncate(state.code_idx);
    }

    finalize_lines(&mut lines, prefix, prefix_style);
    lines
}

fn truncate_long_lines(text: &str) -> Cow<'_, str> {
    if !text.lines().any(|l| l.len() > MAX_LINE_CHARS) {
        return Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if line.len() > MAX_LINE_CHARS {
            let boundary = line.floor_char_boundary(MAX_LINE_CHARS);
            result.push_str(&line[..boundary]);
            result.push_str("...");
        } else {
            result.push_str(line);
        }
    }
    if text.ends_with('\n') {
        result.push('\n');
    }
    Cow::Owned(result)
}

pub struct TruncatedOutput<'a> {
    pub kept: Cow<'a, str>,
    pub skipped: usize,
}

pub fn truncate_output(text: &str, max: usize, keep: Keep) -> TruncatedOutput<'_> {
    let tr = truncate_lines(text, max, keep);
    TruncatedOutput {
        kept: truncate_long_lines(tr.kept),
        skipped: tr.skipped,
    }
}

pub fn truncate_lines(s: &str, max: usize, keep: Keep) -> Truncated<'_> {
    let split = match keep {
        Keep::Head => s.match_indices('\n').nth(max.saturating_sub(1)),
        Keep::Tail => s.rmatch_indices('\n').nth(max.saturating_sub(1)),
    };
    let Some((i, _)) = split else {
        return Truncated {
            kept: s,
            skipped: 0,
            keep,
        };
    };
    let result = match keep {
        Keep::Head => {
            let tail = &s[i..];
            let newlines = tail.matches('\n').count();
            let has_content = tail.bytes().any(|b| b != b'\n');
            Truncated {
                kept: &s[..i],
                skipped: if has_content { newlines } else { 0 },
                keep,
            }
        }
        Keep::Tail => {
            let head = &s[..i];
            let newlines = head.matches('\n').count() + 1;
            let has_content = head.bytes().any(|b| b != b'\n');
            Truncated {
                kept: &s[i + 1..],
                skipped: if has_content { newlines } else { 0 },
                keep,
            }
        }
    };
    if result.skipped > 0 && !should_truncate(result.skipped) {
        return Truncated {
            kept: s,
            skipped: 0,
            keep,
        };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn bs() -> Style {
        theme::current().bold
    }
    fn cs() -> Style {
        theme::current().inline_code
    }
    fn ss() -> Style {
        theme::current().strikethrough
    }
    fn bcs() -> Style {
        overlay_style(bs(), cs())
    }
    fn bis() -> Style {
        theme::current().bold_italic
    }
    fn heading_code() -> Style {
        overlay_style(theme::current().heading, cs())
    }
    const IS: Style = Style::new().add_modifier(Modifier::ITALIC);
    const TEST_WIDTH: u16 = 80;

    fn assert_inline(input: &str, expected: &[(&str, Option<Style>)]) {
        let base = Style::default();
        let spans = parse_inline_markdown(input, base);
        assert_eq!(
            spans.len(),
            expected.len(),
            "span count mismatch for {input:?}: got {spans:?}"
        );
        for (span, (text, style)) in spans.iter().zip(expected) {
            assert_eq!(span.content, *text);
            assert_eq!(span.style, style.unwrap_or(base));
        }
    }

    macro_rules! inline_test {
        ($name:ident, $input:expr, $expected:expr) => {
            #[test]
            fn $name() {
                assert_inline($input, $expected);
            }
        };
    }

    inline_test!(
        inline_bold,
        "a **bold** b",
        &[("a ", None), ("bold", Some(bs())), (" b", None)]
    );
    inline_test!(
        inline_code,
        "use `foo` here",
        &[("use ", None), ("foo", Some(cs())), (" here", None)]
    );
    inline_test!(
        italic_star,
        "some *emphasized* word",
        &[("some ", None), ("emphasized", Some(IS)), (" word", None)]
    );
    inline_test!(italic_underscore, "_italic_", &[("italic", Some(IS))]);
    inline_test!(
        strikethrough,
        "a ~~struck~~ b",
        &[("a ", None), ("struck", Some(ss())), (" b", None)]
    );
    inline_test!(
        triple_star,
        "***bold italic***",
        &[("bold italic", Some(bis()))]
    );

    inline_test!(
        code_inside_bold,
        "**bold `code` bold**",
        &[
            ("bold ", Some(bs())),
            ("code", Some(bcs())),
            (" bold", Some(bs()))
        ]
    );
    inline_test!(
        bold_inside_code,
        "`code **bold** code`",
        &[("code **bold** code", Some(cs()))]
    );
    inline_test!(
        italic_inside_bold,
        "**bold *italic* bold**",
        &[
            ("bold ", Some(bs())),
            ("italic", Some(bis())),
            (" bold", Some(bs()))
        ]
    );
    inline_test!(entire_bold_is_code, "**`all`**", &[("all", Some(bcs()))]);
    inline_test!(entire_code_is_bold, "`**all**`", &[("**all**", Some(cs()))]);

    #[test_case("here is `/home/tony/file.rs` path" ; "path_in_backticks")]
    #[test_case("use `fn main()` and **important**" ; "code_and_bold_real_content")]
    #[test_case("**`/home/tony/c/maki/src/tools/read.rs:23-38`**" ; "bold_code_path")]
    #[test_case("### 1. Data ` Types` — How Output" ; "heading_with_stray_backtick")]
    #[test_case("**/ Diffs Are Structured" ; "unclosed_bold_with_slash")]
    #[test_case("text `code` more **bold** end `code2` fin" ; "mixed_inline")]
    fn inline_parse_invariants(input: &str) {
        let base = Style::default();
        let spans = parse_inline_markdown(input, base);
        let reconstructed: String = spans.iter().map(|s| s.content.as_ref()).collect();

        let strip = |s: &str| -> String {
            s.chars()
                .filter(|c| !matches!(c, '`' | '*' | '~' | '_'))
                .collect()
        };
        assert_eq!(
            strip(&reconstructed),
            strip(input),
            "non-delimiter content lost or reordered\n  input: {input:?}\n  output: {reconstructed:?}"
        );
    }

    #[test_case("line1\nline2\nline3", 3, "p> line1" ; "splits_newlines")]
    #[test_case("\n\nfirst line\nsecond", 2, "p> first line" ; "strips_leading_newlines")]
    fn text_to_lines_cases(input: &str, expected_lines: usize, first_line: &str) {
        let style = Style::default();
        let lines = text_to_lines(input, "p> ", style, style, None, TEST_WIDTH);
        assert_eq!(lines.len(), expected_lines);
        assert_eq!(lines_text(&lines)[0], first_line);
    }

    #[test_case("a\nb\nc", 5, Keep::Head, "a\nb\nc", 0 ; "under_limit_returns_input")]
    #[test_case("a\nb\nc\nd", 2, Keep::Head, "a\nb", 2 ; "head_over_limit")]
    #[test_case("a\nb\nc", 2, Keep::Head, "a\nb\nc", 0 ; "head_singular_no_truncation")]
    #[test_case("a\nb\nc\nd", 2, Keep::Tail, "c\nd", 2 ; "tail_over_limit")]
    #[test_case("a\nb\nc\nd\ne", 3, Keep::Tail, "c\nd\ne", 2 ; "tail_keeps_last_n")]
    #[test_case("a\nb\nc", 2, Keep::Tail, "a\nb\nc", 0 ; "tail_singular_no_truncation")]
    #[test_case("a\nb\nc\n", 3, Keep::Head, "a\nb\nc", 0 ; "head_trailing_newline_no_phantom")]
    #[test_case("\na\nb\nc", 3, Keep::Tail, "a\nb\nc", 0 ; "tail_leading_newline_no_phantom")]
    fn truncate_lines_cases(
        input: &str,
        max: usize,
        keep: Keep,
        expected_kept: &str,
        expected_skipped: usize,
    ) {
        let tr = truncate_lines(input, max, keep);
        assert_eq!(tr.kept, expected_kept);
        assert_eq!(tr.skipped, expected_skipped);
    }

    #[test_case("a\nb\nc\nd", 2, Keep::Head, "a\nb\n... (2 lines) click to expand" ; "head_with_notice")]
    #[test_case("a\nb\nc\nd", 2, Keep::Tail, "... (2 lines) click to expand\nc\nd" ; "tail_with_notice")]
    #[test_case("a\nb\nc",    5, Keep::Head, "a\nb\nc"             ; "no_truncation")]
    fn truncated_into_string(input: &str, max: usize, keep: Keep, expected: &str) {
        assert_eq!(truncate_lines(input, max, keep).into_string(), expected);
    }

    #[test_case(5,  "click to expand"   ; "collapsed_shows_expand")]
    #[test_case(2,  "(2 lines)"          ; "collapsed_plural")]
    fn truncation_notice_text(count: usize, expected_substr: &str) {
        let notice = truncation_notice(count);
        assert!(
            notice.contains(expected_substr),
            "expected {expected_substr:?} in {notice:?}"
        );
    }

    #[test]
    fn truncated_inside_code_block_notice_not_in_code() {
        let style = Style::default();
        let input = "```rust\nfn a() {}\nfn b() {}\nfn c() {}\nfn d() {}";
        let tr = truncate_lines(input, 3, Keep::Head);
        let lines = text_to_lines(tr.kept, "", style, style, None, TEST_WIDTH);
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !text.contains(TRUNCATION_PREFIX),
                "kept text should not contain truncation notice"
            );
        }
        assert!(tr.notice_line().is_some());
    }

    fn block_summary(blocks: &[Block]) -> Vec<(String, Option<String>)> {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::Lines(lbs) => {
                    let joined = lbs
                        .iter()
                        .map(reconstruct_line)
                        .collect::<Vec<_>>()
                        .join("\n");
                    Some((joined, None))
                }
                Block::Code { lang, code } => Some((code.clone(), Some(lang.clone()))),
                Block::Table { .. } => None,
            })
            .collect()
    }

    fn reconstruct_line(lb: &LineBlock) -> String {
        let prefix = maki_markdown::block_prefix(&lb.kind).unwrap_or_default();
        match lb.kind {
            BlockKind::Heading(n) => format!("{} {}", "#".repeat(n as usize), lb.inline),
            BlockKind::HorizontalRule => "---".to_owned(),
            _ => format!("{prefix}{}", lb.inline),
        }
    }

    /// Only asserts the top-level block boundaries (paragraph vs code fence
    /// vs table). Inline contents of normal blocks live in `maki-markdown`'s
    /// own tests.
    #[test_case(
        "before\n```rust\nfn main() {}\n```\nafter",
        &[(false, None), (true, Some("rust")), (false, None)]
        ; "single_code_block"
    )]
    #[test_case(
        "a\n```py\nx=1\n```\nb\n```js\ny=2\n```\nc",
        &[(false, None), (true, Some("py")), (false, None), (true, Some("js")), (false, None)]
        ; "multiple_code_blocks"
    )]
    #[test_case(
        "before\n```rust\nfn main() {}",
        &[(false, None), (true, Some("rust"))]
        ; "unclosed_fence"
    )]
    #[test_case(
        "a\n```rs\n```\nb",
        &[(false, None), (true, Some("rs")), (false, None)]
        ; "empty_code_block"
    )]
    #[test_case(
        "inline ```code``` here\ntext with ``` inside\nand more",
        &[(false, None)]
        ; "mid_line_backticks_not_a_fence"
    )]
    #[test_case(
        "before\n````markdown\n```rust\nfn main() {}\n```\n````\nafter",
        &[(false, None), (true, Some("markdown")), (false, None)]
        ; "four_backtick_fence_nests_three"
    )]
    fn parse_blocks_structure(input: &str, expected: &[(bool, Option<&str>)]) {
        let blocks = parse(input);
        let shape: Vec<(bool, Option<&str>)> = blocks
            .iter()
            .map(|b| match b {
                Block::Lines(_) => (false, None),
                Block::Code { lang, .. } => (true, Some(lang.as_str())),
                Block::Table { .. } => (true, Some("<table>")),
            })
            .collect();
        assert_eq!(shape, expected.to_vec());
    }

    fn lines_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn strip_md(s: &str) -> String {
        s.chars()
            .filter(|c| {
                !matches!(
                    c,
                    '`' | '*'
                        | '#'
                        | '•'
                        | '-'
                        | '+'
                        | '~'
                        | '_'
                        | '─'
                        | '│'
                        | '╭'
                        | '╮'
                        | '├'
                        | '┤'
                        | '╰'
                        | '╯'
                        | '┬'
                        | '┴'
                        | '┼'
                        | '|'
                )
            })
            .collect()
    }

    fn normalize_ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn highlighter_reuse_matches_fresh_render() {
        let style = Style::default();
        let text = "hello\n```rust\nfn main() {}\n```\nbye";
        let full = text_to_lines(text, "p> ", style, style, None, TEST_WIDTH);
        let mut hl = Vec::new();
        let inc = text_to_lines(text, "p> ", style, style, Some(&mut hl), TEST_WIDTH);
        assert_eq!(lines_text(&full), lines_text(&inc));
    }

    #[test_case(
        "Here is **bold** and `code` text.\nLine2 has `more` stuff."
        ; "streaming_mixed_markdown"
    )]
    #[test_case(
        "### 1. Data Types\n\nHere is `/home/file.rs` path\n**bold** end"
        ; "streaming_heading_with_code"
    )]
    #[test_case(
        "**`/home/tony/c/maki/src/tools/read.rs:23-38`**\n\nSome text after"
        ; "streaming_bold_code_path"
    )]
    #[test_case(
        "Before\n```rust\nfn main() {}\n```\nAfter with **bold**"
        ; "streaming_code_block_then_inline"
    )]
    #[test_case(
        "a `b` c **d** e\n`f` **g**\nh"
        ; "streaming_multiline_inline"
    )]
    #[test_case(
        "- **bold item**\n- `code item`\n  - nested"
        ; "streaming_list_with_inline"
    )]
    #[test_case(
        "Here is *italic* and ~~struck~~ text with _underscores_"
        ; "streaming_italic_strike_underscore"
    )]
    #[test_case(
        "Before table\n\n| Name | Value |\n| --- | --- |\n| foo | 42 |\n| bar | 99 |\n\nAfter table"
        ; "streaming_table_between_paragraphs"
    )]
    fn streaming_never_garbles(input: &str) {
        let style = Style::default();
        let step = if input.len() > 200 { 31 } else { 1 };
        let mut end = step;
        while end <= input.len() {
            if !input.is_char_boundary(end) {
                end += 1;
                continue;
            }
            let prefix = &input[..end];
            let lines = text_to_lines(prefix, "", style, style, None, TEST_WIDTH);
            let rendered: String = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");

            for line in rendered.split('\n') {
                if line.is_empty() {
                    continue;
                }
                let trimmed = line.trim_end();
                let without_bar = trimmed
                    .strip_prefix(CODE_BAR)
                    .or_else(|| trimmed.strip_prefix(CODE_BAR.trim_end()))
                    .unwrap_or(trimmed);
                let line_stripped = normalize_ws(&strip_md(without_bar));
                if line_stripped.is_empty() {
                    continue;
                }
                let input_stripped = normalize_ws(&strip_md(prefix));
                assert!(
                    input_stripped.contains(&line_stripped),
                    "rendered line not found in input at prefix len={end}\n  prefix: {prefix:?}\n  rendered line: {line:?}\n  full rendered: {rendered:?}"
                );
            }
            end += step;
        }
    }

    fn hr_text() -> String {
        iter::repeat_n(HR_CHAR, TEST_WIDTH as usize).collect()
    }

    #[test_case(
        "before\n---\nafter",
        vec!["before".to_owned(), hr_text(), "after".to_owned()]
        ; "hr_between_paragraphs"
    )]
    #[test_case(
        "---",
        vec![hr_text()]
        ; "hr_only"
    )]
    fn horizontal_rule_rendering(input: &str, expected: Vec<String>) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        assert_eq!(lines_text(&lines), expected);
    }

    fn prefixed(code: &str) -> String {
        format!("{}{code}", CODE_BAR)
    }

    #[test_case(
        "before\n```rust\nfn main() {}\n```\nafter",
        vec!["before".into(), "".into(), prefixed("fn main() {}"), "".into(), "after".into()]
        ; "margin_around_code_block"
    )]
    #[test_case(
        "before\n\n```rust\ncode\n```\n\nafter",
        vec!["before".into(), "".into(), prefixed("code"), "".into(), "after".into()]
        ; "extra_blanks_collapsed"
    )]
    #[test_case(
        "hello\n```rust\ncode\n```",
        vec!["hello".into(), "".into(), prefixed("code")]
        ; "no_trailing_blank_after_final_code_block"
    )]
    fn code_block_margins(input: &str, expected: Vec<String>) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        assert_eq!(lines_text(&lines), expected);
    }

    #[test]
    fn heading_with_inline_markdown() {
        let style = Style::default();
        let lines = text_to_lines("## **bold** and `code`", "", style, style, None, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "bold and code");
        let styles: Vec<_> = lines[0].spans.iter().map(|s| s.style).collect();
        let heading = theme::current().heading;
        assert!(styles.contains(&heading), "plain heading span missing");
        assert!(
            styles
                .iter()
                .any(|s| s.fg == heading.fg && s.add_modifier.contains(Modifier::BOLD)),
            "bolded word should retain heading color, got {styles:?}"
        );
        assert!(styles.contains(&heading_code()));
    }

    #[test_case(
        "- first\n- second\n- third",
        &["• first", "• second", "• third"]
        ; "simple_unordered_list"
    )]
    #[test_case(
        "- item\n  - nested\n    - deep",
        &["• item", "  • nested", "    • deep"]
        ; "nested_unordered_list"
    )]
    #[test_case(
        "* star item\n+ plus item",
        &["• star item", "• plus item"]
        ; "star_and_plus_markers"
    )]
    #[test_case(
        "1. first\n2. second\n3. third",
        &["1. first", "2. second", "3. third"]
        ; "simple_ordered_list"
    )]
    #[test_case(
        "1. item\n   - nested bullet",
        &["1. item", "  • nested bullet"]
        ; "ordered_then_nested_unordered"
    )]
    #[test_case(
        "10. double digits\n100. triple digits",
        &["10. double digits", "100. triple digits"]
        ; "multi_digit_numbers"
    )]
    fn list_rendering(input: &str, expected: &[&str]) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        assert_eq!(lines_text(&lines), expected);
    }

    #[test_case("- item", "• " ; "unordered_bullet")]
    #[test_case("1. item", "1. " ; "ordered_number")]
    fn list_marker_styled(input: &str, expected_marker: &str) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let marker = lines[0]
            .spans
            .iter()
            .find(|s| s.style == theme::current().list_marker);
        assert_eq!(marker.unwrap().content, expected_marker);
    }

    #[test]
    fn list_item_with_inline_markdown() {
        let style = Style::default();
        let lines = text_to_lines("- **bold** and `code`", "", style, style, None, TEST_WIDTH);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "• bold and code");
    }

    #[test_case(
        "**bold** `code` ```fences```",
        &["p> **bold** `code` ```fences```"]
        ; "plain_ignores_all_markdown"
    )]
    #[test_case(
        "before\n```rust\nfn main() {}\n```\nafter",
        &["p> before", "```rust", "fn main() {}", "```", "after"]
        ; "plain_preserves_code_fences_literally"
    )]
    #[test_case(
        "line1\nline2",
        &["p> line1", "line2"]
        ; "plain_splits_lines"
    )]
    fn plain_content(input: &str, expected: &[&str]) {
        let base = Style::new().fg(ratatui::style::Color::Cyan);
        let lines = plain_lines(input, "p> ", base, base);
        assert_eq!(lines_text(&lines), expected);
        for line in &lines {
            for span in &line.spans {
                assert!(
                    span.style == base || span.style == base.add_modifier(Modifier::BOLD),
                    "unexpected style on {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn render_table_structure() {
        let style = Style::default();
        let input = "| Name | Value |\n| --- | --- |\n| foo | 42 |";
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let joined = lines_text(&lines).join("\n");
        for expected in ["Name", "foo", "42"] {
            assert!(joined.contains(expected), "missing {expected:?} in table");
        }
        assert!(
            lines.len() >= 5,
            "table should have border+header+sep+data+border"
        );
        let sep_lines: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains('├')))
            .collect();
        for sep in &sep_lines {
            assert_eq!(
                sep.spans.first().unwrap().style,
                theme::current().table_border,
                "all separators should use table_border_style"
            );
        }
    }

    #[test_case("| H |\n| --- |\n| a |\n| b |\n| c |", 3 ; "multi_row_separators")]
    #[test_case("| H |\n| --- |\n| only |", 1 ; "single_row_header_only")]
    fn table_separator_count(input: &str, expected: usize) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let sep_count = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains('├')))
            .count();
        assert_eq!(sep_count, expected);
    }

    #[test]
    fn render_table_escaped_pipe_stays_in_cell() {
        let style = Style::default();
        let input = "| Query | Result |\n| --- | --- |\n| `cmd \\| filter` | ok |";
        let lines = text_to_lines(input, "", style, style, None, 80);
        let joined = lines_text(&lines).join("\n");
        assert!(
            joined.contains("cmd \\| filter"),
            "escaped pipe content missing"
        );
        assert!(joined.contains("ok"), "adjacent cell missing");
    }

    #[test]
    fn table_with_prefix() {
        let style = Style::default();
        let input = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        let lines = text_to_lines(input, "p> ", style, style, None, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn table_no_double_blank_before_hr() {
        let style = Style::default();
        let input = "| a | b |\n| --- | --- |\n| 1 | 2 |\n\n---\n\nafter";
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let text = lines_text(&lines);
        let consecutive_blanks = text
            .windows(2)
            .filter(|w| w[0].is_empty() && w[1].is_empty())
            .count();
        assert_eq!(
            consecutive_blanks, 0,
            "should never have two consecutive blank lines"
        );
    }

    #[test]
    fn mismatched_cell_counts_does_not_panic() {
        let style = Style::default();
        let input = "| a | b | c |\n| --- | --- | --- |\n| 1 | 2 |";
        let _ = text_to_lines(input, "", style, style, None, TEST_WIDTH);
    }

    #[test]
    fn header_row_is_bold() {
        let style = Style::default();
        let input = "| Header |\n| --- |\n| Data |";
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let header_span = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.trim() == "Header")
            .expect("Header span");
        assert!(header_span.style.add_modifier.contains(Modifier::BOLD));
    }

    fn assert_table_within_width(input: &str, width: u16) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, width);
        for line in lines_text(&lines) {
            assert!(
                line.width() <= width as usize,
                "line exceeds width {width}: ({}) {line:?}",
                line.width()
            );
        }
    }

    #[test]
    fn table_wraps_and_preserves_content() {
        let style = Style::default();
        let long = "x".repeat(60);
        let input = format!("| Col1 | Col2 |\n| --- | --- |\n| short | {long} |");
        let width: u16 = 40;
        let lines = text_to_lines(&input, "", style, style, None, width);
        let rendered: String = lines_text(&lines).join("");
        let x_count = rendered.chars().filter(|c| *c == 'x').count();
        assert_eq!(x_count, 60, "wrapped table must preserve all content");
        assert_table_within_width(&input, width);
    }

    #[test]
    fn table_narrow_prose_within_width() {
        let input = "| Test | Rationale |\n| --- | --- |\n| name | This is a very long rationale that should definitely be wrapped when the terminal width is narrow enough to require it |";
        assert_table_within_width(input, 50);
    }

    fn wrap_texts(spans: Vec<Span<'static>>, width: usize) -> Vec<String> {
        wrap_cell_spans(spans, width)
            .iter()
            .map(|l| l.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test_case("hello world foo bar", 10, &["hello", "world foo", "bar"] ; "word_boundary")]
    #[test_case("abcdefghij", 6, &["abcdef", "ghij"] ; "char_boundary_fallback")]
    fn wrap_cell_spans_cases(input: &str, width: usize, expected: &[&str]) {
        assert_eq!(
            wrap_texts(vec![Span::raw(input.to_owned())], width),
            expected
        );
    }

    #[test_case(&[10, 90], 50, true  ; "shrinks_proportionally")]
    #[test_case(&[10, 20], 50, false ; "noop_when_fits")]
    fn constrain_col_widths_cases(input: &[usize], available: usize, should_shrink: bool) {
        let mut widths = input.to_vec();
        let original = widths.clone();
        constrain_col_widths(&mut widths, available);
        assert!(widths.iter().sum::<usize>() <= available);
        for w in &widths {
            assert!(*w >= MIN_COL_WIDTH);
        }
        if !should_shrink {
            assert_eq!(widths, original);
        }
    }

    fn content_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.content.as_ref() != CODE_BAR && s.content.as_ref() != CODE_BAR_WRAP)
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn wrap_preserves_content() {
        let code = "a".repeat(20);
        let lines = highlight_code("txt", &code, 12);
        assert!(lines.len() >= 2);
        assert_eq!(content_text(&lines), code);
    }

    #[test]
    fn wrap_zero_width_does_not_panic() {
        let lines = highlight_code("txt", "hello", 0);
        assert!(!lines.is_empty());
    }

    #[test]
    fn wrap_code_lines_preserves_prefix() {
        let style = Style::default();
        let input = format!("```\n{}\n```", "a".repeat(30));
        let lines = text_to_lines(&input, "", style, style, None, 15);
        for line in &lines {
            if is_blank_line(line) {
                continue;
            }
            let first = &line.spans[0].content;
            assert!(
                first.as_ref() == CODE_BAR || first.as_ref() == CODE_BAR_WRAP,
                "code line missing bar prefix: {first:?}"
            );
        }
    }

    #[test]
    fn wrap_code_lines_preserves_lines_before_start() {
        let mut lines = vec![
            Line::from("header"),
            Line::from(vec![
                Span::styled(CODE_BAR, theme::current().code_bar),
                Span::raw("short"),
            ]),
        ];
        wrap_code_lines(&mut lines, 1, 80);
        assert_eq!(lines[0].spans[0].content, "header");
    }

    #[test]
    fn persistent_widths_constrained_to_available() {
        let style = Style::default();
        let width: u16 = 40;
        let mut pw = Vec::new();

        let rows1 = vec![
            vec!["Name".into(), "Description".into()],
            vec!["a".into(), "short".into()],
        ];
        let lines1 = render_table(&rows1, 1, style, width, Some(&mut pw));
        for line in lines_text(&lines1) {
            assert!(
                line.width() <= width as usize,
                "frame 1 overflows ({} > {width}): {line:?}",
                line.width()
            );
        }

        let rows2 = vec![
            vec!["Name".into(), "Description".into()],
            vec!["a".into(), "short".into()],
            vec![
                "b".into(),
                "a very long description that exceeds the available width".into(),
            ],
        ];
        let lines2 = render_table(&rows2, 1, style, width, Some(&mut pw));
        for line in lines_text(&lines2) {
            assert!(
                line.width() <= width as usize,
                "frame 2 overflows ({} > {width}): {line:?}",
                line.width()
            );
        }
    }

    #[test]
    fn persistent_widths_grow_monotonically() {
        let style = Style::default();
        let width: u16 = 120;
        let mut pw = Vec::new();

        let rows1 = vec![
            vec!["A".into(), "B".into()],
            vec!["hi".into(), "there".into()],
        ];
        render_table(&rows1, 1, style, width, Some(&mut pw));
        let pw_after1 = pw.clone();

        let rows2 = vec![
            vec!["A".into(), "B".into()],
            vec!["hi".into(), "there".into()],
            vec!["longer".into(), "x".into()],
        ];
        render_table(&rows2, 1, style, width, Some(&mut pw));
        for (i, (&old, &new)) in pw_after1.iter().zip(pw.iter()).enumerate() {
            assert!(
                new >= old,
                "persistent width shrank at col {i}: {old} -> {new}"
            );
        }
    }

    #[test]
    fn second_table_does_not_widen_first_table() {
        let style = Style::default();
        let width: u16 = 120;
        let mut all_widths: Vec<Vec<usize>> = Vec::new();
        let table1 = "| A | B |\n| --- | --- |\n| x | y |";
        let table2_wide = "\n\nsome text\n\n| Very Wide Column | Another Wide Col |\n| --- | --- |\n| long content here | more long content |";

        let render = |input: &str, widths: &mut Vec<Vec<usize>>| {
            let blocks = parse(input);
            let mut lines = Vec::new();
            let mut state = RenderState {
                table_col_widths: Some(widths),
                ..RenderState::new()
            };
            let ctx = RenderCtx {
                prefix: "",
                text_style: style,
                prefix_style: style,
                width,
            };
            for block in &blocks {
                render_block(block, &mut lines, &mut state, &ctx);
            }
        };

        render(table1, &mut all_widths);
        let table1_widths_before = all_widths[0].clone();

        let both = format!("{table1}{table2_wide}");
        render(&both, &mut all_widths);
        assert_eq!(
            all_widths[0], table1_widths_before,
            "table 1 widths changed after table 2 appeared"
        );
    }

    #[test_case("short\nlines\n", "short\nlines\n" ; "short_text_unchanged")]
    #[test_case(&"a".repeat(MAX_LINE_CHARS), &"a".repeat(MAX_LINE_CHARS) ; "exactly_at_limit_unchanged")]
    #[test_case(&"a".repeat(MAX_LINE_CHARS + 1), &format!("{}...", "a".repeat(MAX_LINE_CHARS)) ; "one_over_limit_truncated")]
    fn truncate_long_lines_cases(input: &str, expected: &str) {
        assert_eq!(&*truncate_long_lines(input), expected);
    }

    #[test]
    fn truncate_long_lines_multibyte_boundary() {
        let mut line = "a".repeat(MAX_LINE_CHARS - 1);
        line.push('\u{00e9}');
        let result = truncate_long_lines(&line);
        assert!(result.ends_with("..."));
        assert!(!result.contains('\u{00e9}'));
    }

    #[test]
    fn truncate_long_lines_mixed_only_long_truncated() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let input = format!("short\n{long}\nshort");
        let result = truncate_long_lines(&input);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "short");
        assert!(lines[1].len() <= MAX_LINE_CHARS + 3);
        assert!(lines[1].ends_with("..."));
        assert_eq!(lines[2], "short");
    }

    #[test_case(&format!("{}\n", "z".repeat(MAX_LINE_CHARS + 10)), true ; "preserves_trailing_newline")]
    #[test_case(&"z".repeat(MAX_LINE_CHARS + 10), false ; "no_trailing_newline_when_absent")]
    fn truncate_long_lines_trailing_newline(input: &str, expect_trailing: bool) {
        assert_eq!(truncate_long_lines(input).ends_with('\n'), expect_trailing);
    }

    #[test]
    fn truncate_output_line_count_and_long_lines() {
        let long = "y".repeat(MAX_LINE_CHARS + 50);
        let input = format!("a\n{long}\nc\nd\ne");
        let result = truncate_output(&input, 3, Keep::Head);
        assert_eq!(result.skipped, 2);
        let kept_lines: Vec<&str> = result.kept.lines().collect();
        assert_eq!(kept_lines.len(), 3);
        assert!(kept_lines[1].ends_with("..."));
        assert!(kept_lines[1].len() <= MAX_LINE_CHARS + 3);
    }

    #[test]
    fn streaming_table_no_flicker_during_separator() {
        let full = "| Name | Value |\n| --- | --- |\n| foo | 42 |";
        let mut ever_table = false;
        for i in 1..=full.len() {
            let partial = &full[..i];
            let blocks = parse(partial);
            let is_table = blocks.iter().any(|b| matches!(b, Block::Table { .. }));
            if is_table {
                ever_table = true;
            }
            assert!(
                !ever_table || is_table,
                "once recognized as table, must stay table at byte {i}: {partial:?}"
            );
        }
        assert!(ever_table, "table should be recognized at some point");
    }

    /// The summary helper isn't used by the renderer, so this test is the
    /// only thing keeping it honest.
    #[test]
    fn block_summary_round_trips_code() {
        let blocks = parse("```rust\nfn x() {}\n```");
        let summary = block_summary(&blocks);
        assert_eq!(
            summary,
            vec![("fn x() {}".to_owned(), Some("rust".to_owned()))]
        );
    }

    // Regression tests for heading-color preservation under emphasis: when
    // base has a foreground, emphasis only adds modifiers; code still
    // recolors because it's semantic.

    fn find_span<'a>(lines: &'a [Line<'_>], needle: &str) -> &'a Span<'a> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content == needle)
            .unwrap_or_else(|| panic!("span {needle:?} not found in {:?}", lines_text(lines)))
    }

    #[test_case(
        "## ***hi***", "hi",
        Modifier::BOLD | Modifier::ITALIC
        ; "bold_italic_on_heading")]
    #[test_case(
        "## *x*", "x",
        Modifier::ITALIC
        ; "italic_on_heading")]
    #[test_case(
        "## ~~x~~", "x",
        Modifier::CROSSED_OUT
        ; "strikethrough_on_heading")]
    fn heading_emphasis_preserves_color(input: &str, needle: &str, expected_mods: Modifier) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let span = find_span(&lines, needle);
        let heading_fg = theme::current().heading.fg;
        assert_eq!(
            span.style.fg, heading_fg,
            "emphasis on heading must keep heading fg, got {:?}",
            span.style
        );
        assert!(
            span.style.add_modifier.contains(expected_mods),
            "missing modifiers {expected_mods:?} on {needle:?}: {:?}",
            span.style
        );
    }

    /// Inline code overrides heading color (semantic), while emphasis only
    /// modifies appearance.
    #[test]
    fn heading_inline_code_recolors() {
        let style = Style::default();
        let lines = text_to_lines("## foo `bar`", "", style, style, None, TEST_WIDTH);
        let bar = find_span(&lines, "bar");
        let code_fg = theme::current().inline_code.fg;
        assert!(code_fg.is_some(), "test assumes theme inline_code has fg");
        assert_eq!(
            bar.style.fg, code_fg,
            "inline code on heading must use code fg, got {:?}",
            bar.style
        );
    }

    // Regression tests: no phantom empty bold span on the first line when
    // the prefix is empty.

    #[test]
    fn no_phantom_empty_span_when_prefix_empty() {
        let style = Style::default();
        let lines = text_to_lines("hello", "", style, style, None, TEST_WIDTH);
        for span in &lines[0].spans {
            assert!(
                !(span.content.is_empty() && span.style.add_modifier.contains(Modifier::BOLD)),
                "phantom empty bold span present: {:?}",
                lines[0].spans
            );
        }
    }

    #[test]
    fn non_empty_prefix_has_no_spurious_empty_span_before_it() {
        let style = Style::default();
        let lines = text_to_lines("hello", "p> ", style, style, None, TEST_WIDTH);
        let first = &lines[0].spans[0];
        assert_eq!(first.content, "p> ", "first span must be the prefix");
        for span in &lines[0].spans {
            assert!(
                !span.content.is_empty(),
                "no span should be empty: {:?}",
                lines[0].spans
            );
        }
    }

    #[test]
    fn paragraph_bold_italic_carries_both_modifiers() {
        let style = Style::default();
        let lines = text_to_lines("***hello***", "", style, style, None, TEST_WIDTH);
        let hello = find_span(&lines, "hello");
        assert!(
            hello
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC),
            "***hello*** must have BOLD|ITALIC, got {:?}",
            hello.style
        );
    }

    #[test]
    fn paragraph_strikethrough_carries_modifier() {
        let style = Style::default();
        let lines = text_to_lines("a ~~gone~~ b", "", style, style, None, TEST_WIDTH);
        let gone = find_span(&lines, "gone");
        assert!(
            gone.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "~~gone~~ must have CROSSED_OUT, got {:?}",
            gone.style
        );
    }

    /// `parse_inline_markdown` always recolors emphasis with the theme
    /// style, even when the base already has a foreground. Heading color
    /// preservation is handled by the heading render path, not here.
    #[test]
    fn parse_inline_with_colored_base_recolors_bold() {
        let base = Style::default().fg(ratatui::style::Color::Cyan);
        let spans = parse_inline_markdown("a **b**", base);
        let b = spans.iter().find(|s| s.content == "b").expect("b span");
        assert_eq!(b.style.fg, theme::current().bold.fg);
        assert!(b.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn parse_inline_with_default_base_recolors_bold() {
        let spans = parse_inline_markdown("a **b**", Style::default());
        let b = spans.iter().find(|s| s.content == "b").expect("b span");
        assert_eq!(b.style.fg, theme::current().bold.fg);
        assert!(b.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn render_is_idempotent() {
        let style = Style::default();
        let input = "## title\n\n- a `code` item\n- **bold** item\n\n```rust\nfn x() {}\n```";
        let a = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        let b = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        assert_eq!(lines_text(&a), lines_text(&b));
        assert_eq!(a.len(), b.len());
        for (la, lb) in a.iter().zip(&b) {
            assert_eq!(la.spans.len(), lb.spans.len());
            for (sa, sb) in la.spans.iter().zip(&lb.spans) {
                assert_eq!(sa.content, sb.content);
                assert_eq!(sa.style, sb.style);
            }
        }
    }

    #[test_case("```unknownlang\nfn x() {}\n```" ; "unknown_language")]
    #[test_case("```\nplain code\n```" ; "no_language")]
    fn code_block_renders_with_bar(input: &str) {
        let style = Style::default();
        let lines = text_to_lines(input, "", style, style, None, TEST_WIDTH);
        assert!(!lines.is_empty());
        let has_bar = lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.as_ref() == CODE_BAR || s.content.as_ref() == CODE_BAR_WRAP);
        assert!(
            has_bar,
            "code block missing bar prefix: {:?}",
            lines_text(&lines)
        );
    }

    #[test]
    fn nested_list_markers_share_style() {
        let style = Style::default();
        let lines = text_to_lines("- a\n  - b", "", style, style, None, TEST_WIDTH);
        let marker_style = theme::current().list_marker;
        let markers: Vec<&Span<'_>> = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.content.trim() == "•")
            .collect();
        assert_eq!(markers.len(), 2, "expected two bullet markers");
        for m in markers {
            assert_eq!(m.style, marker_style);
        }
    }

    #[test]
    fn strikethrough_inside_bold_keeps_both_modifiers() {
        let style = Style::default();
        let lines = text_to_lines(
            "**bold ~~struck~~ bold**",
            "",
            style,
            style,
            None,
            TEST_WIDTH,
        );
        let struck = find_span(&lines, "struck");
        assert!(
            struck
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::CROSSED_OUT),
            "struck word must have BOLD|CROSSED_OUT, got {:?}",
            struck.style
        );
    }
}
