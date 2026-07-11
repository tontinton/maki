use mlua::{Lua, Result as LuaResult, Table};

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use std::cell::RefCell;

thread_local! {
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

pub(crate) fn create_text_table(lua: &Lua) -> LuaResult<Table> {
    let text = lua.create_table()?;

    text.set(
        "html_to_markdown",
        lua.create_function(|lua, html: String| match htmd::convert(&html) {
            Ok(md) => Ok((
                mlua::Value::String(lua.create_string(&md)?),
                mlua::Value::Nil,
            )),
            Err(e) => Ok((
                mlua::Value::Nil,
                mlua::Value::String(lua.create_string(format!("html_to_markdown: {e}"))?),
            )),
        })?,
    )?;

    text.set(
        "fuzzy_match",
        lua.create_function(|_, (query, haystack): (String, String)| {
            if query.is_empty() {
                return Ok(Some(Vec::new()));
            }
            let atom = Atom::new(
                &query,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            );
            let mut buf = Vec::new();
            let hay = Utf32Str::new(&haystack, &mut buf);
            let mut indices = Vec::new();
            let matched = MATCHER.with(|m| {
                atom.indices(hay, &mut m.borrow_mut(), &mut indices).is_some()
            });
            if !matched {
                return Ok(None);
            }
            let positions: Vec<usize> = indices.iter().map(|&i| i as usize + 1).collect();
            Ok(Some(positions))
        })?,
    )?;

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FUZZY_MISS: &str = "fuzzy_match should return nil for non-subsequence";

    fn fuzzy_match(lua: &Lua, query: &str, haystack: &str) -> Option<Vec<usize>> {
        let table = create_text_table(lua).unwrap();
        let func: mlua::Function = table.get("fuzzy_match").unwrap();
        let result: Option<Vec<usize>> = func.call((query, haystack)).unwrap();
        result
    }

    #[test]
    fn fuzzy_match_returns_one_indexed_positions() {
        let lua = Lua::new();
        let positions = fuzzy_match(&lua, "lph", "alpha").expect("lph is a subsequence of alpha");
        assert_eq!(positions, vec![2, 3, 4]);
    }

    #[test]
    fn fuzzy_match_rejects_non_subsequence() {
        let lua = Lua::new();
        assert!(fuzzy_match(&lua, "ab", "ba").is_none(), "{FUZZY_MISS}");
    }

    #[test]
    fn fuzzy_match_empty_query_returns_empty() {
        let lua = Lua::new();
        let positions = fuzzy_match(&lua, "", "alpha").expect("empty query matches");
        assert!(positions.is_empty(), "empty query yields no positions");
    }
}
