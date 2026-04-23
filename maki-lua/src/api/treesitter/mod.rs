pub(crate) mod language;
pub(crate) mod language_tree;
pub(crate) mod node;
pub(crate) mod query;
pub(crate) mod tree;

use mlua::{AnyUserData, Lua, Result as LuaResult, Table};

use crate::language::Language;
use language_tree::LuaLanguageTree;
use node::LuaNode;

pub(crate) fn create_treesitter_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    let get_parser = lua.create_function(move |_, (source, lang_name): (String, String)| {
        let lang = Language::from_name(&lang_name)
            .ok_or_else(|| mlua::Error::runtime(format!("no language registered: {lang_name}")))?;
        Ok(LuaLanguageTree::new(source.into(), lang_name.into(), lang))
    })?;

    t.set("get_parser", get_parser.clone())?;
    t.set("get_string_parser", get_parser)?;

    t.set(
        "get_node_text",
        lua.create_function(|_, (node_ud, source): (AnyUserData, String)| {
            let lua_node = node_ud.borrow::<LuaNode>()?;
            let start = lua_node.node.start_byte();
            let end = lua_node.node.end_byte();
            if end > source.len() {
                return Err(mlua::Error::runtime("node range exceeds source length"));
            }
            Ok(source[start..end].to_owned())
        })?,
    )?;

    t.set(
        "get_node_range",
        lua.create_function(|_, node_ud: AnyUserData| {
            let n = node_ud.borrow::<LuaNode>()?;
            let sp = n.node.start_position();
            let ep = n.node.end_position();
            Ok((
                sp.row as i64,
                sp.column as i64,
                ep.row as i64,
                ep.column as i64,
            ))
        })?,
    )?;

    t.set(
        "get_range",
        lua.create_function(|lua, (node_ud,): (AnyUserData,)| {
            let n = node_ud.borrow::<LuaNode>()?;
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
        })?,
    )?;

    t.set(
        "is_ancestor",
        lua.create_function(|_, (dest, source): (AnyUserData, AnyUserData)| {
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
        })?,
    )?;

    t.set(
        "is_in_node_range",
        lua.create_function(|_, (node_ud, line, col): (AnyUserData, usize, usize)| {
            let n = node_ud.borrow::<LuaNode>()?;
            let sp = n.node.start_position();
            let ep = n.node.end_position();
            let in_range = (line > sp.row || (line == sp.row && col >= sp.column))
                && (line < ep.row || (line == ep.row && col < ep.column));
            Ok(in_range)
        })?,
    )?;

    t.set(
        "node_contains",
        lua.create_function(|_, (node_ud, range): (AnyUserData, Table)| {
            let n = node_ud.borrow::<LuaNode>()?;
            let sr: usize = range.get(1)?;
            let sc: usize = range.get(2)?;
            let er: usize = range.get(3)?;
            let ec: usize = range.get(4)?;
            let sp = n.node.start_position();
            let ep = n.node.end_position();
            let contains = (sr > sp.row || (sr == sp.row && sc >= sp.column))
                && (er < ep.row || (er == ep.row && ec <= ep.column));
            Ok(contains)
        })?,
    )?;

    t.set(
        "get_node",
        lua.create_function(|_, _opts: Option<Table>| -> mlua::Result<Option<LuaNode>> {
            Ok(None)
        })?,
    )?;

    t.set("language", language::create_language_module(lua)?)?;

    t.set("query", query::create_query_module(lua)?)?;

    Ok(t)
}
