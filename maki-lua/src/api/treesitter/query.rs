use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use maki_highlight::predicates::predicates_hold;
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Lua, MultiValue, Value as LuaValue};
use regex::Regex;
use tree_sitter::{Language as TsLanguage, Query, QueryCursor, StreamingIterator};

use crate::api::util::pair::{Pair, try_pair};
use crate::docs::{FnDoc, ParamDoc};

use super::language::resolve_language;
use super::node::LuaNode;

const HIGHLIGHTS_QUERY: &str = "highlights";
const UNKNOWN_LANGUAGE: &str = "unknown language";
const QUERY_PARSE_ERROR: &str = "query parse error";

struct SetQuery {
    owner: Arc<str>,
    query: Arc<Query>,
}

/// Keyed by language and query name.
type QueryRegistry = HashMap<(String, String), SetQuery>;

/// Queries `set` replaced. Shared by all plugins, like in Neovim, but each
/// leaves with the plugin that set it.
static SET_QUERIES: LazyLock<Mutex<QueryRegistry>> = LazyLock::new(Mutex::default);

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
    let q = known_language(&lang)
        .and_then(|ts_lang| compile(&ts_lang, &query))
        .map_err(mlua::Error::runtime)?;
    Ok(LuaQuery { inner: Arc::new(q) })
}

fn known_language(lang: &str) -> Result<TsLanguage, String> {
    resolve_language(lang).ok_or_else(|| format!("{UNKNOWN_LANGUAGE}: {lang}"))
}

fn compile(ts_lang: &TsLanguage, query: &str) -> Result<Query, String> {
    Query::new(ts_lang, query).map_err(|e| format!("{QUERY_PARSE_ERROR}: {e}"))
}

/// Looks up the query {name} for {lang} that `set()` gave it. There are no
/// built-in queries yet, so anything else returns nil.
///
/// @param lang string Language name.
/// @param name string Query name, e.g. `"highlights"`.
/// @return (Query|nil) Query object, or nil if not found.
#[lua_fn]
fn get(_lua: &Lua, lang: String, name: String) -> mlua::Result<Option<LuaQuery>> {
    let queries = SET_QUERIES
        .lock()
        .map_err(|_| mlua::Error::runtime("query registry lock poisoned"))?;
    Ok(queries.get(&(lang, name)).map(|set| LuaQuery {
        inner: Arc::clone(&set.query),
    }))
}

/// Sets the query {name} for {lang} to {text}, like `vim.treesitter.query.set`.
/// A `highlights` query makes code in {lang} highlight with tree-sitter instead
/// of the built-in syntaxes, in chat code blocks and `maki.ui.highlight`.
///
/// @param lang string Language name.
/// @param name string Query name, e.g. `"highlights"`.
/// @param text string Tree-sitter S-expression query.
/// @return (boolean?, string?) `true`, or nil plus an error if {lang} is unknown or the query has a syntax error.
/// @example
/// maki.treesitter.language.add("nu", { path = "~/.local/share/nvim/site/parser/nu.so" })
/// local text = assert(maki.fs.read("~/.local/share/nvim/site/queries/nu/highlights.scm"))
/// maki.treesitter.query.set("nu", "highlights", text)
#[lua_fn]
fn set(
    _lua: &Lua,
    #[ctx] plugin: Arc<str>,
    lang: String,
    name: String,
    text: String,
) -> mlua::Result<Pair<bool>> {
    let ts_lang = try_pair!(known_language(&lang));
    let query = Arc::new(try_pair!(compile(&ts_lang, &text)));
    if name == HIGHLIGHTS_QUERY {
        maki_highlight::treesitter::register(&plugin, &lang, ts_lang, Arc::clone(&query));
    }
    SET_QUERIES
        .lock()
        .map_err(|_| mlua::Error::runtime("query registry lock poisoned"))?
        .insert(
            (lang, name),
            SetQuery {
                owner: plugin,
                query,
            },
        );
    Ok((Some(true), None))
}

pub(crate) fn forget_plugin_queries(plugin: &str) {
    SET_QUERIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, set| *set.owner != *plugin);
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
    "maki.treesitter.query" => pub(crate) fn create_query_module(plugin: Arc<str>), DOCS [
        parse, get, set(plugin),
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

#[cfg(test)]
mod tests {
    use mlua::{Lua, Table};
    use test_case::test_case;

    use super::*;

    const LANG: &str = "lua";
    const FOLDS: &str = "folds";
    const LOCALS: &str = "locals";
    const FOLDS_QUERY: &str = "(function_declaration) @fold";
    const FOLD_CAPTURE: &str = "fold";
    const PLUGIN: &str = "query_test";
    const FORGOTTEN_PLUGIN: &str = "query_test_forgotten";

    fn query_module(lua: &Lua) -> Table {
        query_module_of(lua, PLUGIN)
    }

    fn query_module_of(lua: &Lua, plugin: &str) -> Table {
        create_query_module(lua, Arc::from(plugin)).unwrap()
    }

    fn call_set(query: &Table, lang: &str, name: &str, text: &str) -> Pair<bool> {
        query
            .get::<mlua::Function>("set")
            .unwrap()
            .call((lang, name, text))
            .unwrap()
    }

    fn set_query(query: &Table, name: &str) {
        assert_eq!(call_set(query, LANG, name, FOLDS_QUERY), (Some(true), None));
    }

    fn get_query(query: &Table, name: &str) -> Option<mlua::AnyUserData> {
        query
            .get::<mlua::Function>("get")
            .unwrap()
            .call((LANG, name))
            .unwrap()
    }

    #[test]
    fn get_returns_the_query_set_gave_it() {
        let lua = Lua::new();
        let query = query_module(&lua);
        set_query(&query, FOLDS);
        let got = get_query(&query, FOLDS).unwrap();
        let got = got.borrow::<LuaQuery>().unwrap();
        assert_eq!(got.inner.capture_names(), [FOLD_CAPTURE]);
    }

    #[test]
    fn a_set_query_is_shared_until_its_plugin_is_forgotten() {
        let lua = Lua::new();
        set_query(&query_module_of(&lua, FORGOTTEN_PLUGIN), LOCALS);
        let other = query_module(&lua);
        assert!(get_query(&other, LOCALS).is_some());

        forget_plugin_queries(FORGOTTEN_PLUGIN);

        assert!(get_query(&other, LOCALS).is_none());
    }

    #[test_case("cobol", FOLDS_QUERY, UNKNOWN_LANGUAGE ; "unknown_language")]
    #[test_case(LANG, "(no_such_node) @x", QUERY_PARSE_ERROR ; "invalid_query")]
    fn set_answers_an_error_instead_of_throwing(lang: &str, text: &str, expected: &str) {
        let lua = Lua::new();
        let (ok, err) = call_set(&query_module(&lua), lang, HIGHLIGHTS_QUERY, text);
        assert_eq!(ok, None);
        let err = err.unwrap();
        assert!(err.contains(expected), "{err}");
    }
}
