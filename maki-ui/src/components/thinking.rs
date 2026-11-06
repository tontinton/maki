//! Drawing the model's reasoning. The live stream, saved messages and the
//! `/btw` modal all ask this module, so they never disagree on when thinking
//! is collapsed.

use crate::components::tool_display::thinking_style;
use crate::markdown::{
    EXPAND_HINT, MIN_TRUNCATABLE_LINES, TRUNCATION_PREFIX, paint_semantic, should_truncate,
};
use crate::theme;

use maki_config::ThinkingLines;
use maki_markdown::code_block_start;
use maki_markdown::render::{Line as RLine, Renderer};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use std::iter;

/// How many rows collapsed thinking keeps under its header, or `None` to draw
/// it in full. A click (`expanded`) wins over the config.
pub(crate) fn tail_rows(mode: ThinkingLines, expanded: bool) -> Option<usize> {
    match mode {
        ThinkingLines::Tail(rows) if !expanded => Some(rows),
        _ => None,
    }
}

/// Only drawing moves a typewriter forward, and the header-only view never
/// draws the text. Waiting on that reveal anyway would spin the loop at full
/// frame rate for as long as the model thinks.
pub(crate) fn reveals_text(mode: ThinkingLines, expanded: bool) -> bool {
    tail_rows(mode, expanded) != Some(0)
}

/// A click closes what a click opened, and opens what is drawn collapsed.
/// Thinking short enough to fit its tail is never collapsed, so just like a
/// short tool result it ignores clicks.
pub(crate) fn click_toggles(expanded: bool, drawn_collapsed: bool) -> bool {
    expanded || drawn_collapsed
}

/// The header and tail that stand in for the thinking, or `None` to draw it in
/// full, which is also what `tail` answers when the cut would hide too little.
/// With zero rows `tail` is never called, so the header-only view never ticks
/// a typewriter it does not show.
pub(crate) fn collapsed_view(
    rows: Option<usize>,
    line_count: usize,
    expandable: bool,
    tail: impl FnOnce(usize) -> Option<Vec<Line<'static>>>,
) -> Option<Vec<Line<'static>>> {
    let tail = match rows? {
        0 => Vec::new(),
        rows => tail(rows)?,
    };
    Some(
        iter::once(header(line_count, expandable))
            .chain(tail)
            .collect(),
    )
}

pub(crate) fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|&b| b == b'\n').count() + 1
    }
}

fn header(line_count: usize, expandable: bool) -> Line<'static> {
    let style = thinking_style();
    let mut notice = format!("{TRUNCATION_PREFIX} ({line_count} lines)");
    if expandable {
        notice.push(' ');
        notice.push_str(EXPAND_HINT);
    }
    Line::from(vec![
        Span::styled(style.label, style.label_style()),
        Span::styled(notice, theme::current().tool_dim),
    ])
}

/// The tail of a live stream. When the cut lands in a code block the tail
/// starts at its fence and stays there while the block grows, so a streaming
/// renderer can highlight just the new lines instead of the whole block on
/// every frame. That only holds while the start stays put, so moving it
/// starts over with a fresh renderer.
pub(crate) struct LiveTail {
    start: usize,
    renderer: Renderer,
}

impl Default for LiveTail {
    fn default() -> Self {
        Self {
            start: 0,
            renderer: Renderer::streaming_wrapped(),
        }
    }
}

impl LiveTail {
    fn render(&mut self, text: &str, start: usize, width: u16) -> Vec<RLine> {
        if start != self.start {
            *self = Self {
                start,
                ..Self::default()
            };
        }
        self.renderer.render(&text[start..], width)
    }
}

/// The last `rows` screen rows of `text`, or `None` when the cut would hide
/// too little to be worth a click. Only the end of the text is rendered, so a
/// long stream stays cheap to draw. One source line can wrap into many rows,
/// so there is no telling up front how much text fills `rows`. We start small
/// and double until it is enough. The header already shows the role prefix,
/// so the rows go without it. Finished text has no `live` tail and goes
/// through the shared block cache instead.
pub(crate) fn render_tail(
    text: &str,
    rows: usize,
    width: u16,
    text_style: Style,
    mut live: Option<&mut LiveTail>,
) -> Option<Vec<Line<'static>>> {
    let mut source_lines = rows.saturating_add(MIN_TRUNCATABLE_LINES);
    loop {
        let start = tail_start(text, source_lines);
        let whole = start == 0;
        let semantic = match live.as_deref_mut() {
            Some(live) => live.render(text, start, width),
            None => Renderer::new().render(&text[start..], width),
        };
        let mut lines = paint_semantic(&semantic, "", text_style, Style::default());
        while lines.last().is_some_and(is_blank) {
            lines.pop();
        }
        let hidden = lines.len().saturating_sub(rows);
        let truncates = should_truncate(hidden);
        if truncates || whole {
            return truncates.then(|| lines.split_off(hidden));
        }
        source_lines = source_lines.saturating_mul(2);
    }
}

/// Where the end of `text` holding its last `min_lines` lines starts. If that
/// is inside a code block it starts at the opening fence instead, so the code
/// still renders as code.
fn tail_start(text: &str, min_lines: usize) -> usize {
    let Some((newline, _)) = text.rmatch_indices('\n').nth(min_lines.saturating_sub(1)) else {
        return 0;
    };
    code_block_start(text, newline + 1)
}

fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;
    const TAIL_ROWS: usize = 3;
    const WIDTH: u16 = 24;
    const MANY: usize = 40;
    const CHUNK_BYTES: usize = 7;

    fn rows_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn tail_text(text: &str) -> Option<Vec<String>> {
        render_tail(text, TAIL_ROWS, WIDTH, Style::default(), None).map(|lines| rows_text(&lines))
    }

    fn streamed_tail_text(text: &str) -> Option<Vec<String>> {
        let mut live = LiveTail::default();
        let mut render = |end| {
            render_tail(
                &text[..end],
                TAIL_ROWS,
                WIDTH,
                Style::default(),
                Some(&mut live),
            )
        };
        for end in (CHUNK_BYTES..text.len()).step_by(CHUNK_BYTES) {
            render(end);
        }
        render(text.len()).map(|lines| rows_text(&lines))
    }

    fn steps(count: usize) -> String {
        (1..=count)
            .map(|n| format!("- step {n:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fenced_code() -> String {
        format!("intro\n```rust\n{}```\nafter", "let a = 1;\n".repeat(MANY))
    }

    fn open_fence() -> String {
        format!("intro\n```rust\n{}", "let a = 1;\n".repeat(MANY))
    }

    /// Rendering only the end of the text must match the bottom of the full
    /// render, or the rows would change when the reader expands them. A live
    /// tail fed in chunks carries highlighter state between frames, so it has
    /// to land on the same rows too.
    #[test_case("word ".repeat(MANY) ; "one_line_wrapping_into_many_rows")]
    #[test_case("word\n".repeat(MANY) ; "lines_joining_into_one_paragraph")]
    #[test_case(steps(MANY) ; "many_short_lines")]
    #[test_case(fenced_code() ; "ends_after_a_code_block")]
    #[test_case(open_fence() ; "ends_inside_a_code_block")]
    fn render_tail_matches_the_bottom_of_the_full_render(text: String) {
        let full = paint_semantic(
            &Renderer::new().render(&text, WIDTH),
            "",
            Style::default(),
            Style::default(),
        );
        let shown = full.len() - full.iter().rev().take_while(|l| is_blank(l)).count();
        let expected = Some(rows_text(&full[shown - TAIL_ROWS..shown]));
        assert_eq!(tail_text(&text), expected);
        assert_eq!(streamed_tail_text(&text), expected);
    }

    #[test_case(TAIL_ROWS + MIN_TRUNCATABLE_LINES - 1 => false ; "too_little_hidden")]
    #[test_case(TAIL_ROWS + MIN_TRUNCATABLE_LINES => true ; "enough_hidden")]
    fn render_tail_truncates_only_past_the_threshold(items: usize) -> bool {
        tail_text(&steps(items)).is_some()
    }
}
