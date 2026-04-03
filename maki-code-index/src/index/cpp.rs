use tree_sitter::Node;

use super::common::{
    BodyMemberHandler, BodyMemberRule, ChildKind, LanguageExtractor, Section, SkeletonEntry,
    compact_ws, extract_body_members, extract_enum_variants, node_text, truncate,
};

pub(crate) struct CppExtractor;

impl CppExtractor {
    fn extract_include(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let path = node
            .child_by_field_name("path")
            .map(|n| node_text(n, source))
            .unwrap_or_else(|| {
                node_text(node, source)
                    .trim_start_matches("#include")
                    .trim()
            });
        let cleaned = path.trim_matches(|c| c == '"' || c == '<' || c == '>');
        Some(SkeletonEntry::new_import(
            node,
            vec![cleaned.split('/').map(String::from).collect()],
        ))
    }

    fn extract_using(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = node_text(node, source);
        let cleaned = text
            .strip_prefix("using namespace ")
            .or_else(|| text.strip_prefix("using "))
            .unwrap_or(text)
            .trim_end_matches(';')
            .trim();
        let segments: Vec<String> = cleaned.split("::").map(String::from).collect();
        Some(SkeletonEntry::new_import(node, vec![segments]))
    }

    fn extract_namespace(&self, node: Node, source: &[u8]) -> Vec<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or("(anonymous)");
        let mut entries = vec![SkeletonEntry::new(Section::Module, node, name.to_string())];
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                entries.extend(self.extract_nodes(child, source, &[]));
            }
        }
        entries
    }

    fn extract_class_or_struct(
        &self,
        node: Node,
        source: &[u8],
        is_class: bool,
    ) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let bases = node
            .child_by_field_name("base_class_clause")
            .map(|n| format!(" : {}", node_text(n, source).trim_start_matches(':')))
            .map(|s| compact_ws(&s).into_owned())
            .unwrap_or_default();
        let keyword = if is_class { "class" } else { "struct" };
        let label = format!("{keyword} {name}{bases}");

        let children = self.extract_class_body(node, source);
        let section = if is_class {
            Section::Class
        } else {
            Section::Type
        };
        Some(SkeletonEntry::new(section, node, label).with_children(children))
    }

    fn extract_class_body(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        let rules = [
            BodyMemberRule {
                kind: "function_definition",
                handler: BodyMemberHandler::Method(&|n, s| self.method_sig(n, s)),
            },
            BodyMemberRule {
                kind: "declaration",
                handler: BodyMemberHandler::Method(&|n, s| self.decl_sig(n, s)),
            },
            BodyMemberRule {
                kind: "field_declaration",
                handler: BodyMemberHandler::FieldTruncated {
                    format_fn: &|n, s| {
                        compact_ws(node_text(n, s).trim_end_matches(';')).into_owned()
                    },
                    counter: "field_declaration",
                },
            },
        ];
        extract_body_members(body, source, &rules)
    }

    fn method_sig(&self, node: Node, source: &[u8]) -> Option<String> {
        let ret = node
            .child_by_field_name("type")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let decl = node.child_by_field_name("declarator")?;
        let sig = self.declarator_sig(decl, source)?;
        let text = if ret.is_empty() {
            sig
        } else {
            format!("{ret} {sig}")
        };
        Some(compact_ws(&text).into_owned())
    }

    fn declarator_sig(&self, node: Node, source: &[u8]) -> Option<String> {
        match node.kind() {
            "function_declarator" => {
                let inner = node.child_by_field_name("declarator")?;
                let name = self.declarator_name(inner, source);
                let params = node
                    .child_by_field_name("parameters")
                    .map(|n| node_text(n, source))
                    .unwrap_or("()");
                Some(format!("{name}{params}"))
            }
            "reference_declarator" | "pointer_declarator" => {
                let inner = node
                    .child_by_field_name("declarator")
                    .or_else(|| node.named_child(0))?;
                self.declarator_sig(inner, source)
            }
            _ => Some(self.declarator_name(node, source).to_string()),
        }
    }

    fn declarator_name<'a>(&self, node: Node<'a>, source: &'a [u8]) -> &'a str {
        match node.kind() {
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "destructor_name"
            | "operator_name"
            | "qualified_identifier" => node_text(node, source),
            _ => node
                .child_by_field_name("name")
                .or_else(|| node.named_child(0))
                .map(|n| self.declarator_name(n, source))
                .unwrap_or("_"),
        }
    }

    fn decl_sig(&self, node: Node, source: &[u8]) -> Option<String> {
        let decl = node.child_by_field_name("declarator")?;
        if decl.kind() != "function_declarator" {
            return None;
        }
        self.method_sig(node, source)
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or("(anonymous)");
        let body = node.child_by_field_name("body")?;
        let enumerators = extract_enum_variants(body, source, "enumerator");
        Some(
            SkeletonEntry::new(Section::Type, node, format!("enum {name}"))
                .with_children(enumerators)
                .with_child_kind(ChildKind::Brief),
        )
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let sig = self.method_sig(node, source)?;
        Some(SkeletonEntry::new(Section::Function, node, sig))
    }

    fn extract_template(&self, node: Node, source: &[u8]) -> Vec<SkeletonEntry> {
        let params = node
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("<>");
        let prefix = format!("template{params}");

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            return match child.kind() {
                "function_definition" => {
                    if let Some(sig) = self.method_sig(child, source) {
                        let text = compact_ws(&format!("{prefix} {sig}")).into_owned();
                        vec![SkeletonEntry::new(Section::Function, node, text)]
                    } else {
                        Vec::new()
                    }
                }
                "class_specifier" => {
                    self.extract_template_class_or_struct(child, source, true, &prefix, node)
                }
                "struct_specifier" => {
                    self.extract_template_class_or_struct(child, source, false, &prefix, node)
                }
                "declaration" => {
                    if let Some(sig) = self.decl_sig(child, source) {
                        let text = compact_ws(&format!("{prefix} {sig}")).into_owned();
                        vec![SkeletonEntry::new(Section::Function, node, text)]
                    } else {
                        Vec::new()
                    }
                }
                _ => continue,
            };
        }
        Vec::new()
    }

    fn extract_template_class_or_struct(
        &self,
        node: Node,
        source: &[u8],
        is_class: bool,
        prefix: &str,
        template_node: Node,
    ) -> Vec<SkeletonEntry> {
        let name = match node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
        {
            Some(n) => n,
            None => return Vec::new(),
        };
        let bases = node
            .child_by_field_name("base_class_clause")
            .map(|n| format!(" : {}", node_text(n, source).trim_start_matches(':')))
            .map(|s| compact_ws(&s).into_owned())
            .unwrap_or_default();
        let keyword = if is_class { "class" } else { "struct" };
        let label = compact_ws(&format!("{prefix} {keyword} {name}{bases}")).into_owned();
        let children = self.extract_class_body(node, source);
        let section = if is_class {
            Section::Class
        } else {
            Section::Type
        };
        vec![SkeletonEntry::new(section, template_node, label).with_children(children)]
    }

    fn extract_preproc_def(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let value = node
            .child_by_field_name("value")
            .map(|n| truncate(node_text(n, source), 40))
            .unwrap_or_default();
        let text = if value.is_empty() {
            name.to_string()
        } else {
            format!("{name} {value}")
        };
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            compact_ws(&text).into_owned(),
        ))
    }

    fn extract_type_definition(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let ty = node
            .child_by_field_name("type")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        let decl = node
            .child_by_field_name("declarator")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        Some(SkeletonEntry::new(
            Section::Type,
            node,
            compact_ws(&format!("typedef {ty} {decl}")).into_owned(),
        ))
    }
}

impl LanguageExtractor for CppExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "preproc_include" => self.extract_include(node, source).into_iter().collect(),
            "using_declaration" => self.extract_using(node, source).into_iter().collect(),
            "namespace_definition" => self.extract_namespace(node, source),
            "class_specifier" => self
                .extract_class_or_struct(node, source, true)
                .into_iter()
                .collect(),
            "struct_specifier" => self
                .extract_class_or_struct(node, source, false)
                .into_iter()
                .collect(),
            "enum_specifier" => self.extract_enum(node, source).into_iter().collect(),
            "function_definition" => self.extract_function(node, source).into_iter().collect(),
            "template_declaration" => self.extract_template(node, source),
            "preproc_def" => self.extract_preproc_def(node, source).into_iter().collect(),
            "preproc_function_def" => self.extract_preproc_def(node, source).into_iter().collect(),
            "declaration" => {
                if let Some(d) = node.child_by_field_name("declarator")
                    && d.kind() == "function_declarator"
                    && let Some(sig) = self.decl_sig(node, source)
                {
                    return vec![SkeletonEntry::new(Section::Function, node, sig)];
                }
                Vec::new()
            }
            "type_definition" => self
                .extract_type_definition(node, source)
                .into_iter()
                .collect(),
            "alias_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source))
                    .unwrap_or("_");
                let ty = node
                    .child_by_field_name("type")
                    .map(|n| node_text(n, source))
                    .unwrap_or("_");
                vec![SkeletonEntry::new(
                    Section::Type,
                    node,
                    compact_ws(&format!("using {name} = {ty}")).into_owned(),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "comment" && {
            let t = node_text(node, source);
            t.starts_with("/**") || t.starts_with("///")
        }
    }

    fn import_separator(&self) -> &'static str {
        "::"
    }
}
