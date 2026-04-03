use std::path::Path;

use tree_sitter::{Node, Tree};

use super::super::language::{CaptureResult, ReferenceKind, ScopeContext, SearchScope};
use crate::helpers::{find_ancestor_any, node_at_offset, node_text};

const EXCLUDED_PARENT_KINDS: &[&str] = &[
    "preproc_ifdef",
    "preproc_defined",
    "labeled_statement",
    "goto_statement",
];

const EXCLUDED_GRANDPARENT_KINDS: &[&str] = &["attribute_specifier", "attribute_declaration"];

const TYPE_ID_EXCLUDED_PARENTS: &[&str] = &[
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
];

const DECLARATION_KINDS: &[&str] = &[
    "function_definition",
    "declaration",
    "field_declaration",
    "preproc_def",
    "preproc_function_def",
    "template_declaration",
];

const BOUNDARY_KINDS: &[&str] = &["translation_unit"];

pub fn classify_c_capture(
    capture_name: &str,
    node: Node,
    _source: &[u8],
    _symbol: &str,
) -> Option<CaptureResult> {
    if capture_name == "template_ctx" {
        return None;
    }

    if capture_name == "macro_body" {
        return Some(CaptureResult {
            kind: ReferenceKind::MacroUse,
            text_search: true,
        });
    }

    if let Some(parent) = node.parent() {
        let pk = parent.kind();
        if EXCLUDED_PARENT_KINDS.contains(&pk) {
            return None;
        }
        if capture_name == "type_ref" && TYPE_ID_EXCLUDED_PARENTS.contains(&pk) {
            return None;
        }
        if let Some(gp) = parent.parent()
            && EXCLUDED_GRANDPARENT_KINDS.contains(&gp.kind())
        {
            return None;
        }
    }

    let kind = match capture_name {
        "def" => ReferenceKind::Definition,
        "call" => ReferenceKind::Call,
        "type_ref" => ReferenceKind::TypeRef,
        "ns_ref" => ReferenceKind::Read,
        "field_ref" => ReferenceKind::FieldRef,
        "ref" => ReferenceKind::Read,
        _ => ReferenceKind::Unknown,
    };
    Some(CaptureResult {
        kind,
        text_search: false,
    })
}

pub fn has_storage_class(decl_node: &Node, source: &[u8], class: &str) -> bool {
    let mut cursor = decl_node.walk();
    if decl_node
        .children(&mut cursor)
        .any(|c| c.kind() == "storage_class_specifier" && node_text(c, source) == class)
    {
        return true;
    }
    if decl_node.kind() == "function_definition"
        && let Some(prev) = prev_named_sibling_skip_preproc(decl_node)
        && prev.kind() == "declaration"
        && is_phantom_split_declaration(&prev)
        && has_storage_class(&prev, source, class)
    {
        return true;
    }
    false
}

fn is_phantom_split_declaration(decl: &Node) -> bool {
    let mut cursor = decl.walk();
    !decl
        .children(&mut cursor)
        .any(|c| c.kind() == "function_declarator" || c.kind() == "init_declarator")
}

fn prev_named_sibling_skip_preproc<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    let mut cur = node.prev_named_sibling()?;
    while cur.kind().starts_with("preproc_") || cur.kind() == "comment" {
        cur = cur.prev_named_sibling()?;
    }
    Some(cur)
}

pub fn find_enclosing_declaration(node: Node) -> Option<Node> {
    let mut current = node;
    loop {
        if DECLARATION_KINDS.contains(&current.kind()) {
            return Some(current);
        }
        if BOUNDARY_KINDS.contains(&current.kind()) {
            return None;
        }
        current = current.parent()?;
    }
}

pub fn is_local_variable(node: &Node) -> bool {
    let mut current = *node;
    let mut inside_body = false;
    let mut inside_params = false;
    loop {
        let Some(parent) = current.parent() else {
            return false;
        };
        match parent.kind() {
            "compound_statement" => inside_body = true,
            "parameter_list" => inside_params = true,
            "lambda_capture_specifier" => return false,
            "function_definition" | "lambda_expression" => return inside_body || inside_params,
            "translation_unit" => return false,
            "class_specifier"
            | "struct_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "field_declaration_list" => return false,
            _ => {}
        }
        current = parent;
    }
}

pub fn find_enclosing_function(node: Node) -> Option<Node> {
    find_ancestor_any(node, &["function_definition", "lambda_expression"])
}

pub fn is_header_file(file: &Path) -> bool {
    file.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext, "h" | "hpp" | "hh" | "hxx"))
}

pub fn analyze_scope_c_family(
    tree: &Tree,
    symbol_byte_offset: usize,
    ctx: &ScopeContext,
) -> SearchScope {
    let Some(node) = node_at_offset(tree, symbol_byte_offset) else {
        return SearchScope::Project { subtree: None };
    };

    let is_header = is_header_file(ctx.file);

    if is_local_variable(&node)
        && let Some(func) = find_enclosing_function(node)
    {
        return SearchScope::Local {
            file: ctx.file.to_path_buf(),
            range: Some(func.byte_range()),
        };
    }

    if let Some(decl) = find_enclosing_declaration(node) {
        match decl.kind() {
            "preproc_def" | "preproc_function_def" if !is_header => {
                return SearchScope::Local {
                    file: ctx.file.to_path_buf(),
                    range: None,
                };
            }
            "function_definition" | "declaration"
                if has_storage_class(&decl, ctx.source, "static") && !is_header =>
            {
                return SearchScope::Local {
                    file: ctx.file.to_path_buf(),
                    range: None,
                };
            }
            _ => {}
        }
    }

    SearchScope::Project { subtree: None }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use test_case::test_case;
    use tree_sitter::Parser;

    use crate::Language;

    use super::*;

    fn scope_of(source: &str, symbol: &str) -> SearchScope {
        let mut parser = Parser::new();
        parser.set_language(&Language::C.ts_language()).unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let offset = source.find(symbol).unwrap();
        let ctx = ScopeContext {
            file: Path::new("test.c"),
            source: source.as_bytes(),
            project_root: Path::new("/"),
        };
        analyze_scope_c_family(&tree, offset, &ctx)
    }

    fn scope_of_last(source: &str, symbol: &str) -> SearchScope {
        let mut parser = Parser::new();
        parser.set_language(&Language::C.ts_language()).unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let offset = source.rfind(symbol).unwrap();
        let ctx = ScopeContext {
            file: Path::new("test.c"),
            source: source.as_bytes(),
            project_root: Path::new("/"),
        };
        analyze_scope_c_family(&tree, offset, &ctx)
    }

    #[test_case("static void foo(void) { }",                                    "foo"         ; "static_function")]
    #[test_case("static void __sched notrace __schedule(int sched_mode) { }",   "__schedule"  ; "static_function_with_attribute_macros")]
    #[test_case("static int counter = 0;",                                       "counter"     ; "static_variable")]
    fn file_scoped(source: &str, symbol: &str) {
        assert!(matches!(
            scope_of(source, symbol),
            SearchScope::Local { range: None, .. }
        ));
    }

    #[test]
    fn local_variable_function_scoped() {
        let scope = scope_of("void foo(void) { int x = 1; }", "x");
        assert!(matches!(scope, SearchScope::Local { range: Some(_), .. }));
    }

    #[test]
    fn non_static_function_project_wide() {
        let scope = scope_of("void schedule(void) { }", "schedule");
        assert!(matches!(scope, SearchScope::Project { subtree: None }));
    }

    #[test]
    fn non_static_after_static_forward_decl() {
        let source = "static void __sched __mutex_lock_slowpath(struct mutex *lock);\nvoid __sched mutex_lock(struct mutex *lock) { }";
        let scope = scope_of_last(source, "mutex_lock");
        assert!(
            matches!(scope, SearchScope::Project { subtree: None }),
            "non-static function should be project-wide, got: {scope}"
        );
    }
}
