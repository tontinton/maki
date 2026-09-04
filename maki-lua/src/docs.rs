//! Self-documenting Lua API. `api` modules define functions and userdata
//! methods with `#[maki_lua_macro::lua_fn]` (Lua name from the fn ident, args
//! from the signature, `@param`/`@return` tags validated against real
//! parameters at compile time) and assemble registration plus the `DOCS`
//! consts with `maki_lua_macro::lua_table!` / `lua_class!`. The few functions
//! that cannot fit (raw `MultiValue`, Lua chunks, conditional registration)
//! and `maki.setup` keep hand-written `FnDoc`s. `api_docs()` aggregates
//! everything for maki-docgen, and the drift test below asserts docs match
//! the real `maki` global.

pub struct ModuleDoc {
    /// Dotted path, e.g. "maki.base64". Classes use a type name, e.g.
    /// "maki.treesitter.Node".
    pub name: &'static str,
    pub kind: DocKind,
    pub desc: &'static str,
    pub fns: &'static [FnDoc],
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// A real Lua table; the drift test checks its keys.
    Table,
    /// Methods on userdata handles; not enumerable, skipped by the drift test.
    Class,
}

pub struct FnDoc {
    pub name: &'static str,
    /// Argument list in Neovim notation, e.g. "{path}, {opts?}".
    pub args: &'static str,
    pub desc: &'static str,
    pub params: &'static [ParamDoc],
    /// E.g. "(string) encoded text" or "" when nothing is returned.
    pub returns: &'static str,
    /// Lua snippet rendered as a fenced code block, or "" when absent.
    pub example: &'static str,
    /// Manifest key of the plugin permission gating this function, from the
    /// `guard =` attribute. Rendered into the docs so the reference can
    /// never drift from the actual gate.
    pub guard: Option<&'static str>,
}

pub struct ParamDoc {
    /// E.g. "{path}".
    pub name: &'static str,
    /// E.g. "string|buffer".
    pub ty: &'static str,
    pub desc: &'static str,
}

pub fn api_docs() -> Vec<&'static ModuleDoc> {
    use crate::api;
    vec![
        &api::util::setup::DOCS,
        &api::pack::DOCS,
        &api::tool::DOCS,
        &api::autocmd::DOCS,
        &api::slot::DOCS,
        &api::agent::DOCS,
        &api::agent::SESSION_DOCS,
        &api::r#async::DOCS,
        &api::r#async::SEMAPHORE_DOCS,
        &api::r#async::PERMIT_DOCS,
        &api::base64::DOCS,
        &api::env::DOCS,
        &api::r#fn::DOCS,
        &api::fs::DOCS,
        &api::image::DOCS,
        &api::image::IMAGE_DOCS,
        &api::interpreter::DOCS,
        &api::json::DOCS,
        &api::json::VALIDATOR_DOCS,
        &api::keymap::DOCS,
        &api::log::DOCS,
        &api::model::DOCS,
        &api::net::DOCS,
        &api::plan::DOCS,
        &api::plan::PLAN_ACTION_DOCS,
        &api::session::DOCS,
        &api::top::DOCS,
        &api::top::TIMER_DOCS,
        &api::task::DOCS,
        &api::text::DOCS,
        &api::treesitter::DOCS,
        &api::treesitter::language::DOCS,
        &api::treesitter::query::DOCS,
        &api::treesitter::query::QUERY_DOCS,
        &api::treesitter::tree::DOCS,
        &api::treesitter::node::DOCS,
        &api::treesitter::language_tree::DOCS,
        &api::ui::DOCS,
        &api::ui::win::DOCS,
        &api::ui::buf::DOCS,
        &api::uv::DOCS,
        &api::yaml::DOCS,
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use mlua::{Lua, Table, Value};

    use super::{DocKind, api_docs};
    use crate::api::create_maki_global;
    use crate::plugin_permissions::PluginPermissions;

    fn resolve_table(maki: &Table, path: &str) -> Table {
        let mut table = maki.clone();
        for seg in path.split('.').skip(1) {
            table = table
                .get(seg)
                .unwrap_or_else(|_| panic!("`{path}`: `{seg}` is not a table"));
        }
        table
    }

    fn table_keys(table: &Table) -> BTreeSet<String> {
        let mut keys: BTreeSet<String> = table
            .pairs::<String, Value>()
            .map(|pair| pair.unwrap().0)
            .collect();
        // `maki.notify` is stashed in the `__index` table (see
        // `create_maki_global`), so the drift check has to see through the
        // metatable to consider it registered.
        if let Some(mt) = table.metatable()
            && let Ok(idx) = mt.get::<Table>("__index")
        {
            keys.extend(idx.pairs::<String, Value>().map(|p| p.unwrap().0));
        }
        keys
    }

    /// Docs and registration live side by side; this test keeps them equal so
    /// the generated reference can never drift from the real API.
    #[test]
    fn docs_match_registered_api() {
        let lua = Lua::new();
        let (ui_tx, _ui_rx) = flume::unbounded();
        let maki = create_maki_global(
            &lua,
            Arc::default(),
            Arc::default(),
            Arc::default(),
            Arc::from("docs-test"),
            Some(ui_tx),
            &PluginPermissions::trusted(),
            Arc::default(),
        )
        .unwrap();

        let mut documented: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for module in api_docs() {
            if module.kind == DocKind::Table {
                documented
                    .entry(module.name)
                    .or_default()
                    .extend(module.fns.iter().map(|f| f.name));
            }
        }
        let names: Vec<&str> = documented.keys().copied().collect();
        for name in names {
            let Some((parent, key)) = name.rsplit_once('.') else {
                continue;
            };
            documented
                .get_mut(parent)
                .unwrap_or_else(|| panic!("`{name}` documented but parent `{parent}` is not"))
                .insert(key);
        }

        // Documented here but attached by the runtime rather than by
        // `create_maki_global`, so the table built for this test has none of
        // it. `setup` is attached only when a config store is present.
        const RUNTIME_ATTACHED: [&str; 1] = ["setup"];

        for (name, mut expected) in documented {
            if RUNTIME_ATTACHED
                .iter()
                .any(|only| name == format!("maki.{only}"))
            {
                continue;
            }
            let actual = table_keys(&resolve_table(&maki, name));
            if name == "maki" {
                for only in RUNTIME_ATTACHED {
                    expected.remove(only);
                }
            }
            let expected: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                actual, expected,
                "documented functions for `{name}` do not match registered keys"
            );
        }
    }
}
