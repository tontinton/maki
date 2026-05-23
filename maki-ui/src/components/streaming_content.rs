use crate::animation::Typewriter;
use crate::highlight::CodeHighlighter;
use crate::markdown::{RenderCtx, RenderState, finalize_lines, render_block};
use maki_markdown::parse;

use ratatui::style::Style;
use ratatui::text::Line;

/// Block-level streaming markdown cache.
///
/// Incremental renderer for a streaming markdown message.
///
/// Re-parses all blocks each frame (parsing is cheap string scanning).
/// Code blocks use `CodeHighlighter` which caches completed lines internally,
/// so repeated `render_block` calls only re-highlight the last (incomplete) line.
///
/// Table column widths are monotonically grown per table (`table_col_widths`)
/// so that adding a wider row never causes earlier rows to shift. Each table
/// in a message gets its own independent width vec to prevent cross-table
/// interference.
#[derive(Default)]
struct StreamingCache {
    byte_len: usize,
    lines: Vec<Line<'static>>,
    highlighters: Vec<CodeHighlighter>,
    table_col_widths: Vec<Vec<usize>>,
}

impl StreamingCache {
    fn invalidate(&mut self) {
        *self = Self::default();
    }

    fn get_or_update(
        &mut self,
        visible: &str,
        prefix: &str,
        text_style: Style,
        prefix_style: Style,
        width: u16,
    ) -> bool {
        let len = visible.len();
        if len == self.byte_len && !self.lines.is_empty() {
            return false;
        }
        self.byte_len = len;

        let text = visible.trim_start_matches('\n');
        let blocks = parse(text);

        self.lines.clear();
        let (code_idx, table_idx) = {
            let mut state = RenderState {
                highlighters: Some(&mut self.highlighters),
                table_col_widths: Some(&mut self.table_col_widths),
                ..RenderState::new()
            };
            let ctx = RenderCtx {
                prefix,
                text_style,
                prefix_style,
                width,
            };
            for block in &blocks {
                render_block(block, &mut self.lines, &mut state, &ctx);
            }
            (state.code_idx, state.table_idx)
        };
        self.highlighters.truncate(code_idx);
        self.table_col_widths.truncate(table_idx);

        finalize_lines(&mut self.lines, prefix, prefix_style);
        true
    }
}

pub(crate) struct StreamingContent {
    typewriter: Typewriter,
    cache: StreamingCache,
    prefix: &'static str,
    text_style: Style,
    prefix_style: Style,
}

impl StreamingContent {
    pub fn new(
        prefix: &'static str,
        text_style: Style,
        prefix_style: Style,
        ms_per_char: u64,
    ) -> Self {
        Self {
            typewriter: Typewriter::with_speed(ms_per_char),
            cache: StreamingCache::default(),
            prefix,
            text_style,
            prefix_style,
        }
    }

    pub fn push(&mut self, text: &str) {
        self.typewriter.push(text);
    }

    pub fn clear(&mut self) {
        self.typewriter.clear();
        self.cache.invalidate();
    }

    pub fn take_all(&mut self) -> String {
        self.cache.invalidate();
        self.typewriter.take_all()
    }

    pub fn is_empty(&self) -> bool {
        self.typewriter.is_empty()
    }

    pub fn is_animating(&self) -> bool {
        self.typewriter.is_animating()
    }

    pub fn set_style(&mut self, prefix: &'static str, text_style: Style, prefix_style: Style) {
        self.prefix = prefix;
        self.text_style = text_style;
        self.prefix_style = prefix_style;
        self.cache.invalidate();
    }

    pub fn render_lines(&mut self, width: u16) -> &[Line<'static>] {
        self.typewriter.tick();
        self.cache.get_or_update(
            self.typewriter.visible(),
            self.prefix,
            self.text_style,
            self.prefix_style,
            width,
        );
        &self.cache.lines
    }

    pub fn cached_lines(&self) -> &[Line<'static>] {
        &self.cache.lines
    }

    #[cfg(test)]
    pub fn set_buffer(&mut self, text: &str) {
        self.typewriter.set_buffer(text);
    }
}

impl PartialEq<&str> for StreamingContent {
    fn eq(&self, other: &&str) -> bool {
        self.typewriter == *other
    }
}

impl std::fmt::Debug for StreamingContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingContent")
            .field("typewriter", &self.typewriter)
            .field("prefix", &self.prefix)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::text_to_lines;
    use ratatui::style::Style;
    use test_case::test_case;

    fn cache_lines_text(cache: &StreamingCache) -> Vec<String> {
        cache
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn full_render_lines(text: &str, prefix: &str, width: u16) -> Vec<String> {
        let style = Style::default();
        let mut hl = Vec::new();
        text_to_lines(text, prefix, style, style, Some(&mut hl), width)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test_case(
        "Hello **bold**\n```rust\nfn main() {}\n```\nAfter code\n- list item",
        "p> "
        ; "single_code_block_with_prefix"
    )]
    #[test_case(
        "text\n```py\nx=1\n```\nmiddle\n```js\ny=2\n```\ntail",
        ""
        ; "multiple_code_blocks"
    )]
    #[test_case(
        "Before table\n\n| Name | Value |\n| --- | --- |\n| foo | 42 |\n| bar | 99 |\n\nAfter table",
        ""
        ; "table_between_paragraphs"
    )]
    #[test_case(
        "| H |\n| --- |\n| d |",
        ""
        ; "table_only"
    )]
    #[test_case(
        "| Tier | Tools | When |\n| --- | --- | --- |\n| Best | code_execution | Chained calls |\n| Good | index | File structure |\n| Costly | read | Full file reads |",
        ""
        ; "table_many_rows"
    )]
    #[test_case(
        "Here is some code:\n```rust\nfn main() {}\n```\n\n| Tier | Tools |\n| --- | --- |\n| Best | code_execution |\n| Good | index |\n| Costly | read |",
        ""
        ; "table_after_code_block"
    )]
    fn streaming_cache_final_matches_full_render(full_text: &str, prefix: &str) {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();

        let step = 7;
        let mut end = step;
        while end <= full_text.len() {
            if !full_text.is_char_boundary(end) {
                end += 1;
                continue;
            }
            cache.get_or_update(&full_text[..end], prefix, style, style, width);
            end += step;
        }

        cache.get_or_update(full_text, prefix, style, style, width);
        let incremental = cache_lines_text(&cache);
        let expected = full_render_lines(full_text, prefix, width);
        assert_eq!(
            incremental, expected,
            "final render mismatch for:\n  {full_text:?}"
        );
    }

    #[test]
    fn incremental_cache_correct_after_content_jump() {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();

        cache.get_or_update("partial text", "", style, style, width);

        let text = "block1\n```py\nx=1\n```\nblock2\n```js\ny=2\n```\ntail";
        cache.get_or_update(text, "", style, style, width);

        let expected = full_render_lines(text, "", width);
        assert_eq!(cache_lines_text(&cache), expected);
    }

    #[test]
    fn invalidate_then_rerender_matches_full() {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();
        let text = "hello\n```rust\nfn x(){}\n```\nafter";
        cache.get_or_update(text, "", style, style, width);
        cache.invalidate();
        cache.get_or_update(text, "", style, style, width);
        assert_eq!(cache_lines_text(&cache), full_render_lines(text, "", width));
    }

    #[test_case(
        "| Name | Value |\n| --- | --- |\n| foo | 42 |",
        "\n| bar | 99 |"
        ; "same_column_count_row"
    )]
    #[test_case(
        "| Col |\n| --- |\n| data |",
        "\n| new | val |"
        ; "row_adds_column_at_pipe_boundary"
    )]
    fn streaming_table_no_line_count_oscillation(base: &str, suffix: &str) {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();

        cache.get_or_update(base, "", style, style, width);
        let mut prev_count = cache.lines.len();

        let chars: Vec<char> = suffix.chars().collect();
        for i in 1..=chars.len() {
            let partial: String = chars[..i].iter().collect();
            let text = format!("{base}{partial}");
            cache.get_or_update(&text, "", style, style, width);
            assert!(
                cache.lines.len() >= prev_count.saturating_sub(1),
                "line count dropped from {prev_count} to {} at partial {partial:?}",
                cache.lines.len()
            );
            prev_count = cache.lines.len();
        }
    }

    #[test]
    fn streaming_table_partial_row_always_in_table() {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();

        let base = "| A | B |\n| --- | --- |\n| 1 | 2 |";
        cache.get_or_update(base, "", style, style, width);
        let base_lines = cache_lines_text(&cache);

        let partial = format!("{base}\n| 3 | in pro");
        cache.get_or_update(&partial, "", style, style, width);
        let partial_lines = cache_lines_text(&cache);
        assert!(
            partial_lines.len() > base_lines.len(),
            "partial row should add lines to the table"
        );
        let has_partial_content = partial_lines.iter().any(|l| l.contains("in pro"));
        assert!(
            has_partial_content,
            "partial cell content should be rendered in table"
        );

        let complete = format!("{base}\n| 3 | in progress |");
        cache.get_or_update(&complete, "", style, style, width);
        let complete_lines = cache_lines_text(&cache);
        let has_complete_content = complete_lines.iter().any(|l| l.contains("in progress"));
        assert!(
            has_complete_content,
            "complete cell content should be rendered"
        );
    }

    #[test]
    fn mutations_invalidate_cache() {
        let style = Style::default();

        let mut sc = StreamingContent::new("", style, style, 4);
        sc.set_buffer("hello world");
        sc.render_lines(80);
        sc.clear();
        assert!(sc.is_empty());
        assert_eq!(sc.cache.byte_len, 0);
        assert!(sc.cache.lines.is_empty());

        sc.set_buffer("hello");
        sc.render_lines(80);
        let text = sc.take_all();
        assert_eq!(text, "hello");
        assert!(sc.is_empty());
        assert_eq!(sc.cache.byte_len, 0);

        let mut sc = StreamingContent::new("old> ", style, style, 4);
        sc.set_buffer("text");
        sc.render_lines(80);
        let new_style = Style::default().fg(ratatui::style::Color::Red);
        sc.set_style("new> ", new_style, new_style);
        assert!(sc.cache.lines.is_empty());
    }
}
