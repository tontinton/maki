use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, ChildKind, LanguageExtractor, Section, SkeletonEntry,
    compact_ws, extract_body_members, extract_enum_variants, find_child, node_text, prefixed,
    simple_import,
};

pub(crate) struct JavaExtractor;

impl JavaExtractor {
    fn type_list_text(&self, parent: Node, source: &[u8]) -> String {
        let Some(tl) = find_child(parent, "type_list") else {
            return node_text(parent, source)
                .trim_start_matches("extends")
                .trim_start_matches("implements")
                .trim()
                .to_string();
        };
        node_text(tl, source).to_string()
    }

    fn implements_clause(&self, node: Node, source: &[u8]) -> String {
        node.child_by_field_name("interfaces")
            .map(|n| format!(" implements {}", self.type_list_text(n, source)))
            .unwrap_or_default()
    }

    fn extract_package(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = node_text(node, source);
        let cleaned = text
            .strip_prefix("package ")
            .unwrap_or(text)
            .trim_end_matches(';')
            .trim()
            .to_string();
        Some(SkeletonEntry::new(Section::Module, node, cleaned))
    }

    fn modifiers_text(&self, node: Node, source: &[u8]) -> String {
        let Some(mods) = find_child(node, "modifiers") else {
            return String::new();
        };
        let mut annotations = Vec::new();
        let mut keywords = Vec::new();
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            match child.kind() {
                "marker_annotation" | "annotation" => {
                    annotations.push(node_text(child, source));
                }
                _ => {
                    let text = node_text(child, source);
                    if matches!(
                        text,
                        "public"
                            | "private"
                            | "protected"
                            | "static"
                            | "final"
                            | "abstract"
                            | "default"
                            | "synchronized"
                    ) {
                        keywords.push(text);
                    }
                }
            }
        }
        annotations.extend(keywords);
        annotations.join(" ")
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let superclass = node
            .child_by_field_name("superclass")
            .and_then(|n| find_child(n, "type_identifier").or(Some(n)))
            .map(|n| format!(" extends {}", node_text(n, source)))
            .unwrap_or_default();
        let interfaces = self.implements_clause(node, source);

        let label = prefixed(
            &mods,
            format_args!("class {name}{type_params}{superclass}{interfaces}"),
        );

        let children = self.extract_class_body(node, source);
        Some(SkeletonEntry::new(Section::Class, node, label).with_children(children))
    }

    fn extract_class_body(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        extract_body_members(
            body,
            source,
            &[
                BodyMemberRule {
                    kind: "method_declaration",
                    handler: BodyMemberHandler::Method(&method_signature_opt),
                },
                BodyMemberRule {
                    kind: "constructor_declaration",
                    handler: BodyMemberHandler::Method(&method_signature_opt),
                },
                BodyMemberRule {
                    kind: "field_declaration",
                    handler: BodyMemberHandler::FieldTruncated {
                        format_fn: &field_text,
                        counter: "field_declaration",
                    },
                },
            ],
        )
    }

    fn extract_interface(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let extends = find_child(node, "extends_interfaces")
            .map(|n| format!(" extends {}", self.type_list_text(n, source)))
            .unwrap_or_default();

        let label = prefixed(
            &mods,
            format_args!("interface {name}{type_params}{extends}"),
        );

        let children = self.extract_interface_body(node, source);
        Some(SkeletonEntry::new(Section::Trait, node, label).with_children(children))
    }

    fn extract_interface_body(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        extract_body_members(
            body,
            source,
            &[
                BodyMemberRule {
                    kind: "method_declaration",
                    handler: BodyMemberHandler::Method(&method_signature_opt),
                },
                BodyMemberRule {
                    kind: "constant_declaration",
                    handler: BodyMemberHandler::Method(&|n, s| Some(field_text(n, s))),
                },
            ],
        )
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let interfaces = self.implements_clause(node, source);
        let label = prefixed(&mods, format_args!("enum {name}{type_params}{interfaces}"));

        let body = node.child_by_field_name("body")?;
        let constants = extract_enum_variants(body, source, "enum_constant");

        Some(
            SkeletonEntry::new(Section::Type, node, label)
                .with_children(constants)
                .with_child_kind(ChildKind::Brief),
        )
    }

    fn extract_record(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let params = find_child(node, "formal_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");

        let interfaces = self.implements_clause(node, source);
        let label = prefixed(
            &mods,
            format_args!("record {name}{type_params}{params}{interfaces}"),
        );

        Some(SkeletonEntry::new(Section::Type, node, label))
    }

    fn extract_annotation_type(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let label = prefixed(&mods, format_args!("@interface {name}"));
        Some(SkeletonEntry::new(Section::Type, node, label))
    }
}

fn method_signature_opt(node: Node, source: &[u8]) -> Option<String> {
    let mods = JavaExtractor.modifiers_text(node, source);
    let ret = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    let params = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source))
        .unwrap_or("()");
    let base = if ret.is_empty() {
        format!("{name}{params}")
    } else {
        format!("{ret} {name}{params}")
    };
    Some(compact_ws(&prefixed(&mods, format_args!("{base}"))).into_owned())
}

fn field_text(node: Node, source: &[u8]) -> String {
    let mods = JavaExtractor.modifiers_text(node, source);
    let ty = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    let name = find_child(node, "variable_declarator")
        .and_then(|n| n.child_by_field_name("name"))
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    prefixed(&mods, format_args!("{ty} {name}"))
}

impl LanguageExtractor for JavaExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "import_declaration" => simple_import(node, source, &["import "], ".")
                .into_iter()
                .collect(),
            "package_declaration" => self.extract_package(node, source).into_iter().collect(),
            "class_declaration" => self.extract_class(node, source).into_iter().collect(),
            "interface_declaration" => self.extract_interface(node, source).into_iter().collect(),
            "enum_declaration" => self.extract_enum(node, source).into_iter().collect(),
            "record_declaration" => self.extract_record(node, source).into_iter().collect(),
            "annotation_type_declaration" => self
                .extract_annotation_type(node, source)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "block_comment" && node_text(node, source).starts_with("/**")
    }

    fn import_separator(&self) -> &'static str {
        "."
    }
}
