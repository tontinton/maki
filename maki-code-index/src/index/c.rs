use tree_sitter::Node;

use super::common::{
    ChildKind, LanguageExtractor, Section, SkeletonEntry, compact_ws, extract_enum_variants,
    extract_fields_truncated, line_range, node_text, truncate,
};

pub(crate) struct CExtractor;

impl CExtractor {
    fn extract_include(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let path_node = node.child_by_field_name("path")?;
        let raw = node_text(path_node, source);
        let cleaned = raw.trim_matches('"').trim_matches('<').trim_matches('>');
        let segments = cleaned
            .split(self.import_separator())
            .map(String::from)
            .collect();
        Some(SkeletonEntry::new_import(node, vec![segments]))
    }

    fn build_fn_sig(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let decl = node.child_by_field_name("declarator")?;
        let fn_decl = Self::unwrap_to_fn_declarator(decl)?;
        let name = fn_decl
            .child_by_field_name("declarator")
            .map(|n| node_text(n, source))?;
        let params = fn_decl
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let ret = node
            .child_by_field_name("type")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let sig = if ret.is_empty() {
            format!("{name}{params}")
        } else {
            format!("{ret} {name}{params}")
        };
        Some(SkeletonEntry::new(
            Section::Function,
            node,
            compact_ws(&sig).into_owned(),
        ))
    }

    fn unwrap_to_fn_declarator(node: Node) -> Option<Node> {
        match node.kind() {
            "function_declarator" => Some(node),
            "pointer_declarator" => node
                .child_by_field_name("declarator")
                .and_then(Self::unwrap_to_fn_declarator),
            _ => None,
        }
    }

    fn extract_struct(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let body = node.child_by_field_name("body")?;
        let keyword = node.kind().strip_suffix("_specifier").unwrap_or("struct");
        let label = if name.is_empty() {
            keyword.to_string()
        } else {
            format!("{keyword} {name}")
        };
        let children = extract_fields_truncated(body, source, "field_declaration", |child, src| {
            let text = compact_ws(node_text(child, src).trim_end_matches(';').trim());
            let lr = line_range(child.start_position().row + 1, child.end_position().row + 1);
            format!("{text} {lr}")
        });
        Some(SkeletonEntry::new(Section::Type, node, label).with_children(children))
    }

    fn extract_enum(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .unwrap_or("");
        let body = node.child_by_field_name("body")?;
        let label = if name.is_empty() {
            "enum".to_string()
        } else {
            format!("enum {name}")
        };
        let values = extract_enum_variants(body, source, "enumerator");
        Some(
            SkeletonEntry::new(Section::Type, node, label)
                .with_children(values)
                .with_child_kind(ChildKind::Brief),
        )
    }

    fn extract_typedef(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let type_node = node.child_by_field_name("type")?;
        let declarator = node.child_by_field_name("declarator")?;
        let alias = node_text(declarator, source);

        match type_node.kind() {
            "struct_specifier" | "union_specifier" => {
                if let Some(body) = type_node.child_by_field_name("body") {
                    let inner_name = type_node
                        .child_by_field_name("name")
                        .map(|n| node_text(n, source))
                        .unwrap_or("");
                    let keyword = type_node
                        .kind()
                        .strip_suffix("_specifier")
                        .unwrap_or("struct");
                    let inner = if inner_name.is_empty() {
                        keyword.to_string()
                    } else {
                        format!("{keyword} {inner_name}")
                    };
                    let label = format!("typedef {inner} {alias}");
                    let children = extract_fields_truncated(
                        body,
                        source,
                        "field_declaration",
                        |child, src| {
                            let text =
                                compact_ws(node_text(child, src).trim_end_matches(';').trim());
                            let lr = line_range(
                                child.start_position().row + 1,
                                child.end_position().row + 1,
                            );
                            format!("{text} {lr}")
                        },
                    );
                    return Some(
                        SkeletonEntry::new(Section::Type, node, label).with_children(children),
                    );
                }
            }
            "enum_specifier" => {
                if let Some(body) = type_node.child_by_field_name("body") {
                    let inner_name = type_node
                        .child_by_field_name("name")
                        .map(|n| node_text(n, source))
                        .unwrap_or("");
                    let inner = if inner_name.is_empty() {
                        "enum".to_string()
                    } else {
                        format!("enum {inner_name}")
                    };
                    let label = format!("typedef {inner} {alias}");
                    let values = extract_enum_variants(body, source, "enumerator");
                    return Some(
                        SkeletonEntry::new(Section::Type, node, label)
                            .with_children(values)
                            .with_child_kind(ChildKind::Brief),
                    );
                }
            }
            _ => {}
        }

        let ty_text = node_text(type_node, source);
        Some(SkeletonEntry::new(
            Section::Type,
            node,
            format!("typedef {} {alias}", truncate(ty_text, 60)),
        ))
    }

    fn extract_define(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let value = node
            .child_by_field_name("value")
            .map(|n| format!(" {}", truncate(node_text(n, source), 40)))
            .unwrap_or_default();
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            format!("{name}{value}"),
        ))
    }

    fn extract_declaration(&self, node: Node, source: &[u8]) -> Vec<SkeletonEntry> {
        let Some(decl) = node.child_by_field_name("declarator") else {
            return Vec::new();
        };
        if Self::unwrap_to_fn_declarator(decl).is_some()
            && let Some(entry) = self.build_fn_sig(node, source)
        {
            return vec![entry];
        }
        Vec::new()
    }
}

impl LanguageExtractor for CExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "preproc_include" => self.extract_include(node, source).into_iter().collect(),
            "preproc_def" => self.extract_define(node, source).into_iter().collect(),
            "preproc_function_def" => self.extract_define(node, source).into_iter().collect(),
            "function_definition" => self.build_fn_sig(node, source).into_iter().collect(),
            "struct_specifier" | "union_specifier" => {
                self.extract_struct(node, source).into_iter().collect()
            }
            "enum_specifier" => self.extract_enum(node, source).into_iter().collect(),
            "type_definition" => self.extract_typedef(node, source).into_iter().collect(),
            "declaration" => self.extract_declaration(node, source),
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
        "/"
    }
}
