//! Markdown parser and width-aware renderer.
//!
//! pulldown-cmark parses. [`parse`] only flattens its events into what the
//! renderer lays out: one [`LineBlock`] per source line, and code blocks and
//! tables as blocks of their own. `SpanKind` (text vs code) and `Emphasis`
//! (bold, italic, strike) stay orthogonal, so code inside bold keeps both.

pub mod render;

use std::iter;
use std::mem;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

const BULLET: &str = "• ";
const QUOTE_MARKER: &str = "> ";
const LINE_FEED: &str = "\n";
const LINE_ENDS: [char; 2] = ['\n', '\r'];
const CARRIAGE_RETURN: char = '\r';
const ATX_MARKER: char = '#';
const TABLE_PIPE: char = '|';
const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_CJK_FRIENDLY_EMPHASIS);

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
    let mut builder = Builder::new(text);
    for (event, range) in Parser::new_ext(text, OPTIONS).into_offset_iter() {
        builder.event(event, range);
    }
    builder.finish()
}

/// Moves `offset`, the start of a line, back to where rendering can begin
/// without changing what the rest means. A paragraph can start at any line
/// that still opens a paragraph on its own. Code read from the middle turns
/// into prose, so it starts at its block. A list starts at the top-level item
/// holding the line, which keeps a nested item's depth and closes a fence
/// inside it without going back to the start of a long list. A quote starts
/// at its child block the same way.
pub fn render_start(text: &str, offset: usize) -> usize {
    start_in(
        &mut Parser::new_ext(text, OPTIONS).into_offset_iter(),
        text,
        offset,
    )
}

/// [`render_start`] for a text that has only grown since an earlier call
/// returned `from` for it, with the line at `from` already ended then. The
/// text read from `from` the same as the whole text did, and appending keeps
/// it that way, so only the text from there is parsed. A line still being
/// typed can turn out to continue the block above, hence the ended line. The
/// exception is a paragraph `from` cut into, which can since have become a
/// setext heading or a table that takes in lines above `from`, so that, and
/// an `offset` before `from`, parse the whole text.
pub fn render_start_from(text: &str, from: usize, offset: usize) -> usize {
    let Some(rest) = text.get(from..).filter(|_| from <= offset) else {
        return render_start(text, offset);
    };
    let mut events = Parser::new_ext(rest, OPTIONS).into_offset_iter().peekable();
    match events.peek() {
        Some((Event::Start(tag), _)) if may_take_lines_above(tag, rest) => {
            render_start(text, offset)
        }
        _ => from + start_in(&mut events, rest, offset - from),
    }
}

fn may_take_lines_above(tag: &Tag, text: &str) -> bool {
    let line = text.trim_start_matches(' ');
    match tag {
        Tag::Heading { .. } => !line.starts_with(ATX_MARKER),
        Tag::Table(_) => !line.starts_with(TABLE_PIPE),
        _ => false,
    }
}

fn start_in<'a>(
    events: &mut impl Iterator<Item = (Event<'a>, Range<usize>)>,
    text: &str,
    offset: usize,
) -> usize {
    let rest = text.get(offset..).unwrap_or_default();
    let start = child_holding(events, offset).and_then(|(tag, block)| match tag {
        Tag::Paragraph if opens_paragraph(rest, false) => None,
        Tag::List(first) => item_start(events, text, offset, first, block.start),
        Tag::BlockQuote(_) => child_holding(events, offset).and_then(|(tag, child)| match tag {
            Tag::Paragraph if opens_paragraph(rest, true) => None,
            Tag::List(first) => item_start(events, text, offset, first, child.start),
            _ => Some(child.start),
        }),
        _ => Some(block.start),
    });
    start.map_or(offset, |start| {
        text[..start].rfind(LINE_ENDS).map_or(0, |end| end + 1)
    })
}

/// The first element at the iterator's depth that ends after `offset`, with
/// how many elements come before it. `None` once the enclosing element ends.
fn child_reaching<'a>(
    events: &mut impl Iterator<Item = (Event<'a>, Range<usize>)>,
    offset: usize,
) -> Option<(usize, Tag<'a>, Range<usize>)> {
    let mut depth = 0usize;
    let mut index = 0usize;
    for (event, range) in events {
        match event {
            Event::Start(tag) if depth == 0 && offset < range.end => {
                return Some((index, tag, range));
            }
            Event::Start(_) => {
                index += usize::from(depth == 0);
                depth += 1;
            }
            Event::End(_) if depth == 0 => return None,
            Event::End(_) => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The element at the iterator's depth that holds `offset` and starts before
/// it.
fn child_holding<'a>(
    events: &mut impl Iterator<Item = (Event<'a>, Range<usize>)>,
    offset: usize,
) -> Option<(Tag<'a>, Range<usize>)> {
    child_reaching(events, offset)
        .filter(|(_, _, range)| range.start < offset)
        .map(|(_, tag, range)| (tag, range))
}

/// Where the item at `offset` starts. An ordered item read on its own counts
/// from its typed number, so when the list numbers it differently the whole
/// list has to be read.
fn item_start<'a>(
    events: &mut impl Iterator<Item = (Event<'a>, Range<usize>)>,
    text: &str,
    offset: usize,
    first_number: Option<u64>,
    list_start: usize,
) -> Option<usize> {
    let (index, _, item) = child_reaching(events, offset)?;
    let renumbered = first_number
        .is_some_and(|first| typed_number(&text[item.start..]) != Some(first + index as u64));
    Some(if renumbered { list_start } else { item.start })
}

fn typed_number(item: &str) -> Option<u64> {
    let digits = item.bytes().take_while(u8::is_ascii_digit).count();
    item[..digits].parse().ok()
}

/// Whether the text from a paragraph's line on still opens a paragraph, in a
/// quote when `quoted`. Without the lines above, the line can open a list, a
/// table, HTML or code instead, or lose its quote.
fn opens_paragraph(rest: &str, quoted: bool) -> bool {
    let mut events = Parser::new_ext(rest, OPTIONS);
    (!quoted || matches!(events.next(), Some(Event::Start(Tag::BlockQuote(_)))))
        && matches!(events.next(), Some(Event::Start(Tag::Paragraph)))
}

/// What the lines inside a quote or a list item start with.
enum Container {
    Quote,
    Item { marker: String, shown: bool },
}

/// Where text goes. Prose and table cells collect spans in the open line,
/// code collects raw text.
enum Leaf {
    Prose,
    Code {
        lang: String,
        code: String,
    },
    Table {
        rows: Vec<Vec<Cell>>,
        row: Vec<Cell>,
    },
}

struct Builder<'a> {
    text: &'a str,
    blocks: Vec<Block>,
    lines: Vec<LineBlock>,
    line: Option<LineBlock>,
    leaf: Leaf,
    containers: Vec<Container>,
    list_numbers: Vec<Option<u64>>,
    emphasis: Vec<Emphasis>,
    heading: Option<u8>,
    link_targets: Vec<Option<String>>,
    /// Where the last content ended, to tell whether a blank source line
    /// separates it from the next block.
    content_end: usize,
}

impl<'a> Builder<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            blocks: Vec::new(),
            lines: Vec::new(),
            line: None,
            leaf: Leaf::Prose,
            containers: Vec::new(),
            list_numbers: Vec::new(),
            emphasis: Vec::new(),
            heading: None,
            link_targets: Vec::new(),
            content_end: 0,
        }
    }

    fn event(&mut self, event: Event<'a>, range: Range<usize>) {
        let is_content = !matches!(
            event,
            Event::Start(_) | Event::End(TagEnd::List(_) | TagEnd::Item | TagEnd::BlockQuote(_))
        );
        match event {
            Event::Start(tag) => self.start(tag, range.start),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.leaf {
                Leaf::Code { code, .. } => code.push_str(&text),
                // An HTML line that doesn't end in `\n` gets one as text of
                // its own, and each HTML line is a line here already.
                Leaf::Prose if *text == *LINE_FEED => {}
                Leaf::Prose | Leaf::Table { .. } => self.push_text(&text),
            },
            Event::Code(code) => {
                let span = InlineSpan::code(code.into_string(), self.emphasis());
                self.spans().push(span);
            }
            Event::Html(html) => {
                for line in html.lines() {
                    self.push_text(line);
                    self.close_line();
                }
            }
            Event::InlineHtml(html) => {
                for (index, line) in html.split('\n').enumerate() {
                    if index > 0 {
                        self.close_line();
                    }
                    self.push_text(line.strip_suffix(CARRIAGE_RETURN).unwrap_or(line));
                }
            }
            Event::SoftBreak | Event::HardBreak => self.close_line(),
            Event::Rule => {
                self.start_block(range.start);
                self.show_markers();
                self.push_empty(BlockKind::HorizontalRule);
            }
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
        if is_content {
            self.content_end = range.end;
        }
    }

    fn start(&mut self, tag: Tag<'a>, start: usize) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.start_block(start),
            Tag::Heading { level, .. } => {
                self.start_block(start);
                self.heading = Some(level as u8);
                self.line = Some(self.new_line());
            }
            Tag::BlockQuote(_) => {
                self.start_block(start);
                self.containers.push(Container::Quote);
            }
            Tag::List(first_number) => {
                self.start_block(start);
                self.list_numbers.push(first_number);
            }
            Tag::Item => {
                self.close_line();
                let marker = match self.list_numbers.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => BULLET.to_owned(),
                };
                self.containers.push(Container::Item {
                    marker,
                    shown: false,
                });
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.trim().to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.open_leaf(
                    start,
                    Leaf::Code {
                        lang,
                        code: String::new(),
                    },
                );
            }
            Tag::Table(_) => self.open_leaf(
                start,
                Leaf::Table {
                    rows: Vec::new(),
                    row: Vec::new(),
                },
            ),
            Tag::Emphasis => self.push_emphasis(Emphasis::ITALIC),
            Tag::Strong => self.push_emphasis(Emphasis::BOLD),
            Tag::Strikethrough => self.push_emphasis(Emphasis::STRIKE),
            Tag::Link {
                link_type,
                dest_url,
                ..
            }
            | Tag::Image {
                link_type,
                dest_url,
                ..
            } => {
                let target_is_text = matches!(link_type, LinkType::Autolink | LinkType::Email);
                self.link_targets
                    .push((!target_is_text).then(|| dest_url.into_string()));
            }
            Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_)
            | Tag::ContainerBlock(..)
            | Tag::Highlight => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.close_line(),
            TagEnd::Heading(_) => {
                self.close_line();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.containers.pop();
            }
            TagEnd::List(_) => {
                self.list_numbers.pop();
            }
            TagEnd::Item => {
                self.close_line();
                self.show_markers();
                self.containers.pop();
            }
            TagEnd::CodeBlock => match mem::replace(&mut self.leaf, Leaf::Prose) {
                Leaf::Code { lang, mut code } => {
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    self.blocks.push(Block::Code { lang, code });
                }
                Leaf::Prose | Leaf::Table { .. } => {}
            },
            TagEnd::Table => {
                if let Leaf::Table { rows, .. } = mem::replace(&mut self.leaf, Leaf::Prose) {
                    self.blocks.push(Block::Table { rows });
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Leaf::Table { rows, row } = &mut self.leaf {
                    rows.push(mem::take(row));
                }
            }
            TagEnd::TableCell => {
                let cell = self.line.take().map(|line| line.spans).unwrap_or_default();
                if let Leaf::Table { row, .. } = &mut self.leaf {
                    row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.emphasis.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                if let Some(target) = self.link_targets.pop().flatten() {
                    self.push_text(&format!(" ({target})"));
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_)
            | TagEnd::ContainerBlock(_)
            | TagEnd::Highlight => {}
        }
    }

    /// Ends the open line, and keeps a blank source line before the block as
    /// an empty line.
    fn start_block(&mut self, start: usize) {
        self.close_line();
        let blank_before = self.content_end > 0
            && self
                .text
                .as_bytes()
                .get(self.content_end - 1..start)
                .is_some_and(|gap| gap.iter().filter(|&&b| b == b'\n').count() >= 2);
        if blank_before {
            self.push_empty(BlockKind::Paragraph);
        }
    }

    /// Code and tables are blocks of their own, so the lines before them end
    /// here.
    fn open_leaf(&mut self, start: usize, leaf: Leaf) {
        self.start_block(start);
        self.show_markers();
        self.flush_lines();
        self.leaf = leaf;
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

    /// Table cells collect here too, until their end tag takes the spans.
    fn spans(&mut self) -> &mut Vec<InlineSpan> {
        let line = self.line.take().unwrap_or_else(|| self.new_line());
        &mut self.line.insert(line).spans
    }

    fn push_text(&mut self, text: &str) {
        let emphasis = self.emphasis();
        let spans = self.spans();
        match spans.last_mut() {
            Some(last) if last.kind == SpanKind::Text && last.emphasis == emphasis => {
                last.text.push_str(text);
            }
            _ => spans.push(InlineSpan::text(text, emphasis)),
        }
    }

    fn emphasis(&self) -> Emphasis {
        self.emphasis.last().copied().unwrap_or_default()
    }

    fn push_emphasis(&mut self, added: Emphasis) {
        self.emphasis.push(self.emphasis().merge(added));
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
    #[test_case("x <a\r\nhref=y> z", &["x <a", "href=y> z"] ; "inline_html_drops_a_crlf")]
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
    #[test_case("- a\n- b\n  more", 8 => 4 ; "item_line_starts_at_its_item_not_the_list")]
    #[test_case("> a\nb", 4 => 0 ; "lazy_quote_line_starts_at_the_quote")]
    #[test_case("> a\n> b", 4 => 4 ; "quoted_line_starts_where_asked")]
    #[test_case("> - a\n> - b\n>   more", 12 => 6 ; "item_in_a_quote_starts_at_its_item")]
    #[test_case("> - a\n> - b\n> - c", 12 => 12 ; "first_line_of_an_item_in_a_quote_starts_where_asked")]
    #[test_case("> a\n    > b", 4 => 0 ; "lazy_line_indented_like_code_starts_at_the_quote")]
    #[test_case("1. a\n2. b\n3. c", 10 => 10 ; "item_numbered_as_the_list_counts_starts_where_asked")]
    #[test_case("1. one\n1. two\n1. three", 14 => 0 ; "item_the_list_renumbers_starts_at_the_list")]
    #[test_case("intro\n2. second", 6 => 0 ; "paragraph_line_that_would_open_a_list_starts_at_the_paragraph")]
    #[test_case("intro\na | b\n--- | ---", 6 => 0 ; "paragraph_line_that_would_open_a_table_starts_at_the_paragraph")]
    #[test_case("a\nb\nc", 4 => 4 ; "paragraph_line_starts_where_asked")]
    #[test_case("x\ry\n```\ncode\nmore", 13 => 4 ; "lone_carriage_return_ends_a_line")]
    #[test_case("a\n\n    code\n    more", 3 => 3 ; "indented_code_block_from_its_first_line")]
    #[test_case("a\n\n    code\n    more", 12 => 3 ; "indented_code_block_keeps_its_indent")]
    fn render_start_backs_up_to_a_safe_line(text: &str, offset: usize) -> usize {
        render_start(text, offset)
    }

    const TAIL_LINES: [usize; 4] = [1, 2, 3, 5];

    fn rows(text: &str) -> Vec<String> {
        parse(text)
            .into_iter()
            .flat_map(|block| match block {
                Block::Lines(lines) => lines.iter().map(|line| format!("{line:?}")).collect(),
                other => vec![format!("{other:?}")],
            })
            .collect()
    }

    #[test_case("Intro that runs\nover two lines.\n\n- one\n- two\n  - nested\n    more\n- three\n\n1. first\n2. second\n   ```rust\n   let x = 1;\n   ```\n3. third\n\n> quoted\n> - a\n> - b\n>   cont\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nTitle\n=====\n\n    indented\n    code\n\nlast\n" ; "mixed")]
    #[test_case("1. a\n1. b\n1. c\n   more\n1. d\n" ; "lazy_numbers")]
    #[test_case("one\ntwo\nthree\n---\nafter\nx | y\n--- | ---\n" ; "paragraph_turns_into_a_heading_and_a_table")]
    #[test_case("> l1\n> l2\n> l3\nlazy\n> l4\n\n> - a\n> 1. b\n" ; "quotes")]
    fn render_start_from_the_last_start_reads_like_a_full_parse_while_text_streams(text: &str) {
        for tail_lines in TAIL_LINES {
            let mut last_start = 0;
            for end in (1..=text.len()).filter(|&end| text.is_char_boundary(end)) {
                let shown = &text[..end];
                let offset = shown
                    .rmatch_indices('\n')
                    .nth(tail_lines - 1)
                    .map_or(0, |(newline, _)| newline + 1);
                let resumed = render_start_from(shown, last_start, offset);
                let full = render_start(shown, offset);
                let (earlier, later) = (full.min(resumed), full.max(resumed));
                assert!(
                    rows(&shown[earlier..]).ends_with(&rows(&shown[later..])),
                    "{shown:?} at {offset}: resumed {resumed}, full {full}"
                );
                if shown[resumed..].contains('\n') {
                    last_start = resumed;
                }
            }
        }
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
