use crate::animation::Typewriter;
use crate::markdown::paint_semantic;
use crate::theme;

use maki_markdown::render::Renderer;
use ratatui::style::Style;
use ratatui::text::Line;
use std::hash::{DefaultHasher, Hash, Hasher};

const STREAMING_MAX_LINE_BYTES: usize = 5_000;

/// Block-level streaming markdown cache.
///
/// Memoizes the rendered line tree under a content-addressed key so
/// repaints during streaming are free when nothing changed. The key
/// combines a 64-bit hash of the visible text, its byte length, the
/// render width, and the theme generation. Length is part of the key
/// alongside the hash because hashing alone could in principle collide;
/// requiring both makes accidental reuse on different buffers
/// astronomically unlikely.
#[derive(Default)]
struct StreamingCache {
    key: Option<CacheKey>,
    lines: Vec<Line<'static>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    hash: u64,
    byte_len: usize,
    width: u16,
    theme_gen: u64,
}

impl CacheKey {
    fn for_text(text: &str, width: u16, theme_gen: u64) -> Self {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            byte_len: text.len(),
            width,
            theme_gen,
        }
    }
}

impl StreamingCache {
    fn invalidate(&mut self) {
        self.key = None;
        self.lines.clear();
    }

    /// Returns `true` when the cache was repopulated. The caller passes a
    /// renderer so the highlighter/table-width state persists across calls
    /// for a stable streamed view.
    fn get_or_update(
        &mut self,
        renderer: &mut Renderer,
        visible: &str,
        prefix: &str,
        text_style: Style,
        prefix_style: Style,
        width: u16,
    ) -> bool {
        let key = CacheKey::for_text(visible, width, theme::generation());
        if self.key == Some(key) {
            return false;
        }
        let text = maki_markdown::render::truncate_long_lines_at(visible, STREAMING_MAX_LINE_BYTES);
        let semantic = renderer.render(text.as_ref(), width);
        self.lines = paint_semantic(&semantic, prefix, text_style, prefix_style);
        self.key = Some(key);
        true
    }
}

pub(crate) struct StreamingContent {
    typewriter: Typewriter,
    cache: StreamingCache,
    renderer: Renderer,
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
            renderer: Renderer::streaming(),
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
        self.renderer = Renderer::streaming();
    }

    pub fn take_all(&mut self) -> String {
        self.cache.invalidate();
        self.renderer = Renderer::streaming();
        self.typewriter.take_all()
    }

    pub fn is_empty(&self) -> bool {
        self.typewriter.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.typewriter.buffer_line_count()
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
            &mut self.renderer,
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

    /// A zero character delay makes every `render_lines` reveal the whole
    /// buffer, so nothing here depends on wall-clock timing.
    const INSTANT: u64 = 0;
    const CHUNK_BYTES: usize = 5;
    const TEST_WIDTH: u16 = 80;
    const THEME_A: &str = "dracula";
    const THEME_B: &str = "tokyonight";
    const FIRST_MESSAGE: &str = "intro\n```rust\nfn main() {\n    let x = 42;\n}\n```\ntail";
    /// Code block at the same index as the first message, different language and
    /// a shorter body: exactly the shape a reused `CodeHighlighter` gets wrong.
    const SECOND_MESSAGE: &str = "```python\nx = 1\ny = 2\n```\nend";

    fn lines_text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn cache_lines_text(cache: &StreamingCache) -> Vec<String> {
        lines_text(&cache.lines)
    }

    fn full_render_lines(text: &str, prefix: &str, width: u16) -> Vec<String> {
        let style = Style::default();
        text_to_lines(text, prefix, style, style, width, None)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn fresh_renderer() -> Renderer {
        Renderer::streaming()
    }

    fn stream_message(sc: &mut StreamingContent, text: &str) -> Vec<Line<'static>> {
        let mut sent = 0;
        while sent < text.len() {
            let mut end = (sent + CHUNK_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            sc.push(&text[sent..end]);
            sc.render_lines(TEST_WIDTH);
            sent = end;
        }
        sc.render_lines(TEST_WIDTH).to_vec()
    }

    /// Every render truncates the highlighter vector to the number of code
    /// blocks it saw, so a second message arriving with its fence already
    /// complete is where stale state actually survives.
    fn show_message(sc: &mut StreamingContent, text: &str) -> Vec<Line<'static>> {
        sc.push(text);
        sc.render_lines(TEST_WIDTH).to_vec()
    }

    /// The streaming highlighters are keyed by block position, ignore a language
    /// change, and replay their old spans when the new code is shorter. So a
    /// `StreamingContent` that does not rebuild its renderer on `take_all` or
    /// `clear` paints the previous message's code inside the next one.
    #[test_case(true  ; "take_all")]
    #[test_case(false ; "clear")]
    fn renderer_resets_between_messages(via_take_all: bool) {
        // The default syntect theme paints every scope the same colour, so
        // without a real one a leaked language would not show up in the styles.
        theme::set(theme::load_by_name(THEME_A).expect(THEME_A));
        let style = Style::default();
        let mut sc = StreamingContent::new("", style, style, INSTANT);
        stream_message(&mut sc, FIRST_MESSAGE);
        if via_take_all {
            assert_eq!(sc.take_all(), FIRST_MESSAGE);
        } else {
            sc.clear();
        }
        let reused = show_message(&mut sc, SECOND_MESSAGE);

        let expected = show_message(
            &mut StreamingContent::new("", style, style, INSTANT),
            SECOND_MESSAGE,
        );
        assert_eq!(
            reused, expected,
            "second message must not inherit the first message's highlighter state"
        );
    }

    /// `StreamingCache` keys on `theme::generation()` while the renderer inside
    /// it keys on `maki_highlight::theme_generation()`. The two only agree
    /// because `theme::set` refreshes the syntax theme, and if that coupling
    /// breaks, unchanged text keeps the old palette forever.
    #[test]
    fn theme_change_repaints_identical_text() {
        let style = Style::default();
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();
        theme::set(theme::load_by_name(THEME_A).expect(THEME_A));
        cache.get_or_update(&mut renderer, FIRST_MESSAGE, "", style, style, TEST_WIDTH);
        let before = cache.lines.clone();

        theme::set(theme::load_by_name(THEME_B).expect(THEME_B));
        let rerendered =
            cache.get_or_update(&mut renderer, FIRST_MESSAGE, "", style, style, TEST_WIDTH);

        assert!(rerendered, "a theme change must miss the cache");
        assert_eq!(
            cache_lines_text(&cache),
            lines_text(&before),
            "only the palette changed, the text must not"
        );
        assert_ne!(cache.lines, before, "the repaint must use the new palette");
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
        let mut renderer = fresh_renderer();

        let step = 7;
        let mut end = step;
        while end <= full_text.len() {
            if !full_text.is_char_boundary(end) {
                end += 1;
                continue;
            }
            cache.get_or_update(
                &mut renderer,
                &full_text[..end],
                prefix,
                style,
                style,
                width,
            );
            end += step;
        }

        cache.get_or_update(&mut renderer, full_text, prefix, style, style, width);
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
        let mut renderer = fresh_renderer();

        cache.get_or_update(&mut renderer, "partial text", "", style, style, width);

        let text = "block1\n```py\nx=1\n```\nblock2\n```js\ny=2\n```\ntail";
        cache.get_or_update(&mut renderer, text, "", style, style, width);

        let expected = full_render_lines(text, "", width);
        assert_eq!(cache_lines_text(&cache), expected);
    }

    #[test]
    fn invalidate_then_rerender_matches_full() {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();
        let text = "hello\n```rust\nfn x(){}\n```\nafter";
        cache.get_or_update(&mut renderer, text, "", style, style, width);
        cache.invalidate();
        cache.get_or_update(&mut renderer, text, "", style, style, width);
        assert_eq!(cache_lines_text(&cache), full_render_lines(text, "", width));
    }

    #[test]
    fn cache_invalidates_on_equal_length_content_change() {
        let style = Style::default();
        let width = 80;
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();

        let first = "**bold text**";
        let second = "*italic txt*!";
        assert_eq!(first.len(), second.len());

        cache.get_or_update(&mut renderer, first, "", style, style, width);
        let first_lines = cache_lines_text(&cache);
        assert_eq!(first_lines, full_render_lines(first, "", width));

        cache.get_or_update(&mut renderer, second, "", style, style, width);
        let second_lines = cache_lines_text(&cache);
        assert_eq!(second_lines, full_render_lines(second, "", width));
        assert_ne!(
            first_lines, second_lines,
            "cache must re-render when bytes change at equal length"
        );
    }

    #[test]
    fn cache_invalidates_on_width_change() {
        let style = Style::default();
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();
        let text = "```rust\nfn extremely_long_function_name_that_definitely_will_not_fit(arg_one: &str, arg_two: usize) {}\n```";

        cache.get_or_update(&mut renderer, text, "", style, style, 200);
        let wide = cache.lines.len();
        cache.get_or_update(&mut renderer, text, "", style, style, 30);
        let narrow = cache.lines.len();
        assert!(
            narrow > wide,
            "narrower width must produce more wrapped lines (wide={wide}, narrow={narrow})"
        );
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
        let mut renderer = fresh_renderer();

        cache.get_or_update(&mut renderer, base, "", style, style, width);
        let mut prev_count = cache.lines.len();

        let chars: Vec<char> = suffix.chars().collect();
        for i in 1..=chars.len() {
            let partial: String = chars[..i].iter().collect();
            let text = format!("{base}{partial}");
            cache.get_or_update(&mut renderer, &text, "", style, style, width);
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
        let mut renderer = fresh_renderer();

        let base = "| A | B |\n| --- | --- |\n| 1 | 2 |";
        cache.get_or_update(&mut renderer, base, "", style, style, width);
        let base_lines = cache_lines_text(&cache);

        let partial = format!("{base}\n| 3 | in pro");
        cache.get_or_update(&mut renderer, &partial, "", style, style, width);
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
        cache.get_or_update(&mut renderer, &complete, "", style, style, width);
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
        assert!(sc.cache.key.is_none());
        assert!(sc.cache.lines.is_empty());

        sc.set_buffer("hello");
        sc.render_lines(80);
        let text = sc.take_all();
        assert_eq!(text, "hello");
        assert!(sc.is_empty());
        assert!(sc.cache.key.is_none());

        let mut sc = StreamingContent::new("old> ", style, style, 4);
        sc.set_buffer("text");
        sc.render_lines(80);
        let new_style = Style::default().fg(ratatui::style::Color::Red);
        sc.set_style("new> ", new_style, new_style);
        assert!(sc.cache.lines.is_empty());
    }

    #[test]
    fn cache_miss_on_first_call_returns_true() {
        let style = Style::default();
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();
        let repopulated = cache.get_or_update(&mut renderer, "hello", "", style, style, 80);
        assert!(repopulated, "first call must repopulate (return true)");
    }

    #[test]
    fn cache_hit_returns_false() {
        let style = Style::default();
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();
        cache.get_or_update(&mut renderer, "hello", "", style, style, 80);
        let hit = cache.get_or_update(&mut renderer, "hello", "", style, style, 80);
        assert!(
            !hit,
            "second identical call must be a cache hit (return false)"
        );
    }

    #[test]
    fn set_style_invalidates_visual_output() {
        let style = Style::default();
        let red = Style::default().fg(ratatui::style::Color::Red);

        let mut sc = StreamingContent::new("old> ", style, style, 4);
        sc.set_buffer("hello");
        let before: Vec<String> = sc
            .render_lines(80)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(before.iter().any(|l| l.contains("old> ")));

        sc.set_style("new> ", red, red);
        let after: Vec<String> = sc
            .render_lines(80)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(after.iter().any(|l| l.contains("new> ")));
        assert!(!after.iter().any(|l| l.contains("old> ")));
    }

    #[test]
    fn empty_content_produces_output() {
        let style = Style::default();
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();
        let repopulated = cache.get_or_update(&mut renderer, "", "", style, style, 80);
        assert!(repopulated);
        assert!(
            !cache.lines.is_empty(),
            "empty content should still produce lines"
        );
    }

    #[test]
    fn renderer_persists_across_cache_updates() {
        let style = Style::default();
        let mut cache = StreamingCache::default();
        let mut renderer = fresh_renderer();

        let first_block = "```rust\nfn a() {}\n```";
        cache.get_or_update(&mut renderer, first_block, "", style, style, 80);
        let after_first = cache_lines_text(&cache);

        let both_blocks = "```rust\nfn a() {}\n```\ntext\n```python\ndef b(): pass\n```";
        cache.get_or_update(&mut renderer, both_blocks, "", style, style, 80);
        let after_both = cache_lines_text(&cache);

        assert!(
            after_both.len() > after_first.len(),
            "second code block should add lines"
        );
        let expected = full_render_lines(both_blocks, "", 80);
        assert_eq!(
            after_both, expected,
            "renderer state must produce correct output across updates"
        );
    }
}
