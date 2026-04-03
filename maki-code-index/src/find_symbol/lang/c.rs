use std::sync::LazyLock;

use tree_sitter::{Node, Query, Tree};

use super::super::language::{CaptureResult, RefLanguage, ScopeContext, SearchScope};
use super::c_family;
use crate::Language;

const C_REFERENCE_QUERY: &str = r#"
(call_expression function: (identifier) @call)
(call_expression function: (field_expression field: (field_identifier) @call))
(declaration declarator: (function_declarator declarator: (identifier) @def))
(function_definition declarator: (function_declarator declarator: (identifier) @def))
(preproc_function_def parameters: (preproc_params) value: (preproc_arg) @macro_body)
(preproc_def value: (preproc_arg) @macro_body)
(type_identifier) @type_ref
(field_expression field: (field_identifier) @field_ref)
(field_designator (field_identifier) @field_ref)
(identifier) @ref
"#;

pub static C_LANGUAGE: LazyLock<CLanguage> = LazyLock::new(CLanguage::new);

pub struct CLanguage {
    query: Query,
}

impl CLanguage {
    fn new() -> Self {
        let lang = Language::C.ts_language();
        Self {
            query: Query::new(&lang, C_REFERENCE_QUERY).expect("C reference query is valid"),
        }
    }
}

impl RefLanguage for CLanguage {
    fn extensions(&self) -> &'static [&'static str] {
        &["c", "h"]
    }

    fn reference_query(&self) -> &Query {
        &self.query
    }

    fn identifier_kinds(&self) -> &'static [&'static str] {
        &["identifier", "field_identifier", "type_identifier"]
    }

    fn analyze_scope(
        &self,
        tree: &Tree,
        symbol_byte_offset: usize,
        ctx: &ScopeContext,
    ) -> SearchScope {
        c_family::analyze_scope_c_family(tree, symbol_byte_offset, ctx)
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
