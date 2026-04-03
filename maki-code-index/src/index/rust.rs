use tree_sitter::Node;

use super::common::{
    ChildKind, FIELD_TRUNCATE_THRESHOLD, LanguageExtractor, Section, SkeletonEntry, expand_import,
    find_child, fn_signature, has_test_attr, line_range, node_text, prefixed, relevant_attr_texts,
    truncated_msg, vis_prefix,
};

pub(crate) struct RustExtractor;

impl RustExtractor {
    fn extract_use(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let tree = find_child(node, "use_declaration").unwrap_or(node);
        let argument = find_child(tree, "scoped_identifier")
            .or_else(|| find_child(tree, "use_wildcard"))
            .or_else(|| find_child(tree, "use_list"))
            .or_else(|| find_child(tree, "scoped_use_list"))
            .or_else(|| find_child(tree, "identifier"));

        let text = if let Some(arg) = argument {
            node_text(arg, source).to_string()
        } else {
            let full = node_text(node, source);
            full.strip_prefix("use ")
                .unwrap_or(full)
                .trim_end_matches(';')
                .to_string()
        };

        let paths = expand_import(&text, self.import_separator());
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn extract_struct_or_enum(
        &self,
        node: Node,
        source: &[u8],
        attrs: &[Node],
    ) -> Option<SkeletonEntry> {
        let vis = vis_prefix(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let generics = find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");

        let kind = node.kind().replace("_item", "").replace("_definition", "");
        let text = prefixed(vis, format_args!("{kind} {name}{generics}"));
        let is_enum = find_child(node, "enum_variant_list").is_some();
        let children = self.extract_fields(node, source);
        let attr_texts = relevant_attr_texts(attrs, source);

        let mut entry = SkeletonEntry::new(Section::Type, node, text)
            .with_children(children)
            .with_attrs(attr_texts);
        if is_enum {
            entry = entry.with_child_kind(ChildKind::Brief);
        }
        Some(entry)
    }

    fn extract_fields(&self, node: Node, source: &[u8]) -> Vec<String> {
        let body = find_child(node, "field_declaration_list")
            .or_else(|| find_child(node, "enum_variant_list"));
        let Some(body) = body else { return Vec::new() };

        let mut fields = Vec::new();
        let mut cursor = body.walk();
        let mut total = 0;

        for child in body.children(&mut cursor) {
            match child.kind() {
                "field_declaration" => {
                    total += 1;
                    let vis = vis_prefix(child, source);
                    let name = child
                        .child_by_field_name("name")
                        .map(|n| node_text(n, source))
                        .unwrap_or("_");
                    let ty = child
                        .child_by_field_name("type")
                        .map(|n| node_text(n, source))
                        .unwrap_or("_");
                    if total <= FIELD_TRUNCATE_THRESHOLD || !vis.is_empty() {
                        fields.push(prefixed(vis, format_args!("{name}: {ty}")));
                    }
                }
                "enum_variant" => {
                    total += 1;
                    if total <= FIELD_TRUNCATE_THRESHOLD {
                        let name = child
                            .child_by_field_name("name")
                            .map(|n| node_text(n, source))
                            .unwrap_or("_");
                        fields.push(name.to_string());
                    }
                }
                _ => {}
            }
        }

        if total > FIELD_TRUNCATE_THRESHOLD && fields.len() < total {
            fields.push(truncated_msg(total));
        }

        fields
    }

    fn extract_fn(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let vis = vis_prefix(node, source);
        let sig = fn_signature(node, source)?;
        let text = prefixed(vis, format_args!("{sig}"));
        Some(SkeletonEntry::new(Section::Function, node, text))
    }

    fn extract_trait(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let vis = vis_prefix(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let generics = find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("");

        let text = prefixed(vis, format_args!("{name}{generics}"));
        let children = self.extract_methods(node, source, false);
        Some(SkeletonEntry::new(Section::Trait, node, text).with_children(children))
    }

    fn extract_methods(&self, node: Node, source: &[u8], include_vis: bool) -> Vec<String> {
        let body = find_child(node, "declaration_list");
        let Some(body) = body else { return Vec::new() };

        let mut methods = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            let is_method =
                child.kind() == "function_item" || child.kind() == "function_signature_item";
            if !is_method {
                continue;
            }
            let Some(sig) = fn_signature(child, source) else {
                continue;
            };
            let lr = line_range(child.start_position().row + 1, child.end_position().row + 1);
            if include_vis {
                let vis = vis_prefix(child, source);
                methods.push(format!("{} {lr}", prefixed(vis, format_args!("{sig}"))));
            } else {
                methods.push(format!("{sig} {lr}"));
            }
        }
        methods
    }

    fn extract_impl(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let type_node = node
            .child_by_field_name("type")
            .or_else(|| find_child(node, "type_identifier"))?;
        let type_name = node_text(type_node, source);

        let text = if let Some(tn) = node.child_by_field_name("trait") {
            format!("{} for {type_name}", node_text(tn, source))
        } else {
            type_name.to_string()
        };

        let children = self.extract_methods(node, source, true);
        Some(SkeletonEntry::new(Section::Impl, node, text).with_children(children))
    }

    fn extract_const_or_static(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let ty = node
            .child_by_field_name("type")
            .map(|n| node_text(n, source));
        let vis = vis_prefix(node, source);
        let type_str = ty.map(|t| format!(": {t}")).unwrap_or_default();
        let prefix = if node.kind() == "static_item" {
            "static "
        } else {
            ""
        };
        let text = prefixed(vis, format_args!("{prefix}{name}{type_str}"));
        Some(SkeletonEntry::new(Section::Constant, node, text))
    }

    fn extract_mod(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let vis = vis_prefix(node, source);
        let text = prefixed(vis, format_args!("{name}"));
        Some(SkeletonEntry::new(Section::Module, node, text))
    }

    fn extract_macro(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        Some(SkeletonEntry::new(Section::Macro, node, format!("{name}!")))
    }

    fn extract_type_alias(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let vis = vis_prefix(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let value = node
            .child_by_field_name("type")
            .map(|n| node_text(n, source));
        let val = value.map(|v| format!(" = {v}")).unwrap_or_default();
        let text = prefixed(vis, format_args!("type {name}{val}"));
        Some(SkeletonEntry::new(Section::Type, node, text))
    }
}

impl LanguageExtractor for RustExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], attrs: &[Node]) -> Vec<SkeletonEntry> {
        let entry = match node.kind() {
            "use_declaration" => self.extract_use(node, source),
            "struct_item" | "enum_item" | "union_item" => {
                self.extract_struct_or_enum(node, source, attrs)
            }
            "function_item" => self.extract_fn(node, source),
            "trait_item" => self.extract_trait(node, source),
            "impl_item" => self.extract_impl(node, source),
            "const_item" | "static_item" => self.extract_const_or_static(node, source),
            "mod_item" => self.extract_mod(node, source),
            "macro_definition" => self.extract_macro(node, source),
            "type_item" => self.extract_type_alias(node, source),
            _ => None,
        };
        entry.into_iter().collect()
    }

    fn is_attr(&self, node: Node) -> bool {
        node.kind() == "attribute_item"
    }

    fn is_test_node(&self, node: Node, source: &[u8], attrs: &[Node]) -> bool {
        matches!(node.kind(), "mod_item" | "function_item") && has_test_attr(attrs, source)
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        if node.kind() != "line_comment" {
            return false;
        }
        let text = node_text(node, source);
        text.starts_with("///") && !text.starts_with("////")
    }

    fn is_module_doc(&self, node: Node, source: &[u8]) -> bool {
        if node.kind() != "line_comment" {
            return false;
        }
        let text = node_text(node, source);
        text.starts_with("//!")
    }
}
