use std::sync::Arc;

use maki_lua_macro::{lua_class, lua_fn};
use mlua::{Lua, Result as LuaResult};
use tree_sitter::Tree;

use super::node::LuaNode;

pub(crate) struct LuaTree {
    pub(crate) inner: Arc<Tree>,
}

/// Returns the root node of this tree. This is where you start walking
/// the syntax tree or running queries.
///
/// @return (Node) Root node.
/// @example
/// local root = tree:root()
/// print(root:type()) -- e.g. "chunk" for Lua
#[lua_fn]
fn root(_lua: &Lua, this: &LuaTree) -> LuaResult<LuaNode> {
    Ok(LuaNode::new(
        this.inner.root_node(),
        Arc::clone(&this.inner),
    ))
}

/// Returns an independent copy of this tree.
/// Edits to the copy will not affect the original.
///
/// @return (Tree) A new Tree with the same content.
#[lua_fn]
fn copy(_lua: &Lua, this: &LuaTree) -> LuaResult<LuaTree> {
    Ok(LuaTree {
        inner: Arc::new(this.inner.as_ref().clone()),
    })
}

lua_class! {
    /// A parsed syntax tree.
    ///
    /// Obtained from `LanguageTree:parse()` or `LanguageTree:trees()`.
    /// Call `:root()` to get the root node and start traversing.
    ///
    /// ```lua
    /// local trees = parser:parse()
    /// local root = trees[1]:root()
    /// ```
    "maki.treesitter.Tree" => LuaTree, DOCS [root, copy]
}
