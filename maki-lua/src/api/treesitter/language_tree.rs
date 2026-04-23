use std::sync::Arc;

use crate::language::Language;
use mlua::{Function, UserData, UserDataMethods, Value as LuaValue};
use tree_sitter::{Parser, Tree};

use super::tree::LuaTree;

pub(crate) struct LuaLanguageTree {
    tree: Option<Arc<Tree>>,
    source: Arc<str>,
    lang_name: Arc<str>,
    lang: Language,
}

impl LuaLanguageTree {
    pub(crate) fn new(source: Arc<str>, lang_name: Arc<str>, lang: Language) -> Self {
        Self {
            tree: None,
            source,
            lang_name,
            lang,
        }
    }

    fn ensure_parsed(&mut self) -> Result<Arc<Tree>, mlua::Error> {
        if let Some(ref tree) = self.tree {
            return Ok(Arc::clone(tree));
        }
        let mut parser = Parser::new();
        parser
            .set_language(&self.lang.ts_language())
            .map_err(|e| mlua::Error::runtime(format!("failed to set language: {e}")))?;
        let tree = parser
            .parse(self.source.as_bytes(), None)
            .ok_or_else(|| mlua::Error::runtime("parse returned no tree"))?;
        let tree = Arc::new(tree);
        self.tree = Some(Arc::clone(&tree));
        Ok(tree)
    }
}

impl UserData for LuaLanguageTree {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("parse", |lua, this, _range: LuaValue| {
            let tree = this.ensure_parsed()?;
            let result = lua.create_table()?;
            result.raw_set(
                1,
                LuaTree {
                    inner: Arc::clone(&tree),
                },
            )?;
            Ok(result)
        });

        methods.add_method("lang", |_, this, ()| Ok(this.lang_name.to_string()));

        methods.add_method("children", |lua, _, ()| lua.create_table());

        methods.add_method("trees", |lua, this, ()| {
            let result = lua.create_table()?;
            if let Some(ref tree) = this.tree {
                result.raw_set(
                    1,
                    LuaTree {
                        inner: Arc::clone(tree),
                    },
                )?;
            }
            Ok(result)
        });

        methods.add_method("source", |_, this, ()| Ok(this.source.to_string()));

        methods.add_method(
            "is_valid",
            |_, _, (_exclude_children, _range): (LuaValue, LuaValue)| Ok(true),
        );

        methods.add_method_mut("for_each_tree", |_, this, f: Function| {
            let tree = this.ensure_parsed()?;
            f.call::<()>((LuaTree { inner: tree }, LuaValue::Nil))?;
            Ok(())
        });

        methods.add_method("included_regions", |lua, _, ()| {
            let result = lua.create_table()?;
            result.raw_set(1, lua.create_table()?)?;
            Ok(result)
        });

        methods.add_method("contains", |_, _, _range: LuaValue| Ok(true));

        methods.add_method_mut("destroy", |_, this, ()| {
            this.tree = None;
            Ok(())
        });
    }
}
