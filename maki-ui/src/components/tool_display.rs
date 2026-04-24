use super::render_hints::{BodyFormat, HeaderStyle, RenderHintsRegistry, ToolRenderHints};
use super::status_bar::format_tokens;
use super::{DisplayMessage, ToolStatus};

use super::code_view;
use crate::animation::spinner_frame;
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

use crate::markdown::{Keep, should_truncate, text_to_lines, truncate_output, truncation_notice};
use maki_agent::{
    BatchToolEntry, BatchToolStatus, BufferSnapshot, InstructionBlock, SpanStyle, ToolInput,
    ToolOutput,
};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::highlight::highlight_regex_inline;
use crate::render_worker::RenderWorker;

pub(crate) struct OutputLimits {
    pub max_lines: usize,
    pub keep: Keep,
}

pub(crate) fn output_limits_from_hints(
    name: &str,
    hints: &ToolRenderHints,
    tol: &ToolOutputLines,
) -> OutputLimits {
    let config_value = tol.get(name);
    let max_lines = match hints.truncate_lines {
        Some(hint) if config_value == tol.other => hint,
        _ => config_value,
    };
    OutputLimits {
        max_lines,
        keep: hints.truncate_at.into(),
    }
}

pub const TOOL_INDICATOR: &str = "● ";
pub const TOOL_BODY_INDENT: &str = "  ";

const TOOL_SEPARATOR: &str = "──────────────────";
const CODE_EXECUTION_OUTPUT_SEPARATOR: &str = "────────────";
const BASH_WAITING_LABEL: &str = "Waiting for output...";
const BASH_NO_OUTPUT_LABEL: &str = "No output.";
const BASH_OUTPUT_SEPARATOR: &str = "──────";

const BATCH_INDENT: &str = "  ";
const BATCH_CONTENT_INDENT: &str = "    ";

/// Derived from `ToolInput` variant, not render hints.
#[derive(Clone, Copy, PartialEq)]
enum SeparatorStyle {
    None,
    Bash,
    CodeExecution,
}

impl SeparatorStyle {
    fn from_input(input: Option<&ToolInput>) -> Self {
        match input {
            Some(ToolInput::Code { .. }) => Self::Bash,
            Some(ToolInput::Script { .. }) => Self::CodeExecution,
            None => Self::None,
        }
    }
}

pub(crate) fn tool_output_annotation(output: &ToolOutput) -> Option<String> {
    match output {
        ToolOutput::ReadCode {
            lines, total_lines, ..
        } => {
            let shown = lines.len();
            if *total_lines > shown {
                Some(format!("{shown} of {} lines", total_lines))
            } else {
                Some(format!("{shown} lines"))
            }
        }
        ToolOutput::WriteCode { byte_count, .. } => Some(format!("{byte_count} bytes")),
        ToolOutput::MemoryWrite { lines, .. } | ToolOutput::MemoryRead { lines, .. } => {
            Some(format!("{} lines", lines.len()))
        }
        ToolOutput::GrepResult { entries } => {
            let matches: usize = entries.iter().map(|e| e.match_count()).sum();
            let files = entries.len();
            let f = if files == 1 { "file" } else { "files" };
            Some(format!("{matches} matches in {files} {f}"))
        }
        ToolOutput::GlobResult { files } if !files.is_empty() => {
            Some(format!("{} files", files.len()))
        }
        ToolOutput::ReadDir { text, .. } => {
            let n = text.lines().count();
            Some(format!("{n} entries"))
        }
        ToolOutput::Plain(text) => {
            let n = text.lines().count();
            Some(format!("{n} lines"))
        }
        _ => None,
    }
}

fn extract_path_suffix(s: &str) -> Option<(&str, &str)> {
    let i = s.rfind(" in ")?;
    let path = s[i + 4..].split('"').next().unwrap();
    Some((&s[..i], path))
}

fn style_command_with_path(header: &str) -> Vec<Span<'static>> {
    match extract_path_suffix(header) {
        Some((cmd, path)) => vec![
            Span::styled(format!("{cmd} "), theme::current().tool),
            Span::styled(path.to_owned(), theme::current().tool_path),
        ],
        None => vec![Span::styled(header.to_owned(), theme::current().tool)],
    }
}

fn style_grep_header(header: &str) -> Vec<Span<'static>> {
    let (pattern, rest) = match header.find(" [") {
        Some(i) => (&header[..i], &header[i..]),
        None => match header.rfind(" in ") {
            Some(i) => (&header[..i], &header[i..]),
            None => (header, ""),
        },
    };

    let mut spans = highlight_regex_inline(pattern);

    let after_pattern = if let Some(bracket_end) = rest.find(']') {
        let filter = &rest[..bracket_end + 1];
        spans.push(Span::styled(
            filter.to_owned(),
            theme::current().tool_annotation,
        ));
        &rest[bracket_end + 1..]
    } else {
        rest
    };

    if let Some((_, path)) = extract_path_suffix(after_pattern) {
        spans.push(Span::styled(format!(" {path}"), theme::current().tool_path));
    }

    spans
}

fn style_tool_header(header_style: &HeaderStyle, header: &str) -> Vec<Span<'static>> {
    match header_style {
        HeaderStyle::Path => {
            vec![Span::styled(header.to_owned(), theme::current().tool_path)]
        }
        HeaderStyle::Command => style_command_with_path(header),
        HeaderStyle::Grep => style_grep_header(header),
        HeaderStyle::Plain => vec![Span::styled(header.to_owned(), theme::current().tool)],
    }
}

pub struct RoleStyle {
    pub prefix: &'static str,
    pub text_style: Style,
    pub prefix_style: Style,
    pub use_markdown: bool,
}

pub fn assistant_style() -> RoleStyle {
    RoleStyle {
        prefix: "maki> ",
        text_style: theme::current().assistant,
        prefix_style: theme::current().assistant_prefix,
        use_markdown: true,
    }
}

pub fn user_style() -> RoleStyle {
    RoleStyle {
        prefix: "you> ",
        text_style: theme::current().assistant,
        prefix_style: theme::current().user,
        use_markdown: true,
    }
}

pub fn thinking_style() -> RoleStyle {
    RoleStyle {
        prefix: "thinking> ",
        text_style: theme::current().thinking,
        prefix_style: theme::current().thinking,
        use_markdown: true,
    }
}

pub fn error_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current().error,
        prefix_style: theme::current().tool_error,
        use_markdown: false,
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
    }
}

pub struct ToolLines {
    pub lines: Vec<Line<'static>>,
    pub search_text: String,
    pub highlight: Option<HighlightRequest>,
    pub spinner_lines: Vec<usize>,
    pub content_indent: &'static str,
    pub truncation: SectionFlags,
    pub separator_line: Option<usize>,
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
            | ToolOutput::MemoryWrite { .. }
            | ToolOutput::MemoryRead { .. }
            | ToolOutput::Instructions { .. } => Some(o),
            ToolOutput::Plain(_)
            | ToolOutput::ReadDir { .. }
            | ToolOutput::TodoList(_)
            | ToolOutput::Batch { .. }
            | ToolOutput::GlobResult { .. }
            | ToolOutput::QuestionAnswers(_) => None,
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

pub fn format_turn_usage(usage: &TokenUsage, pricing: &ModelPricing) -> String {
    let cost = usage.cost(pricing);
    format!(
        "{}↑ {}↓ ${cost:.3}",
        format_tokens(usage.total_input()),
        format_tokens(usage.output),
    )
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
        usage.map_or(0, UnicodeWidthStr::width) + timestamp.map_or(0, str::len) + separator;
    let header_width: usize = line.spans.iter().map(|s| s.content.len()).sum();
    let w = width as usize + 1;
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
    Pending,
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

impl From<BatchToolStatus> for Indicator {
    fn from(s: BatchToolStatus) -> Self {
        match s {
            BatchToolStatus::Pending => Self::Pending,
            BatchToolStatus::InProgress => Self::InProgress,
            BatchToolStatus::Success => Self::Success,
            BatchToolStatus::Error => Self::Error,
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
    keep: Keep,
) -> ResolvedOutput<'a> {
    if let Some(ToolOutput::Batch { .. } | ToolOutput::QuestionAnswers(_)) = output {
        return ResolvedOutput {
            text: None,
            full_text: None,
            skipped: 0,
        };
    }

    let full_text: Option<Cow<'a, str>> = match output {
        Some(ToolOutput::Plain(t)) => Some(Cow::Borrowed(t.as_str())),
        Some(ToolOutput::ReadDir { text, .. }) => Some(Cow::Borrowed(text.as_str())),
        Some(o @ ToolOutput::GlobResult { .. }) => Some(Cow::Owned(o.as_display_text())),
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
            let tr = truncate_output(&t, limits.output, keep);
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
    spinner_lines: Vec<usize>,
    content_range: (usize, usize),
    width: u16,
    outer_indent: &'static str,
    truncation: SectionFlags,
    separator_line: Option<usize>,
    limits: RenderLimits,
    keep: Keep,
    header_style: HeaderStyle,
    body_format: BodyFormat,
    separator_style: SeparatorStyle,
}

impl ToolLineBuilder {
    fn new(
        width: u16,
        outer_indent: &'static str,
        expanded: SectionFlags,
        output_limits: OutputLimits,
        hints: &ToolRenderHints,
    ) -> Self {
        let limits = RenderLimits::new(expanded, output_limits.max_lines);
        Self {
            lines: Vec::new(),
            search_text: String::new(),
            spinner_lines: Vec::new(),
            content_range: (0, 0),
            width: width.saturating_sub(outer_indent.len() as u16),
            outer_indent,
            truncation: SectionFlags::default(),
            separator_line: None,
            limits,
            keep: output_limits.keep,
            header_style: hints.header_style,
            body_format: hints.body_format,
            separator_style: SeparatorStyle::None,
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
                for span in &first_line.spans {
                    spans.push(Span::styled(
                        span.text.clone(),
                        resolve_span_style(&span.style),
                    ));
                }
            }
        } else {
            spans.extend(style_tool_header(&self.header_style, header));
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
        let (text, style) = match indicator {
            Indicator::Pending => ("○ ".into(), theme::current().tool_dim),
            Indicator::InProgress => {
                self.spinner_lines.push(0);
                let ch = spinner_frame(started_at.elapsed().as_millis());
                (format!("{ch} "), theme::current().spinner)
            }
            Indicator::Success => (TOOL_INDICATOR.into(), theme::current().tool_success),
            Indicator::Error => (TOOL_INDICATOR.into(), theme::current().tool_error),
        };
        if self.lines.is_empty() {
            return;
        }
        self.lines[0].spans.insert(0, Span::styled(text, style));
    }

    fn push_code_content(&mut self, input: Option<&ToolInput>, output: Option<&ToolOutput>) {
        let content = code_view::render_tool_content(input, output, false, self.limits);
        self.truncation.script |= content.truncation.script;
        self.truncation.output |= content.truncation.output;
        let start = self.lines.len();
        self.separator_line = content.separator_line.map(|l| start + l);
        for mut line in content.lines {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
        self.content_range = (start, self.lines.len());
        self.separator_style = SeparatorStyle::from_input(input);
        if let Some(ToolInput::Code { code, .. }) = input {
            self.push_search_text(code.trim_end());
        }
        if let Some(text) = output.and_then(|o| o.structured_display_text()) {
            self.push_search_text(&text);
        }
    }

    fn push_resolved_output(&mut self, resolved: &ResolvedOutput<'_>, is_done: bool) {
        let has_code = self.content_range.1 > self.content_range.0;

        if resolved.text.is_none() {
            if has_code && self.separator_style == SeparatorStyle::Bash {
                self.push_bash_output_label(TOOL_BODY_INDENT, is_done, false);
            }
            return;
        }

        if has_code {
            if self.separator_style == SeparatorStyle::Bash {
                self.push_bash_output_label(TOOL_BODY_INDENT, is_done, resolved.text.is_some());
            } else if resolved.text.is_some() {
                self.push_code_output_separator(TOOL_BODY_INDENT);
            }
        }

        if let Some(text) = &resolved.text {
            if matches!(self.keep, Keep::Tail) {
                self.push_truncation_count(resolved.skipped);
            }
            match self.body_format {
                BodyFormat::Markdown => self.push_markdown_body(text),
                BodyFormat::Plain => push_text_lines(&mut self.lines, text, TOOL_BODY_INDENT),
            }
            if let Some(full) = &resolved.full_text {
                self.push_search_text(full);
            } else {
                self.push_search_text(text);
            }
            if matches!(self.keep, Keep::Head) {
                self.push_truncation_count(resolved.skipped);
            }
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
            None,
            self.width.saturating_sub(indent),
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

    fn push_bash_output_label(&mut self, indent: &str, is_done: bool, has_output: bool) {
        self.separator_line = Some(self.lines.len());
        self.lines.push(Line::from(Span::styled(
            format!("{indent}{BASH_OUTPUT_SEPARATOR}"),
            theme::current().tool_dim,
        )));
        let label = match (has_output, is_done) {
            (true, _) => return,
            (false, false) => BASH_WAITING_LABEL,
            (false, true) => BASH_NO_OUTPUT_LABEL,
        };
        self.lines.push(Line::from(Span::styled(
            format!("{indent}{label}"),
            theme::current().tool_dim,
        )));
    }

    fn push_snapshot(&mut self, snapshot: &BufferSnapshot, search_fallback: Option<&str>) {
        let total = snapshot.lines.len();
        let max = self.limits.output;
        let start = self.lines.len();
        let (range, skipped) = if total > max {
            let skipped = total - max;
            match self.keep {
                Keep::Tail => (skipped..total, skipped),
                Keep::Head => (0..max, skipped),
            }
        } else {
            (0..total, 0)
        };
        if matches!(self.keep, Keep::Tail) {
            self.push_truncation_count(skipped);
        }
        self.lines
            .extend(snapshot_to_lines_range(snapshot, TOOL_BODY_INDENT, range));
        if matches!(self.keep, Keep::Head) {
            self.push_truncation_count(skipped);
        }
        self.content_range = (start, self.lines.len());
        if let Some(text) = search_fallback {
            self.push_search_text(text);
        }
    }

    fn push_code_output_separator(&mut self, indent: &str) {
        if self.separator_style == SeparatorStyle::CodeExecution {
            self.separator_line = Some(self.lines.len());
            self.lines.push(Line::from(Span::styled(
                format!("{indent}{}", CODE_EXECUTION_OUTPUT_SEPARATOR),
                theme::current().tool_dim,
            )));
        }
    }

    fn prepend_separator(&mut self, index: usize) {
        if index == 0 {
            return;
        }
        let sep = [
            Line::default(),
            Line::from(Span::styled(TOOL_SEPARATOR, theme::current().tool_dim)),
            Line::default(),
        ];
        self.lines.splice(0..0, sep);
        self.spinner_lines.iter_mut().for_each(|l| *l += 3);
        self.content_range.0 += 3;
        self.content_range.1 += 3;
        self.search_text.insert(0, '\n');
    }

    fn finish(
        mut self,
        input: Option<Arc<ToolInput>>,
        output: Option<Arc<ToolOutput>>,
        content_indent: &'static str,
    ) -> ToolLines {
        if !self.outer_indent.is_empty() {
            for line in &mut self.lines {
                line.spans.insert(0, Span::raw(self.outer_indent));
            }
        }
        let highlight = HighlightRequest::new(self.content_range, input, output, self.limits);
        ToolLines {
            lines: self.lines,
            search_text: self.search_text,
            highlight,
            spinner_lines: self.spinner_lines,
            content_indent,
            truncation: self.truncation,
            separator_line: self.separator_line,
        }
    }
}

fn push_text_lines(lines: &mut Vec<Line<'static>>, text: &str, indent: &str) {
    let style = theme::current().tool;
    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::styled(indent.to_owned(), style),
            Span::styled(line.to_owned(), style),
        ]));
    }
}

fn snapshot_to_lines_range(
    snapshot: &BufferSnapshot,
    indent: &str,
    range: std::ops::Range<usize>,
) -> Vec<Line<'static>> {
    snapshot.lines[range]
        .iter()
        .map(|sline| {
            let mut spans = vec![Span::raw(indent.to_string())];
            for span in &sline.spans {
                spans.push(Span::styled(
                    span.text.clone(),
                    resolve_span_style(&span.style),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn resolve_span_style(style: &SpanStyle) -> Style {
    match style {
        SpanStyle::Default => Style::default(),
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
    started_at: Instant,
    width: u16,
    expanded: SectionFlags,
    tool_output_lines: &ToolOutputLines,
    registry: &RenderHintsRegistry,
) -> ToolLines {
    let tool_name = msg.role.tool_name().unwrap_or("?");
    let hints = registry.get(tool_name);
    let limits = output_limits_from_hints(tool_name, hints, tool_output_lines);
    let (header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };

    let mut b = ToolLineBuilder::new(width, "", expanded, limits, hints);
    b.push_header(
        tool_name,
        header,
        msg.annotation.as_deref(),
        msg.render_header.as_ref(),
    );
    b.prepend_indicator(status.into(), started_at);
    b.push_code_content(msg.tool_input.as_deref(), msg.tool_output.as_deref());
    if let Some(ref snapshot) = msg.render_snapshot {
        let search_text = msg
            .tool_output
            .as_ref()
            .and_then(|o| match o.as_ref() {
                ToolOutput::Plain(t) => Some(t.as_str()),
                _ => None,
            })
            .or(body);
        b.push_snapshot(snapshot, search_text);
    } else {
        let is_done = status != ToolStatus::InProgress;
        let resolved = resolve_output(
            msg.tool_output.as_deref(),
            body,
            msg.live_output.as_deref(),
            msg.truncated_lines,
            b.limits,
            b.keep,
        );
        b.push_resolved_output(&resolved, is_done);
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

#[allow(clippy::too_many_arguments)]
pub fn build_batch_entry_lines(
    entry: &BatchToolEntry,
    index: usize,
    started_at: Instant,
    width: u16,
    expanded: SectionFlags,
    tool_output_lines: &ToolOutputLines,
    registry: &RenderHintsRegistry,
    snapshot: Option<&BufferSnapshot>,
    header_snapshot: Option<&BufferSnapshot>,
) -> ToolLines {
    let hints = registry.get(&entry.tool);
    let limits = output_limits_from_hints(&entry.tool, hints, tool_output_lines);
    let mut annotation = entry.annotation.clone();
    if let Some(suffix) = entry.output.as_ref().and_then(tool_output_annotation) {
        append_annotation(&mut annotation, &suffix);
    }

    let mut b = ToolLineBuilder::new(width, BATCH_INDENT, expanded, limits, hints);
    b.push_header(
        &entry.tool,
        &entry.summary,
        annotation.as_deref(),
        header_snapshot,
    );
    b.prepend_indicator(entry.status.into(), started_at);
    b.push_code_content(entry.input.as_ref(), entry.output.as_ref());
    let is_done = matches!(
        entry.status,
        BatchToolStatus::Success | BatchToolStatus::Error
    );
    if let Some(snap) = snapshot {
        let search_text = entry.output.as_ref().and_then(|o| match o {
            ToolOutput::Plain(t) => Some(t.as_str()),
            _ => None,
        });
        b.push_snapshot(snap, search_text);
    } else {
        let resolved = resolve_output(entry.output.as_ref(), None, None, 0, b.limits, b.keep);
        b.push_resolved_output(&resolved, is_done);
    }
    b.prepend_separator(index);
    b.finish(
        entry.input.clone().map(Arc::new),
        entry.output.clone().map(Arc::new),
        BATCH_CONTENT_INDENT,
    )
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
    batch_index: Option<usize>,
) -> ToolLines {
    let in_batch = batch_index.is_some();
    let (outer_indent, content_indent) = if in_batch {
        (BATCH_INDENT, BATCH_CONTENT_INDENT)
    } else {
        ("", TOOL_BODY_INDENT)
    };

    let header = blocks.first().map_or("", |b| b.path.as_str());
    let annotation = if blocks.len() > 1 {
        Some(format!("+{}", blocks.len() - 1))
    } else {
        None
    };

    let limits = OutputLimits {
        max_lines: code_view::instruction_limit(expanded),
        keep: Keep::Tail,
    };
    let exp = SectionFlags {
        script: false,
        output: expanded,
    };
    let mut b = ToolLineBuilder::new(
        width,
        outer_indent,
        exp,
        limits,
        &ToolRenderHints::default(),
    );
    b.push_header("load", header, annotation.as_deref(), None);
    b.prepend_indicator(Indicator::Success, Instant::now());

    let start = b.lines.len();
    let has_truncation =
        code_view::render_instructions(blocks, &mut b.lines, b.limits.output, false);
    b.truncation.output |= has_truncation;
    let inner_indent = &content_indent[outer_indent.len()..];
    for line in &mut b.lines[start..] {
        line.spans.insert(0, Span::raw(inner_indent));
    }
    b.content_range = (start, b.lines.len());

    b.push_search_text(
        &blocks
            .iter()
            .map(|bl| bl.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    if let Some(idx) = batch_index {
        b.prepend_separator(idx);
    }
    let output = Arc::new(ToolOutput::Instructions {
        blocks: blocks.to_vec(),
    });
    b.finish(None, Some(output), content_indent)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: ToolOutputLines = ToolOutputLines::DEFAULT;
    use crate::components::render_hints::RenderHintsRegistry;
    use crate::components::{DisplayRole, ToolRole};
    use crate::markdown::TRUNCATION_PREFIX;
    use maki_agent::tools::{BASH_TOOL_NAME, READ_TOOL_NAME, TASK_TOOL_NAME};
    use maki_agent::{
        BatchToolEntry, BatchToolStatus, GrepFileEntry, GrepMatchGroup, SnapshotLine, SnapshotSpan,
        ToolInput, ToolOutput,
    };
    use test_case::test_case;

    fn reg() -> RenderHintsRegistry {
        RenderHintsRegistry::new()
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
            tool_output: output.map(Arc::new),
            live_output: None,
            annotation: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
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
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        assert_eq!(tl.highlight.is_some(), expect_highlight);
        if let Some(hl) = &tl.highlight {
            assert_eq!(hl.output.is_some(), expect_output);
        }
    }

    fn spans_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn has_styled_span(spans: &[Span<'_>], text: &str, style: Style) -> bool {
        spans
            .iter()
            .any(|s| s.content.contains(text) && s.style == style)
    }

    #[test]
    fn style_tool_header_path_first() {
        let spans = style_tool_header(&HeaderStyle::Path, "src/main.rs");
        assert_eq!(spans_text(&spans), "src/main.rs");
    }

    #[test]
    fn style_tool_header_in_path() {
        let spans = style_tool_header(&HeaderStyle::Command, "echo hi in /tmp");
        let text = spans_text(&spans);
        assert!(text.contains("echo hi"));
        assert!(has_styled_span(&spans, "/tmp", theme::current().tool_path));
    }

    #[test]
    fn style_tool_header_truncates_json_in_path() {
        let spans = style_tool_header(
            &HeaderStyle::Grep,
            "STRIKETHROUGH_STYLE in /home/tony/c/maki2\", \"pattern\": \"STRIKETHROUGH_STYLE\"}",
        );
        let text = spans_text(&spans);
        assert!(text.contains("STRIKETHROUGH_STYLE"));
        assert!(text.contains("/home/tony/c/maki2"));
        assert!(!text.contains("pattern"));
    }

    #[test_case("TODO",                       "TODO"                        ; "pattern_only")]
    #[test_case("TODO [*.rs]",                "TODO [*.rs]"                 ; "with_include")]
    #[test_case("TODO in src/",               "TODO src/"                ; "with_path")]
    #[test_case("\\b(fn|pub)\\s+ [*.rs] in src/", "\\b(fn|pub)\\s+ [*.rs] src/" ; "with_include_and_path")]
    fn grep_header_text_roundtrips(input: &str, expected: &str) {
        assert_eq!(spans_text(&style_grep_header(input)), expected);
    }

    #[test]
    fn grep_header_styles_filter_and_path() {
        let spans = style_grep_header("TODO [*.rs] in src/");
        assert!(has_styled_span(
            &spans,
            "[*.rs]",
            theme::current().tool_annotation
        ));
        assert!(has_styled_span(&spans, "src/", theme::current().tool_path));
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
        let tl = build_tool_lines(
            &msg,
            status,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    fn line_has_styled(tl: &ToolLines, text: &str, style: Style) -> bool {
        tl.lines
            .iter()
            .any(|l| has_styled_span(&l.spans, text, style))
    }

    #[test_case(code_input(), true  ; "shown_with_code_input")]
    #[test_case(None,         false ; "hidden_without_code_input")]
    fn bash_separator(input: Option<ToolInput>, expected: bool) {
        let msg = bash_msg("echo hi\nhello", ToolStatus::Success, input, plain_output());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        assert_eq!(
            line_has_styled(&tl, BASH_OUTPUT_SEPARATOR, theme::current().tool_dim),
            expected,
        );
    }

    #[test_case(ToolStatus::InProgress, None,                                     BASH_WAITING_LABEL   ; "waiting_when_in_progress")]
    #[test_case(ToolStatus::Success,    Some(ToolOutput::Plain(String::new())),    BASH_NO_OUTPUT_LABEL ; "no_output_when_done_empty")]
    fn bash_status_label(status: ToolStatus, output: Option<ToolOutput>, label: &str) {
        let msg = bash_msg("echo hi", status, code_input(), output);
        let tl = build_tool_lines(
            &msg,
            status,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        assert!(line_has_styled(&tl, label, theme::current().tool_dim));
    }

    #[test]
    fn batch_bash_separator_between_code_and_output() {
        let entry = batch_entry(
            "bash",
            BatchToolStatus::Success,
            code_input(),
            Some(ToolOutput::Plain("hello".into())),
        );
        let tl = batch_lines(&entry, 0);
        assert!(line_has_styled(
            &tl,
            BASH_OUTPUT_SEPARATOR,
            theme::current().tool_dim
        ));
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

    fn batch_entry(
        tool: &str,
        status: BatchToolStatus,
        input: Option<ToolInput>,
        output: Option<ToolOutput>,
    ) -> BatchToolEntry {
        BatchToolEntry {
            tool: tool.into(),
            summary: "test".into(),
            status,
            input,
            output,
            annotation: None,
        }
    }

    fn batch_lines(entry: &BatchToolEntry, index: usize) -> ToolLines {
        build_batch_entry_lines(
            entry,
            index,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
            None,
            None,
        )
    }

    #[test_case(80, true  ; "shown_when_width_sufficient")]
    #[test_case(10, false ; "hidden_when_too_narrow")]
    fn append_right_info_timestamp_visibility(width: u16, expect_timestamp: bool) {
        let msg = tool_msg();
        let mut tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
    fn batch_entry_annotation_rendered() {
        let mut entry = batch_entry(
            "read",
            BatchToolStatus::Success,
            None,
            Some(ToolOutput::ReadCode {
                path: "src/main.rs".into(),
                start_line: 1,
                lines: vec!["x".into(); 42],
                total_lines: 42,
                instructions: None,
            }),
        );
        entry.summary = "src/main.rs".into();
        let tl = batch_lines(&entry, 0);
        assert!(lines_text(&tl).contains("(42 lines)"));
    }

    #[test]
    fn batch_entry_code_input_rendered() {
        let entry = batch_entry("bash", BatchToolStatus::Success, code_input(), None);
        let tl = batch_lines(&entry, 0);
        assert!(lines_text(&tl).contains("echo hi"));
    }

    #[test_case(BatchToolStatus::InProgress, &[0]    ; "in_progress_has_spinner")]
    #[test_case(BatchToolStatus::Pending,    &[]     ; "pending_no_spinner")]
    #[test_case(BatchToolStatus::Success,    &[]     ; "success_no_spinner")]
    fn batch_entry_spinner(status: BatchToolStatus, expected: &[usize]) {
        let entry = batch_entry("bash", status, None, None);
        let tl = batch_lines(&entry, 0);
        assert_eq!(tl.spinner_lines, expected);
    }

    #[test]
    fn batch_entry_separator_on_nonzero_index() {
        let entry = batch_entry("bash", BatchToolStatus::Success, None, None);
        let first = batch_lines(&entry, 0);
        let second = batch_lines(&entry, 1);
        assert!(second.lines.len() > first.lines.len());
        assert!(spans_text(&second.lines[1].spans).contains(TOOL_SEPARATOR));
    }

    #[test]
    fn batch_entry_plain_output_rendered() {
        let entry = batch_entry(
            "bash",
            BatchToolStatus::Success,
            None,
            Some(ToolOutput::Plain("hello world".into())),
        );
        let tl = batch_lines(&entry, 0);
        assert!(lines_text(&tl).contains("hello world"));
    }

    #[test]
    fn annotation_rendered_on_header() {
        let mut msg = tool_msg();
        msg.annotation = Some("2m timeout".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("(2m timeout)"));
    }

    #[test]
    fn batch_entry_stored_annotation_rendered() {
        let mut entry = batch_entry("task", BatchToolStatus::Success, None, None);
        entry.annotation = Some("anthropic/claude-haiku-4-20250414".into());
        let tl = batch_lines(&entry, 0);
        assert!(lines_text(&tl).contains("(anthropic/claude-haiku-4-20250414)"));
    }

    #[test_case("bash",  ToolOutput::Plain("ok".into()),                      Some("1 lines")     ; "plain_short_annotates")]
    #[test_case("bash",  ToolOutput::Plain((0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")), Some("20 lines") ; "plain_long_annotates")]
    #[test_case("read",  ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec!["x".into(); 5], total_lines: 5, instructions: None }, Some("5 lines") ; "read_code_full_file")]
    #[test_case("read",  ToolOutput::ReadCode { path: "a.rs".into(), start_line: 10, lines: vec!["x".into(); 5], total_lines: 100, instructions: None }, Some("5 of 100 lines") ; "read_code_partial")]
    #[test_case("write", ToolOutput::WriteCode { path: "a.rs".into(), byte_count: 99, lines: vec![] }, Some("99 bytes") ; "write_code_bytes")]
    #[test_case("grep",  ToolOutput::GrepResult { entries: vec![GrepFileEntry { path: "a.rs".into(), groups: vec![GrepMatchGroup::single(1, "hit")] }] }, Some("1 matches in 1 file") ; "grep_file_count")]
    #[test_case("glob",  ToolOutput::GlobResult { files: vec!["a".into(), "b".into()] }, Some("2 files") ; "glob_file_count")]
    #[test_case("glob",  ToolOutput::GlobResult { files: vec![] },            None                ; "glob_empty_no_annotation")]
    #[test_case("edit",  ToolOutput::Diff { path: "a.rs".into(), before: String::new(), after: String::new(), summary: "ok".into() }, None ; "diff_no_annotation")]
    fn annotation_cases(_tool: &str, output: ToolOutput, expected: Option<&str>) {
        assert_eq!(tool_output_annotation(&output).as_deref(), expected);
    }

    #[test]
    fn task_output_body_visible() {
        let msg = task_msg("**bold** and `code`".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            tool_output: Some(Arc::new(ToolOutput::Plain(output))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
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
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            Instant::now(),
            width,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        assert_hr_fits(&tl, width);
    }

    #[test]
    fn batch_task_hr_fits_within_width() {
        let width: u16 = 60;
        let entry = batch_entry(
            TASK_TOOL_NAME,
            BatchToolStatus::Success,
            None,
            Some(ToolOutput::Plain("before\n\n---\n\nafter".into())),
        );
        let tl = build_batch_entry_lines(
            &entry,
            0,
            Instant::now(),
            width,
            SectionFlags::default(),
            &TOL,
            &reg(),
            None,
            None,
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
            tool_output: Some(Arc::new(ToolOutput::Plain(body.to_owned()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
        }
    }

    #[test]
    fn index_output_truncated_at_max_lines() {
        let body: String = (0..150).map(|i| format!("  line_{i}\n")).collect();
        let msg = index_msg(&body);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            tool_output: Some(Arc::new(ToolOutput::Plain("plain fallback".into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
        }
    }

    fn make_snapshot(lines: Vec<Vec<SnapshotSpan>>) -> BufferSnapshot {
        BufferSnapshot {
            lines: lines
                .into_iter()
                .map(|spans| SnapshotLine { spans })
                .collect(),
        }
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
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("from_snapshot"));
        assert!(!text.contains("plain fallback"));
        assert!(
            tl.search_text.contains("plain fallback"),
            "search_text should contain tool output for Ctrl+F"
        );
    }

    #[test]
    fn snapshot_truncation_head() {
        let lines: Vec<Vec<SnapshotSpan>> = (0..150)
            .map(|i| {
                vec![SnapshotSpan {
                    text: format!("line_{i}"),
                    style: SpanStyle::Default,
                }]
            })
            .collect();
        let snapshot = make_snapshot(lines);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
        );
        let text = lines_text(&tl);
        assert!(text.contains("line_0"));
        assert!(!text.contains("line_149"));
        assert!(text.contains(TRUNCATION_PREFIX));
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
        Some(ToolOutput::Plain(String::new())), None, "bash", false
        ; "empty_plain_resolves_to_none"
    )]
    #[test_case(
        Some(ToolOutput::Batch { entries: vec![], text: String::new() }),
        None, "batch", false
        ; "batch_always_none"
    )]
    #[test_case(
        Some(ToolOutput::ReadDir { text: "dir listing".into(), instructions: None }),
        None, "read", true
        ; "readdir_uses_text_field"
    )]
    #[test_case(
        Some(ToolOutput::GlobResult { files: vec!["a.rs".into()] }),
        None, "glob", true
        ; "glob_uses_display_text"
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
        let r = reg();
        let hints = r.get(tool);
        let ol = output_limits_from_hints(tool, hints, &TOL);
        let limits = RenderLimits::new(SectionFlags::default(), ol.max_lines);
        let resolved = resolve_output(output.as_ref(), body, None, 0, limits, ol.keep);
        assert_eq!(resolved.text.is_some(), expect_text);
    }

    #[test]
    fn resolve_output_pre_truncated_forwarded() {
        let r = reg();
        let hints = r.get("bash");
        let ol = output_limits_from_hints("bash", hints, &TOL);
        let limits = RenderLimits::new(SectionFlags::default(), ol.max_lines);
        let resolved = resolve_output(None, Some("short"), None, 42, limits, ol.keep);
        assert_eq!(resolved.skipped, 42);
    }

    #[test]
    fn resolve_output_truncation_overrides_pre_truncated() {
        let long = n_lines(200);
        let r = reg();
        let hints = r.get("bash");
        let ol = output_limits_from_hints("bash", hints, &TOL);
        let limits = RenderLimits::new(SectionFlags::default(), ol.max_lines);
        let resolved = resolve_output(None, Some(&long), None, 5, limits, ol.keep);
        assert!(resolved.skipped > 5);
    }

    fn bash_output_msg(line_count: usize, live: bool) -> DisplayMessage {
        let full_body = n_lines(line_count);
        let r = reg();
        let hints = r.get("bash");
        let ol = output_limits_from_hints("bash", hints, &TOL);
        let tr = truncate_output(&full_body, ol.max_lines, ol.keep);
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
                Some(Arc::new(ToolOutput::Plain(full_body))),
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
            tool_output,
            live_output,
            annotation: None,
            plan_path: None,
            truncated_lines,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
        }
    }

    #[test]
    fn bash_expanded_live_output() {
        let msg = bash_output_msg(200, true);
        let collapsed = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            Instant::now(),
            80,
            exp(false),
            &TOL,
            &reg(),
        );
        let expanded = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            Instant::now(),
            80,
            exp(true),
            &TOL,
            &reg(),
        );
        let collapsed_text = lines_text(&collapsed);
        let expanded_text = lines_text(&expanded);
        assert!(collapsed.truncation.any());
        assert!(!expanded.truncation.any());
        assert!(!expanded_text.contains(BASH_WAITING_LABEL));
        assert!(expanded_text.contains("line 0"));
        assert!(!collapsed_text.contains("line 0"));
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
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            exp(expanded),
            &TOL,
            &reg(),
        );
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
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            exp(expanded),
            &TOL,
            &reg(),
        );
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
        }
    }

    #[test_case(false, true,  false ; "collapsed_truncates_instructions")]
    #[test_case(true,  false, true  ; "expanded_shows_all_instructions")]
    fn instructions_segment(expanded: bool, expect_truncation: bool, expect_all_visible: bool) {
        let msg = read_msg_with_instructions(3, 30);
        let output = msg.tool_output.as_deref().unwrap();
        let blocks = output.instructions().unwrap();
        let tl = build_instructions_lines(blocks, 80, expanded, None);
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
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
        let tl = build_instructions_lines(&blocks, 80, false, None);
        assert!(tl.highlight.is_some());
        let text = lines_text(&tl);
        assert!(text.contains("follow style guide"));
    }

    #[test]
    fn instructions_in_batch_has_indent_and_separator() {
        let blocks = vec![InstructionBlock {
            path: "agents.md".into(),
            content: "follow style guide".into(),
        }];
        let tl = build_instructions_lines(&blocks, 80, false, Some(1));
        let text = lines_text(&tl);
        assert!(text.contains("load> "));
        assert_eq!(tl.content_indent, BATCH_CONTENT_INDENT);
        assert!(
            tl.lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.contains('─'))),
            "batch instruction should have separator"
        );
    }

    #[test]
    fn output_limits_plugin_hint_wins_over_config_other() {
        let hints = ToolRenderHints {
            truncate_lines: Some(50),
            ..Default::default()
        };
        let ol = output_limits_from_hints("my_lua_tool", &hints, &TOL);
        assert_eq!(ol.max_lines, 50);
    }

    #[test]
    fn output_limits_per_tool_config_wins_over_plugin_hint() {
        let hints = ToolRenderHints {
            truncate_lines: Some(50),
            ..Default::default()
        };
        let ol = output_limits_from_hints("bash", &hints, &TOL);
        assert_eq!(ol.max_lines, TOL.bash);
    }

    #[test]
    fn snapshot_truncation_tail() {
        let mut custom_reg = RenderHintsRegistry::new();
        custom_reg.register(
            Arc::from("my_tail_tool"),
            &maki_agent::RawRenderHints {
                truncate_at: Some("tail".into()),
                ..Default::default()
            },
        );
        let lines: Vec<Vec<SnapshotSpan>> = (0..150)
            .map(|i| {
                vec![SnapshotSpan {
                    text: format!("line_{i}"),
                    style: SpanStyle::Default,
                }]
            })
            .collect();
        let snapshot = make_snapshot(lines);
        let msg = DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                status: ToolStatus::Success,
                name: "my_tail_tool".into(),
            })),
            text: "header\nfallback".into(),
            tool_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("fallback".into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &custom_reg,
        );
        let text = lines_text(&tl);
        assert!(
            text.contains("line_149"),
            "tail truncation should show last lines"
        );
        assert!(
            !text.contains("line_0"),
            "tail truncation should hide first lines"
        );
        assert!(text.contains(TRUNCATION_PREFIX));
    }

    #[test]
    fn snapshot_empty_has_no_content_lines() {
        let snapshot = make_snapshot(vec![]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            tool_output: Some(Arc::new(ToolOutput::Plain("llm_output_here".into()))),
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
            tool_output: None,
            live_output: None,
            annotation: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            Instant::now(),
            80,
            SectionFlags::default(),
            &TOL,
            &reg(),
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
    fn resolve_span_style_inline_no_modifiers() {
        use maki_agent::types::InlineStyle;
        let style = SpanStyle::Inline(InlineStyle::default());
        let resolved = resolve_span_style(&style);
        assert_eq!(resolved.fg, None);
        assert_eq!(resolved.bg, None);
        assert_eq!(resolved, Style::default());
    }

    #[test]
    fn snapshot_to_lines_range_adds_indent_prefix() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "content".into(),
            style: SpanStyle::Default,
        }]]);
        let lines = snapshot_to_lines_range(&snapshot, ">>", 0..1);
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
        let lines = snapshot_to_lines_range(&snapshot, "", 0..1);
        let texts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["", "aaa", "bbb", "ccc"]);
    }
}
