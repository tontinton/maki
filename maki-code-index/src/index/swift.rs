use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, LanguageExtractor, Section, SkeletonEntry, compact_ws,
    extract_body_members, find_child, line_range, node_text, prefixed, truncate,
};

pub(crate) struct SwiftExtractor;

fn modifiers_text<'a>(node: Node<'a>, source: &'a [u8]) -> String {
    let Some(mods) = find_child(node, "modifiers") else {
        return String::new();
    };
    let mut cursor = mods.walk();
    mods.children(&mut cursor)
        .filter_map(|child| match child.kind() {
            "visibility_modifier" => {
                let t = node_text(child, source);
                let vis = t.split('(').next().unwrap_or(t).trim();
                Some(vis.to_string())
            }
            "function_modifier" | "member_modifier" | "mutation_modifier" => {
                Some(node_text(child, source).to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn params_text(node: Node, source: &[u8]) -> String {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "parameter" {
            let external = child
                .child_by_field_name("external_name")
                .map(|n| node_text(n, source));
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or("_");
            let ty = child
                .child_by_field_name("type")
                .map(|n| node_text(n, source))
                .unwrap_or("_");
            let label = match external {
                Some(ext) if ext != name => format!("{ext} {name}: {ty}"),
                _ => format!("{name}: {ty}"),
            };
            parts.push(label);
        }
    }
    format!("({})", parts.join(", "))
}

fn swift_fn_signature(node: Node, source: &[u8]) -> Option<String> {
    let mods = modifiers_text(node, source);
    let keyword = if node.kind() == "init_declaration" {
        "init"
    } else {
        "func"
    };
    let name = if node.kind() == "init_declaration" {
        String::new()
    } else {
        node.child_by_field_name("name")
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_else(|| "_".to_string())
    };

    let params = params_text(node, source);
    let ret = node
        .child_by_field_name("return_type")
        .map(|n| format!(" -> {}", node_text(n, source)))
        .unwrap_or_default();

    let throws = {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|c| c.kind() == "throws")
            .map(|n| format!(" {}", node_text(n, source)))
            .unwrap_or_default()
    };

    let base = if name.is_empty() {
        format!("{keyword}{params}{throws}{ret}")
    } else {
        format!("{keyword} {name}{params}{throws}{ret}")
    };
    let sig = prefixed(&mods, format_args!("{base}"));
    Some(compact_ws(&sig).into_owned())
}

fn property_text_str(node: Node, source: &[u8]) -> String {
    let mods = modifiers_text(node, source);
    let Some(pat) = node.child_by_field_name("name") else {
        return String::new();
    };
    let name = pat
        .child_by_field_name("bound_identifier")
        .map(|n| node_text(n, source))
        .unwrap_or_else(|| node_text(pat, source));

    let mut cursor = node.walk();
    let type_ann = node.children(&mut cursor).find_map(|child| {
        if child.kind() == "type_annotation" {
            Some(node_text(child, source).to_string())
        } else {
            None
        }
    });

    let keyword = {
        let mut c = node.walk();
        node.children(&mut c)
            .find(|ch| !ch.is_named())
            .map(|ch| node_text(ch, source))
            .unwrap_or("var")
    };

    let ty_str = type_ann.unwrap_or_default();
    let base = format!("{keyword} {name}{ty_str}");
    prefixed(&mods, format_args!("{base}"))
}

impl SwiftExtractor {
    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = node_text(node, source);
        let module = text
            .strip_prefix("import ")
            .unwrap_or(text)
            .split_whitespace()
            .last()
            .unwrap_or(text)
            .trim();
        let paths = vec![
            module
                .split(self.import_separator())
                .map(String::from)
                .collect(),
        ];
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn inheritance_text(&self, node: Node, source: &[u8]) -> String {
        let mut parts = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "inheritance_specifier" {
                parts.push(node_text(child, source).to_string());
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(": {}", parts.join(", "))
        }
    }

    fn extract_class_like(
        &self,
        node: Node,
        source: &[u8],
        section: Section,
        keyword: &str,
    ) -> Option<SkeletonEntry> {
        let mods = modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let inheritance = self.inheritance_text(node, source);
        let label = prefixed(&mods, format_args!("{keyword} {name}{inheritance}"));
        let children = self.extract_body_members_swift(node, source);
        Some(SkeletonEntry::new(section, node, label).with_children(children))
    }

    fn extract_enum_body(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let inheritance = self.inheritance_text(node, source);
        let label = prefixed(&mods, format_args!("enum {name}{inheritance}"));

        let body = node.child_by_field_name("body")?;
        let mut cases = Vec::new();
        let mut methods = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "enum_entry" => {
                    let mut ec = child.walk();
                    for name_node in child.children(&mut ec) {
                        if name_node.kind() == "simple_identifier" {
                            let lr = line_range(
                                child.start_position().row + 1,
                                child.end_position().row + 1,
                            );
                            cases.push(format!("case {} {lr}", node_text(name_node, source)));
                        }
                    }
                }
                "function_declaration" => {
                    if let Some(sig) = swift_fn_signature(child, source) {
                        let lr = line_range(
                            child.start_position().row + 1,
                            child.end_position().row + 1,
                        );
                        methods.push(format!("{sig} {lr}"));
                    }
                }
                _ => {}
            }
        }

        let mut children = cases;
        children.extend(methods);
        Some(SkeletonEntry::new(Section::Type, node, label).with_children(children))
    }

    fn extract_body_members_swift(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        let rules = [
            BodyMemberRule {
                kind: "function_declaration",
                handler: BodyMemberHandler::Method(&swift_fn_signature),
            },
            BodyMemberRule {
                kind: "init_declaration",
                handler: BodyMemberHandler::Method(&swift_fn_signature),
            },
            BodyMemberRule {
                kind: "property_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &property_text_str,
                    counter: "property_declaration",
                },
            },
        ];
        extract_body_members(body, source, &rules)
    }

    fn extract_property(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mut cursor = node.walk();
        let is_let = node
            .children(&mut cursor)
            .any(|c| !c.is_named() && node_text(c, source) == "let");
        if !is_let {
            return None;
        }
        let text = property_text_str(node, source);
        if text.is_empty() {
            return None;
        }
        Some(SkeletonEntry::new(Section::Constant, node, text))
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let sig = swift_fn_signature(node, source)?;
        Some(SkeletonEntry::new(Section::Function, node, sig))
    }

    fn extract_typealias(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let value = node
            .child_by_field_name("value")
            .map(|n| truncate(node_text(n, source), 60))
            .unwrap_or_default();
        let base = if value.is_empty() {
            format!("typealias {name}")
        } else {
            format!("typealias {name} = {value}")
        };
        let label = prefixed(&mods, format_args!("{base}"));
        Some(SkeletonEntry::new(Section::Type, node, label))
    }

    fn extract_protocol(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let inheritance = self.inheritance_text(node, source);
        let label = prefixed(&mods, format_args!("protocol {name}{inheritance}"));

        let mut members = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                match child.kind() {
                    "protocol_function_declaration" => {
                        if let Some(sig) = swift_fn_signature(child, source) {
                            let lr = line_range(
                                child.start_position().row + 1,
                                child.end_position().row + 1,
                            );
                            members.push(format!("{sig} {lr}"));
                        }
                    }
                    "protocol_property_declaration" => {
                        let prop_name = child
                            .child_by_field_name("name")
                            .map(|n| node_text(n, source))
                            .unwrap_or("_");
                        let mut c = child.walk();
                        let ty = child
                            .children(&mut c)
                            .find_map(|n| {
                                if n.kind() == "type_annotation" {
                                    Some(node_text(n, source).to_string())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        let lr = line_range(
                            child.start_position().row + 1,
                            child.end_position().row + 1,
                        );
                        members.push(format!("var {prop_name}{ty} {lr}"));
                    }
                    _ => {}
                }
            }
        }

        Some(SkeletonEntry::new(Section::Trait, node, label).with_children(members))
    }
}

impl LanguageExtractor for SwiftExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "import_declaration" => self.extract_import(node, source).into_iter().collect(),
            "class_declaration" => {
                let kind = node
                    .child_by_field_name("declaration_kind")
                    .map(|n| node_text(n, source))
                    .unwrap_or("class");
                match kind {
                    "class" | "actor" => self
                        .extract_class_like(node, source, Section::Class, kind)
                        .into_iter()
                        .collect(),
                    "struct" => self
                        .extract_class_like(node, source, Section::Type, "struct")
                        .into_iter()
                        .collect(),
                    "enum" => self.extract_enum_body(node, source).into_iter().collect(),
                    "extension" => self
                        .extract_class_like(node, source, Section::Impl, "extension")
                        .into_iter()
                        .collect(),
                    _ => Vec::new(),
                }
            }
            "protocol_declaration" => self.extract_protocol(node, source).into_iter().collect(),
            "function_declaration" => self.extract_function(node, source).into_iter().collect(),
            "property_declaration" => self.extract_property(node, source).into_iter().collect(),
            "typealias_declaration" => self.extract_typealias(node, source).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        matches!(node.kind(), "comment" | "multiline_comment") && {
            let t = node_text(node, source);
            t.starts_with("///") || t.starts_with("/**")
        }
    }

    fn import_separator(&self) -> &'static str {
        "."
    }
}
