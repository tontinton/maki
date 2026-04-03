use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use tree_sitter::{Node, Query, Tree};

use super::super::language::{
    CaptureResult, RefLanguage, ReferenceKind, ScopeContext, SearchScope,
};
use crate::Language;
use crate::helpers::{find_ancestor_any, find_child, node_at_offset, node_text};

const RUST_REFERENCE_QUERY: &str = r#"
; Definitions
(function_item name: (identifier) @def)
(struct_item name: (type_identifier) @def)
(enum_item name: (type_identifier) @def)
(trait_item name: (type_identifier) @def)
(impl_item trait: (type_identifier) @type_ref)
(impl_item type: (type_identifier) @type_ref)
(const_item name: (identifier) @def)
(static_item name: (identifier) @def)
(type_item name: (type_identifier) @def)
(mod_item name: (identifier) @def)

; Function/method calls
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier name: (identifier) @call))
(call_expression function: (field_expression field: (field_identifier) @call))

; Type references
(type_identifier) @type_ref
(scoped_type_identifier name: (type_identifier) @type_ref)

; Field access
(field_expression field: (field_identifier) @field_ref)
(shorthand_field_identifier) @field_ref

; General identifiers
(identifier) @ref
"#;

const ITEM_KINDS: &[&str] = &[
    "function_item",
    "struct_item",
    "enum_item",
    "trait_item",
    "const_item",
    "static_item",
    "type_item",
    "mod_item",
    "impl_item",
];

const SCOPE_KINDS: &[&str] = &[
    "function_item",
    "closure_expression",
    "block",
    "impl_item",
    "trait_item",
    "mod_item",
];

const BINDING_PARENT_KINDS: &[&str] = &[
    "let_declaration",
    "for_expression",
    "match_arm",
    "if_let_expression",
    "while_let_expression",
];

pub static RUST_LANGUAGE: LazyLock<RustLanguage> = LazyLock::new(RustLanguage::new);

pub struct RustLanguage {
    query: Query,
}

impl RustLanguage {
    fn new() -> Self {
        let lang = Language::Rust.ts_language();
        Self {
            query: Query::new(&lang, RUST_REFERENCE_QUERY).expect("Rust reference query is valid"),
        }
    }
}

fn find_cargo_crate_root(file: &Path) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            let content = std::fs::read_to_string(&cargo_toml).ok()?;
            if content.contains("[package]") {
                return Some(dir.to_path_buf());
            }
        }
        dir = dir.parent()?;
    }
}

impl RefLanguage for RustLanguage {
    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn reference_query(&self) -> &Query {
        &self.query
    }

    fn identifier_kinds(&self) -> &'static [&'static str] {
        &["identifier", "type_identifier", "field_identifier"]
    }

    fn analyze_scope(
        &self,
        tree: &Tree,
        symbol_byte_offset: usize,
        ctx: &ScopeContext,
    ) -> SearchScope {
        let Some(node) = node_at_offset(tree, symbol_byte_offset) else {
            return SearchScope::Project { subtree: None };
        };

        if is_local_binding(&node)
            && let Some(scope_node) = find_ancestor_any(node, SCOPE_KINDS)
        {
            return SearchScope::Local {
                file: ctx.file.to_path_buf(),
                range: Some(scope_node.byte_range()),
            };
        }

        if let Some(item) = find_ancestor_any(node, ITEM_KINDS) {
            let crate_root = find_cargo_crate_root(ctx.file);
            match get_visibility(&item, ctx.source) {
                Visibility::Private => {
                    return SearchScope::Local {
                        file: ctx.file.to_path_buf(),
                        range: None,
                    };
                }
                Visibility::PubSuper => {
                    let parent_dir = ctx
                        .file
                        .parent()
                        .and_then(|p| p.parent())
                        .unwrap_or(ctx.project_root);
                    return SearchScope::Project {
                        subtree: Some(parent_dir.to_path_buf()),
                    };
                }
                Visibility::PubCrate => {
                    return SearchScope::Project {
                        subtree: crate_root,
                    };
                }
                Visibility::PubIn(mod_path) => {
                    let subtree = resolve_mod_path(&mod_path, ctx, crate_root.as_deref());
                    return SearchScope::Project { subtree };
                }
                Visibility::Pub => {}
            }
        }

        SearchScope::Project { subtree: None }
    }

    fn classify_capture(
        &self,
        capture_name: &str,
        node: Node,
        _source: &[u8],
        _symbol: &str,
    ) -> Option<CaptureResult> {
        if let Some(parent) = node.parent()
            && matches!(parent.kind(), "lifetime" | "label" | "loop_label")
        {
            return None;
        }

        let kind = match capture_name {
            "def" => ReferenceKind::Definition,
            "call" => ReferenceKind::Call,
            "type_ref" => ReferenceKind::TypeRef,
            "field_ref" => ReferenceKind::FieldRef,
            "ref" => ReferenceKind::Read,
            "write" => ReferenceKind::Write,
            _ => ReferenceKind::Unknown,
        };
        Some(CaptureResult {
            kind,
            text_search: false,
        })
    }
}

#[derive(Debug)]
enum Visibility {
    Private,
    Pub,
    PubCrate,
    PubSuper,
    PubIn(String),
}

fn get_visibility(item: &Node, source: &[u8]) -> Visibility {
    let vis_node = item
        .child_by_field_name("visibility")
        .or_else(|| find_child(*item, "visibility_modifier"));

    let Some(vis) = vis_node else {
        return Visibility::Private;
    };

    let text = node_text(vis, source);
    if text == "pub" {
        return Visibility::Pub;
    }
    if text.contains("crate") {
        return Visibility::PubCrate;
    }
    if text.contains("super") {
        return Visibility::PubSuper;
    }
    if text.starts_with("pub(in ") {
        let path = text.trim_start_matches("pub(in ").trim_end_matches(')');
        return Visibility::PubIn(path.to_string());
    }

    Visibility::Pub
}

fn is_local_binding(node: &Node) -> bool {
    let mut current = *node;
    loop {
        let Some(parent) = current.parent() else {
            return false;
        };
        match parent.kind() {
            kind if BINDING_PARENT_KINDS.contains(&kind) => {
                return parent.child_by_field_name("pattern").is_some_and(|pat| {
                    node.byte_range().start >= pat.start_byte()
                        && node.byte_range().end <= pat.end_byte()
                });
            }
            "parameter" | "closure_parameters" => return true,
            "function_item" | "impl_item" | "trait_item" | "mod_item" | "source_file" => {
                return false;
            }
            _ => current = parent,
        }
    }
}

fn resolve_mod_path(
    mod_path: &str,
    ctx: &ScopeContext,
    crate_root: Option<&Path>,
) -> Option<PathBuf> {
    let mut dir = ctx.file.parent()?.to_path_buf();

    for part in mod_path.split("::") {
        match part {
            "super" => dir = dir.parent()?.to_path_buf(),
            "crate" => dir = crate_root?.to_path_buf(),
            "self" => {}
            name => {
                let mod_dir = dir.join(name);
                if mod_dir.is_dir() {
                    dir = mod_dir;
                } else {
                    let mod_file = dir.join(format!("{name}.rs"));
                    if mod_file.exists() {
                        return Some(mod_file.parent()?.to_path_buf());
                    }
                    return None;
                }
            }
        }
    }

    Some(dir)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use test_case::test_case;
    use tree_sitter::Parser;

    use super::*;

    fn scope_of(source: &str, symbol: &str) -> SearchScope {
        let mut parser = Parser::new();
        parser.set_language(&Language::Rust.ts_language()).unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let offset = source.find(symbol).unwrap();
        let ctx = ScopeContext {
            file: Path::new("test.rs"),
            source: source.as_bytes(),
            project_root: Path::new("/"),
        };
        RUST_LANGUAGE.analyze_scope(&tree, offset, &ctx)
    }

    #[test]
    fn local_let_binding() {
        let scope = scope_of("fn foo() { let x = 1; x }", "x");
        assert!(matches!(scope, SearchScope::Local { range: Some(_), .. }));
    }

    #[test]
    fn private_function() {
        let scope = scope_of("fn private_fn() {}", "private_fn");
        assert!(matches!(scope, SearchScope::Local { range: None, .. }));
    }

    #[test_case("pub(crate) fn crate_fn() {}",  "crate_fn"  ; "pub_crate")]
    #[test_case("pub fn public_fn() {}",         "public_fn" ; "pub_fn")]
    fn project_scoped(source: &str, symbol: &str) {
        assert!(matches!(
            scope_of(source, symbol),
            SearchScope::Project { .. }
        ));
    }
}
