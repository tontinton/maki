use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, ChildKind, LanguageExtractor, Section, SkeletonEntry,
    compact_ws, extract_body_members, extract_enum_variants, find_child, node_text, prefixed,
    simple_import,
};

pub(crate) struct CSharpExtractor;

const MODIFIER_KEYWORDS: &[&str] = &[
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "abstract",
    "sealed",
    "override",
    "virtual",
    "async",
    "readonly",
    "extern",
    "partial",
    "new",
    "unsafe",
    "volatile",
];

impl CSharpExtractor {
    fn base_list_text(&self, node: Node, source: &[u8]) -> String {
        let Some(bl) = find_child(node, "base_list") else {
            return String::new();
        };
        let text = node_text(bl, source);
        let trimmed = text.trim_start_matches(':').trim();
        format!(" : {trimmed}")
    }

    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        simple_import(node, source, &["using "], ".")
    }

    fn extract_namespace(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        Some(SkeletonEntry::new(Section::Module, node, name.to_string()))
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text_free(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let bases = self.base_list_text(node, source);
        let label = prefixed(&mods, format_args!("class {name}{bases}"));
        let children = self.extract_declaration_list(node, source);
        Some(SkeletonEntry::new(Section::Class, node, label).with_children(children))
    }

    fn extract_struct(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text_free(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let bases = self.base_list_text(node, source);
        let label = prefixed(&mods, format_args!("struct {name}{bases}"));
        let children = self.extract_declaration_list(node, source);
        Some(SkeletonEntry::new(Section::Type, node, label).with_children(children))
    }

    fn extract_interface(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text_free(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let bases = self.base_list_text(node, source);
        let label = prefixed(&mods, format_args!("interface {name}{bases}"));
        let children = self.extract_interface_body(node, source);
        Some(SkeletonEntry::new(Section::Trait, node, label).with_children(children))
    }

    fn extract_record(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text_free(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = find_child(node, "parameter_list")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let bases = self.base_list_text(node, source);
        let label = prefixed(&mods, format_args!("record {name}{params}{bases}"));
        Some(SkeletonEntry::new(Section::Type, node, label))
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text_free(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let label = prefixed(&mods, format_args!("enum {name}"));
        let body = node.child_by_field_name("body")?;
        let constants = extract_enum_variants(body, source, "enum_member_declaration");
        Some(
            SkeletonEntry::new(Section::Type, node, label)
                .with_children(constants)
                .with_child_kind(ChildKind::Brief),
        )
    }

    fn extract_declaration_list(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        let rules = [
            BodyMemberRule {
                kind: "method_declaration",
                handler: BodyMemberHandler::Method(&|n, s| Some(method_signature_free(n, s))),
            },
            BodyMemberRule {
                kind: "constructor_declaration",
                handler: BodyMemberHandler::Method(&|n, s| Some(method_signature_free(n, s))),
            },
            BodyMemberRule {
                kind: "field_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &field_text_free,
                    counter: "fields",
                },
            },
            BodyMemberRule {
                kind: "property_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &property_text_free,
                    counter: "fields",
                },
            },
        ];
        extract_body_members(body, source, &rules)
    }

    fn extract_interface_body(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        let rules = [
            BodyMemberRule {
                kind: "method_declaration",
                handler: BodyMemberHandler::Method(&|n, s| Some(method_signature_free(n, s))),
            },
            BodyMemberRule {
                kind: "property_declaration",
                handler: BodyMemberHandler::Method(&|n, s| Some(property_text_free(n, s))),
            },
        ];
        extract_body_members(body, source, &rules)
    }
}

fn modifiers_text_free(node: Node, source: &[u8]) -> String {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let text = node_text(child, source);
        let include = match child.kind() {
            "modifier" => MODIFIER_KEYWORDS.contains(&text),
            "attribute_list" => true,
            _ => false,
        };
        if include {
            parts.push(text.to_string());
        }
    }
    parts.join(" ")
}

fn method_signature_free(node: Node, source: &[u8]) -> String {
    let mods = modifiers_text_free(node, source);
    let ret = node
        .child_by_field_name("returns")
        .or_else(|| node.child_by_field_name("type"))
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
    compact_ws(&prefixed(&mods, format_args!("{base}"))).into_owned()
}

fn field_text_free(node: Node, source: &[u8]) -> String {
    let mods = modifiers_text_free(node, source);
    let decl = find_child(node, "variable_declaration");
    let ty = decl
        .and_then(|n| n.child_by_field_name("type"))
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    let name = decl
        .and_then(|d| find_child(d, "variable_declarator"))
        .and_then(|n| n.child_by_field_name("name"))
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    prefixed(&mods, format_args!("{ty} {name}"))
}

fn property_text_free(node: Node, source: &[u8]) -> String {
    let mods = modifiers_text_free(node, source);
    let ty = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("_");
    prefixed(&mods, format_args!("{ty} {name}"))
}

impl LanguageExtractor for CSharpExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "using_directive" => self.extract_import(node, source).into_iter().collect(),
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                self.extract_namespace(node, source).into_iter().collect()
            }
            "class_declaration" => self.extract_class(node, source).into_iter().collect(),
            "struct_declaration" => self.extract_struct(node, source).into_iter().collect(),
            "interface_declaration" => self.extract_interface(node, source).into_iter().collect(),
            "enum_declaration" => self.extract_enum(node, source).into_iter().collect(),
            "record_declaration" => self.extract_record(node, source).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "single_line_doc_comment"
            || (node.kind() == "comment" && node_text(node, source).starts_with("///"))
    }

    fn import_separator(&self) -> &'static str {
        "."
    }
}
