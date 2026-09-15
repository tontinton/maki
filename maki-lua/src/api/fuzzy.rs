//! `maki.fn.matchfuzzy` / `maki.fn.matchfuzzypos`, Neovim's fuzzy list
//! filters backed by nucleo — the same matcher the file picker uses, so a
//! plugin ranking candidates agrees with the rest of the UI.

use std::cmp::Reverse;

use maki_lua_macro::lua_fn;
use mlua::{Lua, Result as LuaResult, Table, Value};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// One ranked candidate: its 1-based position in the input list, nucleo's
/// score, and the 0-based char offsets of the needle inside the haystack.
struct Hit {
    index: usize,
    score: u32,
    indices: Vec<u32>,
}

struct Opts {
    key: Option<String>,
    limit: Option<usize>,
    path: bool,
}

fn parse_opts(opts: Option<&Table>) -> LuaResult<Opts> {
    let Some(t) = opts else {
        return Ok(Opts {
            key: None,
            limit: None,
            path: false,
        });
    };
    Ok(Opts {
        key: t.get::<Option<String>>("key")?,
        limit: t.get::<Option<usize>>("limit")?,
        path: t.get::<Option<bool>>("path")?.unwrap_or(false),
    })
}

/// The text to match {item} on. A table entry with no value at `key` has
/// nothing to match and drops out, like Neovim's.
fn candidate(item: &Value, key: Option<&str>, i: usize) -> LuaResult<Option<String>> {
    match item {
        Value::String(s) => Ok(Some(s.to_str()?.to_owned())),
        Value::Table(t) => match key {
            Some(key) => Ok(t.get::<Option<String>>(key)?),
            None => Err(mlua::Error::runtime(format!(
                "matchfuzzy: item {i} is a table; pass `key` to name the field to match on"
            ))),
        },
        other => Err(mlua::Error::runtime(format!(
            "matchfuzzy: item {i} must be a string or table, got {}",
            other.type_name()
        ))),
    }
}

fn rank(list: &Table, needle: &str, opts: &Opts) -> LuaResult<Vec<Hit>> {
    let config = if opts.path {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    let mut matcher = Matcher::new(config);
    let atom = Atom::new(
        needle,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );

    let mut hits = Vec::new();
    let mut buf = Vec::new();
    let mut indices = Vec::new();
    for (i, item) in list.sequence_values::<Value>().enumerate() {
        let item = item?;
        let Some(text) = candidate(&item, opts.key.as_deref(), i + 1)? else {
            continue;
        };
        if needle.is_empty() {
            // Neovim hands an empty needle the list back untouched; there is
            // nothing to score and every entry keeps its place.
            hits.push(Hit {
                index: i + 1,
                score: 0,
                indices: Vec::new(),
            });
            continue;
        }
        buf.clear();
        indices.clear();
        let haystack = Utf32Str::new(&text, &mut buf);
        if let Some(score) = atom.indices(haystack, &mut matcher, &mut indices) {
            indices.sort_unstable();
            indices.dedup();
            hits.push(Hit {
                index: i + 1,
                score: u32::from(score),
                indices: indices.clone(),
            });
        }
    }

    // Stable, so equal scores keep the caller's order: hand this an
    // mtime-sorted file list and recency breaks ties for free.
    hits.sort_by_key(|h| Reverse(h.score));
    if let Some(limit) = opts.limit {
        hits.truncate(limit);
    }
    Ok(hits)
}

/// Filter {list} down to the entries that fuzzy-match {needle}, best match
/// first. Mirrors Neovim's `vim.fn.matchfuzzy`.
///
/// Matching is nucleo's: the needle's characters must appear in order but
/// need not be adjacent, so `"fmini"` finds `"plugins/file_mention/init.lua"`.
/// Scoring rewards matches on word starts and, with `path`, on the basename.
///
/// @param list table List of strings, or of tables when `key` is set.
/// @param needle string Text to search for. An empty needle returns {list} unchanged.
/// @param opts table? Optional settings:
///   `key` (string?) field to match on when {list} holds tables.
///   `limit` (integer?) keep at most this many matches.
///   `path` (boolean?) score the entries as file paths: favour the basename
///   and characters right after a `/`.
/// @return (table) The matching entries, best match first.
/// @example
/// maki.fn.matchfuzzy({ "src/main.rs", "docs/readme.md" }, "srmn", { path = true })
/// -- { "src/main.rs" }
#[lua_fn]
fn matchfuzzy(lua: &Lua, list: Table, needle: String, opts: Option<Table>) -> LuaResult<Table> {
    let opts = parse_opts(opts.as_ref())?;
    let hits = rank(&list, &needle, &opts)?;
    let out = lua.create_table_with_capacity(hits.len(), 0)?;
    for (i, hit) in hits.iter().enumerate() {
        out.set(i + 1, list.get::<Value>(hit.index)?)?;
    }
    Ok(out)
}

/// Like `matchfuzzy`, but also reports where each entry matched. Mirrors
/// Neovim's `vim.fn.matchfuzzypos`: returns `{ matches, positions, scores }`,
/// three parallel lists.
///
/// Positions are 0-based character offsets into the matched text, ascending —
/// the shape a renderer needs to highlight the matched characters.
///
/// @param list table List of strings, or of tables when `key` is set.
/// @param needle string Text to search for. An empty needle returns {list} unchanged, with no positions.
/// @param opts table? Same settings as `matchfuzzy`: `key`, `limit`, `path`.
/// @return (table) `{ matches, positions, scores }`.
/// @example
/// local matches, positions = unpack(maki.fn.matchfuzzypos({ "init.lua" }, "iua"))
/// -- matches = { "init.lua" }, positions = { { 0, 6, 7 } }
#[lua_fn]
fn matchfuzzypos(lua: &Lua, list: Table, needle: String, opts: Option<Table>) -> LuaResult<Table> {
    let opts = parse_opts(opts.as_ref())?;
    let hits = rank(&list, &needle, &opts)?;

    let matches = lua.create_table_with_capacity(hits.len(), 0)?;
    let positions = lua.create_table_with_capacity(hits.len(), 0)?;
    let scores = lua.create_table_with_capacity(hits.len(), 0)?;
    for (i, hit) in hits.iter().enumerate() {
        matches.set(i + 1, list.get::<Value>(hit.index)?)?;
        positions.set(
            i + 1,
            lua.create_sequence_from(hit.indices.iter().copied())?,
        )?;
        scores.set(i + 1, hit.score)?;
    }
    lua.create_sequence_from([matches, positions, scores])
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::{matchfuzzy__register, matchfuzzypos__register};

    /// A Lua state with both fuzzy functions on a global `f` table.
    fn lua() -> Lua {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        matchfuzzy__register(&t, &lua).unwrap();
        matchfuzzypos__register(&t, &lua).unwrap();
        lua.globals().set("f", t).unwrap();
        lua
    }

    fn strings(lua: &Lua, chunk: &str) -> Vec<String> {
        lua.load(chunk).eval::<Vec<String>>().unwrap()
    }

    #[test]
    fn matches_a_subsequence_across_path_separators() {
        let lua = lua();
        // The glob this replaces could not do it: `*` never crosses `/`.
        let got = strings(
            &lua,
            r#"return f.matchfuzzy({ "plugins/file_mention/init.lua", "src/main.rs" }, "fmini")"#,
        );
        assert_eq!(got, ["plugins/file_mention/init.lua"]);
    }

    #[test]
    fn basename_match_outranks_a_scattered_path_match() {
        let lua = lua();
        let got = strings(
            &lua,
            r#"return f.matchfuzzy(
                 { "crates/init/src/lua/away.rs", "config/init.lua" },
                 "init.lua",
                 { path = true }
               )"#,
        );
        assert_eq!(got[0], "config/init.lua");
    }

    #[test]
    fn empty_needle_returns_the_list_unchanged() {
        let lua = lua();
        let got = strings(&lua, r#"return f.matchfuzzy({ "b", "a", "c" }, "")"#);
        assert_eq!(got, ["b", "a", "c"]);
    }

    #[test]
    fn limit_caps_the_result() {
        let lua = lua();
        let got = strings(
            &lua,
            r#"return f.matchfuzzy({ "ab", "acb", "adcb" }, "ab", { limit = 2 })"#,
        );
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn key_selects_the_field_on_table_items() {
        let lua = lua();
        let got = lua
            .load(
                r#"local m = f.matchfuzzy(
                     { { name = "alpha" }, { name = "beta" } }, "alp", { key = "name" }
                   )
                   return m[1].name"#,
            )
            .eval::<String>()
            .unwrap();
        assert_eq!(got, "alpha");
    }

    #[test]
    fn table_items_without_key_are_a_programmer_error() {
        let lua = lua();
        let err = lua
            .load(r#"return f.matchfuzzy({ { name = "alpha" } }, "alp")"#)
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass `key`"), "unexpected error: {err}");
    }

    #[test]
    fn equal_scores_keep_the_input_order() {
        let lua = lua();
        // Same needle, same shape: only the caller's ordering can break the
        // tie, which is what lets an mtime-sorted list stay recency-ordered.
        let got = strings(&lua, r#"return f.matchfuzzy({ "xa", "ya", "za" }, "a")"#);
        assert_eq!(got, ["xa", "ya", "za"]);
    }

    #[test]
    fn matchfuzzypos_reports_ascending_char_offsets() {
        let lua = lua();
        let got = lua
            .load(
                r#"local m, pos = table.unpack(f.matchfuzzypos({ "init.lua" }, "iua"))
                   return table.concat(pos[1], ",") .. "|" .. m[1]"#,
            )
            .eval::<String>()
            .unwrap();
        let (offsets, matched) = got.split_once('|').unwrap();
        assert_eq!(matched, "init.lua");
        let offsets: Vec<usize> = offsets.split(',').map(|s| s.parse().unwrap()).collect();
        assert_eq!(offsets.len(), 3, "one offset per needle char: {offsets:?}");
        assert!(
            offsets.windows(2).all(|w| w[0] < w[1]),
            "offsets must ascend: {offsets:?}"
        );
        let chars: Vec<char> = "init.lua".chars().collect();
        assert_eq!(offsets.iter().map(|&i| chars[i]).collect::<String>(), "iua");
    }

    #[test]
    fn matchfuzzypos_offsets_are_chars_not_bytes() {
        let lua = lua();
        let got = lua
            .load(
                r#"local _, pos = table.unpack(f.matchfuzzypos({ "héllo" }, "o"))
                   return pos[1][1]"#,
            )
            .eval::<usize>()
            .unwrap();
        // "héllo" is 6 bytes but 5 chars; the final "o" is char 4.
        assert_eq!(got, 4);
    }

    #[test]
    fn non_matching_entries_are_dropped() {
        let lua = lua();
        let got = strings(&lua, r#"return f.matchfuzzy({ "alpha", "beta" }, "zzz")"#);
        assert!(got.is_empty(), "expected no matches, got {got:?}");
    }
}
