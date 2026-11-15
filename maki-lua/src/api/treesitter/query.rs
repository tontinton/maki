use std::collections::HashMap;
use std::sync::Arc;

use maki_highlight::predicates::predicates_hold;
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Lua, MultiValue, Value as LuaValue};
use regex::Regex;
use tree_sitter::{Query, QueryCursor, StreamingIterator};

use crate::docs::{FnDoc, ParamDoc};
use crate::language::Language;

use super::node::LuaNode;

#[allow(non_upper_case_globals)]
const iter_captures__doc: FnDoc = FnDoc {
    name: "iter_captures",
    args: "{node}, {source}, {start_row?}, {stop_row?}",
    desc: "Iterates over every capture matched by this query. Each call to the returned iterator yields `(capture_index, node, metadata, match, active)`. Use this when you care about individual captures rather than whole pattern matches.",
    params: &[
        ParamDoc {
            name: "{node}",
            ty: "Node",
            desc: "Root node to search within.",
        },
        ParamDoc {
            name: "{source}",
            ty: "string",
            desc: "Source text the tree was parsed from.",
        },
        ParamDoc {
            name: "{start_row?}",
            ty: "integer",
            desc: "Only match rows >= this value (0-based).",
        },
        ParamDoc {
            name: "{stop_row?}",
            ty: "integer",
            desc: "Only match rows < this value (0-based).",
        },
    ],
    returns: "(function) Iterator yielding (integer, Node, table, table, integer).",
    guard: None,
    value: false,
    example: "local q = maki.treesitter.query.parse(\"lua\", \"(identifier) @id\")\nfor idx, node, meta in q:iter_captures(root, source) do\n  print(idx, node:type())\nend",
};

#[allow(non_upper_case_globals)]
const iter_matches__doc: FnDoc = FnDoc {
    name: "iter_matches",
    args: "{node}, {source}, {start_row?}, {stop_row?}",
    desc: "Iterates over every full pattern match in this query. Each call to the returned iterator yields `(pattern_index, captures, metadata, active)` where captures is a table keyed by capture index. Use this when you need all captures for a pattern together.",
    params: &[
        ParamDoc {
            name: "{node}",
            ty: "Node",
            desc: "Root node to search within.",
        },
        ParamDoc {
            name: "{source}",
            ty: "string",
            desc: "Source text the tree was parsed from.",
        },
        ParamDoc {
            name: "{start_row?}",
            ty: "integer",
            desc: "Only match rows >= this value (0-based).",
        },
        ParamDoc {
            name: "{stop_row?}",
            ty: "integer",
            desc: "Only match rows < this value (0-based).",
        },
    ],
    returns: "(function) Iterator yielding (integer, table, table, integer).",
    guard: None,
    value: false,
    example: "local q = maki.treesitter.query.parse(\"lua\", \"(function_declaration name: (identifier) @name)\"\n)\nfor pat, captures, meta in q:iter_matches(root, source) do\n  for cap_idx, nodes in pairs(captures) do\n    print(nodes[1]:type())\n  end\nend",
};

pub(crate) struct LuaQuery {
    pub(crate) inner: Arc<Query>,
}

fn query_fields<F: mlua::UserDataFields<LuaQuery>>(fields: &mut F) {
    fields.add_field_method_get("captures", |lua, this| {
        let tbl = lua.create_table()?;
        for (i, name) in this.inner.capture_names().iter().enumerate() {
            tbl.raw_set(i + 1, *name)?;
        }
        Ok(tbl)
    });

    fields.add_field_method_get("info", |lua, _| lua.create_table());
}

fn query_methods<M: mlua::UserDataMethods<LuaQuery>>(methods: &mut M) {
    methods.add_method("iter_captures", |lua, this, args: MultiValue| {
        let parsed = IterArgs::parse(args, "iter_captures")?;
        let results = collect_captures(&this.inner, &parsed)?;
        stateful_iter(lua, results)
    });

    methods.add_method("iter_matches", |lua, this, args: MultiValue| {
        let parsed = IterArgs::parse(args, "iter_matches")?;
        let results = collect_matches(&this.inner, &parsed)?;
        stateful_iter(lua, results)
    });
}

lua_class! {
    /// A compiled tree-sitter query.
    ///
    /// Get one by calling `maki.treesitter.query.parse(lang, query_string)`.
    /// Then use `:iter_captures()` or `:iter_matches()` to run it against a syntax tree.
    ///
    /// ```lua
    /// local q = maki.treesitter.query.parse("lua", "(identifier) @id")
    /// for idx, node, meta in q:iter_captures(root, source) do
    ///   print(node:type())
    /// end
    /// ```
    "maki.treesitter.Query" => LuaQuery, QUERY_DOCS [manual iter_captures, manual iter_matches] fields query_fields, extra query_methods
}

/// Compiles a tree-sitter query string for {lang}.
/// Throws if the language is unknown or the query has a syntax error.
///
/// @param lang string Language name, e.g. `"lua"`.
/// @param query string Tree-sitter S-expression query.
/// @return (Query) Compiled query object.
/// @example
/// local q = maki.treesitter.query.parse("lua", "(identifier) @id")
#[lua_fn]
fn parse(_lua: &Lua, lang: String, query: String) -> mlua::Result<LuaQuery> {
    let ts_lang = Language::from_name(&lang)
        .ok_or_else(|| mlua::Error::runtime(format!("unknown language: {lang}")))?
        .ts_language();
    let q = Query::new(&ts_lang, &query)
        .map_err(|e| mlua::Error::runtime(format!("query parse error: {e}")))?;
    Ok(LuaQuery { inner: Arc::new(q) })
}

/// Looks up a named built-in query for {lang} (not yet implemented, always returns nil).
///
/// @param lang string Language name.
/// @param name string Query name, e.g. `"highlights"`.
/// @return (Query|nil) Query object, or nil if not found.
#[lua_fn]
fn get(_lua: &Lua, lang: String, name: String) -> mlua::Result<Option<LuaQuery>> {
    let _ = (lang, name);
    Ok(None)
}

lua_table! {
    /// Query compilation and lookup.
    ///
    /// Mirrors `vim.treesitter.query`. Use `parse()` to compile a tree-sitter
    /// query string into a `Query` object you can run against parsed trees.
    ///
    /// ```lua
    /// local q = maki.treesitter.query.parse("lua", "(string) @str")
    /// ```
    "maki.treesitter.query" => pub(crate) fn create_query_module(), DOCS [
        parse, get,
    ]
}

struct IterArgs {
    lua_node: LuaNode,
    source: String,
    start_row: Option<usize>,
    stop_row: Option<usize>,
}

impl IterArgs {
    fn parse(args: MultiValue, fn_name: &str) -> mlua::Result<Self> {
        let mut args_iter = args.into_iter();

        let node_ud = args_iter
            .next()
            .and_then(|v| v.as_userdata().cloned())
            .ok_or_else(|| mlua::Error::runtime(format!("{fn_name}: expected node as arg 1")))?;
        let lua_node = node_ud.borrow::<LuaNode>()?;

        let source = args_iter
            .next()
            .and_then(|v| match v {
                LuaValue::String(s) => s.to_str().ok().map(|s| s.to_owned()),
                _ => None,
            })
            .ok_or_else(|| mlua::Error::runtime(format!("{fn_name}: expected source as arg 2")))?;

        let start_row = args_iter.next().and_then(lua_to_usize);
        let stop_row = args_iter.next().and_then(lua_to_usize);

        Ok(Self {
            lua_node: lua_node.clone(),
            source,
            start_row,
            stop_row,
        })
    }
}

trait IterEntry: Send + 'static {
    fn into_lua_values(self, lua: &mlua::Lua) -> mlua::Result<MultiValue>;
}

struct CaptureEntry {
    capture_index: u32,
    node: LuaNode,
    metadata: HashMap<String, String>,
}

impl IterEntry for CaptureEntry {
    fn into_lua_values(self, lua: &mlua::Lua) -> mlua::Result<MultiValue> {
        let meta_table = lua.create_table()?;
        for (k, v) in &self.metadata {
            meta_table.set(k.as_str(), v.as_str())?;
        }
        Ok(MultiValue::from_iter([
            lua.pack(self.capture_index + 1)?,
            lua.pack(self.node)?,
            LuaValue::Table(meta_table),
            LuaValue::Table(lua.create_table()?),
            LuaValue::Integer(1),
        ]))
    }
}

struct MatchEntry {
    pattern_index: usize,
    captures: HashMap<u32, Vec<LuaNode>>,
    metadata: HashMap<String, String>,
}

impl IterEntry for MatchEntry {
    fn into_lua_values(self, lua: &mlua::Lua) -> mlua::Result<MultiValue> {
        let captures_table = lua.create_table()?;
        for (cap_idx, nodes) in self.captures {
            let nodes_table = lua.create_table()?;
            for (j, n) in nodes.into_iter().enumerate() {
                nodes_table.raw_set(j + 1, n)?;
            }
            captures_table.raw_set((cap_idx as i64) + 1, nodes_table)?;
        }
        let meta_table = lua.create_table()?;
        for (k, v) in &self.metadata {
            meta_table.set(k.as_str(), v.as_str())?;
        }
        Ok(MultiValue::from_iter([
            lua.pack(self.pattern_index + 1)?,
            LuaValue::Table(captures_table),
            LuaValue::Table(meta_table),
            LuaValue::Integer(1),
        ]))
    }
}

fn stateful_iter<E: IterEntry>(lua: &mlua::Lua, results: Vec<E>) -> mlua::Result<mlua::Function> {
    let mut results = results.into_iter();
    lua.create_function_mut(move |lua, ()| {
        results
            .next()
            .map_or_else(|| Ok(MultiValue::new()), |entry| entry.into_lua_values(lua))
    })
}

fn new_cursor(start_row: Option<usize>, stop_row: Option<usize>) -> QueryCursor {
    let mut cursor = QueryCursor::new();
    if let Some(start) = start_row {
        let end = stop_row.unwrap_or(usize::MAX);
        cursor.set_point_range(tree_sitter::Point::new(start, 0)..tree_sitter::Point::new(end, 0));
    }
    cursor
}

fn collect_captures(query: &Query, args: &IterArgs) -> mlua::Result<Vec<CaptureEntry>> {
    let source_bytes = args.source.as_bytes();
    let mut cursor = new_cursor(args.start_row, args.stop_row);
    let mut regex_cache = HashMap::new();
    let mut results = Vec::new();

    let node = args.lua_node.ts_node()?;
    let mut captures = cursor.captures(query, node, source_bytes);
    while let Some((m, capture_idx)) = captures.next() {
        let mut metadata = HashMap::new();
        if !evaluate_predicates(
            query,
            m.pattern_index,
            m.captures(),
            source_bytes,
            &mut metadata,
            &mut regex_cache,
        ) {
            continue;
        }
        let capture = &m.captures()[*capture_idx];
        results.push(CaptureEntry {
            capture_index: capture.index,
            node: LuaNode::new(capture.node, Arc::clone(&args.lua_node.tree)),
            metadata,
        });
    }
    Ok(results)
}

fn collect_matches(query: &Query, args: &IterArgs) -> mlua::Result<Vec<MatchEntry>> {
    let source_bytes = args.source.as_bytes();
    let mut cursor = new_cursor(args.start_row, args.stop_row);
    let mut regex_cache = HashMap::new();
    let mut results = Vec::new();

    let node = args.lua_node.ts_node()?;
    let mut matches = cursor.matches(query, node, source_bytes);
    while let Some(m) = matches.next() {
        let mut metadata = HashMap::new();
        if !evaluate_predicates(
            query,
            m.pattern_index,
            m.captures(),
            source_bytes,
            &mut metadata,
            &mut regex_cache,
        ) {
            continue;
        }
        let mut captures_map: HashMap<u32, Vec<LuaNode>> = HashMap::new();
        for capture in m.captures() {
            captures_map
                .entry(capture.index)
                .or_default()
                .push(LuaNode::new(capture.node, Arc::clone(&args.lua_node.tree)));
        }
        results.push(MatchEntry {
            pattern_index: m.pattern_index,
            captures: captures_map,
            metadata,
        });
    }
    Ok(results)
}

fn evaluate_predicates(
    query: &Query,
    pattern_index: usize,
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &[u8],
    metadata: &mut HashMap<String, String>,
    regex_cache: &mut HashMap<String, Option<Regex>>,
) -> bool {
    for prop in query.property_settings(pattern_index) {
        if let Some(val) = &prop.value {
            metadata.insert(prop.key.to_string(), val.to_string());
        }
    }
    predicates_hold(query, pattern_index, captures, source, regex_cache)
}

fn lua_to_usize(v: LuaValue) -> Option<usize> {
    match v {
        LuaValue::Integer(n) => Some(n as usize),
        LuaValue::Number(n) => Some(n as usize),
        _ => None,
    }
}
