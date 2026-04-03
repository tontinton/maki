use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, ChildKind, LanguageExtractor, Section, SkeletonEntry,
    compact_ws, extract_body_members, find_child, node_text, prefixed, truncate,
};

pub(crate) struct ScalaExtractor;

impl ScalaExtractor {
    fn modifiers_text(&self, node: Node, source: &[u8]) -> String {
        let Some(mods) = find_child(node, "modifiers") else {
            return String::new();
        };
        let mut cursor = mods.walk();
        mods.children(&mut cursor)
            .filter_map(|c| match c.kind() {
                "access_modifier" | "case" | "abstract" | "sealed" | "final" | "implicit"
                | "lazy" | "override" | "open" => Some(node_text(c, source)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn type_params_text<'a>(&self, node: Node<'a>, source: &'a [u8]) -> &'a str {
        node.child_by_field_name("type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("")
    }

    fn extends_text(&self, node: Node, source: &[u8]) -> String {
        node.child_by_field_name("extend")
            .map(|n| format!(" {}", node_text(n, source)))
            .unwrap_or_default()
    }

    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = node_text(node, source);
        let cleaned = text.strip_prefix("import ").unwrap_or(text).trim();
        let paths = super::common::expand_import(cleaned, self.import_separator());
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn extract_package(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_else(|| {
                let text = node_text(node, source);
                text.strip_prefix("package ")
                    .unwrap_or(text)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
        Some(SkeletonEntry::new(Section::Module, node, name))
    }

    fn extract_class_like(
        &self,
        node: Node,
        source: &[u8],
        keyword: &str,
        section: Section,
    ) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = self.type_params_text(node, source);
        let extends = self.extends_text(node, source);
        let label = compact_ws(&prefixed(
            &mods,
            format_args!("{keyword} {name}{type_params}{extends}"),
        ))
        .into_owned();
        let children = self.extract_template_body(node, source);
        Some(SkeletonEntry::new(section, node, label).with_children(children))
    }

    fn extract_template_body(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body").or_else(|| {
            find_child(node, "template_body").or_else(|| find_child(node, "_braced_template_body"))
        }) else {
            return Vec::new();
        };
        let rules = [
            BodyMemberRule {
                kind: "function_definition",
                handler: BodyMemberHandler::Method(&|n, s| Some(self.fn_sig(n, s))),
            },
            BodyMemberRule {
                kind: "function_declaration",
                handler: BodyMemberHandler::Method(&|n, s| Some(self.fn_sig(n, s))),
            },
            BodyMemberRule {
                kind: "val_definition",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &|n, s| self.val_text(n, s),
                    counter: "val",
                },
            },
            BodyMemberRule {
                kind: "var_definition",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &|n, s| self.val_text(n, s),
                    counter: "val",
                },
            },
            BodyMemberRule {
                kind: "val_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &|n, s| self.val_text(n, s),
                    counter: "val",
                },
            },
            BodyMemberRule {
                kind: "var_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &|n, s| self.val_text(n, s),
                    counter: "val",
                },
            },
        ];
        extract_body_members(body, source, &rules)
    }

    fn fn_sig(&self, node: Node, source: &[u8]) -> String {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        let type_params = self.type_params_text(node, source);
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let ret = node
            .child_by_field_name("return_type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        compact_ws(&prefixed(
            &mods,
            format_args!("def {name}{type_params}{params}{ret}"),
        ))
        .into_owned()
    }

    fn val_text(&self, node: Node, source: &[u8]) -> String {
        let mods = self.modifiers_text(node, source);
        let keyword = if node.kind().starts_with("var") {
            "var"
        } else {
            "val"
        };
        let pattern = node
            .child_by_field_name("pattern")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        let ty = node
            .child_by_field_name("type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        truncate(
            &compact_ws(&prefixed(&mods, format_args!("{keyword} {pattern}{ty}"))),
            80,
        )
        .into_owned()
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let sig = self.fn_sig(node, source);
        Some(SkeletonEntry::new(Section::Function, node, sig))
    }

    fn extract_val(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = self.val_text(node, source);
        Some(SkeletonEntry::new(Section::Constant, node, text))
    }

    fn extract_type_def(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = self.type_params_text(node, source);
        let rhs = node
            .child_by_field_name("type")
            .map(|n| format!(" = {}", node_text(n, source)))
            .unwrap_or_default();
        let label = compact_ws(&prefixed(
            &mods,
            format_args!("type {name}{type_params}{rhs}"),
        ))
        .into_owned();
        Some(SkeletonEntry::new(Section::Type, node, label))
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let type_params = self.type_params_text(node, source);
        let extends = self.extends_text(node, source);
        let label = compact_ws(&prefixed(
            &mods,
            format_args!("enum {name}{type_params}{extends}"),
        ))
        .into_owned();
        let mut cases = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                if child.kind() == "enum_case_definitions" {
                    cases.push(node_text(child, source).to_string());
                }
            }
        }
        Some(
            SkeletonEntry::new(Section::Type, node, label)
                .with_children(cases)
                .with_child_kind(ChildKind::Brief),
        )
    }
}

impl LanguageExtractor for ScalaExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "import_declaration" => self.extract_import(node, source).into_iter().collect(),
            "package_clause" => self.extract_package(node, source).into_iter().collect(),
            "class_definition" => self
                .extract_class_like(node, source, "class", Section::Class)
                .into_iter()
                .collect(),
            "object_definition" => self
                .extract_class_like(node, source, "object", Section::Class)
                .into_iter()
                .collect(),
            "trait_definition" => self
                .extract_class_like(node, source, "trait", Section::Trait)
                .into_iter()
                .collect(),
            "function_definition" | "function_declaration" => {
                self.extract_function(node, source).into_iter().collect()
            }
            "val_definition" | "var_definition" | "val_declaration" | "var_declaration" => {
                self.extract_val(node, source).into_iter().collect()
            }
            "type_definition" => self.extract_type_def(node, source).into_iter().collect(),
            "enum_definition" => self.extract_enum(node, source).into_iter().collect(),
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
