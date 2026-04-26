use crate::highlight::{fallback_span, highlight_line};
use crate::markdown::{should_truncate, truncation_notice};
use crate::theme;

use maki_agent::diff::{DiffLine, DiffSpan, compute_hunks};
use maki_agent::{GrepFileEntry, InstructionBlock, ToolInput, ToolOutput};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::parsing::SyntaxReference;
use syntect::util::LinesWithEndings;

pub(crate) const MAX_CODE_EXECUTION_LINES: usize = 100;
pub(crate) const MAX_INSTRUCTION_LINES: usize = 15;

pub(crate) fn instruction_limit(expanded: bool) -> usize {
    if expanded {
        usize::MAX
    } else {
        MAX_INSTRUCTION_LINES
    }
}

fn nr_width(max_nr: usize) -> usize {
    max_nr.max(1).ilog10() as usize + 1
}

fn gutter(nr_str: &str) -> Span<'static> {
    Span::styled(format!("{nr_str} "), theme::current().diff_line_nr)
}

fn gap_ellipsis() -> Line<'static> {
    Line::from(vec![
        Span::styled("...".to_owned(), theme::current().tool_dim),
        Span::raw("  ".to_owned()),
    ])
}

fn truncation_line(truncated: usize) -> Line<'static> {
    Line::from(Span::styled(
        truncation_notice(truncated),
        theme::current().tool_dim,
    ))
}

fn highlight_spans(hl: &mut maki_highlight::Highlighter, text: &str) -> Vec<Span<'static>> {
    let with_nl = format!("{text}\n");
    highlight_line(hl, &with_nl)
        .into_iter()
        .filter(|span| !span.content.is_empty())
        .collect()
}

fn render_code(
    mut hl: Option<maki_highlight::Highlighter>,
    start_line: usize,
    code_lines: &[String],
    total_count: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, bool) {
    let capped = code_lines.len().min(max_lines);
    let hidden = total_count.saturating_sub(capped);
    let has_truncation = should_truncate(hidden);
    let display_count = if has_truncation {
        capped
    } else {
        code_lines.len()
    };
    let max_nr = start_line + display_count.saturating_sub(1);
    let w = nr_width(max_nr);

    let mut lines: Vec<Line<'static>> = code_lines
        .iter()
        .take(display_count)
        .enumerate()
        .map(|(i, text)| {
            let nr = start_line + i;
            let mut spans = vec![gutter(&format!("{nr:>w$}"))];
            match &mut hl {
                Some(h) => spans.extend(highlight_spans(h, text)),
                None => spans.push(fallback_span(text)),
            }
            Line::from(spans)
        })
        .collect();

    if has_truncation {
        lines.push(truncation_line(hidden));
    }
    (lines, has_truncation)
}

/// Syntect is stateful, so to color line N you need lines 1..N first.
/// A diff has two files, each with its own parser state. We keep one
/// walker per side and step them in lockstep with the hunks.
struct FileWalker<'a> {
    lines: LinesWithEndings<'a>,
    pos: usize,
    hl: maki_highlight::Highlighter,
}

impl<'a> FileWalker<'a> {
    fn new(content: &'a str, syntax: &'static SyntaxReference) -> Self {
        Self {
            lines: LinesWithEndings::from(content),
            pos: 1,
            hl: maki_highlight::Highlighter::for_syntax(syntax),
        }
    }

    /// Advances the parser without keeping styled output (for lines we
    /// skip over in the diff). Debug-asserts if we run past EOF.
    fn skip(&mut self) -> bool {
        let Some(line) = self.lines.next() else {
            debug_assert!(
                false,
                "FileWalker::skip called past EOF at pos {}",
                self.pos
            );
            return false;
        };
        self.hl.advance(line);
        self.pos += 1;
        true
    }

    fn highlight_next(&mut self) -> Option<Vec<Span<'static>>> {
        let Some(line) = self.lines.next() else {
            debug_assert!(
                false,
                "FileWalker::highlight_next called past EOF at pos {}",
                self.pos
            );
            return None;
        };
        let spans = highlight_line(&mut self.hl, line);
        self.pos += 1;
        Some(spans)
    }

    fn skip_to(&mut self, target: usize) {
        while self.pos < target {
            if !self.skip() {
                return;
            }
        }
        debug_assert_eq!(
            self.pos, target,
            "FileWalker overshot or failed to reach target",
        );
    }
}

fn render_diff(
    syntax: Option<&'static SyntaxReference>,
    before: &str,
    after: &str,
) -> Vec<Line<'static>> {
    let hunks = compute_hunks(before, after);
    let Some(last) = hunks.last() else {
        return Vec::new();
    };
    let numbered = last
        .lines
        .iter()
        .filter(|l| !matches!(l, DiffLine::Added(_)))
        .count();
    let w = nr_width(last.before_start + numbered.saturating_sub(1));

    let mut walkers = syntax.map(|s| (FileWalker::new(before, s), FileWalker::new(after, s)));

    let mut lines = Vec::new();
    for (i, hunk) in hunks.iter().enumerate() {
        if i > 0 {
            lines.push(gap_ellipsis());
        }
        if let Some((before, after)) = walkers.as_mut() {
            before.skip_to(hunk.before_start);
            after.skip_to(hunk.after_start);
        }

        let mut line_nr = hunk.before_start;
        for dl in &hunk.lines {
            lines.push(render_hunk_line(dl, walkers.as_mut(), &mut line_nr, w));
        }
    }

    lines
}

fn numbered_gutter(line_nr: &mut usize, w: usize) -> Span<'static> {
    let span = gutter(&format!("{line_nr:>w$}"));
    *line_nr += 1;
    span
}

/// Unchanged lines step both walkers but take spans from `after`.
/// Removed/added lines only step their own side.
fn render_hunk_line(
    dl: &DiffLine,
    walkers: Option<&mut (FileWalker<'_>, FileWalker<'_>)>,
    line_nr: &mut usize,
    w: usize,
) -> Line<'static> {
    let theme = theme::current();
    match dl {
        DiffLine::Unchanged(t) => {
            let after_spans = walkers.and_then(|(before, after)| {
                before.skip();
                after.highlight_next()
            });
            let mut spans = vec![numbered_gutter(line_nr, w), Span::raw("  ")];
            spans.extend(syntax_to_spans(after_spans, t));
            Line::from(spans)
        }
        DiffLine::Removed(ds) => {
            let before_spans = walkers.and_then(|(before, _)| before.highlight_next());
            let mut spans = vec![numbered_gutter(line_nr, w)];
            spans.extend(diff_change_spans(
                "- ",
                ds,
                before_spans,
                theme.diff_old,
                theme.diff_old_emphasis,
            ));
            Line::from(spans)
        }
        DiffLine::Added(ds) => {
            let after_spans = walkers.and_then(|(_, after)| after.highlight_next());
            let mut spans = vec![gutter(&" ".repeat(w))];
            spans.extend(diff_change_spans(
                "+ ",
                ds,
                after_spans,
                theme.diff_new,
                theme.diff_new_emphasis,
            ));
            Line::from(spans)
        }
    }
}

fn syntax_to_spans(syntax: Option<Vec<Span<'static>>>, text: &str) -> Vec<Span<'static>> {
    match syntax {
        Some(s) => s,
        None => vec![fallback_span(text)],
    }
}

fn diff_change_spans(
    prefix: &'static str,
    ds: &[DiffSpan],
    syntax: Option<Vec<Span<'static>>>,
    base: Style,
    emph: Style,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        prefix,
        base.patch(theme::current().code_fallback),
    )];
    match syntax {
        Some(syn) => spans.extend(merge_syntax_with_diff(&syn, ds, base, emph)),
        None => {
            let full: String = ds.iter().map(|s| s.text.as_str()).collect();
            spans.push(Span::styled(
                maki_highlight::normalize_text(&full),
                base.patch(theme::current().code_fallback),
            ));
        }
    }
    spans
}

fn render_grep_results(
    entries: &[GrepFileEntry],
    max_lines: usize,
    highlight: bool,
) -> (Vec<Line<'static>>, bool) {
    let mut out = Vec::new();
    let mut budget = max_lines;
    let total_matches: usize = entries.iter().map(|e| e.match_count()).sum();
    let mut rendered_matches: usize = 0;

    let global_max_nr = entries
        .iter()
        .flat_map(|e| {
            e.groups
                .iter()
                .flat_map(|g| g.lines.iter().map(|l| l.line_nr))
        })
        .max()
        .unwrap_or(1);
    let w = nr_width(global_max_nr);
    let multi = entries.len() > 1;
    let dim = theme::current().tool_dim;

    for entry in entries {
        if budget == 0 {
            break;
        }

        if multi {
            out.push(Line::from(Span::styled(
                entry.path.clone(),
                theme::current().tool_path,
            )));
        }

        let syntax = highlight.then(|| maki_highlight::syntax_for_path(&entry.path));
        let has_context = entry.groups.iter().any(|g| g.lines.len() > 1);

        for (gi, group) in entry.groups.iter().enumerate() {
            if budget == 0 {
                break;
            }
            if gi > 0 && has_context {
                out.push(Line::from(Span::styled("  --".to_owned(), dim)));
                budget -= 1;
            }
            for line in &group.lines {
                if budget == 0 {
                    break;
                }
                let mut spans = vec![gutter(&format!("{:>w$}", line.line_nr))];
                let text_spans = if let Some(syn) = syntax {
                    highlight_spans(
                        &mut maki_highlight::Highlighter::for_syntax(syn),
                        &line.text,
                    )
                } else if line.is_match {
                    vec![fallback_span(&line.text)]
                } else {
                    vec![Span::styled(line.text.clone(), dim)]
                };
                if line.is_match {
                    spans.extend(text_spans);
                    rendered_matches += 1;
                } else {
                    spans.extend(
                        text_spans
                            .into_iter()
                            .map(|s| Span::styled(s.content, theme::dim_style(s.style, 0.3))),
                    );
                }
                out.push(Line::from(spans));
                budget -= 1;
            }
        }
    }
    let hidden = if budget == 0 {
        total_matches - rendered_matches
    } else {
        0
    };
    let truncated = should_truncate(hidden);
    if truncated {
        out.push(truncation_line(hidden));
    }
    (out, truncated)
}

pub(crate) fn render_instructions(
    blocks: &[InstructionBlock],
    lines: &mut Vec<Line<'static>>,
    max_lines: usize,
    highlight: bool,
) -> bool {
    let dim = theme::current().tool_dim;
    let mut used = 0;
    let mut truncated = false;
    let multi = blocks.len() > 1;

    for (i, block) in blocks.iter().enumerate() {
        if used >= max_lines {
            truncated = true;
            break;
        }

        if multi {
            lines.push(Line::from(Span::styled(block.path.clone(), dim)));
            used += 1;
            if i > 0 && used >= max_lines {
                truncated = true;
                break;
            }
        }

        if block.content.is_empty() {
            continue;
        }

        let code_lines: Vec<String> = block.content.lines().map(String::from).collect();
        let total = code_lines.len();
        let remaining = max_lines.saturating_sub(used);
        let hl = highlight.then(|| maki_highlight::Highlighter::for_path(&block.path));
        let (rendered, was_truncated) = render_code(hl, 1, &code_lines, total, remaining);
        used += rendered.len();
        truncated |= was_truncated;
        lines.extend(rendered);
    }
    truncated
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionFlags {
    pub script: bool,
    pub output: bool,
}

impl SectionFlags {
    pub fn any(self) -> bool {
        self.script || self.output
    }
}

#[derive(Clone, Copy)]
pub struct RenderLimits {
    pub script: usize,
    pub output: usize,
}

impl RenderLimits {
    pub fn new(expanded: SectionFlags, output_limit: usize) -> Self {
        Self {
            script: if expanded.script {
                usize::MAX
            } else {
                MAX_CODE_EXECUTION_LINES
            },
            output: if expanded.output {
                usize::MAX
            } else {
                output_limit
            },
        }
    }

    pub fn is_output_expanded(self) -> bool {
        self.output == usize::MAX
    }
}

pub struct ToolContent {
    pub lines: Vec<Line<'static>>,
    pub truncation: SectionFlags,
}

pub fn render_tool_content(
    input: Option<&ToolInput>,
    output: Option<&ToolOutput>,
    highlight: bool,
    limits: RenderLimits,
) -> ToolContent {
    let mut lines = Vec::new();
    let mut truncation = SectionFlags::default();
    if let Some((language, code)) = input.map(|i| match i {
        ToolInput::Script { language, code } | ToolInput::Code { language, code } => {
            (language, code)
        }
    }) {
        let code_lines: Vec<String> = code
            .trim_end_matches('\n')
            .lines()
            .map(String::from)
            .collect();
        let total = code_lines.len();
        let hl = highlight.then(|| maki_highlight::Highlighter::for_token(language));
        let (code_result, trunc) = render_code(hl, 1, &code_lines, total, limits.script);
        truncation.script = trunc;
        lines.extend(code_result);
    }
    let (output_lines, output_trunc) = match output {
        Some(ToolOutput::ReadCode {
            path,
            start_line,
            lines: code_lines,
            ..
        }) => render_code(
            highlight.then(|| maki_highlight::Highlighter::for_path(path)),
            *start_line,
            code_lines,
            code_lines.len(),
            limits.output,
        ),
        Some(
            ToolOutput::WriteCode {
                path,
                lines: code_lines,
                ..
            }
            | ToolOutput::MemoryWrite {
                path,
                lines: code_lines,
            }
            | ToolOutput::MemoryRead {
                path,
                lines: code_lines,
            },
        ) => render_code(
            highlight.then(|| maki_highlight::Highlighter::for_path(path)),
            1,
            code_lines,
            code_lines.len(),
            limits.output,
        ),
        Some(ToolOutput::Diff {
            path,
            before,
            after,
            ..
        }) => (
            render_diff(
                highlight.then(|| maki_highlight::syntax_for_path(path)),
                before,
                after,
            ),
            false,
        ),
        Some(ToolOutput::GrepResult { entries }) => {
            render_grep_results(entries, limits.output, highlight)
        }
        Some(ToolOutput::Instructions { blocks }) => {
            let mut instruction_lines = Vec::new();
            let trunc =
                render_instructions(blocks, &mut instruction_lines, limits.output, highlight);
            (instruction_lines, trunc)
        }
        Some(ToolOutput::ReadDir { .. }) => (Vec::new(), false),
        _ => (Vec::new(), false),
    };
    truncation.output = output_trunc;
    if !lines.is_empty() && !output_lines.is_empty() {
        lines.push(Line::default());
    }
    lines.extend(output_lines);
    ToolContent { lines, truncation }
}

fn merge_syntax_with_diff(
    syntax_spans: &[Span<'static>],
    diff_spans: &[DiffSpan],
    base: Style,
    emphasis: Style,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();

    let syn_iter = syntax_spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)));

    let mut diff_iter = diff_spans.iter().flat_map(|ds| {
        let bg = if ds.emphasized { emphasis } else { base };
        ds.text.chars().map(move |_| bg)
    });

    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;

    for (syn_char, syn_style) in syn_iter {
        let bg = diff_iter.next().unwrap_or(base);
        let combined = syn_style.patch(bg);

        if current_style == Some(combined) {
            current_text.push(syn_char);
        } else {
            if !current_text.is_empty() {
                result.push(Span::styled(
                    std::mem::take(&mut current_text),
                    current_style.unwrap(),
                ));
            }
            current_text.push(syn_char);
            current_style = Some(combined);
        }
    }

    if !current_text.is_empty() {
        result.push(Span::styled(current_text, current_style.unwrap()));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::TRUNCATION_PREFIX;
    use maki_agent::GrepMatchGroup;
    use test_case::test_case;

    fn plain(text: &str) -> DiffSpan {
        DiffSpan {
            text: text.into(),
            emphasized: false,
        }
    }

    use ratatui::style::Color;

    const READ_MAX_LINES: usize = 5;

    #[test_case(20, 20, READ_MAX_LINES + 1 ; "truncates_with_ellipsis")]
    #[test_case(3,  3,  3                    ; "no_truncation_when_short")]
    #[test_case(5,  50, 5 + 1                ; "total_exceeds_available_lines")]
    #[test_case(6,  6,  6                    ; "one_hidden_shows_all")]
    fn render_code_line_count(input_lines: usize, total: usize, expected: usize) {
        let code_lines: Vec<String> = (0..input_lines).map(|i| format!("line {i}")).collect();
        let (result, _) = render_code(
            Some(maki_highlight::Highlighter::for_path("test.rs")),
            1,
            &code_lines,
            total,
            READ_MAX_LINES,
        );
        assert_eq!(result.len(), expected);
    }

    fn diff_fg(lines: &[Line<'static>], substr: &str) -> ratatui::style::Color {
        lines
            .iter()
            .find_map(|l| {
                l.spans
                    .iter()
                    .find(|s| s.content.contains(substr))
                    .and_then(|s| s.style.fg)
            })
            .unwrap_or_else(|| panic!("no fg-styled span containing {substr:?}"))
    }

    /// Walk the file from scratch up to `prefix`, then highlight `text`
    /// and return the fg for `find`. This is our ground truth.
    fn fg_in_context(path: &str, prefix: &str, text: &str, find: &str) -> ratatui::style::Color {
        let mut hl = maki_highlight::Highlighter::for_path(path);
        for line in prefix.lines() {
            let with_nl = format!("{line}\n");
            let _ = highlight_line(&mut hl, &with_nl);
        }
        let with_nl = format!("{text}\n");
        highlight_line(&mut hl, &with_nl)
            .into_iter()
            .find_map(|span| {
                if span.content.contains(find) {
                    span.style.fg
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("ref fg for {find:?} missing"))
    }

    /// Context lines inside a block comment must carry the full-file parser
    /// state, not a fresh one from the hunk start.
    #[test]
    fn diff_context_line_inside_block_comment_matches_full_file_state() {
        let before = "/*\nalpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\nOLD\ngolf\n*/\n";
        let after = "/*\nalpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\nNEW\ngolf\n*/\n";

        let lines = render_diff(
            Some(maki_highlight::syntax_for_path("test.rs")),
            before,
            after,
        );

        let expected = fg_in_context("test.rs", "/*\nalpha\nbravo\ncharlie\n", "delta", "delta");
        assert_eq!(diff_fg(&lines, "delta"), expected);
    }

    /// When an edit removes `*/`, formerly-code lines become comment.
    /// Unchanged context lines must use the AFTER parser state.
    #[test]
    fn diff_unchanged_line_uses_after_state_when_close_tag_removed() {
        let before = "/*\ndoc\n*/\nfn x() {}\n";
        let after = "/*\ndoc\nfn x() {}\n";

        let lines = render_diff(
            Some(maki_highlight::syntax_for_path("test.rs")),
            before,
            after,
        );

        let expected = fg_in_context("test.rs", "/*\ndoc\n", "fn x() {}", "fn");
        assert_eq!(diff_fg(&lines, "fn x() {}"), expected);
    }

    #[test]
    fn merge_syntax_with_diff_emphasis_split() {
        let base = Style::new().bg(Color::Red);
        let emph = Style::new().bg(Color::Green);
        let syn = vec![Span::styled("abcde", Style::new().fg(Color::White))];
        let diff = vec![
            plain("abc"),
            DiffSpan {
                text: "de".into(),
                emphasized: true,
            },
        ];
        let result = merge_syntax_with_diff(&syn, &diff, base, emph);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content.as_ref(), "abc");
        assert_eq!(result[0].style.fg, Some(Color::White));
        assert_eq!(result[0].style.bg, Some(Color::Red));
        assert_eq!(result[1].content.as_ref(), "de");
        assert_eq!(result[1].style.bg, Some(Color::Green));
    }

    #[test]
    fn merge_syntax_longer_than_diff_preserves_trailing() {
        let base = Style::new().bg(Color::Red);
        let syn = vec![
            Span::styled("ab", Style::new().fg(Color::Blue)),
            Span::styled("cd", Style::new().fg(Color::Cyan)),
        ];
        let diff = vec![plain("ab")];
        let result = merge_syntax_with_diff(&syn, &diff, base, Style::default());
        assert_eq!(spans_text(&result), "abcd");
    }

    fn grep_entries(files: &[(&str, &[usize])]) -> Vec<GrepFileEntry> {
        files
            .iter()
            .map(|(path, nrs)| GrepFileEntry {
                path: path.to_string(),
                groups: nrs
                    .iter()
                    .map(|&n| GrepMatchGroup::single(n, format!("code at {path}:{n}")))
                    .collect(),
            })
            .collect()
    }

    #[test_case(&[("a.rs", &[1,2,3,4,5,6,7,8,9,10_usize] as &[usize])], 3, 4  ; "truncates_with_ellipsis")]
    #[test_case(&[("a.rs", &[1_usize,2])],                                5, 2  ; "no_truncation_when_fits")]
    #[test_case(&[("a.rs", &[1_usize,2,3]), ("b.rs", &[10,20])],          4, 6  ; "multi_file_budget_one_hidden")]
    #[test_case(&[("a.rs", &[1_usize,2])],                                1, 1  ; "one_hidden_match_no_truncation")]
    fn render_grep_line_count(files: &[(&str, &[usize])], max: usize, expected: usize) {
        let entries = grep_entries(files);
        assert_eq!(render_grep_results(&entries, max, true).0.len(), expected);
    }

    fn spans_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_text(line: &Line) -> String {
        spans_text(&line.spans)
    }

    #[test]
    fn multi_file_grep_headers_and_alignment() {
        let entries = grep_entries(&[("a.rs", &[1]), ("b.rs", &[100])]);
        let (lines, _) = render_grep_results(&entries, 10, false);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("a.rs")));
        assert!(texts.iter().any(|t| t.contains("b.rs")));

        let gutter_width =
            |line: &str| line.find(|c: char| c.is_alphabetic()).unwrap_or(usize::MAX);
        let content_gutters: Vec<usize> = texts
            .iter()
            .filter(|t| !t.contains(".rs"))
            .map(|t| gutter_width(t))
            .collect();
        assert!(
            content_gutters.windows(2).all(|w| w[0] == w[1]),
            "gutter widths should be uniform across files: {content_gutters:?}"
        );
    }

    #[test_case(MAX_INSTRUCTION_LINES, true,  true  ; "collapsed_truncates")]
    #[test_case(usize::MAX,             false, false ; "expanded_shows_all")]
    fn render_instructions_truncation(
        max_lines: usize,
        expect_truncated: bool,
        expect_notice: bool,
    ) {
        let long_content: String = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: long_content,
        }];
        let mut lines = Vec::new();
        let truncated = render_instructions(&blocks, &mut lines, max_lines, false);
        assert_eq!(truncated, expect_truncated);
        let has_notice = lines
            .iter()
            .any(|l| line_text(l).contains(TRUNCATION_PREFIX));
        assert_eq!(has_notice, expect_notice);
    }

    #[test]
    fn render_instructions_empty_content() {
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: String::new(),
        }];
        let mut lines = Vec::new();
        let truncated = render_instructions(&blocks, &mut lines, MAX_INSTRUCTION_LINES, false);
        assert!(!truncated);
        assert_eq!(lines.len(), 0);
    }

    #[test_case("héllo",           &["hé", "llo"]          ; "accented")]
    #[test_case("🦀x",              &["🦀", "x"]            ; "emoji")]
    #[test_case("sep := \"│\"",     &["sep := \"", "│\""]   ; "box_drawing")]
    #[test_case("日本語",           &["日本", "語"]         ; "cjk")]
    fn merge_syntax_with_diff_multibyte(input: &str, parts: &[&str]) {
        let base = Style::new().bg(Color::Red);
        let emph = Style::new().bg(Color::Green);
        let syn = vec![Span::styled(
            input.to_owned(),
            Style::new().fg(Color::White),
        )];
        let diff: Vec<DiffSpan> = parts
            .iter()
            .enumerate()
            .map(|(i, &t)| DiffSpan {
                text: t.into(),
                emphasized: i == 0,
            })
            .collect();
        let result = merge_syntax_with_diff(&syn, &diff, base, emph);
        assert_eq!(spans_text(&result), input);
    }
}
