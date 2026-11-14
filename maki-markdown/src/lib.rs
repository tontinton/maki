//! Markdown parser and width-aware renderer.
//!
//! comrak parses. [`parse`] only flattens its tree into what the
//! renderer lays out: one [`LineBlock`] per source line, and code blocks and
//! tables as blocks of their own. `SpanKind` (text vs code) and `Emphasis`
//! (bold, italic, strike) stay orthogonal, so code inside bold keeps both.

pub mod render;

use std::iter;
use std::mem;

use comrak::nodes::{ListType, NodeValue, Sourcepos};
use comrak::{Arena, Node, Options, parse_document};
use unicode_width::UnicodeWidthStr;

const BULLET: &str = "• ";
const QUOTE_MARKER: &str = "> ";
const MAILTO: &str = "mailto:";

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
    ListItem,
    HorizontalRule,
}

/// One source line. `prefix` holds the quote and list markers in front of it,
/// or blanks of the same width on an item's later lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineBlock {
    pub kind: BlockKind,
    pub prefix: String,
    pub spans: Vec<InlineSpan>,
}

pub type Cell = Vec<InlineSpan>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Block {
    Lines(Vec<LineBlock>),
    Code {
        lang: String,
        code: String,
    },
    /// The first row is the header.
    Table {
        rows: Vec<Vec<Cell>>,
    },
}

pub fn parse(text: &str) -> Vec<Block> {
    let arena = Arena::new();
    let root = parse_document(&arena, text, &options());
    let mut builder = Builder::default();
    builder.block(root);
    builder.finish()
}

/// Moves `offset`, the start of a line, back to where rendering can begin
/// without changing what the rest means. A paragraph can start at any of its
/// lines. Anything else starts at its outermost block: code read from the
/// middle turns into prose, a nested item loses its depth, and a fence inside
/// a list or a quote only closes when read from there.
pub fn render_start(text: &str, offset: usize) -> usize {
    let line = line_starts(text)
        .take_while(|&start| start <= offset)
        .count();
    let arena = Arena::new();
    let root = parse_document(&arena, text, &options());
    root.children()
        .map(|block| block.data())
        .find(|block| block.sourcepos.start.line < line && line <= block.sourcepos.end.line)
        .filter(|block| !matches!(block.value, NodeValue::Paragraph))
        .and_then(|block| line_starts(text).nth(block.sourcepos.start.line - 1))
        .unwrap_or(offset)
}

fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.cjk_friendly_emphasis = true;
    options
}

/// Where each line starts. A lone `\r` ends a line too, as it does for comrak.
fn line_starts(text: &str) -> impl Iterator<Item = usize> {
    let bytes = text.as_bytes();
    let ends = bytes.iter().enumerate().filter(|&(at, &byte)| {
        byte == b'\n' || (byte == b'\r' && bytes.get(at + 1) != Some(&b'\n'))
    });
    iter::once(0).chain(ends.map(|(at, _)| at + 1))
}

/// What the lines inside a quote or a list item start with.
enum Container {
    Quote,
    Item { marker: String, shown: bool },
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    lines: Vec<LineBlock>,
    line: Option<LineBlock>,
    containers: Vec<Container>,
    heading: Option<u8>,
    /// The source line the last content ended on, to tell whether a blank
    /// line separates it from the next block.
    content_end_line: usize,
}

impl Builder {
    fn block(&mut self, node: Node<'_>) {
        let ast = node.data();
        let at = ast.sourcepos;
        match &ast.value {
            NodeValue::Paragraph => {
                self.start_block(at);
                self.inlines(node, Emphasis::default());
                self.close_line();
            }
            NodeValue::Heading(heading) => {
                self.start_block(at);
                self.heading = Some(heading.level);
                self.line = Some(self.new_line());
                self.inlines(node, Emphasis::default());
                self.close_line();
                self.heading = None;
            }
            NodeValue::BlockQuote => {
                self.start_block(at);
                self.containers.push(Container::Quote);
                self.children(node);
                self.containers.pop();
                return;
            }
            NodeValue::List(list) => {
                self.start_block(at);
                let mut number = (list.list_type == ListType::Ordered).then_some(list.start);
                for item in node.children() {
                    let marker = match &mut number {
                        Some(number) => {
                            let marker = format!("{number}. ");
                            *number += 1;
                            marker
                        }
                        None => BULLET.to_owned(),
                    };
                    self.containers.push(Container::Item {
                        marker,
                        shown: false,
                    });
                    self.children(item);
                    self.close_line();
                    self.show_markers();
                    self.containers.pop();
                }
                return;
            }
            NodeValue::CodeBlock(code) => {
                self.open_leaf(at);
                let code_text = code.literal.strip_suffix('\n').unwrap_or(&code.literal);
                self.blocks.push(Block::Code {
                    lang: code.info.trim().to_owned(),
                    code: code_text.to_owned(),
                });
            }
            NodeValue::HtmlBlock(html) => {
                self.start_block(at);
                for line in html.literal.lines() {
                    self.push_text(line, Emphasis::default());
                    self.close_line();
                }
            }
            NodeValue::ThematicBreak => {
                self.start_block(at);
                self.show_markers();
                self.push_empty(BlockKind::HorizontalRule);
            }
            NodeValue::Table(_) => {
                self.open_leaf(at);
                let rows = node
                    .children()
                    .map(|row| row.children().map(|cell| self.cell(cell)).collect())
                    .collect();
                self.blocks.push(Block::Table { rows });
            }
            _ => {
                self.children(node);
                return;
            }
        }
        self.content_end_line = at.end.line;
    }

    fn children(&mut self, node: Node<'_>) {
        for child in node.children() {
            self.block(child);
        }
    }

    fn cell(&mut self, cell: Node<'_>) -> Cell {
        self.line = Some(self.new_line());
        self.inlines(cell, Emphasis::default());
        self.line.take().map(|line| line.spans).unwrap_or_default()
    }

    fn inlines(&mut self, node: Node, emphasis: Emphasis) {
        for child in node.children() {
            let ast = child.data();
            match &ast.value {
                NodeValue::Text(text) => self.push_text(text, emphasis),
                NodeValue::Code(code) => {
                    let span = InlineSpan::code(code.literal.clone(), emphasis);
                    self.spans().push(span);
                }
                NodeValue::SoftBreak | NodeValue::LineBreak => self.close_line(),
                NodeValue::Emph => self.inlines(child, emphasis.merge(Emphasis::ITALIC)),
                NodeValue::Strong => self.inlines(child, emphasis.merge(Emphasis::BOLD)),
                NodeValue::Strikethrough => self.inlines(child, emphasis.merge(Emphasis::STRIKE)),
                NodeValue::Link(link) | NodeValue::Image(link) => {
                    self.inlines(child, emphasis);
                    if !is_autolink(child, &link.url) {
                        self.push_text(&format!(" ({})", link.url), emphasis);
                    }
                }
                NodeValue::HtmlInline(html) => {
                    for (index, line) in html.split('\n').enumerate() {
                        if index > 0 {
                            self.close_line();
                        }
                        self.push_text(line, emphasis);
                    }
                }
                _ => self.inlines(child, emphasis),
            }
        }
    }

    /// Ends the open line, and keeps a blank source line before the block as
    /// an empty line.
    fn start_block(&mut self, at: Sourcepos) {
        self.close_line();
        if self.content_end_line > 0 && at.start.line > self.content_end_line + 1 {
            self.push_empty(BlockKind::Paragraph);
            self.content_end_line = at.start.line - 1;
        }
    }

    /// Code and tables are blocks of their own, so the lines before them end
    /// here.
    fn open_leaf(&mut self, at: Sourcepos) {
        self.start_block(at);
        self.show_markers();
        self.flush_lines();
    }

    fn push_empty(&mut self, kind: BlockKind) {
        self.lines.push(LineBlock {
            kind,
            prefix: String::new(),
            spans: Vec::new(),
        });
    }

    /// An item that holds no text of its own, or opens with a code block or a
    /// table, still gets a line for its marker.
    fn show_markers(&mut self) {
        let unshown = self
            .containers
            .iter()
            .any(|container| matches!(container, Container::Item { shown: false, .. }));
        if unshown {
            let line = self.new_line();
            self.lines.push(line);
        }
    }

    fn new_line(&mut self) -> LineBlock {
        let mut prefix = String::new();
        let mut in_item = false;
        for container in &mut self.containers {
            match container {
                Container::Quote => prefix.push_str(QUOTE_MARKER),
                Container::Item { marker, shown } => {
                    in_item = true;
                    if mem::replace(shown, true) {
                        prefix.extend(iter::repeat_n(' ', marker.width()));
                    } else {
                        prefix.push_str(marker);
                    }
                }
            }
        }
        let kind = match self.heading {
            Some(level) => BlockKind::Heading(level),
            None if in_item => BlockKind::ListItem,
            None => BlockKind::Paragraph,
        };
        LineBlock {
            kind,
            prefix,
            spans: Vec::new(),
        }
    }

    fn spans(&mut self) -> &mut Vec<InlineSpan> {
        let line = self.line.take().unwrap_or_else(|| self.new_line());
        &mut self.line.insert(line).spans
    }

    fn push_text(&mut self, text: &str, emphasis: Emphasis) {
        let spans = self.spans();
        match spans.last_mut() {
            Some(last) if last.kind == SpanKind::Text && last.emphasis == emphasis => {
                last.text.push_str(text);
            }
            _ => spans.push(InlineSpan::text(text, emphasis)),
        }
    }

    fn close_line(&mut self) {
        if let Some(line) = self.line.take() {
            self.lines.push(line);
        }
    }

    fn flush_lines(&mut self) {
        self.close_line();
        if !self.lines.is_empty() {
            self.blocks.push(Block::Lines(mem::take(&mut self.lines)));
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_lines();
        self.blocks
    }
}

/// `<https://x>` and `<a@b.c>` already show their target as their text.
fn is_autolink(link: Node<'_>, url: &str) -> bool {
    let mut children = link.children();
    let (Some(only), None) = (children.next(), children.next()) else {
        return false;
    };
    match &only.data().value {
        NodeValue::Text(text) => {
            text.as_ref() == url || url.strip_prefix(MAILTO) == Some(text.as_ref())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const FENCED: &str = "intro\n```rust\nlet a = 1;\n```\nafter";
    const FOUR_BACKTICK_FENCE: &str = "````\n```\ninner\n```\n````";
    const FENCE_IN_ITEM: &str = "- item\n\n  ```rust\n  let x = 1;\n  ```\n";

    fn lines(text: &str) -> Vec<LineBlock> {
        parse(text)
            .into_iter()
            .flat_map(|block| match block {
                Block::Lines(lines) => lines,
                other => panic!("expected lines, got {other:?}"),
            })
            .collect()
    }

    fn visible(line: &LineBlock) -> String {
        line.spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn spans(text: &str) -> Vec<InlineSpan> {
        lines(text).remove(0).spans
    }

    #[test_case("a\nb\nc", &["a", "b", "c"] ; "soft_breaks_keep_one_line_each")]
    #[test_case("a\n\nb", &["a", "", "b"] ; "blank_line_between_paragraphs")]
    #[test_case("# H\npara", &["H", "para"] ; "no_blank_line_after_a_tight_heading")]
    #[test_case("- a\n- b\n\n- c", &["a", "b", "", "c"] ; "blank_line_inside_a_loose_list")]
    #[test_case("para\n\n- a", &["para", "", "a"] ; "blank_line_before_a_tight_list")]
    #[test_case("> q1\n>\n> q2", &["q1", "", "q2"] ; "blank_quote_line")]
    #[test_case("<div>\nhi\n</div>", &["<div>", "hi", "</div>"] ; "html_stays_text")]
    #[test_case("x <a\nhref=y> z", &["x <a", "href=y> z"] ; "inline_html_keeps_its_line_break")]
    fn parse_keeps_source_lines(text: &str, want: &[&str]) {
        let got: Vec<String> = lines(text).iter().map(visible).collect();
        assert_eq!(got, want);
    }

    #[test_case("- a\n  - b", &["• ", "  • "] ; "nested_bullets")]
    #[test_case("1. one\n   cont\n   1. nested\n2. two", &["1. ", "   ", "   1. ", "2. "] ; "ordered_items_and_continuation")]
    #[test_case("1. a\n1. b", &["1. ", "2. "] ; "ordered_items_count_up")]
    #[test_case("> - a\n>   b", &["> • ", ">   "] ; "item_inside_a_quote")]
    #[test_case("- ", &["• "] ; "empty_item_keeps_its_marker")]
    #[test_case("* ---", &["• ", ""] ; "marker_comes_before_a_rule_in_its_item")]
    fn parse_prefixes_follow_quotes_and_items(text: &str, want: &[&str]) {
        let got: Vec<String> = lines(text).into_iter().map(|line| line.prefix).collect();
        assert_eq!(got, want);
    }

    #[test_case("#", &[1] ; "bare_hash")]
    #[test_case("### x", &[3] ; "level_three")]
    #[test_case("a\nb\n---", &[2, 2] ; "every_line_of_a_setext_heading")]
    fn parse_heading_level(text: &str, levels: &[u8]) {
        let kinds: Vec<BlockKind> = lines(text).into_iter().map(|line| line.kind).collect();
        let want: Vec<BlockKind> = levels.iter().copied().map(BlockKind::Heading).collect();
        assert_eq!(kinds, want);
    }

    #[test]
    fn parse_code_inside_bold_keeps_both_axes() {
        assert_eq!(
            spans("**bold `code` bold**"),
            [
                InlineSpan::text("bold ", Emphasis::BOLD),
                InlineSpan::code("code", Emphasis::BOLD),
                InlineSpan::text(" bold", Emphasis::BOLD),
            ]
        );
    }

    #[test_case("**注意：**请先备份", "注意：", "请先备份" ; "after_full_width_punctuation")]
    #[test_case("这是**加粗（括号）**文字", "加粗（括号）", "文字" ; "after_a_full_width_bracket")]
    fn parse_bold_closes_inside_cjk_text(text: &str, bold: &str, rest: &str) {
        let spans = spans(text);
        let tail = &spans[spans.len() - 2..];
        assert_eq!(
            tail,
            [
                InlineSpan::text(bold, Emphasis::BOLD),
                InlineSpan::text(rest, Emphasis::default()),
            ]
        );
    }

    #[test]
    fn parse_nested_emphasis_merges() {
        let emphasis: Vec<Emphasis> = spans("*a **b** c*")
            .into_iter()
            .map(|span| span.emphasis)
            .collect();
        assert_eq!(
            emphasis,
            [Emphasis::ITALIC, Emphasis::BOLD_ITALIC, Emphasis::ITALIC]
        );
    }

    #[test_case("about ~5s, then ~10s" ; "tildes_before_numbers")]
    #[test_case("a\\*b\\*" ; "escaped_stars_lose_the_backslash_only")]
    fn parse_keeps_text_as_typed(text: &str) {
        let want = text.replace('\\', "");
        assert_eq!(spans(text), [InlineSpan::text(want, Emphasis::default())]);
    }

    #[test_case("[t](http://u)", "t (http://u)" ; "inline_link_shows_its_target")]
    #[test_case("<http://v>", "http://v" ; "autolink_shows_once")]
    #[test_case("![alt](img.png)", "alt (img.png)" ; "image_shows_alt_and_target")]
    fn parse_links(text: &str, want: &str) {
        assert_eq!(visible(&lines(text)[0]), want);
    }

    #[test_case("```rust\nfn x() {}\n```", "rust", "fn x() {}" ; "closed_fence")]
    #[test_case("```\nfoo\nbar", "", "foo\nbar" ; "unclosed_fence_runs_to_the_end")]
    #[test_case(FOUR_BACKTICK_FENCE, "", "```\ninner\n```" ; "longer_fence_wraps_a_shorter_one")]
    #[test_case(FENCE_IN_ITEM, "rust", "let x = 1;" ; "fence_inside_a_list_item")]
    #[test_case("a\n\n    if x:\n        y()\n\nb", "", "if x:\n    y()" ; "indented_block")]
    fn parse_code_block(text: &str, want_lang: &str, want_code: &str) {
        let code = parse(text).into_iter().find_map(|block| match block {
            Block::Code { lang, code } => Some((lang, code)),
            _ => None,
        });
        assert_eq!(code, Some((want_lang.into(), want_code.into())));
    }

    #[test]
    fn parse_table_cells_carry_inline_spans() {
        let blocks = parse("| Name | `v` |\n| --- | --- |\n| foo | **42** |");
        assert_eq!(
            blocks,
            [Block::Table {
                rows: vec![
                    vec![
                        vec![InlineSpan::text("Name", Emphasis::default())],
                        vec![InlineSpan::code("v", Emphasis::default())],
                    ],
                    vec![
                        vec![InlineSpan::text("foo", Emphasis::default())],
                        vec![InlineSpan::text("42", Emphasis::BOLD)],
                    ],
                ],
            }]
        );
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

    #[test_case(FENCED, 0 => 0 ; "before_any_fence")]
    #[test_case(FENCED, 14 => 6 ; "inside_a_fence")]
    #[test_case(FENCED, 29 => 29 ; "after_a_closed_fence")]
    #[test_case("```\nfoo\nbar", 8 => 0 ; "inside_an_unclosed_fence")]
    #[test_case(FOUR_BACKTICK_FENCE, 9 => 0 ; "inner_fence_does_not_close_the_outer")]
    #[test_case(FENCE_IN_ITEM, 20 => 0 ; "fence_inside_a_list_item_starts_at_the_list")]
    #[test_case("para\n\n> ```\n> code\n> ```\nafter", 12 => 6 ; "fence_inside_a_quote_starts_at_the_quote")]
    #[test_case("~~~\ncode\n", 4 => 0 ; "tilde_fence")]
    #[test_case("- a\n  - b\n    - c\n    - d", 10 => 0 ; "line_indented_like_code_starts_at_its_list")]
    #[test_case("- a\n  - b\n  - c", 10 => 0 ; "nested_item_starts_at_its_list")]
    #[test_case("> a\nb", 4 => 0 ; "lazy_quote_line_starts_at_the_quote")]
    #[test_case("a\nb\nc", 4 => 4 ; "paragraph_line_starts_where_asked")]
    #[test_case("x\ry\n```\ncode\nmore", 13 => 4 ; "lone_carriage_return_ends_a_line")]
    #[test_case("a\n\n    code\n    more", 3 => 3 ; "indented_code_block_from_its_first_line")]
    #[test_case("a\n\n    code\n    more", 12 => 3 ; "indented_code_block_keeps_its_indent")]
    fn render_start_backs_up_to_a_safe_line(text: &str, offset: usize) -> usize {
        render_start(text, offset)
    }

    #[test]
    fn parse_never_panics_on_arbitrary_input() {
        const MARKDOWN_BYTES: &[char] = &[
            '*', '_', '`', '~', '|', '>', '-', '#', '[', ']', '(', ')', '\n', ' ',
        ];
        let mut rng = fastrand::Rng::with_seed(0xC0FFEE);
        for _ in 0..500 {
            let text: String = (0..rng.usize(0..200))
                .map(|_| {
                    if rng.bool() {
                        MARKDOWN_BYTES[rng.usize(..MARKDOWN_BYTES.len())]
                    } else {
                        rng.char(..)
                    }
                })
                .collect();
            parse(&text);
            let offset = rng.usize(..=text.len());
            assert!(render_start(&text, offset) <= offset);
        }
    }
}
