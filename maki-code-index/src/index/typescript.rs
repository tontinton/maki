//! TypeScript and JavaScript extractor (shared `TsJsExtractor` handles both).
//! Export visibility is detected by checking if the parent node is `export_statement`.
//! Interfaces expand their property/method signatures as children.
//! JSDoc comments (`/** ... */`) extend the line range of the next declaration.
//! No test detection for the same reasons as Python.

use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, LanguageExtractor, Section, SkeletonEntry, compact_ws,
    extract_body_members, find_child, node_text, truncate,
};

fn ts_return_type(node: Node, source: &[u8]) -> String {
    let r = node_text(node, source);
    if r.starts_with(':') {
        r.to_string()
    } else {
        format!(": {r}")
    }
}

fn class_member_sig(node: Node, source: &[u8]) -> Option<String> {
    let mn = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    let params = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source))
        .unwrap_or_default();
    let ret = node
        .child_by_field_name("return_type")
        .map(|n| ts_return_type(n, source))
        .unwrap_or_default();
    Some(format!("{mn}{params}{ret}"))
}

pub(crate) struct TsJsExtractor;

impl TsJsExtractor {
    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = node_text(node, source);
        let cleaned = text
            .strip_prefix("import ")
            .unwrap_or(text)
            .trim_end_matches(';')
            .to_string();
        let paths = vec![vec![cleaned]];
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn export_prefix(&self, node: Node) -> &'static str {
        if self.is_exported(node) {
            "export "
        } else {
            ""
        }
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;

        let body = node.child_by_field_name("body")?;
        let rules = [
            BodyMemberRule {
                kind: "method_definition",
                handler: BodyMemberHandler::Method(&class_member_sig),
            },
            BodyMemberRule {
                kind: "public_field_definition",
                handler: BodyMemberHandler::Method(&class_member_sig),
            },
            BodyMemberRule {
                kind: "property_definition",
                handler: BodyMemberHandler::Method(&class_member_sig),
            },
        ];
        let methods = extract_body_members(body, source, &rules);

        let ep = self.export_prefix(node);
        Some(SkeletonEntry::new(Section::Class, node, format!("{ep}{name}")).with_children(methods))
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let ret_str = node
            .child_by_field_name("return_type")
            .map(|n| ts_return_type(n, source))
            .unwrap_or_default();

        let ep = self.export_prefix(node);
        Some(SkeletonEntry::new(
            Section::Function,
            node,
            compact_ws(&format!("{ep}{name}{params}{ret_str}")).into_owned(),
        ))
    }

    fn extract_interface(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let body = node.child_by_field_name("body")?;

        let mut fields = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "property_signature" || child.kind() == "method_signature" {
                let text = node_text(child, source)
                    .trim_end_matches([',', ';'])
                    .to_string();
                fields.push(text);
            }
        }

        let ep = self.export_prefix(node);
        Some(
            SkeletonEntry::new(Section::Type, node, format!("{ep}interface {name}"))
                .with_children(fields),
        )
    }

    fn extract_type_alias(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let val_str = node
            .child_by_field_name("value")
            .map(|n| format!(" = {}", truncate(node_text(n, source), 80)))
            .unwrap_or_default();
        let ep = self.export_prefix(node);
        Some(SkeletonEntry::new(
            Section::Type,
            node,
            format!("{ep}type {name}{val_str}"),
        ))
    }

    fn extract_const(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let decl = find_child(node, "variable_declarator")?;
        let name = decl
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_str = decl
            .child_by_field_name("type")
            .map(|n| ts_return_type(n, source))
            .unwrap_or_default();
        let val_str = decl
            .child_by_field_name("value")
            .map(|n| format!(" = {}", truncate(node_text(n, source), 60)))
            .unwrap_or_default();
        let ep = self.export_prefix(node);
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            format!("{ep}{name}{type_str}{val_str}"),
        ))
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let ep = self.export_prefix(node);
        Some(SkeletonEntry::new(
            Section::Type,
            node,
            format!("{ep}enum {name}"),
        ))
    }

    fn is_exported(&self, node: Node) -> bool {
        node.parent()
            .is_some_and(|p| p.kind() == "export_statement")
    }

    fn extract_export_statement(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_declaration" => return self.extract_class(child, source),
                "function_declaration" => return self.extract_function(child, source),
                "interface_declaration" => return self.extract_interface(child, source),
                "type_alias_declaration" => return self.extract_type_alias(child, source),
                "lexical_declaration" => return self.extract_lexical_declaration(child, source),
                "enum_declaration" => return self.extract_enum(child, source),
                _ => {}
            }
        }
        None
    }

    fn extract_lexical_declaration(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let kind_text = node.child(0).map(|n| node_text(n, source)).unwrap_or("");
        if kind_text == "const" {
            self.extract_const(node, source)
        } else {
            None
        }
    }
}

impl LanguageExtractor for TsJsExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        let entry = match node.kind() {
            "import_statement" => self.extract_import(node, source),
            "class_declaration" => self.extract_class(node, source),
            "function_declaration" => self.extract_function(node, source),
            "interface_declaration" => self.extract_interface(node, source),
            "type_alias_declaration" => self.extract_type_alias(node, source),
            "enum_declaration" => self.extract_enum(node, source),
            "lexical_declaration" => self.extract_lexical_declaration(node, source),
            "export_statement" => self.extract_export_statement(node, source),
            _ => None,
        };
        entry.into_iter().collect()
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "comment" && node_text(node, source).starts_with("/**")
    }
}
