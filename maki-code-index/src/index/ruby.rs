use tree_sitter::Node;

use super::common::{
    LanguageExtractor, Section, SkeletonEntry, compact_ws, find_child, line_range, node_text,
    truncate,
};

pub(crate) struct RubyExtractor;

impl RubyExtractor {
    fn extract_require(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let method = node.child_by_field_name("method")?;
        let method_name = node_text(method, source);
        if method_name != "require" && method_name != "require_relative" {
            return None;
        }
        let args = node.child_by_field_name("arguments")?;
        let string_node = find_child(args, "string")?;
        let raw = find_child(string_node, "string_content")
            .map(|n| node_text(n, source))
            .unwrap_or_else(|| {
                let t = node_text(string_node, source);
                t.trim_matches(|c| c == '"' || c == '\'')
            });
        let paths = vec![
            raw.split(self.import_separator())
                .map(String::from)
                .collect(),
        ];
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(name_node, source);
        let superclass = node
            .child_by_field_name("superclass")
            .map(|n| {
                let t = node_text(n, source).trim_start_matches('<').trim();
                format!(" < {t}")
            })
            .unwrap_or_default();

        let methods = node
            .child_by_field_name("body")
            .map(|body| self.extract_body_methods(body, source))
            .unwrap_or_default();

        Some(
            SkeletonEntry::new(Section::Class, node, format!("{name}{superclass}"))
                .with_children(methods),
        )
    }

    fn extract_body_methods(&self, body: Node, source: &[u8]) -> Vec<String> {
        let mut methods = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "method" => {
                    if let Some(sig) = self.method_sig(child, source, false) {
                        methods.push(sig);
                    }
                }
                "singleton_method" => {
                    if let Some(sig) = self.method_sig(child, source, true) {
                        methods.push(sig);
                    }
                }
                _ => {}
            }
        }
        methods
    }

    fn method_sig(&self, node: Node, source: &[u8], is_singleton: bool) -> Option<String> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let prefix = if is_singleton { "self." } else { "" };
        let lr = line_range(node.start_position().row + 1, node.end_position().row + 1);
        Some(compact_ws(&format!("{prefix}{name}{params} {lr}")).into_owned())
    }

    fn extract_module(&self, node: Node, source: &[u8]) -> Vec<SkeletonEntry> {
        let Some(name) = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
        else {
            return Vec::new();
        };
        let mut entries = vec![SkeletonEntry::new(Section::Module, node, name.to_string())];
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                entries.extend(self.extract_nodes(child, source, &[]));
            }
        }
        entries
    }
    fn extract_method(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        Some(SkeletonEntry::new(
            Section::Function,
            node,
            compact_ws(&format!("{name}{params}")).into_owned(),
        ))
    }

    fn extract_singleton_method(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        Some(SkeletonEntry::new(
            Section::Function,
            node,
            compact_ws(&format!("self.{name}{params}")).into_owned(),
        ))
    }

    fn extract_constant(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let left = node.child_by_field_name("left")?;
        let name = node_text(left, source);
        if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
            return None;
        }
        let value = node
            .child_by_field_name("right")
            .map(|n| truncate(node_text(n, source), 60));
        let val_str = value.map(|v| format!(" = {v}")).unwrap_or_default();
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            format!("{name}{val_str}"),
        ))
    }
}

impl LanguageExtractor for RubyExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "module" => self.extract_module(node, source),
            kind => {
                let entry = match kind {
                    "call" => self.extract_require(node, source),
                    "class" => self.extract_class(node, source),
                    "method" => self.extract_method(node, source),
                    "singleton_method" => self.extract_singleton_method(node, source),
                    "assignment" => self.extract_constant(node, source),
                    _ => None,
                };
                entry.into_iter().collect()
            }
        }
    }

    fn is_doc_comment(&self, node: Node, _source: &[u8]) -> bool {
        node.kind() == "comment"
    }

    fn import_separator(&self) -> &'static str {
        "/"
    }
}
