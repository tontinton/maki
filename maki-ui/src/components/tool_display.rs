use super::status_bar::format_tokens;
use super::{DisplayMessage, ToolStatus};

use super::code_view;
use crate::animation::{spinner_frame, spinner_str};
use crate::theme;
use code_view::RenderLimits;
use code_view::SectionFlags;
use maki_config::ToolOutputLines;

use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use maki_providers::{ModelPricing, TokenUsage};

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::markdown::{should_truncate, text_to_lines, truncate_output, truncation_notice};
use maki_agent::{
    BufferSnapshot, InstructionBlock, SnapshotSpan, SpanStyle, ToolInput, ToolOutput,
};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::render_worker::RenderWorker;

pub struct RenderCtx<'a> {
    pub started_at: Instant,
    pub width: u16,
    pub tool_output_lines: &'a ToolOutputLines,
}

pub const TOOL_INDICATOR: &str = "● ";
pub const TOOL_BODY_INDENT: &str = "  ";
pub(crate) const SPINNER_STYLE_NAME: &str = "spinner";
pub(crate) const SPINNER_STYLE_PREFIX: &str = "spinner:";

const CODE_OUTPUT_DIVIDER: &str = "  ────────────";

pub struct RoleStyle {
    pub prefix: &'static str,
    pub text_style: Style,
    pub prefix_style: Style,
    pub use_markdown: bool,
    pub max_line_bytes: Option<usize>,
}

pub fn assistant_style() -> RoleStyle {
    RoleStyle {
        prefix: "maki> ",
        text_style: theme::current().assistant,
        prefix_style: theme::current().assistant_prefix,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn user_style() -> RoleStyle {
    RoleStyle {
        prefix: "you> ",
        text_style: theme::current().assistant,
        prefix_style: theme::current().user,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn thinking_style() -> RoleStyle {
    RoleStyle {
        prefix: "thinking> ",
        text_style: theme::current().thinking,
        prefix_style: theme::current().thinking,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn error_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current().error,
        prefix_style: theme::current().tool_error,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub fn done_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current()
            .tool_success
            .add_modifier(ratatui::style::Modifier::BOLD),
        prefix_style: theme::current().tool_success,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub struct ToolLines {
    pub lines: Vec<Line<'static>>,
    pub search_text: String,
    pub highlight: Option<HighlightRequest>,
    pub spinner_lines: Vec<(usize, usize)>,
    /// Index of the first live-buffer snapshot line, recorded in the same
    /// pass that lays out `lines`, so click rows can never drift from them.
    pub snapshot_base: Option<usize>,
    pub content_indent: &'static str,
    pub truncation: SectionFlags,
}

pub struct HighlightRequest {
    pub range: (usize, usize),
    pub input: Option<Arc<ToolInput>>,
    pub output: Option<Arc<ToolOutput>>,
    pub limits: RenderLimits,
}

impl HighlightRequest {
    fn new(
        range: (usize, usize),
        input: Option<Arc<ToolInput>>,
        output: Option<Arc<ToolOutput>>,
        limits: RenderLimits,
    ) -> Option<Self> {
        if range.0 == range.1 {
            return None;
        }
        let output = output.and_then(|o| match *o {
            ToolOutput::ReadCode { .. }
            | ToolOutput::WriteCode { .. }
            | ToolOutput::Diff { .. }
            | ToolOutput::GrepResult { .. }
            | ToolOutput::Instructions { .. } => Some(o),
            ToolOutput::Plain(_)
            | ToolOutput::Markdown(_)
            | ToolOutput::ReadDir(_)
            | ToolOutput::TodoList(_)
            | ToolOutput::Batch { .. }
            | ToolOutput::Image { .. } => None,
        });
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(Self {
            range,
            input,
            output,
            limits,
        })
    }
}

impl ToolLines {
    pub fn send_highlight(&self, worker: &RenderWorker) -> Option<u64> {
        let hl = self.highlight.as_ref()?;
        Some(worker.send(hl.input.clone(), hl.output.clone(), hl.limits))
    }
}

pub fn format_timestamp_now() -> String {
    let zoned = Timestamp::now().to_zoned(TimeZone::system());
    zoned.strftime("%H:%M:%S").to_string()
}

pub fn format_turn_usage(usage: &TokenUsage, pricing: &ModelPricing, fast: bool) -> String {
    let tokens = format!(
        "{}↑ {}↓",
        format_tokens(usage.total_input()),
        format_tokens(usage.output),
    );
    if pricing.is_zero() {
        tokens
    } else {
        let cost = usage.cost(pricing, fast);
        format!("{tokens} ${cost:.3}")
    }
}

pub fn append_right_info(
    line: &mut Line<'static>,
    usage: Option<&str>,
    timestamp: Option<&str>,
    width: u16,
) {
    if usage.is_none() && timestamp.is_none() {
        return;
    }
    let separator = if usage.is_some() && timestamp.is_some() {
        2
    } else {
        0
    };
    let suffix_len =
        usage.map_or(0, UnicodeWidthStr::width) + timestamp.map_or(0, str::len) + separator + 1;
    let header_width: usize = line
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let w = width as usize;
    if header_width + 1 + suffix_len > w {
        return;
    }
    let pad = w - header_width - suffix_len;
    line.spans.push(Span::raw(" ".repeat(pad)));
    if let Some(u) = usage {
        line.spans
            .push(Span::styled(u.to_owned(), theme::current().tool_dim));
        if timestamp.is_some() {
            line.spans.push(Span::raw("  "));
        }
    }
    if let Some(ts) = timestamp {
        line.spans
            .push(Span::styled(ts.to_owned(), theme::current().timestamp));
    }
}

enum Indicator {
    InProgress,
    Success,
    Error,
}

impl From<ToolStatus> for Indicator {
    fn from(s: ToolStatus) -> Self {
        match s {
            ToolStatus::InProgress => Self::InProgress,
            ToolStatus::Success => Self::Success,
            ToolStatus::Error => Self::Error,
        }
    }
}

struct ResolvedOutput<'a> {
    text: Option<Cow<'a, str>>,
    full_text: Option<Cow<'a, str>>,
    skipped: usize,
}

fn resolve_output<'a>(
    output: Option<&'a ToolOutput>,
    body: Option<&'a str>,
    live_output: Option<&'a str>,
    pre_truncated: usize,
    limits: RenderLimits,
) -> ResolvedOutput<'a> {
    let full_text: Option<Cow<'a, str>> = match output {
        Some(ToolOutput::Plain(t) | ToolOutput::Markdown(t) | ToolOutput::ReadDir(t)) => {
            Some(Cow::Borrowed(t.text.as_str()))
        }
        Some(ToolOutput::Batch { text }) => Some(Cow::Borrowed(text.as_str())),
        _ => None,
    };

    let expanded = limits.is_output_expanded();
    let (raw_text, already_truncated): (Option<Cow<'a, str>>, usize) = if expanded {
        match &full_text {
            Some(t) => (Some(t.clone()), 0),
            None if output.is_some() => {
                return ResolvedOutput {
                    text: None,
                    full_text: None,
                    skipped: 0,
                };
            }
            None => match live_output {
                Some(live) => (Some(Cow::Borrowed(live)), 0),
                None => match body {
                    Some(b) => (Some(Cow::Borrowed(b)), pre_truncated),
                    None => (None, 0),
                },
            },
        }
    } else {
        match (body, &full_text) {
            (Some(b), _) => (Some(Cow::Borrowed(b)), pre_truncated),
            (None, Some(t)) => (Some(t.clone()), 0),
            (None, None) if output.is_some() => {
                return ResolvedOutput {
                    text: None,
                    full_text: None,
                    skipped: 0,
                };
            }
            (None, None) => (None, 0),
        }
    };

    let (text, skipped) = match raw_text {
        Some(t) if !t.is_empty() => {
            let tr = truncate_output(&t, limits.output);
            let s = if tr.skipped > 0 {
                tr.skipped
            } else {
                already_truncated
            };
            (Some(Cow::Owned(tr.kept.into_owned())), s)
        }
        _ => (None, already_truncated),
    };

    ResolvedOutput {
        text,
        full_text,
        skipped,
    }
}

struct ToolLineBuilder {
    lines: Vec<Line<'static>>,
    search_text: String,
    spinner_lines: Vec<(usize, usize)>,
    snapshot_base: Option<usize>,
    content_range: (usize, usize),
    width: u16,
    truncation: SectionFlags,
    limits: RenderLimits,
    markdown: bool,
}

impl ToolLineBuilder {
    fn new(width: u16, expanded: SectionFlags, max_output_lines: usize) -> Self {
        let limits = RenderLimits::new(expanded, max_output_lines);
        Self {
            lines: Vec::new(),
            search_text: String::new(),
            spinner_lines: Vec::new(),
            snapshot_base: None,
            content_range: (0, 0),
            width,
            truncation: SectionFlags::default(),
            limits,
            markdown: false,
        }
    }

    fn apply_output_format(&mut self, output: Option<&ToolOutput>) {
        if output.is_some_and(ToolOutput::is_markdown) {
            self.markdown = true;
        }
    }

    fn push_header(
        &mut self,
        tool_name: &str,
        header: &str,
        annotation: Option<&str>,
        render_header: Option<&BufferSnapshot>,
    ) {
        let mut spans = vec![Span::styled(
            format!("{tool_name}> "),
            theme::current().tool_prefix,
        )];
        if let Some(snapshot) = render_header {
            if let Some(first_line) = snapshot.lines.first() {
                let line_idx = self.lines.len();
                let spinners = &mut self.spinner_lines;
                bake_spans(&first_line.spans, &mut spans, spinner_str(0), |span_idx| {
                    spinners.push((line_idx, span_idx));
                });
            }
        } else {
            spans.push(Span::styled(header.to_owned(), theme::current().tool));
        }
        let mut copy = format!("{tool_name}> {header}");
        if let Some(ann) = annotation {
            spans.push(Span::styled(
                format!(" ({ann})"),
                theme::current().tool_annotation,
            ));
            write!(copy, " ({ann})").unwrap();
        }
        self.lines.push(Line::from(spans));
        self.search_text = copy;
    }

    fn push_search_text(&mut self, text: &str) {
        if !self.search_text.is_empty() {
            self.search_text.push('\n');
        }
        self.search_text.push_str(text);
    }

    fn prepend_indicator(&mut self, indicator: Indicator, started_at: Instant) {
        if self.lines.is_empty() {
            return;
        }
        let (text, style) = match indicator {
            Indicator::InProgress => {
                let ch = spinner_frame(started_at.elapsed().as_millis());
                (format!("{ch} "), theme::current().spinner)
            }
            Indicator::Success => (TOOL_INDICATOR.into(), theme::current().tool_success),
            Indicator::Error => (TOOL_INDICATOR.into(), theme::current().tool_error),
        };
        for (line, span) in &mut self.spinner_lines {
            if *line == 0 {
                *span += 1;
            }
        }
        if matches!(indicator, Indicator::InProgress) {
            self.spinner_lines.push((0, 0));
        }
        self.lines[0].spans.insert(0, Span::styled(text, style));
    }

    fn push_code_content(&mut self, input: Option<&ToolInput>, output: Option<&ToolOutput>) {
        let content = code_view::render_tool_content(input, output, false, self.limits);
        self.truncation.script |= content.truncation.script;
        self.truncation.output |= content.truncation.output;
        let start = self.lines.len();
        for mut line in content.lines {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
        self.content_range = (start, self.lines.len());
        if let Some(ToolInput::Code { code, .. } | ToolInput::Script { code, .. }) = input {
            self.push_search_text(code.trim_end());
        }
        if let Some(text) = output.and_then(|o| o.structured_display_text()) {
            self.push_search_text(&text);
        }
    }

    fn push_resolved_output(&mut self, resolved: &ResolvedOutput<'_>) {
        if resolved.text.is_none() {
            return;
        }

        if self.content_range.1 > self.content_range.0 {
            self.lines.push(Line::from(Span::styled(
                CODE_OUTPUT_DIVIDER,
                theme::current().tool_dim,
            )));
        }

        if let Some(text) = &resolved.text {
            if self.markdown {
                self.push_markdown_body(text);
            } else {
                push_text_lines(&mut self.lines, text, TOOL_BODY_INDENT);
            }
            if let Some(full) = &resolved.full_text {
                self.push_search_text(full);
            } else {
                self.push_search_text(text);
            }
            self.push_truncation_count(resolved.skipped);
        }
    }

    fn push_markdown_body(&mut self, text: &str) {
        let style = theme::current().assistant;
        let indent = TOOL_BODY_INDENT.len() as u16;
        let md_lines = text_to_lines(
            text,
            "",
            style,
            style,
            self.width.saturating_sub(indent),
            Some(maki_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
        );
        for mut line in md_lines {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
    }

    fn push_truncation_count(&mut self, skipped: usize) {
        if should_truncate(skipped) {
            self.truncation.output = true;
            let text = truncation_notice(skipped);
            let mut line = Line::from(Span::styled(text, theme::current().tool_dim));
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
    }

    fn push_snapshot(
        &mut self,
        snapshot: &BufferSnapshot,
        search_fallback: Option<&str>,
        started_at: Instant,
    ) {
        let base = self.lines.len();
        self.snapshot_base = Some(base);
        let total = snapshot.lines.len();
        let frame = spinner_str(started_at.elapsed().as_millis());
        let (lines, spinners) =
            snapshot_to_lines_range(snapshot, TOOL_BODY_INDENT, 0..total, frame);
        self.lines.extend(lines);
        self.spinner_lines
            .extend(spinners.into_iter().map(|(line, span)| (base + line, span)));
        self.push_search_text(&snapshot.text());
        if let Some(text) = search_fallback {
            self.push_search_text(text);
        }
    }

    fn finish(
        self,
        input: Option<Arc<ToolInput>>,
        output: Option<Arc<ToolOutput>>,
        content_indent: &'static str,
    ) -> ToolLines {
        let highlight = HighlightRequest::new(self.content_range, input, output, self.limits);
        ToolLines {
            lines: self.lines,
            search_text: self.search_text,
            highlight,
            spinner_lines: self.spinner_lines,
            snapshot_base: self.snapshot_base,
            content_indent,
            truncation: self.truncation,
        }
    }
}

fn push_text_lines(lines: &mut Vec<Line<'static>>, text: &str, indent: &'static str) {
    let style = theme::current().tool;
    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::styled(indent, style),
            Span::styled(line.to_owned(), style),
        ]));
    }
}

/// Bakes snapshot spans onto `out`. `"spinner"`-styled spans bake to the
/// current frame, and `on_spinner` gets their span index in the same pass,
/// so animation offsets can never drift from the baked spans.
fn bake_spans(
    src: &[SnapshotSpan],
    out: &mut Vec<Span<'static>>,
    spinner_frame: &'static str,
    mut on_spinner: impl FnMut(usize),
) {
    for span in src {
        if matches!(&span.style, SpanStyle::Named(n) if n == SPINNER_STYLE_NAME) {
            on_spinner(out.len());
            out.push(Span::styled(spinner_frame, theme::current().spinner));
        } else {
            out.push(Span::styled(
                span.text.clone(),
                resolve_span_style(&span.style),
            ));
        }
    }
}

fn snapshot_to_lines_range(
    snapshot: &BufferSnapshot,
    indent: &str,
    range: std::ops::Range<usize>,
    spinner_frame: &'static str,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut spinners = Vec::new();
    let lines = snapshot.lines[range]
        .iter()
        .enumerate()
        .map(|(i, sline)| {
            let mut spans = vec![Span::raw(indent.to_string())];
            bake_spans(&sline.spans, &mut spans, spinner_frame, |span_idx| {
                spinners.push((i, span_idx));
            });
            Line::from(spans)
        })
        .collect();
    (lines, spinners)
}

pub(crate) fn resolve_span_style(style: &SpanStyle) -> Style {
    match style {
        SpanStyle::Default => theme::current().tool,
        SpanStyle::Named(name) => theme::style_by_name(name),
        SpanStyle::Inline(inline) => {
            let mut s = Style::default();
            if let Some((r, g, b)) = inline.fg {
                s = s.fg(Color::Rgb(r, g, b));
            }
            if let Some((r, g, b)) = inline.bg {
                s = s.bg(Color::Rgb(r, g, b));
            }
            if inline.bold {
                s = s.bold();
            }
            if inline.italic {
                s = s.italic();
            }
            if inline.underline {
                s = s.underlined();
            }
            if inline.dim {
                s = s.dim();
            }
            if inline.strikethrough {
                s = s.crossed_out();
            }
            if inline.reversed {
                s = s.reversed();
            }
            s
        }
    }
}

pub fn build_tool_lines(
    msg: &DisplayMessage,
    status: ToolStatus,
    rctx: &RenderCtx,
    expanded: SectionFlags,
) -> ToolLines {
    let tool_name = msg.role.tool_name().unwrap_or("?");
    let (header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };

    let mut b = ToolLineBuilder::new(rctx.width, expanded, rctx.tool_output_lines.get(tool_name));
    b.apply_output_format(msg.tool_output.as_deref());
    b.push_header(
        tool_name,
        header,
        msg.annotation.as_deref(),
        msg.render_header.as_ref(),
    );
    b.prepend_indicator(status.into(), rctx.started_at);
    let has_snapshot = msg.render_snapshot.is_some();
    b.push_code_content(
        msg.tool_input.as_deref(),
        if has_snapshot {
            None
        } else {
            msg.tool_output.as_deref()
        },
    );
    if let Some(ref snapshot) = msg.render_snapshot {
        let search_text = msg
            .tool_output
            .as_ref()
            .and_then(|o| match o.as_ref() {
                ToolOutput::Plain(t) | ToolOutput::Markdown(t) | ToolOutput::ReadDir(t) => {
                    Some(t.text.as_str())
                }
                _ => None,
            })
            .or(body);
        b.push_snapshot(snapshot, search_text, rctx.started_at);
        // With pre-permission previews, an error (say a denial) can land
        // while only the script snapshot is on screen. Show the error below
        // it, unless the handler already drew it into the body.
        if matches!(status, ToolStatus::Error) {
            let err_text = msg.tool_output.as_deref().map(|o| o.as_text());
            let shown = err_text.as_deref().or(body).map_or("", str::trim);
            if !shown.is_empty() && !snapshot.text().contains(shown) {
                let resolved = resolve_output(
                    msg.tool_output.as_deref(),
                    body,
                    msg.live_output.as_deref(),
                    msg.truncated_lines,
                    b.limits,
                );
                b.push_resolved_output(&resolved);
            }
        }
    } else {
        let resolved = resolve_output(
            msg.tool_output.as_deref(),
            body,
            msg.live_output.as_deref(),
            msg.truncated_lines,
            b.limits,
        );
        b.push_resolved_output(&resolved);
    }
    b.finish(
        msg.tool_input.clone(),
        msg.tool_output.clone(),
        TOOL_BODY_INDENT,
    )
}

pub fn truncate_to_header(text: &mut String) {
    let end = text.find('\n').unwrap_or(text.len());
    text.truncate(end);
}

pub(crate) fn append_annotation(ann: &mut Option<String>, suffix: &str) {
    match ann {
        Some(a) => write!(a, " · {suffix}").unwrap(),
        None => *ann = Some(suffix.to_owned()),
    }
}

pub fn build_instructions_lines(
    blocks: &[InstructionBlock],
    width: u16,
    expanded: bool,
) -> ToolLines {
    let header = blocks.first().map_or("", |b| b.path.as_str());
    let annotation = if blocks.len() > 1 {
        Some(format!("+{}", blocks.len() - 1))
    } else {
        None
    };

    let exp = SectionFlags {
        script: false,
        output: expanded,
    };
    let mut b = ToolLineBuilder::new(width, exp, code_view::instruction_limit(expanded));
    b.push_header("load", header, annotation.as_deref(), None);
    b.prepend_indicator(Indicator::Success, Instant::now());

    let start = b.lines.len();
    let has_truncation =
        code_view::render_instructions(blocks, &mut b.lines, b.limits.output, false);
    b.truncation.output |= has_truncation;
    for line in &mut b.lines[start..] {
        line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
    }
    b.content_range = (start, b.lines.len());

    b.push_search_text(
        &blocks
            .iter()
            .map(|bl| bl.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    let output = Arc::new(ToolOutput::Instructions {
        blocks: blocks.to_vec(),
    });
    b.finish(None, Some(output), TOOL_BODY_INDENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: ToolOutputLines = ToolOutputLines::DEFAULT;
    use crate::components::{DisplayRole, ToolRole};
    use crate::markdown::TRUNCATION_PREFIX;
    use maki_agent::tools::{BASH_TOOL_NAME, READ_TOOL_NAME, TASK_TOOL_NAME};
    use maki_agent::{SnapshotLine, SnapshotSpan, TextOutput, ToolInput, ToolOutput};
    use test_case::test_case;

    fn test_rctx(width: u16) -> RenderCtx<'static> {
        RenderCtx {
            started_at: Instant::now(),
            width,
            tool_output_lines: &TOL,
        }
    }

    fn exp(both: bool) -> SectionFlags {
        SectionFlags {
            script: both,
            output: both,
        }
    }

    fn code_input() -> Option<ToolInput> {
        Some(ToolInput::Code {
            language: "sh".into(),
            code: "echo hi\n".into(),
        })
    }

    fn code_output() -> Option<ToolOutput> {
        Some(ToolOutput::ReadCode {
            path: "test.rs".into(),
            start_line: 1,
            lines: vec!["fn main() {}".into()],
            total_lines: 1,
            instructions: None,
        })
    }

    fn plain_output() -> Option<ToolOutput> {
        Some(ToolOutput::Plain("ok".into()))
    }

    fn bash_msg(
        text: &str,
        status: ToolStatus,
        input: Option<ToolInput>,
        output: Option<ToolOutput>,
    ) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status,
                name: BASH_TOOL_NAME.into(),
            })),
            text: text.into(),
            tool_input: input.map(Arc::new),
            tool_raw_input: None,
            tool_output: output.map(Arc::new),
            live_output: None,
            annotation: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    #[test_case(code_input(),  code_output(),   true,  true  ; "code_input_keeps_code_output")]
    #[test_case(None,          code_output(),   true,  true  ; "code_output_only")]
    #[test_case(None,          plain_output(),  false, false ; "no_content_no_highlight")]
    fn highlight_request(
        input: Option<ToolInput>,
        output: Option<ToolOutput>,
        expect_highlight: bool,
        expect_output: bool,
    ) {
        let msg = bash_msg("header\nbody", ToolStatus::Success, input, output);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        assert_eq!(tl.highlight.is_some(), expect_highlight);
        if let Some(hl) = &tl.highlight {
            assert_eq!(hl.output.is_some(), expect_output);
        }
    }

    fn has_styled_span(spans: &[Span<'_>], text: &str, style: Style) -> bool {
        spans
            .iter()
            .any(|s| s.content.contains(text) && s.style == style)
    }

    fn lines_text(tl: &ToolLines) -> String {
        tl.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test_case(ToolStatus::InProgress, None           ; "live_streaming_shows_body")]
    #[test_case(ToolStatus::Success,    plain_output() ; "done_with_plain_output_shows_body")]
    fn bash_body_visible(status: ToolStatus, output: Option<ToolOutput>) {
        let msg = bash_msg("echo hi\nline1\nline2", status, code_input(), output);
        let tl = build_tool_lines(&msg, status, &test_rctx(80), SectionFlags::default());
        let text = lines_text(&tl);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    fn line_has_styled(tl: &ToolLines, text: &str, style: Style) -> bool {
        tl.lines
            .iter()
            .any(|l| has_styled_span(&l.spans, text, style))
    }

    #[test_case("header\nbody\nmore", "header" ; "multiline")]
    #[test_case("header",            "header" ; "single_line")]
    fn truncate_to_header_cases(input: &str, expected: &str) {
        let mut text = input.to_string();
        truncate_to_header(&mut text);
        assert_eq!(text, expected);
    }

    fn tool_msg() -> DisplayMessage {
        bash_msg("cmd", ToolStatus::Success, None, None)
    }

    #[test_case(80, true  ; "shown_when_width_sufficient")]
    #[test_case(10, false ; "hidden_when_too_narrow")]
    fn append_right_info_timestamp_visibility(width: u16, expect_timestamp: bool) {
        let msg = tool_msg();
        let mut tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let span_count_before = tl.lines[0].spans.len();
        append_right_info(&mut tl.lines[0], None, Some("12:34:56"), width);
        if expect_timestamp {
            let last = tl.lines[0].spans.last().unwrap();
            assert_eq!(last.style, theme::current().timestamp);
            assert!(tl.lines[0].spans.len() > span_count_before);
        } else {
            assert_eq!(tl.lines[0].spans.len(), span_count_before);
        }
    }

    #[test]
    fn annotation_rendered_on_header() {
        let mut msg = tool_msg();
        msg.annotation = Some("2m timeout".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("(2m timeout)"));
    }

    #[test]
    fn task_output_body_visible() {
        let msg = task_msg("**bold** and `code`".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("bold"));
        assert!(text.contains("code"));
    }

    fn task_msg(output: String) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: TASK_TOOL_NAME.into(),
            })),
            text: "Find auth".into(),
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Markdown(output.into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    fn n_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_truncation_styled(tl: &ToolLines) {
        let last = tl.lines.last().unwrap();
        let span = last
            .spans
            .iter()
            .find(|s| s.content.contains(TRUNCATION_PREFIX));
        assert!(span.is_some(), "expected truncation prefix");
        assert_eq!(span.unwrap().style, theme::current().tool_dim);
    }

    fn task_truncation_tl(output: String) -> ToolLines {
        let msg = task_msg(output);
        build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        )
    }

    #[test]
    fn task_output_truncated_and_styled() {
        let task_max = TOL.task;
        let tl = task_truncation_tl(n_lines(200));
        let body_lines = tl.lines.len() - 1;
        assert!(
            body_lines <= task_max + 1,
            "expected at most {} body lines, got {body_lines}",
            task_max + 1,
        );
        assert_truncation_styled(&tl);
    }

    fn assert_hr_fits(tl: &ToolLines, width: u16) {
        let hr_line = tl
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('─')));
        assert!(hr_line.is_some());
        let total_width: usize = hr_line
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert!(
            total_width <= width as usize,
            "HR ({total_width} chars) should fit in {width} cols"
        );
    }

    #[test]
    fn task_hr_fits_within_indented_width() {
        let width: u16 = 60;
        let msg = task_msg("before\n\n---\n\nafter".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(width),
            SectionFlags::default(),
        );
        assert_hr_fits(&tl, width);
    }

    fn index_msg(body: &str) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: "index".into(),
            })),
            text: format!("src/lib.rs\n{body}"),
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain(body.to_owned().into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    #[test]
    fn index_output_truncated_at_max_lines() {
        let body: String = (0..150).map(|i| format!("  line_{i}\n")).collect();
        let msg = index_msg(&body);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("line_0"));
        assert!(!text.contains("line_149"));
        assert!(text.contains(TRUNCATION_PREFIX));
    }

    fn snapshot_msg(snapshot: BufferSnapshot) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: "index".into(),
            })),
            text: "src/lib.rs\nplain fallback".into(),
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("plain fallback".into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    fn make_snapshot(lines: Vec<Vec<SnapshotSpan>>) -> BufferSnapshot {
        BufferSnapshot {
            lines: Arc::new(
                lines
                    .into_iter()
                    .map(|spans| SnapshotLine { spans })
                    .collect(),
            ),
        }
    }

    #[test]
    fn snapshot_search_text_derives_from_rendered_lines() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "import asyncio".into(),
            style: SpanStyle::Named("keyword".into()),
        }]]);
        let tl = build_tool_lines(
            &snapshot_msg(snapshot),
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        assert!(
            tl.search_text.contains("import asyncio"),
            "search must index the rendered snapshot body, got: {}",
            tl.search_text
        );
    }

    #[test]
    fn snapshot_base_recorded_where_snapshot_lines_start() {
        let snapshot = make_snapshot(vec![
            vec![SnapshotSpan {
                text: "child one".into(),
                style: SpanStyle::Default,
            }],
            vec![SnapshotSpan {
                text: "child two".into(),
                style: SpanStyle::Default,
            }],
        ]);
        let tl = build_tool_lines(
            &snapshot_msg(snapshot),
            ToolStatus::InProgress,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let base = tl.snapshot_base.expect("snapshot must record its base");
        let line_text = |i: usize| {
            tl.lines[i]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert!(line_text(base).contains("child one"));
        assert!(line_text(base + 1).contains("child two"));
    }

    #[test]
    fn snapshot_base_absent_without_snapshot() {
        let msg = DisplayMessage {
            render_snapshot: None,
            ..snapshot_msg(make_snapshot(vec![]))
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        assert_eq!(tl.snapshot_base, None);
    }

    #[test]
    fn header_spinner_span_bakes_and_shifts_with_indicator() {
        let header = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "3 tools ".into(),
                style: SpanStyle::Default,
            },
            SnapshotSpan {
                text: "· ".into(),
                style: SpanStyle::Named(SPINNER_STYLE_NAME.into()),
            },
        ]]);
        let msg = DisplayMessage {
            render_header: Some(header),
            render_snapshot: None,
            ..snapshot_msg(make_snapshot(vec![]))
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            SectionFlags::default(),
        );
        // indicator + `tool> ` prefix + "3 tools " sit before the header spinner.
        assert_eq!(tl.spinner_lines, vec![(0, 3), (0, 0)]);
    }

    const DENIAL_MSG: &str = "Permission denied: user rejected";

    fn error_snapshot_msg(snapshot_text: &str) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Error,
                name: "code_execution".into(),
            })),
            text: "2 lines".into(),
            tool_output: Some(Arc::new(ToolOutput::Plain(DENIAL_MSG.into()))),
            ..snapshot_msg(make_snapshot(vec![vec![SnapshotSpan {
                text: snapshot_text.into(),
                style: SpanStyle::Default,
            }]]))
        }
    }

    #[test]
    fn error_with_snapshot_renders_error_below() {
        let msg = error_snapshot_msg("1 print('hi')");
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Error,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("print('hi')"), "snapshot stays: {text}");
        assert!(text.contains(DENIAL_MSG), "denial must show: {text}");
    }

    #[test]
    fn error_already_in_snapshot_is_not_duplicated() {
        let msg = error_snapshot_msg(DENIAL_MSG);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Error,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert_eq!(
            text.matches(DENIAL_MSG).count(),
            1,
            "error must render exactly once: {text}"
        );
    }

    #[test]
    fn snapshot_renders_styled_spans() {
        let snapshot = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "pub".into(),
                style: SpanStyle::Named("keyword".into()),
            },
            SnapshotSpan {
                text: " fn main()".into(),
                style: SpanStyle::Named("tool".into()),
            },
        ]]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let t = theme::current();
        assert!(line_has_styled(&tl, "pub", t.index_keyword));
        assert!(line_has_styled(&tl, " fn main()", t.tool));
    }

    #[test]
    fn snapshot_overrides_text_output() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "from_snapshot".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("from_snapshot"));
        assert!(!text.contains("plain fallback"));
        assert!(
            tl.search_text.contains("plain fallback"),
            "search_text should contain tool output for Ctrl+F"
        );
    }

    #[test_case(None,       None,    "bash",  false ; "none_output_none_body")]
    #[test_case(None,       Some("hello"), "bash", true ; "none_output_with_body")]
    #[test_case(
        Some(ToolOutput::Plain("world".into())), None, "bash", true
        ; "plain_no_body_uses_plain"
    )]
    #[test_case(
        Some(ToolOutput::Plain("world".into())), Some("override"), "bash", true
        ; "body_takes_priority_over_plain"
    )]
    #[test_case(
        Some(ToolOutput::Plain(String::new().into())), None, "bash", false
        ; "empty_plain_resolves_to_none"
    )]
    #[test_case(
        Some(ToolOutput::Batch { text: "legacy batch text".into() }),
        None, "batch", true
        ; "legacy_batch_falls_back_to_text"
    )]
    #[test_case(
        Some(ToolOutput::ReadDir(TextOutput { text: "dir listing".into(), instructions: None, state: None })),
        None, "read", true
        ; "readdir_uses_text_field"
    )]
    #[test_case(
        Some(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec![], total_lines: 0, instructions: None }),
        None, "read", false
        ; "structured_output_resolves_to_none"
    )]
    fn resolve_output_text_presence(
        output: Option<ToolOutput>,
        body: Option<&str>,
        tool: &str,
        expect_text: bool,
    ) {
        let limits = RenderLimits::new(SectionFlags::default(), TOL.get(tool));
        let resolved = resolve_output(output.as_ref(), body, None, 0, limits);
        assert_eq!(resolved.text.is_some(), expect_text);
    }

    #[test]
    fn resolve_output_pre_truncated_forwarded() {
        let limits = RenderLimits::new(SectionFlags::default(), TOL.get("bash"));
        let resolved = resolve_output(None, Some("short"), None, 42, limits);
        assert_eq!(resolved.skipped, 42);
    }

    #[test]
    fn resolve_output_truncation_overrides_pre_truncated() {
        let long = n_lines(200);
        let limits = RenderLimits::new(SectionFlags::default(), TOL.get("bash"));
        let resolved = resolve_output(None, Some(&long), None, 5, limits);
        assert!(resolved.skipped > 5);
    }

    fn bash_output_msg(line_count: usize, live: bool) -> DisplayMessage {
        let full_body = n_lines(line_count);
        let tr = truncate_output(&full_body, TOL.get("bash"));
        let text = if tr.kept.is_empty() {
            "header".into()
        } else {
            format!("header\n{}", tr.kept)
        };
        let truncated_lines = tr.skipped;
        let (status, tool_output, live_output) = if live {
            (ToolStatus::InProgress, None, Some(full_body))
        } else {
            (
                ToolStatus::Success,
                Some(Arc::new(ToolOutput::Plain(full_body.into()))),
                None,
            )
        };
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status,
                name: BASH_TOOL_NAME.into(),
            })),
            text,
            tool_input: None,
            tool_raw_input: None,
            tool_output,
            live_output,
            annotation: None,
            plan_path: None,
            truncated_lines,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    #[test]
    fn bash_expanded_live_output() {
        let msg = bash_output_msg(200, true);
        let collapsed = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), exp(false));
        let expanded = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), exp(true));
        let collapsed_text = lines_text(&collapsed);
        let expanded_text = lines_text(&expanded);
        assert!(collapsed.truncation.any());
        assert!(!expanded.truncation.any());
        assert!(expanded_text.contains("line 0"));
        assert!(expanded_text.contains("line 199"));
        assert!(collapsed_text.contains("line 0"));
        assert!(!collapsed_text.contains("line 199"));
    }

    #[test_case(200, true,  false, false ; "expanded_shows_all")]
    #[test_case(200, false, true,  true  ; "collapsed_truncates")]
    #[test_case(3,   false, false, false ; "short_no_truncation")]
    fn bash_output_truncation(
        line_count: usize,
        expanded: bool,
        expect_truncation: bool,
        expect_expand_notice: bool,
    ) {
        let msg = bash_output_msg(line_count, false);
        let tl = build_tool_lines(&msg, ToolStatus::Success, &test_rctx(80), exp(expanded));
        let text = lines_text(&tl);
        assert_eq!(tl.truncation.any(), expect_truncation);
        assert_eq!(text.contains("click to expand"), expect_expand_notice);
    }

    fn read_output_msg(line_count: usize) -> DisplayMessage {
        read_output_msg_with(line_count, "line", None)
    }

    #[test_case(20, false, true,  true  ; "read_collapsed_truncates")]
    #[test_case(20, true,  false, false ; "read_expanded_shows_all")]
    #[test_case(3,  false, false, false ; "read_short_no_truncation")]
    fn read_output_truncation(
        line_count: usize,
        expanded: bool,
        expect_truncation: bool,
        expect_expand_notice: bool,
    ) {
        let msg = read_output_msg(line_count);
        let tl = build_tool_lines(&msg, ToolStatus::Success, &test_rctx(80), exp(expanded));
        assert_eq!(tl.truncation.any(), expect_truncation);
        let text = lines_text(&tl);
        assert_eq!(text.contains("click to expand"), expect_expand_notice);
    }

    fn read_msg_with_instructions(code_lines: usize, instruction_lines: usize) -> DisplayMessage {
        let inst_content: String = (0..instruction_lines)
            .map(|i| format!("inst {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        read_output_msg_with(
            code_lines,
            "code",
            Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: inst_content,
            }]),
        )
    }

    fn read_output_msg_with(
        line_count: usize,
        prefix: &str,
        instructions: Option<Vec<InstructionBlock>>,
    ) -> DisplayMessage {
        let lines: Vec<String> = (0..line_count).map(|i| format!("{prefix} {i}")).collect();
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: READ_TOOL_NAME.into(),
            })),
            text: "read /src/main.rs".into(),
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::ReadCode {
                path: "main.rs".into(),
                start_line: 1,
                lines,
                total_lines: line_count,
                instructions,
            })),
            live_output: None,
            annotation: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        }
    }

    #[test_case(false, true,  false ; "collapsed_truncates_instructions")]
    #[test_case(true,  false, true  ; "expanded_shows_all_instructions")]
    fn instructions_segment(expanded: bool, expect_truncation: bool, expect_all_visible: bool) {
        let msg = read_msg_with_instructions(3, 30);
        let output = msg.tool_output.as_deref().unwrap();
        let blocks = output.instructions().unwrap();
        let tl = build_instructions_lines(blocks, 80, expanded);
        assert_eq!(tl.truncation.any(), expect_truncation);
        let text = lines_text(&tl);
        assert_eq!(text.contains("inst 29"), expect_all_visible);
    }

    #[test]
    fn read_code_tool_lines_exclude_instructions() {
        let msg = read_msg_with_instructions(3, 30);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(
            !text.contains("inst 0"),
            "instruction content should not appear in read tool lines"
        );
    }

    #[test]
    fn instructions_has_highlight_request() {
        let blocks = vec![InstructionBlock {
            path: "agents.md".into(),
            content: "follow style guide".into(),
        }];
        let tl = build_instructions_lines(&blocks, 80, false);
        assert!(tl.highlight.is_some());
        let text = lines_text(&tl);
        assert!(text.contains("follow style guide"));
    }

    #[test]
    fn snapshot_empty_has_no_content_lines() {
        let snapshot = make_snapshot(vec![]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        assert!(
            !lines_text(&tl).contains("plain fallback"),
            "snapshot present means text path should not be used"
        );
        assert_eq!(tl.lines.len(), 1, "only the header line");
    }

    #[test]
    fn snapshot_within_limit_no_truncation() {
        let lines: Vec<Vec<SnapshotSpan>> = (0..3)
            .map(|i| {
                vec![SnapshotSpan {
                    text: format!("row_{i}"),
                    style: SpanStyle::Default,
                }]
            })
            .collect();
        let snapshot = make_snapshot(lines);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("row_0"));
        assert!(text.contains("row_2"));
        assert!(!text.contains(TRUNCATION_PREFIX));
    }

    #[test]
    fn snapshot_search_text_uses_tool_output_not_body() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "visible".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: "index".into(),
            })),
            text: "src/lib.rs\nbody_text_here".into(),
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("llm_output_here".into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        assert!(
            tl.search_text.contains("llm_output_here"),
            "search_text should come from ToolOutput::Plain, not body text"
        );
    }

    #[test]
    fn snapshot_search_text_falls_back_to_body_when_no_plain_output() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "visible".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: "index".into(),
            })),
            text: "header\nbody_fallback".into(),
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            thinking_collapsed: false,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            SectionFlags::default(),
        );
        assert!(
            tl.search_text.contains("body_fallback"),
            "search_text should fall back to msg body when no plain output"
        );
    }

    #[test]
    fn resolve_span_style_inline_all_modifiers() {
        use maki_agent::types::InlineStyle;
        let style = SpanStyle::Inline(InlineStyle {
            fg: Some((10, 20, 30)),
            bg: Some((40, 50, 60)),
            bold: true,
            italic: true,
            underline: true,
            dim: true,
            strikethrough: true,
            reversed: true,
        });
        let resolved = resolve_span_style(&style);
        assert_eq!(resolved.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(resolved.bg, Some(Color::Rgb(40, 50, 60)));
        use ratatui::style::Modifier;
        assert!(resolved.add_modifier.contains(Modifier::BOLD));
        assert!(resolved.add_modifier.contains(Modifier::ITALIC));
        assert!(resolved.add_modifier.contains(Modifier::UNDERLINED));
        assert!(resolved.add_modifier.contains(Modifier::DIM));
        assert!(resolved.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(resolved.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn default_span_resolves_to_theme_tool() {
        let _guard = crate::theme::THEME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        theme::set(theme::load_by_name("dracula").expect("dracula theme"));
        assert_eq!(
            resolve_span_style(&SpanStyle::Default),
            theme::current().tool
        );
    }

    #[test]
    fn snapshot_to_lines_range_adds_indent_prefix() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "content".into(),
            style: SpanStyle::Default,
        }]]);
        let (lines, _) = snapshot_to_lines_range(&snapshot, ">>", 0..1, "⠋ ");
        assert_eq!(lines.len(), 1);
        let first_span = &lines[0].spans[0];
        assert_eq!(first_span.content.as_ref(), ">>");
    }

    #[test]
    fn snapshot_multi_span_line_preserves_order() {
        let snapshot = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "aaa".into(),
                style: SpanStyle::Default,
            },
            SnapshotSpan {
                text: "bbb".into(),
                style: SpanStyle::Named("dim".into()),
            },
            SnapshotSpan {
                text: "ccc".into(),
                style: SpanStyle::Default,
            },
        ]]);
        let (lines, _) = snapshot_to_lines_range(&snapshot, "", 0..1, "⠋ ");
        let texts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["", "aaa", "bbb", "ccc"]);
    }

    #[test]
    fn spinner_spans_bake_to_frame_and_record_positions() {
        let snapshot = make_snapshot(vec![
            vec![SnapshotSpan {
                text: "plain".into(),
                style: SpanStyle::Default,
            }],
            vec![
                SnapshotSpan {
                    text: "before ".into(),
                    style: SpanStyle::Default,
                },
                SnapshotSpan {
                    text: "· ".into(),
                    style: SpanStyle::Named(SPINNER_STYLE_NAME.into()),
                },
            ],
        ]);
        let (lines, spinners) = snapshot_to_lines_range(&snapshot, "", 0..2, "⠹ ");
        assert_eq!(spinners, vec![(1, 2)]);
        assert_eq!(lines[1].spans[2].content.as_ref(), "⠹ ");
    }
}
