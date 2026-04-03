use std::sync::LazyLock;

use tree_sitter::{Node, Query, Tree};

use super::super::language::{CaptureResult, RefLanguage, ScopeContext, SearchScope};
use super::c_family;
use crate::Language;

const CPP_REFERENCE_QUERY: &str = r#"
; Definitions
(function_definition declarator: (function_declarator declarator: (identifier) @def))
(function_definition declarator: (function_declarator declarator: (field_identifier) @def))
(function_definition declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @def)))
(function_definition declarator: (function_declarator declarator: (qualified_identifier name: (destructor_name) @def)))
(declaration declarator: (function_declarator declarator: (identifier) @def))
(declaration declarator: (function_declarator declarator: (field_identifier) @def))
(declaration declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @def)))
(field_declaration declarator: (function_declarator declarator: (field_identifier) @def))

; Class/struct/enum/union definitions
(class_specifier name: (type_identifier) @def)
(struct_specifier name: (type_identifier) @def)
(enum_specifier name: (type_identifier) @def)
(union_specifier name: (type_identifier) @def)
(alias_declaration name: (type_identifier) @def)
(type_definition declarator: (type_identifier) @def)
(concept_definition name: (identifier) @def)

; Field/variable declarations
(field_declaration declarator: (field_identifier) @def)
(init_declarator declarator: (identifier) @def)

; Namespace definitions
(namespace_definition name: (namespace_identifier) @def)

; Template declarations (the inner nodes handle the actual defs)
(template_declaration) @template_ctx

; Function/method calls
(call_expression function: (identifier) @call)
(call_expression function: (field_expression field: (field_identifier) @call))
(call_expression function: (qualified_identifier name: (identifier) @call))
(call_expression function: (template_function name: (identifier) @call))
(call_expression function: (qualified_identifier name: (template_function name: (identifier) @call)))

; Type references
(type_identifier) @type_ref
(qualified_identifier scope: (namespace_identifier) @ns_ref)

; Field access
(field_expression field: (field_identifier) @field_ref)
(field_designator (field_identifier) @field_ref)

; Preprocessor
(preproc_function_def name: (identifier) @def)
(preproc_def name: (identifier) @def)
(preproc_function_def parameters: (preproc_params) value: (preproc_arg) @macro_body)
(preproc_def value: (preproc_arg) @macro_body)

; General identifiers (catch-all, lowest priority)
(identifier) @ref
"#;

pub static CPP_LANGUAGE: LazyLock<CppLanguage> = LazyLock::new(CppLanguage::new);

pub struct CppLanguage {
    query: Query,
}

impl CppLanguage {
    fn new() -> Self {
        let lang = Language::Cpp.ts_language();
        Self {
            query: Query::new(&lang, CPP_REFERENCE_QUERY).expect("C++ reference query is valid"),
        }
    }
}

impl RefLanguage for CppLanguage {
    fn extensions(&self) -> &'static [&'static str] {
        &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "h"]
    }

    fn reference_query(&self) -> &Query {
        &self.query
    }

    fn identifier_kinds(&self) -> &'static [&'static str] {
        &[
            "identifier",
            "field_identifier",
            "type_identifier",
            "namespace_identifier",
            "destructor_name",
        ]
    }

    fn analyze_scope(
        &self,
        tree: &Tree,
        symbol_byte_offset: usize,
        ctx: &ScopeContext,
    ) -> SearchScope {
        let base = c_family::analyze_scope_c_family(tree, symbol_byte_offset, ctx);
        if matches!(base, SearchScope::Project { .. })
            && let Some(node) = crate::helpers::node_at_offset(tree, symbol_byte_offset)
            && is_in_anonymous_namespace(node)
            && !c_family::is_header_file(ctx.file)
        {
            return SearchScope::Local {
                file: ctx.file.to_path_buf(),
                range: None,
            };
        }
        base
    }

    fn classify_capture(
        &self,
        capture_name: &str,
        node: Node,
        source: &[u8],
        symbol: &str,
    ) -> Option<CaptureResult> {
        c_family::classify_c_capture(capture_name, node, source, symbol)
    }
}

fn is_in_anonymous_namespace(node: Node) -> bool {
    let mut current = node;
    loop {
        let Some(parent) = current.parent() else {
            return false;
        };
        if parent.kind() == "namespace_definition" {
            let mut cursor = parent.walk();
            let has_name = parent
                .children(&mut cursor)
                .any(|c| c.kind() == "namespace_identifier");
            if !has_name {
                return true;
            }
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tree_sitter::Parser;

    use crate::Language;
    use crate::find_symbol::language::{RefLanguage, ScopeContext, SearchScope};

    use super::*;

    fn scope_of_with_file(source: &str, symbol: &str, file: &str) -> SearchScope {
        let mut parser = Parser::new();
        parser.set_language(&Language::Cpp.ts_language()).unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let offset = source.find(symbol).unwrap();
        let ctx = ScopeContext {
            file: Path::new(file),
            source: source.as_bytes(),
            project_root: Path::new("/"),
        };
        CPP_LANGUAGE.analyze_scope(&tree, offset, &ctx)
    }

    fn scope_of(source: &str, symbol: &str) -> SearchScope {
        scope_of_with_file(source, symbol, "test.cpp")
    }

    #[test]
    fn anonymous_namespace_file_scoped() {
        let scope = scope_of("namespace { void internal() { } }", "internal");
        assert!(matches!(scope, SearchScope::Local { range: None, .. }));
    }

    #[test]
    fn anonymous_namespace_in_header_stays_project() {
        let scope = scope_of_with_file("namespace { void internal() { } }", "internal", "test.hpp");
        assert!(matches!(scope, SearchScope::Project { .. }));
    }

    #[test]
    fn named_namespace_project_scoped() {
        let scope = scope_of("namespace foo { void bar() { } }", "bar");
        assert!(matches!(scope, SearchScope::Project { subtree: None }));
    }
}
