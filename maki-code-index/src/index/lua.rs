use tree_sitter::Node;

use super::common::{
    LanguageExtractor, Section, SkeletonEntry, compact_ws, find_child, node_text, truncate,
};

pub(crate) struct LuaExtractor;

impl LuaExtractor {
    fn require_module<'a>(&self, call: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
        let name = call.child_by_field_name("name")?;
        if node_text(name, source) != "require" {
            return None;
        }
        let args = call.child_by_field_name("arguments")?;
        let mut cursor = args.walk();
        args.children(&mut cursor)
            .find(|n| n.kind() == "string")
            .map(|n| node_text(n, source).trim_matches(|c| c == '"' || c == '\''))
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
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

    fn extract_var_decl(&self, node: Node, source: &[u8]) -> Vec<SkeletonEntry> {
        let Some(assign) = find_child(node, "assignment_statement") else {
            return Vec::new();
        };
        let Some(var_list) = find_child(assign, "variable_list") else {
            return Vec::new();
        };
        let Some(expr_list) = find_child(assign, "expression_list") else {
            return Vec::new();
        };

        let names: Vec<&str> = {
            let mut cursor = var_list.walk();
            var_list
                .children(&mut cursor)
                .filter(|n| n.kind() == "identifier")
                .map(|n| node_text(n, source))
                .collect()
        };

        let exprs: Vec<Node> = {
            let mut cursor = expr_list.walk();
            expr_list
                .children(&mut cursor)
                .filter(|n| n.is_named())
                .collect()
        };

        // Check if any expression is a require() call → import entries
        let has_require = exprs
            .iter()
            .any(|e| e.kind() == "function_call" && self.require_module(*e, source).is_some());
        if has_require {
            return exprs
                .iter()
                .filter_map(|expr| {
                    if expr.kind() != "function_call" {
                        return None;
                    }
                    let module = self.require_module(*expr, source)?;
                    let segs = module
                        .split(self.import_separator())
                        .map(String::from)
                        .collect();
                    Some(SkeletonEntry::new_import(*expr, vec![segs]))
                })
                .collect();
        }

        // UPPER_CASE single-name declarations → constant
        if names.len() == 1 {
            let name = names[0];
            if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !name.is_empty() {
                let val = exprs.first().map(|e| truncate(node_text(*e, source), 60));
                let text = match val {
                    Some(v) => format!("{name} = {v}"),
                    None => name.to_string(),
                };
                return vec![SkeletonEntry::new(Section::Constant, node, text)];
            }
        }

        Vec::new()
    }
}

impl LanguageExtractor for LuaExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "function_declaration" => self.extract_function(node, source).into_iter().collect(),
            "variable_declaration" => self.extract_var_decl(node, source),
            "function_call" => {
                let Some(module) = self.require_module(node, source) else {
                    return Vec::new();
                };
                let segs = module
                    .split(self.import_separator())
                    .map(String::from)
                    .collect();
                vec![SkeletonEntry::new_import(node, vec![segs])]
            }
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "comment" && node_text(node, source).starts_with("---")
    }

    fn import_separator(&self) -> &'static str {
        "."
    }
}
