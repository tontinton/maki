use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mlua::{AnyUserData, MultiValue, UserData, UserDataMethods, Value};
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

impl UserData for LuaNode {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("type", |_, this, ()| Ok(this.node.kind().to_owned()));

        methods.add_method("symbol", |_, this, ()| Ok(this.node.kind_id() as i64));

        methods.add_method("id", |_, this, ()| Ok(format!("{}", this.node.id())));

        methods.add_method("range", |_, this, include_bytes: Option<bool>| {
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
        });

        methods.add_method("start", |_, this, ()| {
            let sp = this.node.start_position();
            Ok((
                sp.row as i64,
                sp.column as i64,
                this.node.start_byte() as i64,
            ))
        });

        methods.add_method("end_", |_, this, ()| {
            let ep = this.node.end_position();
            Ok((ep.row as i64, ep.column as i64, this.node.end_byte() as i64))
        });

        methods.add_method("byte_length", |_, this, ()| {
            Ok((this.node.end_byte() - this.node.start_byte()) as i64)
        });

        methods.add_method("child", |_, this, index: u32| {
            Ok(this.wrap_opt(this.node.child(index)))
        });

        methods.add_method("named_child", |_, this, index: u32| {
            Ok(this.wrap_opt(this.node.named_child(index)))
        });

        methods.add_method("child_count", |_, this, ()| {
            Ok(this.node.child_count() as i64)
        });

        methods.add_method("named_child_count", |_, this, ()| {
            Ok(this.node.named_child_count() as i64)
        });

        methods.add_method("children", |lua, this, ()| {
            let tbl = lua.create_table()?;
            let mut cursor = this.node.walk();
            for (i, child) in this.node.children(&mut cursor).enumerate() {
                tbl.raw_set(i + 1, this.wrap(child))?;
            }
            Ok(tbl)
        });

        methods.add_method("named_children", |lua, this, ()| {
            let tbl = lua.create_table()?;
            let mut cursor = this.node.walk();
            for (i, child) in this.node.named_children(&mut cursor).enumerate() {
                tbl.raw_set(i + 1, this.wrap(child))?;
            }
            Ok(tbl)
        });

        methods.add_method("iter_children", |lua, this, ()| {
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
        });

        methods.add_method("field", |lua, this, name: String| {
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
        });

        methods.add_method(
            "parent",
            |_, this, ()| Ok(this.wrap_opt(this.node.parent())),
        );

        methods.add_method("next_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.node.next_sibling()))
        });

        methods.add_method("prev_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.node.prev_sibling()))
        });

        methods.add_method("next_named_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.node.next_named_sibling()))
        });

        methods.add_method("prev_named_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.node.prev_named_sibling()))
        });

        methods.add_method("child_with_descendant", |_, this, desc: AnyUserData| {
            let desc = desc.borrow::<LuaNode>()?;
            Ok(this.wrap_opt(this.node.child_with_descendant(desc.node)))
        });

        methods.add_method(
            "descendant_for_range",
            |_, this, (sr, sc, er, ec): (usize, usize, usize, usize)| {
                let start = Point::new(sr, sc);
                let end = Point::new(er, ec);
                Ok(this.wrap_opt(this.node.descendant_for_point_range(start, end)))
            },
        );

        methods.add_method(
            "named_descendant_for_range",
            |_, this, (sr, sc, er, ec): (usize, usize, usize, usize)| {
                let start = Point::new(sr, sc);
                let end = Point::new(er, ec);
                Ok(this.wrap_opt(this.node.named_descendant_for_point_range(start, end)))
            },
        );

        methods.add_method("named", |_, this, ()| Ok(this.node.is_named()));

        methods.add_method("extra", |_, this, ()| Ok(this.node.is_extra()));

        methods.add_method("missing", |_, this, ()| Ok(this.node.is_missing()));

        methods.add_method("has_error", |_, this, ()| Ok(this.node.has_error()));

        methods.add_method("has_changes", |_, this, ()| Ok(this.node.has_changes()));

        methods.add_method("equal", |_, this, other: AnyUserData| {
            let other = other.borrow::<LuaNode>()?;
            Ok(this.node.id() == other.node.id())
        });

        methods.add_method("sexpr", |_, this, ()| Ok(this.node.to_sexp()));

        methods.add_method("tree", |_, this, ()| {
            Ok(LuaTree {
                inner: Arc::clone(&this.tree),
            })
        });
    }
}
