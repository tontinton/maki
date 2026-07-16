pub(crate) mod language;
pub(crate) mod language_tree;
pub(crate) mod node;
pub(crate) mod query;
pub(crate) mod tree;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{AnyUserData, Lua, Result as LuaResult, Table};

use crate::language::Language;
use language_tree::LuaLanguageTree;
use node::LuaNode;

fn parse_impl(
    lua: &Lua,
    source: String,
    lang_name: String,
) -> LuaResult<(mlua::Value, mlua::Value)> {
    let Some(lang) = Language::from_name(&lang_name) else {
        return Ok((
            mlua::Value::Nil,
            mlua::Value::String(lua.create_string(format!("no language registered: {lang_name}"))?),
        ));
    };
    Ok((
        mlua::Value::UserData(lua.create_userdata(LuaLanguageTree::new(
            source.into(),
            lang_name.into(),
            lang,
        ))?),
        mlua::Value::Nil,
    ))
}

/// Creates a `LanguageTree` for {source} using the grammar named {lang}.
/// This is the main entry point for parsing source code with tree-sitter.
/// Signature matches `vim.treesitter.get_parser()`, so Neovim plugins can be copy-pasted.
///
/// @param source string Source text to parse.
/// @param lang string Language name, e.g. `"rust"` or `"lua"`.
/// @return (LanguageTree|nil, string|nil) Parser, or nil and an error message.
/// @example
/// local parser, err = maki.treesitter.get_parser(src, "lua")
/// if err then print("error: " .. err) end
#[lua_fn]
fn get_parser(lua: &Lua, source: String, lang: String) -> LuaResult<(mlua::Value, mlua::Value)> {
    parse_impl(lua, source, lang)
}

/// Alias for `get_parser`. Use whichever name you prefer.
///
/// @param source string Source text to parse.
/// @param lang string Language name.
/// @return (LanguageTree|nil, string|nil) Parser, or nil and an error message.
#[lua_fn]
fn get_string_parser(
    lua: &Lua,
    source: String,
    lang: String,
) -> LuaResult<(mlua::Value, mlua::Value)> {
    parse_impl(lua, source, lang)
}

/// Gets the text that {node} covers in {source}.
/// Useful when you have a captured node and need the actual source substring.
///
/// @param node Node The node whose text you want.
/// @param source string Original source text the tree was parsed from.
/// @return (string) Substring covered by the node.
/// @example
/// local text = maki.treesitter.get_node_text(node, source)
/// print(text)
#[lua_fn]
fn get_node_text(_lua: &Lua, node: AnyUserData, source: String) -> LuaResult<String> {
    let lua_node = node.borrow::<LuaNode>()?;
    let start = lua_node.node.start_byte();
    let end = lua_node.node.end_byte();
    if end > source.len() {
        return Err(mlua::Error::runtime("node range exceeds source length"));
    }
    Ok(source[start..end].to_owned())
}

/// Returns the range of {node} as four 0-based integers: start_row, start_col, end_row, end_col.
///
/// @param node Node The node to query.
/// @return (integer, integer, integer, integer) start_row, start_col, end_row, end_col.
/// @example
/// local sr, sc, er, ec = maki.treesitter.get_node_range(node)
#[lua_fn]
fn get_node_range(_lua: &Lua, node: AnyUserData) -> LuaResult<(i64, i64, i64, i64)> {
    let n = node.borrow::<LuaNode>()?;
    let sp = n.node.start_position();
    let ep = n.node.end_position();
    Ok((
        sp.row as i64,
        sp.column as i64,
        ep.row as i64,
        ep.column as i64,
    ))
}

/// Returns a six-element table for {node}: `{start_row, start_col, start_byte, end_row, end_col, end_byte}`.
/// This gives you byte offsets in addition to row/column positions.
///
/// @param node Node The node to query.
/// @return (table) Six-element array: start_row, start_col, start_byte, end_row, end_col, end_byte.
/// @example
/// local r = maki.treesitter.get_range(node)
/// print("bytes: " .. r[3] .. "-" .. r[6])
#[lua_fn]
fn get_range(lua: &Lua, node: AnyUserData) -> LuaResult<Table> {
    let n = node.borrow::<LuaNode>()?;
    let sp = n.node.start_position();
    let ep = n.node.end_position();
    let tbl = lua.create_table()?;
    tbl.set(1, sp.row as i64)?;
    tbl.set(2, sp.column as i64)?;
    tbl.set(3, n.node.start_byte() as i64)?;
    tbl.set(4, ep.row as i64)?;
    tbl.set(5, ep.column as i64)?;
    tbl.set(6, n.node.end_byte() as i64)?;
    Ok(tbl)
}

/// Checks whether {dest} is an ancestor of {source} (or the same node).
/// Walks up from {source} toward the root looking for {dest}.
///
/// @param dest Node Potential ancestor node.
/// @param source Node Node to check ancestry for.
/// @return (boolean)
#[lua_fn]
fn is_ancestor(_lua: &Lua, dest: AnyUserData, source: AnyUserData) -> LuaResult<bool> {
    let dest = dest.borrow::<LuaNode>()?;
    let source = source.borrow::<LuaNode>()?;
    let mut current = Some(source.node);
    while let Some(node) = current {
        if node.id() == dest.node.id() {
            return Ok(true);
        }
        current = node.parent();
    }
    Ok(false)
}

/// Checks whether the 0-based position ({line}, {col}) falls inside {node}.
/// Handy for cursor-position checks.
///
/// @param node Node Node to test against.
/// @param line integer 0-based line number.
/// @param col integer 0-based column number.
/// @return (boolean)
#[lua_fn]
fn is_in_node_range(_lua: &Lua, node: AnyUserData, line: usize, col: usize) -> LuaResult<bool> {
    let n = node.borrow::<LuaNode>()?;
    let sp = n.node.start_position();
    let ep = n.node.end_position();
    Ok((line > sp.row || (line == sp.row && col >= sp.column))
        && (line < ep.row || (line == ep.row && col < ep.column)))
}

/// Checks whether {node} fully contains the given {range}.
///
/// @param node Node Node to test.
/// @param range table Four-element array `{start_row, start_col, end_row, end_col}`.
/// @return (boolean)
#[lua_fn]
fn node_contains(_lua: &Lua, node: AnyUserData, range: Table) -> LuaResult<bool> {
    let n = node.borrow::<LuaNode>()?;
    let sr: usize = range.get(1)?;
    let sc: usize = range.get(2)?;
    let er: usize = range.get(3)?;
    let ec: usize = range.get(4)?;
    let sp = n.node.start_position();
    let ep = n.node.end_position();
    Ok((sr > sp.row || (sr == sp.row && sc >= sp.column))
        && (er < ep.row || (er == ep.row && ec <= ep.column)))
}

/// Placeholder for cursor-based node lookup (not yet implemented, always returns nil).
///
/// @param opts table? Options (currently unused).
/// @return (Node|nil) Always nil.
#[lua_fn]
fn get_node(_lua: &Lua, opts: Option<Table>) -> LuaResult<Option<LuaNode>> {
    let _ = opts;
    Ok(None)
}

lua_table! {
    /// Tree-sitter parsing and query API.
    ///
    /// Mirrors `vim.treesitter` from Neovim, so plugins can be shared between the two.
    /// Start with `get_parser()` to parse source code, then use `get_node_text()` and
    /// the `query` sub-module to extract information from the syntax tree.
    ///
    /// ```lua
    /// local parser, err = maki.treesitter.get_parser(source, "lua")
    /// local trees = parser:parse()
    /// local root = trees[1]:root()
    /// ```
    extend "maki.treesitter" => pub(crate) fn add_treesitter_fns(), DOCS [
        get_parser, get_string_parser, get_node_text, get_node_range, get_range,
        is_ancestor, is_in_node_range, node_contains, get_node,
    ]
}

pub(crate) fn create_treesitter_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;
    add_treesitter_fns(&t, lua)?;
    t.set("language", language::create_language_module(lua)?)?;
    t.set("query", query::create_query_module(lua)?)?;
    Ok(t)
}
