use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, ChildKind, LanguageExtractor, Section, SkeletonEntry,
    compact_ws, extract_body_members, extract_enum_variants, find_child, node_text, prefixed,
};

pub(crate) struct PhpExtractor;

impl PhpExtractor {
    fn modifiers(&self, node: Node, source: &[u8]) -> String {
        let mut parts = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "visibility_modifier"
                | "static_modifier"
                | "abstract_modifier"
                | "final_modifier"
                | "readonly_modifier" => parts.push(node_text(child, source)),
                _ => {}
            }
        }
        parts.join(" ")
    }

    fn extract_use(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mut paths: Vec<Vec<String>> = Vec::new();
        let mut prefix_parts: Vec<String> = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "namespace_use_clause" => {
                    if let Some(p) = self.use_clause_path(child, source) {
                        paths.push(p);
                    }
                }
                "namespace_name" => {
                    prefix_parts = node_text(child, source)
                        .split('\\')
                        .map(String::from)
                        .collect();
                }
                "namespace_use_group" => {
                    let mut gc = child.walk();
                    for clause in child.children(&mut gc) {
                        if clause.kind() == "namespace_use_clause"
                            && let Some(mut p) = self.use_clause_path(clause, source)
                        {
                            let mut full = prefix_parts.clone();
                            full.append(&mut p);
                            paths.push(full);
                        }
                    }
                }
                _ => {}
            }
        }
        if paths.is_empty() {
            None
        } else {
            Some(SkeletonEntry::new_import(node, paths))
        }
    }

    fn use_clause_path(&self, node: Node, source: &[u8]) -> Option<Vec<String>> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "qualified_name" | "name" => {
                    let text = node_text(child, source);
                    return Some(text.split('\\').map(String::from).collect());
                }
                _ => {}
            }
        }
        None
    }

    fn extract_namespace(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        Some(SkeletonEntry::new(Section::Module, node, name.to_string()))
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let ret = node
            .child_by_field_name("return_type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        let sig = compact_ws(&format!("function {name}{params}{ret}")).into_owned();
        Some(SkeletonEntry::new(Section::Function, node, sig))
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let extends = find_child(node, "base_clause")
            .and_then(|b| {
                let mut c = b.walk();
                b.children(&mut c)
                    .find(|ch| matches!(ch.kind(), "qualified_name" | "name"))
            })
            .map(|n| format!(" extends {}", node_text(n, source)))
            .unwrap_or_default();
        let implements = find_child(node, "class_interface_clause")
            .map(|n| {
                let mut c = n.walk();
                let names: Vec<&str> = n
                    .children(&mut c)
                    .filter(|ch| matches!(ch.kind(), "qualified_name" | "name"))
                    .map(|ch| node_text(ch, source))
                    .collect();
                if names.is_empty() {
                    String::new()
                } else {
                    format!(" implements {}", names.join(", "))
                }
            })
            .unwrap_or_default();

        let label = prefixed(&mods, format_args!("class {name}{extends}{implements}"));
        let children = self.extract_body(node, source);
        Some(SkeletonEntry::new(Section::Class, node, label).with_children(children))
    }

    fn extract_body(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        let rules = [
            BodyMemberRule {
                kind: "method_declaration",
                handler: BodyMemberHandler::Method(&|n, s| Some(self.method_sig(n, s))),
            },
            BodyMemberRule {
                kind: "property_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &|n, s| self.property_text(n, s),
                    counter: "property_declaration",
                },
            },
        ];
        extract_body_members(body, source, &rules)
    }

    fn method_sig(&self, node: Node, source: &[u8]) -> String {
        let mods = self.modifiers(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let ret = node
            .child_by_field_name("return_type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        compact_ws(&prefixed(
            &mods,
            format_args!("function {name}{params}{ret}"),
        ))
        .into_owned()
    }

    fn property_text(&self, node: Node, source: &[u8]) -> String {
        let mods = self.modifiers(node, source);
        let ty = node
            .child_by_field_name("type")
            .map(|n| format!("{} ", node_text(n, source)))
            .unwrap_or_default();
        let mut cursor = node.walk();
        let name = node
            .children(&mut cursor)
            .find(|ch| ch.kind() == "property_element")
            .and_then(|el| el.child_by_field_name("name"))
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        compact_ws(&prefixed(&mods, format_args!("{ty}{name}"))).into_owned()
    }

    fn extract_interface(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let extends = find_child(node, "base_clause")
            .map(|b| {
                let mut c = b.walk();
                let names: Vec<&str> = b
                    .children(&mut c)
                    .filter(|ch| matches!(ch.kind(), "qualified_name" | "name"))
                    .map(|ch| node_text(ch, source))
                    .collect();
                if names.is_empty() {
                    String::new()
                } else {
                    format!(" extends {}", names.join(", "))
                }
            })
            .unwrap_or_default();
        let label = format!("interface {name}{extends}");
        let children = self.extract_body(node, source);
        Some(SkeletonEntry::new(Section::Trait, node, label).with_children(children))
    }

    fn extract_trait(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let children = self.extract_body(node, source);
        Some(
            SkeletonEntry::new(Section::Trait, node, format!("trait {name}"))
                .with_children(children),
        )
    }

    fn extract_const(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers(node, source);
        let mut cursor = node.walk();
        let elements: Vec<String> = node
            .children(&mut cursor)
            .filter(|ch| ch.kind() == "const_element")
            .filter_map(|el| {
                let mut c = el.walk();
                el.children(&mut c)
                    .find(|ch| ch.kind() == "name")
                    .map(|n| node_text(n, source).to_string())
            })
            .collect();
        if elements.is_empty() {
            return None;
        }
        let label = prefixed(&mods, format_args!("const {}", elements.join(", ")));
        Some(SkeletonEntry::new(Section::Constant, node, label))
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let backing = find_child(node, "primitive_type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        let implements = find_child(node, "class_interface_clause")
            .map(|n| {
                let mut c = n.walk();
                let names: Vec<&str> = n
                    .children(&mut c)
                    .filter(|ch| matches!(ch.kind(), "qualified_name" | "name"))
                    .map(|ch| node_text(ch, source))
                    .collect();
                if names.is_empty() {
                    String::new()
                } else {
                    format!(" implements {}", names.join(", "))
                }
            })
            .unwrap_or_default();

        let body = node.child_by_field_name("body")?;
        let cases = extract_enum_variants(body, source, "enum_case");
        let label = format!("enum {name}{backing}{implements}");
        Some(
            SkeletonEntry::new(Section::Type, node, label)
                .with_children(cases)
                .with_child_kind(ChildKind::Brief),
        )
    }
}

impl LanguageExtractor for PhpExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "namespace_use_declaration" => self.extract_use(node, source).into_iter().collect(),
            "namespace_definition" => self.extract_namespace(node, source).into_iter().collect(),
            "function_definition" => self.extract_function(node, source).into_iter().collect(),
            "class_declaration" => self.extract_class(node, source).into_iter().collect(),
            "interface_declaration" => self.extract_interface(node, source).into_iter().collect(),
            "trait_declaration" => self.extract_trait(node, source).into_iter().collect(),
            "const_declaration" => self.extract_const(node, source).into_iter().collect(),
            "enum_declaration" => self.extract_enum(node, source).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "comment" && node_text(node, source).starts_with("/**")
    }

    fn import_separator(&self) -> &'static str {
        "\\"
    }
}
