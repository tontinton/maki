use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use maki_lua_macro::{lua_class, lua_fn};
use mlua::{AnyUserData, Function, Lua, MultiValue, Result as LuaResult, Value};
use tree_sitter::{Node, Point, Tree};

use super::tree::LuaTree;

#[derive(Clone)]
pub(crate) struct LuaNode {
    pub(crate) tree: Arc<Tree>,
    pub(crate) node: Node<'static>,
}

impl LuaNode {
    pub(crate) fn new(node: Node<'_>, tree: Arc<Tree>) -> Self {
        // SAFETY: Node borrows from Tree. We keep Tree alive via Arc,
        // so the borrow can never dangle.
        let node: Node<'static> = unsafe { std::mem::transmute(node) };
        Self { tree, node }
    }

    fn wrap(&self, node: Node) -> Self {
        Self::new(node, Arc::clone(&self.tree))
    }

    fn wrap_opt(&self, node: Option<Node>) -> Option<Self> {
        node.map(|n| self.wrap(n))
    }
}

/// Returns the grammar type name for this node, like `"function_definition"` or `"identifier"`.
///
/// @return (string) Grammar type name.
#[lua_fn]
fn r#type(_lua: &Lua, this: &LuaNode) -> LuaResult<String> {
    Ok(this.node.kind().to_owned())
}

/// Returns the numeric symbol id for this node's grammar type.
/// Two nodes with the same type always share the same symbol id.
///
/// @return (integer) Symbol id.
#[lua_fn]
fn symbol(_lua: &Lua, this: &LuaNode) -> LuaResult<i64> {
    Ok(this.node.kind_id() as i64)
}

/// Returns a unique string identifier for this specific node in the tree.
/// Useful for deduplication or as a table key.
///
/// @return (string) Node identity string.
#[lua_fn]
fn id(_lua: &Lua, this: &LuaNode) -> LuaResult<String> {
    Ok(format!("{}", this.node.id()))
}

/// Returns the range of this node as multiple return values.
/// Without {include_bytes}: `start_row, start_col, end_row, end_col`.
/// With {include_bytes} set to true: `start_row, start_col, start_byte, end_row, end_col, end_byte`.
///
/// @param include_bytes boolean When true, byte offsets are included in the return values.
/// @return (integer, integer, integer, integer) Four values, or six when include_bytes is true.
/// @example
/// local sr, sc, er, ec = node:range()
/// local sr, sc, sb, er, ec, eb = node:range(true)
#[lua_fn]
fn range(_lua: &Lua, this: &LuaNode, include_bytes: Option<bool>) -> LuaResult<MultiValue> {
    let sp = this.node.start_position();
    let ep = this.node.end_position();
    if include_bytes.unwrap_or(false) {
        Ok(MultiValue::from_iter([
            Value::Integer(sp.row as i64),
            Value::Integer(sp.column as i64),
            Value::Integer(this.node.start_byte() as i64),
            Value::Integer(ep.row as i64),
            Value::Integer(ep.column as i64),
            Value::Integer(this.node.end_byte() as i64),
        ]))
    } else {
        Ok(MultiValue::from_iter([
            Value::Integer(sp.row as i64),
            Value::Integer(sp.column as i64),
            Value::Integer(ep.row as i64),
            Value::Integer(ep.column as i64),
        ]))
    }
}

/// Returns the start position of this node: row, column, and byte offset (all 0-based).
///
/// @return (integer, integer, integer) start_row, start_col, start_byte.
#[lua_fn]
fn start(_lua: &Lua, this: &LuaNode) -> LuaResult<(i64, i64, i64)> {
    let sp = this.node.start_position();
    Ok((
        sp.row as i64,
        sp.column as i64,
        this.node.start_byte() as i64,
    ))
}

/// Returns the end position of this node: row, column, and byte offset (all 0-based).
///
/// @return (integer, integer, integer) end_row, end_col, end_byte.
#[lua_fn]
fn end_(_lua: &Lua, this: &LuaNode) -> LuaResult<(i64, i64, i64)> {
    let ep = this.node.end_position();
    Ok((ep.row as i64, ep.column as i64, this.node.end_byte() as i64))
}

/// Returns how many bytes this node spans in the source text.
///
/// @return (integer) Byte length.
#[lua_fn]
fn byte_length(_lua: &Lua, this: &LuaNode) -> LuaResult<i64> {
    Ok((this.node.end_byte() - this.node.start_byte()) as i64)
}

/// Returns the child at position {index} (0-based), including anonymous nodes like punctuation.
/// Returns nil if {index} is out of bounds.
///
/// @param index integer 0-based child index.
/// @return (Node|nil) Child node, or nil.
#[lua_fn]
fn child(_lua: &Lua, this: &LuaNode, index: u32) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.child(index)))
}

/// Returns the named child at position {index} (0-based), skipping anonymous nodes.
/// Returns nil if {index} is out of bounds.
///
/// @param index integer 0-based named child index.
/// @return (Node|nil) Named child node, or nil.
#[lua_fn]
fn named_child(_lua: &Lua, this: &LuaNode, index: u32) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.named_child(index)))
}

/// Returns the total number of children, including anonymous nodes.
///
/// @return (integer) Child count.
#[lua_fn]
fn child_count(_lua: &Lua, this: &LuaNode) -> LuaResult<i64> {
    Ok(this.node.child_count() as i64)
}

/// Returns the number of named children (skipping anonymous punctuation nodes).
///
/// @return (integer) Named child count.
#[lua_fn]
fn named_child_count(_lua: &Lua, this: &LuaNode) -> LuaResult<i64> {
    Ok(this.node.named_child_count() as i64)
}

/// Returns all children (named and anonymous) as a Lua table.
///
/// @return (table) Array of Node.
/// @example
/// for _, child in ipairs(node:children()) do
///   print(child:type())
/// end
#[lua_fn]
fn children(lua: &Lua, this: &LuaNode) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    let mut cursor = this.node.walk();
    for (i, child) in this.node.children(&mut cursor).enumerate() {
        tbl.raw_set(i + 1, this.wrap(child))?;
    }
    Ok(tbl)
}

/// Returns all named children as a Lua table, skipping anonymous nodes.
///
/// @return (table) Array of Node.
#[lua_fn]
fn named_children(lua: &Lua, this: &LuaNode) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    let mut cursor = this.node.walk();
    for (i, child) in this.node.named_children(&mut cursor).enumerate() {
        tbl.raw_set(i + 1, this.wrap(child))?;
    }
    Ok(tbl)
}

/// Returns an iterator function that yields `(child, field_name)` for every child.
/// The field name is nil for children that are not assigned to a grammar field.
///
/// @return (function) Iterator yielding (Node, string|nil).
/// @example
/// for child, field in node:iter_children() do
///   if field then print(field .. ": " .. child:type()) end
/// end
#[lua_fn]
fn iter_children(lua: &Lua, this: &LuaNode) -> LuaResult<Function> {
    let count = this.node.child_count() as u32;
    let mut entries: Vec<(LuaNode, Option<String>)> = Vec::with_capacity(count as usize);
    for i in 0..count {
        if let Some(child) = this.node.child(i) {
            let field = this.node.field_name_for_child(i).map(str::to_owned);
            entries.push((this.wrap(child), field));
        }
    }
    let idx = Arc::new(AtomicUsize::new(0));
    let entries = Arc::new(entries);
    lua.create_function(move |lua, ()| {
        let i = idx.fetch_add(1, Ordering::Relaxed);
        if i >= entries.len() {
            return Ok(MultiValue::new());
        }
        let (ref lua_node, ref field) = entries[i];
        let child = lua_node.clone();
        Ok(MultiValue::from_iter([
            Value::UserData(lua.create_userdata(child)?),
            match field {
                Some(s) => Value::String(lua.create_string(s)?),
                None => Value::Nil,
            },
        ]))
    })
}

/// Returns all children assigned to the grammar field {name} as a table.
/// For example, a function node might have a `"name"` or `"body"` field.
///
/// @param name string Field name defined in the grammar.
/// @return (table) Array of Node.
/// @example
/// local bodies = node:field("body")
#[lua_fn]
fn field(lua: &Lua, this: &LuaNode, name: String) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    let mut cursor = this.node.walk();
    for (i, child) in this
        .node
        .children_by_field_name(&name, &mut cursor)
        .enumerate()
    {
        tbl.raw_set(i + 1, this.wrap(child))?;
    }
    Ok(tbl)
}

/// Returns the parent of this node, or nil if this is the root.
///
/// @return (Node|nil) Parent node.
#[lua_fn]
fn parent(_lua: &Lua, this: &LuaNode) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.parent()))
}

/// Returns the next sibling (named or anonymous), or nil if this is the last child.
///
/// @return (Node|nil) Next sibling.
#[lua_fn]
fn next_sibling(_lua: &Lua, this: &LuaNode) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.next_sibling()))
}

/// Returns the previous sibling (named or anonymous), or nil if this is the first child.
///
/// @return (Node|nil) Previous sibling.
#[lua_fn]
fn prev_sibling(_lua: &Lua, this: &LuaNode) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.prev_sibling()))
}

/// Returns the next named sibling, skipping anonymous nodes. Returns nil at the end.
///
/// @return (Node|nil) Next named sibling.
#[lua_fn]
fn next_named_sibling(_lua: &Lua, this: &LuaNode) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.next_named_sibling()))
}

/// Returns the previous named sibling, skipping anonymous nodes. Returns nil at the start.
///
/// @return (Node|nil) Previous named sibling.
#[lua_fn]
fn prev_named_sibling(_lua: &Lua, this: &LuaNode) -> LuaResult<Option<LuaNode>> {
    Ok(this.wrap_opt(this.node.prev_named_sibling()))
}

/// Finds the direct child of this node that contains {descendant}.
/// Returns nil if {descendant} is not actually inside this node.
///
/// @param descendant Node A node that may be a descendant.
/// @return (Node|nil) Direct child containing the descendant.
#[lua_fn]
fn child_with_descendant(
    _lua: &Lua,
    this: &LuaNode,
    descendant: AnyUserData,
) -> LuaResult<Option<LuaNode>> {
    let desc = descendant.borrow::<LuaNode>()?;
    Ok(this.wrap_opt(this.node.child_with_descendant(desc.node)))
}

/// Finds the smallest node inside this node that spans the given point range.
/// Includes both named and anonymous nodes.
///
/// @param start_row integer Start row (0-based).
/// @param start_col integer Start column (0-based).
/// @param end_row integer End row (0-based).
/// @param end_col integer End column (0-based).
/// @return (Node|nil) Smallest node covering the range, or nil.
#[lua_fn]
fn descendant_for_range(
    _lua: &Lua,
    this: &LuaNode,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
) -> LuaResult<Option<LuaNode>> {
    let start = Point::new(start_row, start_col);
    let end = Point::new(end_row, end_col);
    Ok(this.wrap_opt(this.node.descendant_for_point_range(start, end)))
}

/// Like `descendant_for_range`, but only considers named nodes.
///
/// @param start_row integer Start row (0-based).
/// @param start_col integer Start column (0-based).
/// @param end_row integer End row (0-based).
/// @param end_col integer End column (0-based).
/// @return (Node|nil) Smallest named node covering the range, or nil.
#[lua_fn]
fn named_descendant_for_range(
    _lua: &Lua,
    this: &LuaNode,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
) -> LuaResult<Option<LuaNode>> {
    let start = Point::new(start_row, start_col);
    let end = Point::new(end_row, end_col);
    Ok(this.wrap_opt(this.node.named_descendant_for_point_range(start, end)))
}

/// Returns true if this is a named node (not anonymous punctuation like `,` or `(`).
///
/// @return (boolean)
#[lua_fn]
fn named(_lua: &Lua, this: &LuaNode) -> LuaResult<bool> {
    Ok(this.node.is_named())
}

/// Returns true if this node is an "extra" (like a comment) that can appear anywhere in the grammar.
///
/// @return (boolean)
#[lua_fn]
fn extra(_lua: &Lua, this: &LuaNode) -> LuaResult<bool> {
    Ok(this.node.is_extra())
}

/// Returns true if this node is "missing", meaning it was inserted by the parser during error recovery.
///
/// @return (boolean)
#[lua_fn]
fn missing(_lua: &Lua, this: &LuaNode) -> LuaResult<bool> {
    Ok(this.node.is_missing())
}

/// Returns true if this node or any of its descendants contain a syntax error.
///
/// @return (boolean)
#[lua_fn]
fn has_error(_lua: &Lua, this: &LuaNode) -> LuaResult<bool> {
    Ok(this.node.has_error())
}

/// Returns true if this node has been marked as changed since the last parse.
///
/// @return (boolean)
#[lua_fn]
fn has_changes(_lua: &Lua, this: &LuaNode) -> LuaResult<bool> {
    Ok(this.node.has_changes())
}

/// Returns true if this node and {other} are the same node in the tree.
///
/// @param other Node Node to compare against.
/// @return (boolean)
#[lua_fn]
fn equal(_lua: &Lua, this: &LuaNode, other: AnyUserData) -> LuaResult<bool> {
    let other = other.borrow::<LuaNode>()?;
    Ok(this.node.id() == other.node.id())
}

/// Returns the S-expression (lisp-like) string for this node and its children.
/// Handy for debugging the tree structure.
///
/// @return (string) S-expression.
/// @example
/// print(node:sexpr()) -- e.g. "(identifier)"
#[lua_fn]
fn sexpr(_lua: &Lua, this: &LuaNode) -> LuaResult<String> {
    Ok(this.node.to_sexp())
}

/// Returns the Tree that this node belongs to.
///
/// @return (Tree) The owning tree.
#[lua_fn]
fn tree(_lua: &Lua, this: &LuaNode) -> LuaResult<LuaTree> {
    Ok(LuaTree {
        inner: Arc::clone(&this.tree),
    })
}

lua_class! {
    /// A single node in a parsed syntax tree.
    ///
    /// Nodes are obtained from `Tree:root()`, navigation methods like `:child()`,
    /// or from query captures. Each node knows its type, range, and children.
    ///
    /// ```lua
    /// local root = tree:root()
    /// print(root:type(), root:child_count())
    /// for child, field in root:iter_children() do
    ///   print(child:type(), field)
    /// end
    /// ```
    "maki.treesitter.Node" => LuaNode, DOCS [r#type, symbol, id, range, start, end_, byte_length, child, named_child, child_count, named_child_count, children, named_children, iter_children, field, parent, next_sibling, prev_sibling, next_named_sibling, prev_named_sibling, child_with_descendant, descendant_for_range, named_descendant_for_range, named, extra, missing, has_error, has_changes, equal, sexpr, tree]
}
