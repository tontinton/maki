use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::language::Language;
use mlua::{MultiValue, Table, UserData, UserDataFields, UserDataMethods, Value as LuaValue};
use regex::Regex;
use tree_sitter::{Node, Query, QueryCursor, QueryPredicateArg, StreamingIterator, Tree};

use super::node::LuaNode;

pub(crate) struct LuaQuery {
    pub(crate) inner: Arc<Query>,
}

impl UserData for LuaQuery {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("captures", |lua, this| {
            let tbl = lua.create_table()?;
            for (i, name) in this.inner.capture_names().iter().enumerate() {
                tbl.raw_set(i + 1, *name)?;
            }
            Ok(tbl)
        });

        fields.add_field_method_get("info", |lua, _| lua.create_table());
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("iter_captures", |lua, this, args: MultiValue| {
            let parsed = IterArgs::parse(args, "iter_captures")?;
            let results = collect_captures(&this.inner, &parsed);
            stateful_iter(lua, results)
        });

        methods.add_method("iter_matches", |lua, this, args: MultiValue| {
            let parsed = IterArgs::parse(args, "iter_matches")?;
            let results = collect_matches(&this.inner, &parsed);
            stateful_iter(lua, results)
        });
    }
}

pub(crate) fn create_query_module(lua: &mlua::Lua) -> mlua::Result<Table> {
    let query_table = lua.create_table()?;

    query_table.set(
        "parse",
        lua.create_function(move |_, (lang_name, query_str): (String, String)| {
            let lang = Language::from_name(&lang_name)
                .ok_or_else(|| mlua::Error::runtime(format!("unknown language: {lang_name}")))?;
            let query = Query::new(&lang.ts_language(), &query_str)
                .map_err(|e| mlua::Error::runtime(format!("query parse error: {e}")))?;
            Ok(LuaQuery {
                inner: Arc::new(query),
            })
        })?,
    )?;

    query_table.set(
        "get",
        lua.create_function(|_, (_lang, _name): (String, String)| Ok(LuaValue::Nil))?,
    )?;

    Ok(query_table)
}

struct IterArgs {
    node: Node<'static>,
    tree: Arc<Tree>,
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
            node: lua_node.node,
            tree: Arc::clone(&lua_node.tree),
            source,
            start_row,
            stop_row,
        })
    }
}

trait IterEntry: Send + Sync + 'static {
    fn to_lua_values(&self, lua: &mlua::Lua) -> mlua::Result<MultiValue>;
}

struct CaptureEntry {
    capture_index: u32,
    node: LuaNode,
    metadata: HashMap<String, String>,
}

impl IterEntry for CaptureEntry {
    fn to_lua_values(&self, lua: &mlua::Lua) -> mlua::Result<MultiValue> {
        let meta_table = lua.create_table()?;
        for (k, v) in &self.metadata {
            meta_table.set(k.as_str(), v.as_str())?;
        }
        Ok(MultiValue::from_iter([
            LuaValue::Integer((self.capture_index + 1) as i64),
            lua.pack(self.node.clone())?,
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
    fn to_lua_values(&self, lua: &mlua::Lua) -> mlua::Result<MultiValue> {
        let captures_table = lua.create_table()?;
        for (cap_idx, nodes) in &self.captures {
            let nodes_table = lua.create_table()?;
            for (j, n) in nodes.iter().enumerate() {
                nodes_table.raw_set(j + 1, n.clone())?;
            }
            captures_table.raw_set((*cap_idx as i64) + 1, nodes_table)?;
        }
        let meta_table = lua.create_table()?;
        for (k, v) in &self.metadata {
            meta_table.set(k.as_str(), v.as_str())?;
        }
        Ok(MultiValue::from_iter([
            LuaValue::Integer((self.pattern_index + 1) as i64),
            LuaValue::Table(captures_table),
            LuaValue::Table(meta_table),
            LuaValue::Integer(1),
        ]))
    }
}

fn stateful_iter<E: IterEntry>(lua: &mlua::Lua, results: Vec<E>) -> mlua::Result<mlua::Function> {
    let index = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(results);
    lua.create_function(move |lua, ()| {
        let i = index.fetch_add(1, Ordering::Relaxed);
        if i >= results.len() {
            return Ok(MultiValue::new());
        }
        results[i].to_lua_values(lua)
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

fn collect_captures(query: &Query, args: &IterArgs) -> Vec<CaptureEntry> {
    let source_bytes = args.source.as_bytes();
    let mut cursor = new_cursor(args.start_row, args.stop_row);
    let mut regex_cache = HashMap::new();
    let mut results = Vec::new();

    let mut captures = cursor.captures(query, args.node, source_bytes);
    while let Some((m, capture_idx)) = captures.next() {
        let mut metadata = HashMap::new();
        if !evaluate_predicates(
            query,
            m.pattern_index,
            m.captures,
            source_bytes,
            &mut metadata,
            &mut regex_cache,
        ) {
            continue;
        }
        let capture = &m.captures[*capture_idx];
        results.push(CaptureEntry {
            capture_index: capture.index,
            node: LuaNode::new(capture.node, Arc::clone(&args.tree)),
            metadata,
        });
    }
    results
}

fn collect_matches(query: &Query, args: &IterArgs) -> Vec<MatchEntry> {
    let source_bytes = args.source.as_bytes();
    let mut cursor = new_cursor(args.start_row, args.stop_row);
    let mut regex_cache = HashMap::new();
    let mut results = Vec::new();

    let mut matches = cursor.matches(query, args.node, source_bytes);
    while let Some(m) = matches.next() {
        let mut metadata = HashMap::new();
        if !evaluate_predicates(
            query,
            m.pattern_index,
            m.captures,
            source_bytes,
            &mut metadata,
            &mut regex_cache,
        ) {
            continue;
        }
        let mut captures_map: HashMap<u32, Vec<LuaNode>> = HashMap::new();
        for capture in m.captures {
            captures_map
                .entry(capture.index)
                .or_default()
                .push(LuaNode::new(capture.node, Arc::clone(&args.tree)));
        }
        results.push(MatchEntry {
            pattern_index: m.pattern_index,
            captures: captures_map,
            metadata,
        });
    }
    results
}

#[derive(Clone, Copy)]
struct PredicateModifiers {
    negated: bool,
    any: bool,
}

fn parse_predicate_op(op: &str) -> (PredicateModifiers, &str) {
    let (negated, rest) = op
        .strip_prefix("not-")
        .map(|r| (true, r))
        .unwrap_or((false, op));
    let (any, base) = rest
        .strip_prefix("any-")
        .map(|r| (true, r))
        .unwrap_or((false, rest));
    (PredicateModifiers { negated, any }, base)
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

    for predicate in query.general_predicates(pattern_index) {
        let (mods, base_op) = parse_predicate_op(predicate.operator.as_ref());
        let args = &predicate.args;

        match base_op {
            "eq?" if eval_eq(captures, source, args, mods.any) == mods.negated => {
                return false;
            }
            "match?" | "lua-match?"
                if eval_match(captures, source, args, mods.any, regex_cache) == mods.negated =>
            {
                return false;
            }
            "contains?" if eval_contains(captures, source, args, mods.any) == mods.negated => {
                return false;
            }
            "any-of?" if eval_any_of(captures, source, args) == mods.negated => {
                return false;
            }
            "has-ancestor?" if eval_has_ancestor(captures, args) == mods.negated => {
                return false;
            }
            "has-parent?" if eval_has_parent(captures, args) == mods.negated => {
                return false;
            }
            "set!" => {
                eval_set(args, metadata);
            }
            _ => {}
        }
    }
    true
}

fn capture_text<'a>(
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &'a [u8],
    idx: u32,
) -> Option<&'a str> {
    captures
        .iter()
        .find(|c| c.index == idx)
        .and_then(|c| std::str::from_utf8(&source[c.node.start_byte()..c.node.end_byte()]).ok())
}

fn resolve_arg<'a>(
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &'a [u8],
    arg: &'a QueryPredicateArg,
) -> Option<&'a str> {
    match arg {
        QueryPredicateArg::Capture(idx) => capture_text(captures, source, *idx),
        QueryPredicateArg::String(s) => Some(s.as_ref()),
    }
}

fn eval_eq(
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
    any: bool,
) -> bool {
    let (Some(lhs), Some(rhs)) = (
        args.first().and_then(|a| resolve_arg(captures, source, a)),
        args.get(1).and_then(|a| resolve_arg(captures, source, a)),
    ) else {
        return false;
    };
    if any {
        lhs.lines().any(|line| line == rhs)
    } else {
        lhs == rhs
    }
}

fn eval_match(
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
    any: bool,
    regex_cache: &mut HashMap<String, Option<Regex>>,
) -> bool {
    let Some(text) = args.first().and_then(|a| resolve_arg(captures, source, a)) else {
        return false;
    };
    let Some(QueryPredicateArg::String(pattern)) = args.get(1) else {
        return false;
    };
    let re = regex_cache.entry(pattern.to_string()).or_insert_with(|| {
        Regex::new(pattern.as_ref())
            .inspect_err(|_| tracing::debug!(pattern = pattern.as_ref(), "invalid regex predicate"))
            .ok()
    });
    let Some(re) = re else { return false };
    if any {
        text.lines().any(|line| re.is_match(line))
    } else {
        re.is_match(text)
    }
}

fn eval_contains(
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
    any: bool,
) -> bool {
    let Some(text) = args.first().and_then(|a| resolve_arg(captures, source, a)) else {
        return false;
    };
    let Some(needle) = args.get(1).and_then(|a| resolve_arg(captures, source, a)) else {
        return false;
    };
    if any {
        text.lines().any(|line| line.contains(needle))
    } else {
        text.contains(needle)
    }
}

fn eval_any_of(
    captures: &[tree_sitter::QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
) -> bool {
    let Some(QueryPredicateArg::Capture(idx)) = args.first() else {
        return false;
    };
    let Some(text) = capture_text(captures, source, *idx) else {
        return false;
    };
    args[1..].iter().any(|arg| match arg {
        QueryPredicateArg::String(s) => text == s.as_ref(),
        QueryPredicateArg::Capture(idx) => {
            capture_text(captures, source, *idx).is_some_and(|t| t == text)
        }
    })
}

fn eval_has_ancestor(
    captures: &[tree_sitter::QueryCapture<'_>],
    args: &[QueryPredicateArg],
) -> bool {
    let Some(QueryPredicateArg::Capture(idx)) = args.first() else {
        return false;
    };
    let Some(QueryPredicateArg::String(type_name)) = args.get(1) else {
        return false;
    };
    let Some(cap) = captures.iter().find(|c| c.index == *idx) else {
        return false;
    };
    let mut current = cap.node.parent();
    while let Some(parent) = current {
        if parent.kind() == type_name.as_ref() {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn eval_has_parent(captures: &[tree_sitter::QueryCapture<'_>], args: &[QueryPredicateArg]) -> bool {
    let Some(QueryPredicateArg::Capture(idx)) = args.first() else {
        return false;
    };
    let Some(QueryPredicateArg::String(type_name)) = args.get(1) else {
        return false;
    };
    let Some(cap) = captures.iter().find(|c| c.index == *idx) else {
        return false;
    };
    cap.node
        .parent()
        .is_some_and(|p| p.kind() == type_name.as_ref())
}

fn eval_set(args: &[QueryPredicateArg], metadata: &mut HashMap<String, String>) {
    let (Some(QueryPredicateArg::String(key)), Some(QueryPredicateArg::String(value))) =
        (args.first(), args.get(1))
    else {
        return;
    };
    metadata.insert(key.to_string(), value.to_string());
}

fn lua_to_usize(v: LuaValue) -> Option<usize> {
    match v {
        LuaValue::Integer(n) => Some(n as usize),
        LuaValue::Number(n) => Some(n as usize),
        _ => None,
    }
}
