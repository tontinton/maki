use std::borrow::Cow;

use crate::theme;
use crate::theme::Theme;
use maki_markdown::Emphasis;
use maki_markdown::render::{self, Line as RLine, LineKind, Span as RSpan, StyleToken};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub const TRUNCATION_PREFIX: &str = "...";
const MIN_TRUNCATABLE_LINES: usize = 2;

/// Add `over`'s modifiers on top of `base`, keeping `base`'s colors.
fn apply_modifiers(base: Style, over: Style) -> Style {
    base.add_modifier(over.add_modifier)
        .remove_modifier(over.sub_modifier)
}

/// Recolor `base` with `over`'s colors and OR the modifiers.
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

/// When `preserve_base_color` is set (headings), emphasis only adds modifiers
/// so the heading colour survives. Otherwise it recolors from the theme.
fn apply_emphasis(base: Style, emphasis: Emphasis, preserve_base_color: bool, t: &Theme) -> Style {
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
    style
}

fn style_for_token(
    token: &StyleToken,
    emphasis: Emphasis,
    base: Style,
    preserve_base_color: bool,
    t: &Theme,
) -> Style {
    match token {
        StyleToken::Text => apply_emphasis(base, emphasis, preserve_base_color, t),
        StyleToken::InlineCode => {
            let style = apply_emphasis(base, emphasis, preserve_base_color, t);
            overlay_style(style, t.inline_code)
        }
        StyleToken::Heading => apply_emphasis(t.heading, emphasis, true, t),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let mut s = Style::default().fg(ratatui::style::Color::Rgb(fg.0, fg.1, fg.2));
            if *bold {
                s = s.add_modifier(Modifier::BOLD);
            }
            if *italic {
                s = s.add_modifier(Modifier::ITALIC);
            }
            if *underline {
                s = s.add_modifier(Modifier::UNDERLINED);
            }
            s
        }
        StyleToken::CodeBar => t.code_bar,
        StyleToken::ListMarker => t.list_marker,
        StyleToken::TableBorder => t.table_border,
        StyleToken::HorizontalRule => t.horizontal_rule,
    }
}

/// Heading lines preserve heading colour through emphasis. Code lines start
/// from `Style::default()` so highlighter colours stand alone.
fn paint_line(line: &RLine, text_style: Style, t: &Theme) -> Line<'static> {
    let (base, preserve_color) = match line.kind {
        LineKind::Heading => (t.heading, true),
        LineKind::Code => (Style::default(), false),
        _ => (text_style, false),
    };
    let spans = line
        .spans
        .iter()
        .map(
            |RSpan {
                 text,
                 style,
                 emphasis,
             }| {
                Span::styled(
                    text.clone(),
                    style_for_token(style, *emphasis, base, preserve_color, t),
                )
            },
        )
        .collect::<Vec<_>>();
    Line::from(spans)
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
}

pub(crate) fn hr_line(width: u16, style: Style) -> Line<'static> {
    Line::from(Span::styled(render::hr_text(width), style))
}

fn prefix_span(prefix: &str, style: Style) -> Span<'static> {
    Span::styled(prefix.to_owned(), style.add_modifier(Modifier::BOLD))
}

/// Returns `Line::default()` when the prefix is empty so callers can use it
/// as a blank first line without an empty styled span sneaking in.
fn prefix_line(prefix: &str, style: Style) -> Line<'static> {
    if prefix.is_empty() {
        Line::default()
    } else {
        Line::from(prefix_span(prefix, style))
    }
}

/// Inline block kinds (paragraph, heading, list) share line 1 with their
/// prefix. Standalone kinds (code, table, hr) need a separate leader line.
fn shares_line_with_prefix(kind: &LineKind) -> bool {
    matches!(
        kind,
        LineKind::Paragraph | LineKind::Heading | LineKind::ListItem | LineKind::Blank
    )
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
            if !prefix.is_empty() {
                spans.push(prefix_span(prefix, prefix_style));
            }
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

/// Paint semantic lines into ratatui lines, splicing the prefix onto
/// the first line (or as a standalone leader for non-inline blocks).
pub(crate) fn paint_semantic(
    semantic: &[RLine],
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let t = theme::current();
    let mut lines: Vec<Line<'static>> = semantic
        .iter()
        .map(|l| paint_line(l, text_style, &t))
        .collect();

    if lines.is_empty() {
        lines.push(prefix_line(prefix, prefix_style));
        return lines;
    }

    if shares_line_with_prefix(&semantic[0].kind) {
        if !prefix.is_empty() {
            lines[0].spans.insert(0, prefix_span(prefix, prefix_style));
        }
    } else if !prefix.is_empty() {
        lines.insert(0, prefix_line(prefix, prefix_style));
    }

    lines
}

pub fn text_to_lines(
    text: &str,
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
    width: u16,
) -> Vec<Line<'static>> {
    let text = render::truncate_long_lines(text);
    let semantic = render::Renderer::unwrapped().render(text.as_ref(), width, 0);
    paint_semantic(&semantic, prefix, text_style, prefix_style)
}

pub struct TruncatedOutput<'a> {
    pub kept: Cow<'a, str>,
    pub skipped: usize,
}

pub fn truncate_output(text: &str, max: usize, keep: Keep) -> TruncatedOutput<'_> {
    let tr = truncate_lines(text, max, keep);
    TruncatedOutput {
        kept: render::truncate_long_lines(tr.kept),
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
            }
        }
        Keep::Tail => {
            let head = &s[..i];
            let newlines = head.matches('\n').count() + 1;
            let has_content = head.bytes().any(|b| b != b'\n');
            Truncated {
                kept: &s[i + 1..],
                skipped: if has_content { newlines } else { 0 },
            }
        }
    };
    if result.skipped > 0 && !should_truncate(result.skipped) {
        return Truncated {
            kept: s,
            skipped: 0,
        };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_markdown::render::CODE_BAR;
    use test_case::test_case;

    const TEST_WIDTH: u16 = 80;

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

    fn find_span<'a>(lines: &'a [Line<'_>], needle: &str) -> &'a Span<'a> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content == needle)
            .unwrap_or_else(|| panic!("span {needle:?} not found in {:?}", lines_text(lines)))
    }

    #[test]
    fn heading_uses_theme_heading_style() {
        let style = Style::default();
        let lines = text_to_lines("# hello", "", style, style, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let heading_fg = theme::current().heading.fg;
        assert_eq!(lines[0].spans[0].style.fg, heading_fg);
    }

    #[test]
    fn code_block_emits_code_bar_with_theme_color() {
        let style = Style::default();
        let lines = text_to_lines("```\nhello\n```", "", style, style, TEST_WIDTH);
        let bar = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.as_ref() == CODE_BAR)
            .expect("code bar span");
        assert_eq!(bar.style, theme::current().code_bar);
    }

    #[test]
    fn bold_uses_theme_bold_fg_and_modifier() {
        let style = Style::default();
        let lines = text_to_lines("**bold**", "", style, style, TEST_WIDTH);
        let bold = find_span(&lines, "bold");
        assert_eq!(bold.style.fg, theme::current().bold.fg);
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn heading_emphasis_preserves_heading_color() {
        let style = Style::default();
        let lines = text_to_lines("## ***hi***", "", style, style, TEST_WIDTH);
        let hi = find_span(&lines, "hi");
        assert_eq!(hi.style.fg, theme::current().heading.fg);
        assert!(
            hi.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn heading_inline_code_recolors_to_code_fg() {
        let style = Style::default();
        let lines = text_to_lines("## foo `bar`", "", style, style, TEST_WIDTH);
        let bar = find_span(&lines, "bar");
        assert_eq!(bar.style.fg, theme::current().inline_code.fg);
    }

    #[test]
    fn list_marker_uses_list_marker_style() {
        let style = Style::default();
        let lines = text_to_lines("- item", "", style, style, TEST_WIDTH);
        let marker = lines[0]
            .spans
            .iter()
            .find(|s| s.style == theme::current().list_marker)
            .expect("list marker span");
        assert_eq!(marker.content, "• ");
    }

    #[test_case("hello", "p> hello"           ; "paragraph_inline")]
    #[test_case("# title", "p> title"         ; "heading_inline")]
    #[test_case("- item", "p> • item"         ; "list_inline")]
    fn prefix_inlined_on_first_line_blocks(input: &str, expected: &str) {
        let style = Style::default();
        let lines = text_to_lines(input, "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines_text(&lines)[0], expected);
    }

    #[test]
    fn prefix_emits_standalone_line_for_code_block() {
        let style = Style::default();
        let lines = text_to_lines("```\ncode\n```", "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn prefix_emits_standalone_line_for_table() {
        let style = Style::default();
        let input = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        let lines = text_to_lines(input, "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn empty_input_yields_single_prefix_line() {
        let style = Style::default();
        let lines = text_to_lines("", "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn no_phantom_empty_bold_span_when_prefix_empty() {
        let style = Style::default();
        let lines = text_to_lines("hello", "", style, style, TEST_WIDTH);
        for span in &lines[0].spans {
            assert!(
                !(span.content.is_empty() && span.style.add_modifier.contains(Modifier::BOLD)),
                "phantom empty bold span: {:?}",
                lines[0].spans
            );
        }
    }

    #[test_case("a\nb\nc", 5, Keep::Head, "a\nb\nc", 0     ; "under_limit")]
    #[test_case("a\nb\nc\nd", 2, Keep::Head, "a\nb", 2     ; "head_over_limit")]
    #[test_case("a\nb\nc\nd", 2, Keep::Tail, "c\nd", 2     ; "tail_over_limit")]
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
    fn plain_content(input: &str, expected: &[&str]) {
        let base = Style::new().fg(ratatui::style::Color::Cyan);
        let lines = plain_lines(input, "p> ", base, base);
        assert_eq!(lines_text(&lines), expected);
    }

    #[test_case(5,  "click to expand"   ; "collapsed_shows_expand")]
    #[test_case(2,  "(2 lines)"         ; "collapsed_plural")]
    fn truncation_notice_text(count: usize, expected_substr: &str) {
        let notice = truncation_notice(count);
        assert!(
            notice.contains(expected_substr),
            "expected {expected_substr:?} in {notice:?}"
        );
    }

    #[test]
    fn strikethrough_uses_theme_strikethrough_style() {
        let style = Style::default();
        let lines = text_to_lines("~~struck~~", "", style, style, TEST_WIDTH);
        let struck = find_span(&lines, "struck");
        assert!(struck.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(struck.style.fg, theme::current().strikethrough.fg);
    }

    #[test]
    fn italic_uses_italic_modifier() {
        let style = Style::default();
        let lines = text_to_lines("*italic*", "", style, style, TEST_WIDTH);
        let it = find_span(&lines, "italic");
        assert!(it.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn table_border_uses_theme_table_border_style() {
        let style = Style::default();
        let input = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        let lines = text_to_lines(input, "", style, style, TEST_WIDTH);
        let border_span = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| {
                let c = s.content.as_ref();
                c.contains('╭') || c.contains('│')
            })
            .expect("box-drawing border span");
        assert_eq!(border_span.style, theme::current().table_border);
    }

    #[test]
    fn horizontal_rule_uses_theme_style_and_fill_char() {
        let style = Style::default();
        let lines = text_to_lines("---", "", style, style, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let hr = &lines[0].spans[0];
        assert_eq!(hr.style, theme::current().horizontal_rule);
        assert!(
            hr.content.chars().all(|c| c == '─'),
            "HR should be filled with ─ chars, got {:?}",
            hr.content
        );
    }

    #[test]
    fn prefix_on_hr_gets_standalone_leader_line() {
        let style = Style::default();
        let lines = text_to_lines("---", "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
        assert!(
            lines[1].spans[0].content.chars().all(|c| c == '─'),
            "second line should be the HR"
        );
    }

    #[test]
    fn code_block_highlight_spans_have_rgb_color() {
        let style = Style::default();
        let lines = text_to_lines("```rust\nfn x() {}\n```", "", style, style, TEST_WIDTH);
        let code_spans: Vec<_> = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| {
                s.content.as_ref() != CODE_BAR && !s.content.is_empty() && s.content.as_ref() != "│"
            })
            .collect();
        let has_rgb = code_spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(ratatui::style::Color::Rgb(_, _, _))));
        assert!(
            has_rgb,
            "expected at least one Rgb-colored span in code block, got: {:?}",
            code_spans
                .iter()
                .map(|s| (&s.content, s.style.fg))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn inline_code_inside_bold_gets_overlay() {
        let style = Style::default();
        let lines = text_to_lines("**a `code` b**", "", style, style, TEST_WIDTH);
        let code = find_span(&lines, "code");
        assert_eq!(code.style.fg, theme::current().inline_code.fg);
        assert!(
            code.style.add_modifier.contains(Modifier::BOLD),
            "inline code inside bold should inherit BOLD modifier"
        );
    }
}
