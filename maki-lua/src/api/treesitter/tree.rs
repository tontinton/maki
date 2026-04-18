use std::sync::Arc;

use mlua::{UserData, UserDataMethods};
use tree_sitter::Tree;

use super::node::LuaNode;

pub(crate) struct LuaTree {
    pub(crate) inner: Arc<Tree>,
}

impl UserData for LuaTree {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("root", |_, this, ()| {
            Ok(LuaNode::new(
                this.inner.root_node(),
                Arc::clone(&this.inner),
            ))
        });

        methods.add_method("copy", |_, this, ()| {
            Ok(LuaTree {
                inner: Arc::new(this.inner.as_ref().clone()),
            })
        });
    }
}
