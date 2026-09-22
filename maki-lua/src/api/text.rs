use std::cell::RefCell;
use std::cmp::Reverse;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::util::pair::{Pair, pair};

thread_local! {
    /// `Matcher::new` allocates a score matrix of about 135KB, so building one
    /// per call would make the obvious loop over candidates allocate once per
    /// candidate. Its config is overwritten on every borrow, which is the only
    /// thing that differs between callers.
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

/// Convert an HTML string to Markdown.
/// Useful for cleaning up web content fetched with `maki.webfetch`.
///
/// @param html string HTML source text.
/// @return (string?, string?) Markdown text on success, or nil plus an error message.
/// @example
/// local md, err = maki.text.html_to_markdown("<h1>Hello</h1><p>world</p>")
/// if err then return end
/// print(md) -- "# Hello\n\nworld"
#[lua_fn]
fn html_to_markdown(_lua: &Lua, html: String) -> LuaResult<Pair<String>> {
    Ok(pair(
        htmd::convert(&html).map_err(|e| format!("html_to_markdown: {e}")),
    ))
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
    opts.and_then(|o| o.get::<bool>(key).ok()).unwrap_or(false)
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
        let mut scored: Vec<(usize, u32, mlua::String)> = Vec::new();
        for (i, entry) in haystacks.sequence_values::<mlua::String>().enumerate() {
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
    /// Text utilities: format conversion and the fuzzy matcher the built-in
    /// pickers use.
    ///
    /// ```lua
    /// local md = maki.text.html_to_markdown(html)
    /// local hits = maki.text.fuzzy_list("mrs", names, { limit = 10 })
    /// ```
    "maki.text" => pub(crate) fn create_text_table(), DOCS [
        html_to_markdown, fuzzy, fuzzy_list,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PATHS: [&str; 3] = ["maki-ui/src/main.rs", "README.md", "docs/index.html"];

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
