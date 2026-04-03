use tree_sitter::Node;

use super::common::{LanguageExtractor, Section, SkeletonEntry, node_text, truncate};

pub(crate) struct BashExtractor;

impl BashExtractor {
    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        Some(SkeletonEntry::new(
            Section::Function,
            node,
            format!("{name}()"),
        ))
    }

    fn extract_variable(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        if !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return None;
        }
        let value = node
            .child_by_field_name("value")
            .map(|n| format!(" = {}", truncate(node_text(n, source), 60)))
            .unwrap_or_default();
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            format!("{name}{value}"),
        ))
    }
}

impl LanguageExtractor for BashExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "function_definition" => self.extract_function(node, source).into_iter().collect(),
            "variable_assignment" => self.extract_variable(node, source).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, _node: Node, _source: &[u8]) -> bool {
        false
    }
}
