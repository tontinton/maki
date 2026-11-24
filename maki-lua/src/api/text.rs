use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::convert::identity;
use std::io;
use std::iter::successors;

use htmd::element_handler::{HandlerResult, Handlers};
use htmd::node::{Node, NodeType};
use htmd::options::{BulletListMarker, Options};
use htmd::{Element, HtmlToMarkdown};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaString, Result as LuaResult, Table};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use tokio::task::spawn_blocking;

use super::util::convert::TableExt;
use super::util::pair::{Pair, pair};

const PRE_TAG: &str = "pre";
/// Elements that sit inside a line of text. Every other element starts and
/// ends a line.
const INLINE_TAGS: [&str; 32] = [
    "a", "abbr", "b", "bdi", "bdo", "big", "cite", "code", "data", "del", "dfn", "em", "font", "i",
    "ins", "kbd", "label", "mark", "q", "s", "samp", "small", "span", "strike", "strong", "sub",
    "sup", "time", "tt", "u", "var", "wbr",
];
const TABLE_TAG: &str = "table";
const CELL_TAGS: [&str; 2] = ["td", "th"];
const HEADER_TAGS: [&str; 2] = ["th", "thead"];
const CELL_END: char = '|';
/// A pipe inside a cell is escaped, so only a cell's end has a bare one after a space.
const PADDED_CELL_END: &str = " |";
const SEPARATOR_CELL: &str = "---";
const LIST_MARKER_SPACING: u8 = 1;

thread_local! {
    /// `Matcher::new` allocates a score matrix of about 135KB, so building one
    /// per call would make the obvious loop over candidates allocate once per
    /// candidate. Its config is overwritten on every borrow, which is the only
    /// thing that differs between callers.
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

fn has_header(node: &Node<'_>) -> bool {
    node.children().iter().any(|child| match child.tag_name() {
        Some(tag) if HEADER_TAGS.contains(&tag) => true,
        Some(TABLE_TAG) => false,
        _ => has_header(&child),
    })
}

/// htmd's own rule for writing a table as pipe rows. Any other table comes
/// out as its cells' content, where a line that starts with `|` is somebody's
/// code and not ours to touch.
fn is_pipe_table(table: &Node<'_>) -> bool {
    let in_cell = successors(table.parent(), Node::parent).any(|ancestor| {
        ancestor
            .tag_name()
            .is_some_and(|tag| CELL_TAGS.contains(&tag))
    });
    !in_cell && has_header(table)
}

/// htmd pads every cell to its column's width, so one long cell costs its
/// length again in every other row. Markdown needs none of it.
fn unpad_table(table: HandlerResult) -> HandlerResult {
    let content = table.content.split_inclusive('\n').map(unpad_row).collect();
    HandlerResult { content, ..table }
}

fn unpad_row(line: &str) -> Cow<'_, str> {
    let Some(cells) = line
        .trim_end_matches('\n')
        .strip_prefix(CELL_END)
        .and_then(|row| row.strip_suffix(PADDED_CELL_END))
    else {
        return Cow::Borrowed(line);
    };
    let cells: Vec<&str> = cells.split(PADDED_CELL_END).map(str::trim).collect();
    let is_separator = cells
        .iter()
        .all(|cell| cell.len() >= SEPARATOR_CELL.len() && cell.bytes().all(|b| b == b'-'));
    let mut row = String::from(CELL_END);
    for cell in cells {
        let cell = if is_separator { SEPARATOR_CELL } else { cell };
        if !cell.is_empty() {
            row.push(' ');
            row.push_str(cell);
        }
        row.push(' ');
        row.push(CELL_END);
    }
    row.push('\n');
    Cow::Owned(row)
}

/// Convert an HTML string to Markdown.
/// Useful for cleaning up web content fetched with `maki.webfetch`.
/// Invalid UTF-8 becomes U+FFFD.
///
/// @param html string HTML source text.
/// @param opts table? `skip_tags` (string[]) elements dropped together with their content, like `script` or `nav`.
/// @return (string?, string?) Markdown text on success, or nil plus an error message.
/// @example
/// local md, err = maki.text.html_to_markdown("<h1>Hello</h1><p>world</p>")
/// if err then return end
/// print(md) -- "# Hello\n\nworld"
///
/// local article = maki.text.html_to_markdown(page, { skip_tags = { "script", "style", "nav" } })
#[lua_fn]
async fn html_to_markdown(
    _lua: Lua,
    html: LuaString,
    opts: Option<Table>,
) -> LuaResult<Pair<String>> {
    let html = html.to_string_lossy();
    let skip_tags: Vec<String> = opts.as_ref().opt("skip_tags")?.unwrap_or_default();
    Ok(pair(
        spawn_blocking(move || {
            let mut builder = HtmlToMarkdown::builder()
                .options(Options {
                    bullet_list_marker: BulletListMarker::Dash,
                    ul_bullet_spacing: LIST_MARKER_SPACING,
                    ol_number_spacing: LIST_MARKER_SPACING,
                    ..Options::default()
                })
                .add_handler(
                    vec![TABLE_TAG],
                    |handlers: &dyn Handlers, element: &Element| {
                        let unpad = if is_pipe_table(element.node) {
                            unpad_table
                        } else {
                            identity
                        };
                        handlers.fallback(element).map(unpad)
                    },
                );
            // htmd asserts on an empty tag list.
            if !skip_tags.is_empty() {
                builder = builder.skip_tags(skip_tags.iter().map(String::as_str).collect());
            }
            builder.build().convert(&html)
        })
        .await
        .map_err(mlua::Error::external)?
        .map_err(|e| format!("html_to_markdown: {e}")),
    ))
}

/// Convert an HTML string to plain text.
/// Entities are decoded and whitespace is collapsed outside `pre`. Inline elements such as
/// `b` or `a` leave no trace, and every other element starts a new line.
/// Invalid UTF-8 becomes U+FFFD.
///
/// @param html string HTML source text.
/// @param opts table? `skip_tags` (string[]) elements dropped together with their content, like `script` or `nav`.
/// @return (string?, string?) Plain text on success, or nil plus an error message.
/// @example
/// local text, err = maki.text.html_to_text("<h1>Fish &amp; chips</h1><p>to <b>go</b></p>")
/// if err then return end
/// print(text) -- "Fish & chips\nto go"
#[lua_fn]
async fn html_to_text(_lua: Lua, html: LuaString, opts: Option<Table>) -> LuaResult<Pair<String>> {
    let html = html.to_string_lossy();
    let skip_tags: Vec<String> = opts.as_ref().opt("skip_tags")?.unwrap_or_default();
    Ok(pair(
        spawn_blocking(move || visible_text(&html, &skip_tags))
            .await
            .map_err(mlua::Error::external)?
            .map_err(|e| format!("html_to_text: {e}")),
    ))
}

/// What separates the next visible character from the text before it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Gap {
    None,
    Space,
    Line,
}

enum Step<'a> {
    Visit(Node<'a>),
    EndLine,
    EndPre,
}

fn visible_text(html: &str, skip_tags: &[String]) -> io::Result<String> {
    let mut text = String::new();
    let mut gap = Gap::None;
    let mut pre_depth = 0usize;
    let tree = HtmlToMarkdown::new().html_to_tree(html)?;
    let mut steps = vec![Step::Visit(tree.root())];
    while let Some(step) = steps.pop() {
        let node = match step {
            Step::Visit(node) => node,
            Step::EndLine => {
                gap = Gap::Line;
                continue;
            }
            Step::EndPre => {
                pre_depth -= 1;
                text.truncate(text.trim_end_matches('\n').len());
                continue;
            }
        };
        if let Some(contents) = node.text() {
            for c in contents.chars() {
                if pre_depth == 0 && c.is_whitespace() {
                    gap = gap.max(Gap::Space);
                    continue;
                }
                match gap {
                    _ if text.is_empty() => {}
                    Gap::None => {}
                    Gap::Space => text.push(' '),
                    Gap::Line => text.push('\n'),
                }
                gap = Gap::None;
                text.push(c);
            }
            continue;
        }
        match node.tag_name() {
            Some(tag) if skip_tags.iter().any(|skipped| skipped == tag) => continue,
            Some(tag) => {
                if !INLINE_TAGS.contains(&tag) {
                    gap = Gap::Line;
                    steps.push(Step::EndLine);
                }
                if tag == PRE_TAG {
                    pre_depth += 1;
                    steps.push(Step::EndPre);
                }
            }
            None if node.node_type() == NodeType::Document => {}
            None => continue,
        }
        let first_child = steps.len();
        steps.extend(successors(node.first_child(), Node::next_sibling).map(Step::Visit));
        steps[first_child..].reverse();
    }
    Ok(text)
}

/// Return {s} as valid UTF-8, with every invalid byte sequence replaced by
/// U+FFFD. Lua strings are bytes, and `maki.net` and `maki.fs` hand them over
/// as-is, so decode before measuring or cutting a body as text. An invalid
/// byte can grow to 3 bytes, so a cut made before decoding can triple in size.
///
/// Needs no plugin permission.
///
/// @param s string Bytes to decode.
/// @return (string) Valid UTF-8 text, `s` itself when it already is.
/// @example
/// local resp = maki.net.request(url)
/// local text = maki.text.to_utf8(resp.body)
#[lua_fn]
fn to_utf8(lua: &Lua, s: LuaString) -> LuaResult<LuaString> {
    if s.to_str().is_ok() {
        return Ok(s);
    }
    lua.create_string(s.to_string_lossy())
}

/// `Config::DEFAULT.match_paths()` is what the file picker ranks with, and
/// plain `Config::DEFAULT` is what the model, command and list pickers use.
const fn config(paths: bool) -> Config {
    match paths {
        true => Config::DEFAULT.match_paths(),
        false => Config::DEFAULT,
    }
}

fn compile(needle: &str) -> Pattern {
    Pattern::new(
        needle,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    )
}

fn want(opts: Option<&Table>, key: &str) -> bool {
    opts.and_then(|o| o.opt_bool(key)).unwrap_or(false)
}

/// Where the match landed in {text}, in the shape `maki.fs.fuzzy_files`
/// reports: 1-based inclusive byte ranges, runs of touching characters
/// coalesced, so `text:sub(from, to)` is the matched text.
///
/// Both ranking APIs answer the same question, so a caller drawing a
/// highlight must not have to remember which of them it asked.
fn highlights_table(
    lua: &Lua,
    pattern: &Pattern,
    matcher: &mut Matcher,
    text: &str,
    buf: &mut Vec<char>,
    scratch: &mut Vec<u32>,
) -> LuaResult<Table> {
    scratch.clear();
    let haystack = Utf32Str::new(text, buf);
    let _ = pattern.indices(haystack, matcher, scratch);
    let ranges = maki_agent::byte_highlights(text, haystack, scratch);
    let out = lua.create_table_with_capacity(ranges.len(), 0)?;
    for (from, to) in ranges {
        out.push(lua.create_sequence_from([from, to])?)?;
    }
    Ok(out)
}

/// Scores {needle} against {haystack} with the fuzzy matcher the built-in
/// pickers use. {needle} is one pattern, spaces included.
///
/// Higher is better. Scores are only comparable across haystacks scored
/// against the same needle. An empty needle matches everything with score 0.
///
/// The second return value lists where the match landed, in the same shape
/// as `maki.fs.fuzzy_files`: 1-based inclusive `{ from, to }` byte ranges,
/// ascending, with adjacent characters merged. `haystack:sub(from, to)` is
/// the matched text.
///
/// Needs no plugin permission.
///
/// @param needle string What the user typed.
/// @param haystack string The candidate to score it against.
/// @param opts table? Options:
///   `paths` (boolean) rank {haystack} as a path, favouring the last segment, like the file picker. Off by default, like the model, command and list pickers.
/// @return (integer|nil, table|nil) Score and matched byte ranges, or nil when the needle does not match.
/// @example
/// local score, at = maki.text.fuzzy("mrs", "maki-ui/src/main.rs", { paths = true })
/// if score then print(("maki-ui/src/main.rs"):sub(at[1][1], at[1][2])) end
#[lua_fn]
fn fuzzy(
    lua: &Lua,
    needle: String,
    haystack: String,
    opts: Option<Table>,
) -> LuaResult<(Option<u32>, Option<Table>)> {
    let pattern = compile(&needle);
    let paths = want(opts.as_ref(), "paths");
    MATCHER.with_borrow_mut(|matcher| {
        matcher.config = config(paths);
        let mut buf = Vec::new();
        let Some(score) = pattern.score(Utf32Str::new(&haystack, &mut buf), matcher) else {
            return Ok((None, None));
        };
        let at = highlights_table(lua, &pattern, matcher, &haystack, &mut buf, &mut Vec::new())?;
        Ok((Some(score), Some(at)))
    })
}

/// Scores {needle} against every entry of {haystacks} and returns the
/// matches, best first.
///
/// Ties keep their input order, so candidates you pre-sorted (by mtime, say)
/// stay in that order for an empty needle. `index` is the 1-based position in
/// {haystacks}, and `highlights` uses the byte ranges of `fuzzy`. Entries
/// that are not valid UTF-8 are skipped.
///
/// To rank files, use `maki.fs.fuzzy_files` instead. It queries the index
/// the host already keeps, so no candidate list crosses into Lua.
///
/// Needs no plugin permission.
///
/// @param needle string What the user typed.
/// @param haystacks table Array of candidate strings.
/// @param opts table? Options:
///   `limit` (integer) keep at most this many results.
///   `paths` (boolean) rank candidates as paths, like the file picker. Off by default.
///   `highlights` (boolean) also return where the query matched, off by default since it costs a second pass.
/// @return (table) Array of `{ text, index, score, highlights? }`, best first.
/// @example
/// for _, m in ipairs(maki.text.fuzzy_list(query, names, { limit = 10 })) do
///   print(m.text, m.score)
/// end
#[lua_fn]
fn fuzzy_list(
    lua: &Lua,
    needle: String,
    haystacks: Table,
    opts: Option<Table>,
) -> LuaResult<Table> {
    let limit = opts
        .as_ref()
        .and_then(|o| o.get::<usize>("limit").ok())
        .unwrap_or(usize::MAX);
    let want_highlights = want(opts.as_ref(), "highlights");
    let paths = want(opts.as_ref(), "paths");

    let pattern = compile(&needle);
    MATCHER.with_borrow_mut(|matcher| {
        matcher.config = config(paths);
        let mut buf = Vec::new();
        // The Lua strings are held rather than copied into Rust ones: a
        // picker calls this on every keystroke over the same list.
        let mut scored: Vec<(usize, u32, LuaString)> = Vec::new();
        for (i, entry) in haystacks.sequence_values::<LuaString>().enumerate() {
            let Ok(text) = entry else { continue };
            let score = match text.to_str() {
                Ok(utf8) => pattern.score(Utf32Str::new(&utf8, &mut buf), matcher),
                Err(_) => None,
            };
            if let Some(score) = score {
                scored.push((i + 1, score, text));
            }
        }
        // A stable sort is what keeps equal scores in the caller's order.
        scored.sort_by_key(|(_, score, _)| Reverse(*score));
        scored.truncate(limit);

        let out = lua.create_table_with_capacity(scored.len(), 0)?;
        let mut scratch = Vec::new();
        for (index, score, text) in scored {
            let entry = lua.create_table_with_capacity(0, 3 + usize::from(want_highlights))?;
            entry.set("text", &text)?;
            entry.set("index", index)?;
            entry.set("score", score)?;
            if want_highlights && let Ok(utf8) = text.to_str() {
                let ranges =
                    highlights_table(lua, &pattern, matcher, &utf8, &mut buf, &mut scratch)?;
                entry.set("highlights", ranges)?;
            }
            out.push(entry)?;
        }
        Ok(out)
    })
}

lua_table! {
    /// Text utilities: format conversion, UTF-8 decoding and the fuzzy matcher
    /// the built-in pickers use.
    ///
    /// ```lua
    /// local md = maki.text.html_to_markdown(html)
    /// local plain = maki.text.html_to_text(html)
    /// local text = maki.text.to_utf8(bytes)
    /// local hits = maki.text.fuzzy_list("mrs", names, { limit = 10 })
    /// ```
    "maki.text" => pub(crate) fn create_text_table(), DOCS [
        html_to_markdown, html_to_text, to_utf8, fuzzy, fuzzy_list,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Function;
    use test_case::test_case;

    const PATHS: [&str; 3] = ["maki-ui/src/main.rs", "README.md", "docs/index.html"];
    const LATIN1_HTML: &[u8] = b"<h1>caf\xe9</h1>";
    const LOSSY_MARKDOWN: &str = "# caf\u{fffd}";
    const LATIN1_TEXT: &[u8] = b"caf\xe9 cr\xe8me";
    const LOSSY_TEXT: &str = "caf\u{fffd} cr\u{fffd}me";
    const PAGE_BODY: &str = "body";
    const TEXT_SKIP_TAGS: [&str; 3] = ["script", "style", "noscript"];
    const PADDED_TABLE: &[u8] = b"<table><tr><th>a</th><th>b</th></tr><tr><td>a much longer cell</td><td></td></tr><tr><td>x | y</td><td><code>a|b</code></td></tr><tr><td>-</td><td>-</td></tr></table>";
    const UNPADDED_TABLE: &str =
        "| a | b |\n| --- | --- |\n| a much longer cell | |\n| x \\| y | `a\\|b` |\n| \\- | \\- |";
    const CODE_IN_LAYOUT_TABLE: &[u8] = b"<table><tr><td>1</td><td><pre><code>| id  | name |\n| --- | ---- |\n| 1   | a    |</code></pre></td></tr></table>";
    const ALIGNED_CODE: &str = "1\n\n```\n| id  | name |\n| --- | ---- |\n| 1   | a    |\n```";
    const LISTS: &[u8] = b"<ul><li>one<ul><li>nested</li></ul></li></ul><ol><li>first</li></ol><pre>| kept    |</pre>";
    const COMPACT_LISTS: &str = "- one\n  - nested\n\n1. first\n\n| kept    |";
    const CHROME_PAGE: &[u8] = br#"<nav><a href="/">Home</a></nav><script type="application/json">{"menu":1}</script><p>body</p>"#;

    fn best(needle: &str) -> Option<&'static str> {
        let pattern = compile(needle);
        MATCHER.with_borrow_mut(|matcher| {
            matcher.config = config(false);
            let mut buf = Vec::new();
            let mut scored: Vec<(usize, u32)> = PATHS
                .iter()
                .enumerate()
                .filter_map(|(i, p)| Some((i, pattern.score(Utf32Str::new(p, &mut buf), matcher)?)))
                .collect();
            scored.sort_by_key(|entry| Reverse(entry.1));
            scored.first().map(|(i, _)| PATHS[*i])
        })
    }

    #[test_case("mainrs", Some("maki-ui/src/main.rs") ; "scattered_subsequence")]
    #[test_case("README", Some("README.md") ; "exact_name")]
    #[test_case("readme", Some("README.md") ; "smart_case_is_insensitive")]
    #[test_case("index", Some("docs/index.html") ; "segment_start")]
    #[test_case("zzz", None ; "no_match_at_all")]
    fn fuzzy_ranks_paths(needle: &str, want: Option<&str>) {
        assert_eq!(best(needle), want);
    }

    const NESTED: &str = "pkg/main.rs";
    const PUNCTUATED: &str = "pkg,main.rs";

    /// `paths = true` is the config the file picker ranks with, and under it a
    /// path separator is the only thing that starts a new segment. The default
    /// config hands a comma the same bonus, so these two score alike there and
    /// the nested one wins here. Without a difference this size the option
    /// would be decoration.
    #[test]
    fn the_paths_option_scores_a_separator_above_other_punctuation() {
        let pattern = compile("main");
        let score = |paths: bool, haystack: &str| {
            MATCHER.with_borrow_mut(|matcher| {
                matcher.config = config(paths);
                let mut buf = Vec::new();
                pattern
                    .score(Utf32Str::new(haystack, &mut buf), matcher)
                    .unwrap()
            })
        };
        assert_eq!(
            score(false, NESTED),
            score(false, PUNCTUATED),
            "the default config treats both as boundaries"
        );
        assert!(
            score(true, NESTED) > score(true, PUNCTUATED),
            "only a separator starts a path segment"
        );
    }

    fn lua_with_text() -> Lua {
        let lua = Lua::new();
        let text = create_text_table(&lua).unwrap();
        lua.globals().set("text", text).unwrap();
        lua
    }

    async fn to_markdown(html: &[u8], skip_tags: Option<&[&str]>) -> String {
        let lua = lua_with_text();
        let text: Table = lua.globals().get("text").unwrap();
        let convert: Function = text.get("html_to_markdown").unwrap();
        let opts = skip_tags.map(|tags| {
            lua.create_table_from([("skip_tags", tags.to_vec())])
                .unwrap()
        });
        let (markdown, error): (String, Option<String>) = convert
            .call_async((lua.create_string(html).unwrap(), opts))
            .await
            .unwrap();
        assert_eq!(error, None);
        markdown.trim().to_owned()
    }

    #[test_case(b"<h1>hello</h1>", "# hello" ; "utf8")]
    #[test_case(LATIN1_HTML, LOSSY_MARKDOWN ; "non_utf8")]
    #[tokio::test]
    async fn html_bytes_are_decoded_at_the_text_boundary(input: &[u8], expected: &str) {
        assert_eq!(to_markdown(input, None).await, expected);
    }

    #[test_case(PADDED_TABLE, UNPADDED_TABLE ; "tables_are_not_padded")]
    #[test_case(CODE_IN_LAYOUT_TABLE, ALIGNED_CODE ; "code_in_a_headerless_table_keeps_its_alignment")]
    #[test_case(LISTS, COMPACT_LISTS ; "list_markers_take_one_space")]
    #[tokio::test]
    async fn markdown_spends_no_bytes_on_alignment(input: &[u8], expected: &str) {
        assert_eq!(to_markdown(input, None).await, expected);
    }

    #[test_case(CHROME_PAGE, &["script", "nav"], PAGE_BODY ; "listed_tags")]
    #[test_case(PAGE_BODY.as_bytes(), &[], PAGE_BODY ; "empty_list")]
    #[tokio::test]
    async fn skip_tags_drop_elements_with_their_content(
        input: &[u8],
        skip_tags: &[&str],
        expected: &str,
    ) {
        assert_eq!(to_markdown(input, Some(skip_tags)).await, expected);
    }

    fn to_text(html: &str) -> String {
        let skip_tags = TEXT_SKIP_TAGS.map(str::to_owned);
        visible_text(html, &skip_tags).unwrap()
    }

    #[test_case("<div><p>Hello <b>wor</b>ld</p></div>", "Hello world" ; "inline_tags_leave_no_gap")]
    #[test_case("   <p>  lots   of\n\n spaces  </p>   ", "lots of spaces" ; "whitespace_collapses")]
    #[test_case("<h1>Title</h1><p>one</p>two<br>three", "Title\none\ntwo\nthree" ; "blocks_start_a_line")]
    #[test_case("<table><tr><td>a</td><td>b</td></tr></table>", "a\nb" ; "cells_are_blocks")]
    #[test_case("<p>fish &amp; chips &lt;3 &#233;</p>", "fish & chips <3 \u{e9}" ; "entities_decoded")]
    #[test_case(r#"<p title="a > b">x<!-- c > d -->y</p>"#, "xy" ; "gt_in_attributes_and_comments")]
    #[test_case("a<SCRIPT>var x = '<div>no</div>';</SCRIPT>b<style>p{}</style><noscript>js</noscript>", "ab" ; "skipped_tags_drop_their_content")]
    #[test_case("<p>run:</p><pre><code>fn main() {\n    body();\n}\n</code></pre><p>done</p>", "run:\nfn main() {\n    body();\n}\ndone" ; "pre_keeps_its_whitespace")]
    #[test_case("hello<div", "hello" ; "unclosed_tag")]
    #[test_case("<div><span></span></div>", "" ; "no_text")]
    fn html_to_text_keeps_what_a_reader_sees(html: &str, expected: &str) {
        assert_eq!(to_text(html), expected);
    }

    #[test_case(b"caf\xc3\xa9", "caf\u{e9}" ; "valid_utf8_is_unchanged")]
    #[test_case(LATIN1_TEXT, LOSSY_TEXT ; "invalid_bytes_become_replacement_chars")]
    fn to_utf8_decodes_lossily(input: &[u8], expected: &str) {
        let lua = lua_with_text();
        let text: Table = lua.globals().get("text").unwrap();
        let to_utf8: Function = text.get("to_utf8").unwrap();
        let decoded: String = to_utf8.call(lua.create_string(input).unwrap()).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn fuzzy_returns_nil_when_the_needle_is_absent() {
        let lua = lua_with_text();
        let got: mlua::Value = lua
            .load("return text.fuzzy('zzz', 'README.md')")
            .eval()
            .unwrap();
        assert!(got.is_nil());
    }

    /// What the ranges are for: slicing the matched text straight out.
    fn sliced(lua: &Lua, needle: &str, haystack: &str) -> Vec<String> {
        lua.load(format!(
            "local out = {{}} \
             local _, at = text.fuzzy('{needle}', '{haystack}') \
             for _, r in ipairs(at) do out[#out + 1] = ('{haystack}'):sub(r[1], r[2]) end \
             return out"
        ))
        .eval()
        .unwrap()
    }

    #[test]
    fn fuzzy_reports_one_range_per_run_of_matched_characters() {
        let lua = lua_with_text();
        let got: Vec<Vec<usize>> = lua
            .load("local _, at = text.fuzzy('rs', 'main.rs') return at")
            .eval()
            .unwrap();
        assert_eq!(
            got,
            vec![vec![6, 7]],
            "the r and the s touch, so they coalesce into one 1-based range"
        );
    }

    /// Ranges have to survive a multi-byte character earlier in the string,
    /// or a highlight drawn from them lands on the wrong character.
    #[test]
    fn fuzzy_ranges_count_bytes_not_characters() {
        let lua = lua_with_text();
        assert_eq!(sliced(&lua, "x", "ö/x"), vec!["x"]);
    }

    const COMBINING: &str = "本é x";
    const EMOJI: &str = "👍🏽 ok.rs";

    /// The matcher counts grapheme clusters, and a combining mark makes a
    /// cluster longer than one character. Counting characters here reports an
    /// offset inside the mark, which Lua then refuses to slice on.
    #[test]
    fn fuzzy_ranges_survive_a_combining_mark() {
        let lua = lua_with_text();
        assert_eq!(sliced(&lua, "x", COMBINING), vec!["x"]);
    }

    /// A skin tone modifier is a second codepoint in one cluster, so every
    /// offset after it drifts by four bytes when characters are counted.
    #[test]
    fn fuzzy_ranges_survive_a_modifier_emoji() {
        let lua = lua_with_text();
        assert_eq!(sliced(&lua, "rs", EMOJI), vec!["rs"]);
        assert_eq!(sliced(&lua, "ok", EMOJI), vec!["ok"]);
    }

    #[test]
    fn fuzzy_list_drops_what_does_not_match() {
        let lua = lua_with_text();
        let got: usize = lua
            .load("return #text.fuzzy_list('zzz', { 'a.rs', 'b.rs' })")
            .eval()
            .unwrap();
        assert_eq!(got, 0);
    }

    #[test]
    fn fuzzy_list_honours_the_limit() {
        let lua = lua_with_text();
        let got: usize = lua
            .load("return #text.fuzzy_list('rs', { 'a.rs', 'b.rs', 'c.rs' }, { limit = 2 })")
            .eval()
            .unwrap();
        assert_eq!(got, 2);
    }

    /// An empty needle is how a picker shows its candidates before anything is
    /// typed, so the caller's own order has to come back out.
    #[test]
    fn fuzzy_list_keeps_the_input_order_for_an_empty_needle() {
        let lua = lua_with_text();
        let got: Vec<String> = lua
            .load(
                "local out = {} \
                 for _, m in ipairs(text.fuzzy_list('', { 'z.rs', 'a.rs', 'm.rs' })) do \
                   out[#out + 1] = m.text \
                 end \
                 return out",
            )
            .eval()
            .unwrap();
        assert_eq!(got, vec!["z.rs", "a.rs", "m.rs"]);
    }

    #[test]
    fn fuzzy_list_reports_the_position_in_the_input() {
        let lua = lua_with_text();
        let got: usize = lua
            .load("return text.fuzzy_list('readme', { 'a.rs', 'README.md' })[1].index")
            .eval()
            .unwrap();
        assert_eq!(got, 2);
    }

    #[test]
    fn fuzzy_list_only_computes_highlights_when_asked() {
        let lua = lua_with_text();
        let without: mlua::Value = lua
            .load("return text.fuzzy_list('rs', { 'main.rs' })[1].highlights")
            .eval()
            .unwrap();
        assert!(without.is_nil());
        let with: Vec<Vec<usize>> = lua
            .load(
                "return text.fuzzy_list('rs', { 'main.rs' }, { highlights = true })[1].highlights",
            )
            .eval()
            .unwrap();
        assert_eq!(with, vec![vec![6, 7]]);
    }

    /// Lua strings are byte strings, so a list read off disk or out of a
    /// process can hold one that is not UTF-8. Failing the whole call over it
    /// would take every other candidate down with it.
    #[test]
    fn fuzzy_list_skips_an_entry_that_is_not_utf8() {
        let lua = lua_with_text();
        let got: Vec<usize> = lua
            .load(
                "local out = {} \
                 for _, m in ipairs(text.fuzzy_list('rs', { 'a.rs', '\\xff.rs', 'b.rs' })) do \
                   out[#out + 1] = m.index \
                 end \
                 return out",
            )
            .eval()
            .unwrap();
        assert_eq!(got, vec![1, 3], "the positions of the two readable ones");
    }

    /// The shared matcher is reconfigured on every borrow, so a path-aware
    /// call must not leave the next plain call ranking paths.
    #[test]
    fn the_shared_matcher_does_not_leak_its_config() {
        let lua = lua_with_text();
        let plain: u32 = lua
            .load("return (text.fuzzy('main', 'vendor/x/main.rs'))")
            .eval()
            .unwrap();
        let _: u32 = lua
            .load("return (text.fuzzy('main', 'vendor/x/main.rs', { paths = true }))")
            .eval()
            .unwrap();
        let again: u32 = lua
            .load("return (text.fuzzy('main', 'vendor/x/main.rs'))")
            .eval()
            .unwrap();
        assert_eq!(plain, again);
    }
}
