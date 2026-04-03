use tree_sitter::Node;

use super::common::{
    LanguageExtractor, Section, SkeletonEntry, compact_ws, find_child, line_range, node_text,
    truncate,
};

pub(crate) struct PythonExtractor;

impl PythonExtractor {
    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let text = node_text(node, source);
        let cleaned = text
            .strip_prefix("import ")
            .or_else(|| text.strip_prefix("from "))
            .unwrap_or(text)
            .trim();
        let paths = if let Some((base, names)) = cleaned.split_once(" import ") {
            let base_parts: Vec<&str> = base.split('.').collect();
            names
                .split(',')
                .map(|name| {
                    let mut path: Vec<String> = base_parts.iter().map(|s| s.to_string()).collect();
                    path.push(name.trim().to_string());
                    path
                })
                .collect()
        } else {
            vec![cleaned.split('.').map(String::from).collect()]
        };
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let body = node.child_by_field_name("body")?;

        let mut methods = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            let method_node = match child.kind() {
                "decorated_definition" => find_child(child, "function_definition"),
                "function_definition" => Some(child),
                _ => None,
            };

            if let Some(fn_node) = method_node {
                let fn_name = fn_node
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source))
                    .unwrap_or("_");
                let params = fn_node
                    .child_by_field_name("parameters")
                    .map(|n| node_text(n, source))
                    .unwrap_or("()");
                let ret = fn_node
                    .child_by_field_name("return_type")
                    .map(|n| node_text(n, source));
                let ret_str = ret.map(|r| format!(" -> {r}")).unwrap_or_default();
                let lr = line_range(
                    fn_node.start_position().row + 1,
                    fn_node.end_position().row + 1,
                );

                if child.kind() == "decorated_definition" {
                    let mut dec_cursor = child.walk();
                    for dec in child.children(&mut dec_cursor) {
                        if dec.kind() == "decorator" {
                            methods.push(node_text(dec, source).to_string());
                        }
                    }
                }

                methods.push(compact_ws(&format!("{fn_name}{params}{ret_str} {lr}")).into_owned());
            }
        }

        Some(SkeletonEntry::new(Section::Class, node, name.to_string()).with_children(methods))
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let actual = if node.kind() == "decorated_definition" {
            find_child(node, "function_definition")?
        } else {
            node
        };

        let name = actual
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let params = actual
            .child_by_field_name("parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let ret = actual
            .child_by_field_name("return_type")
            .map(|n| node_text(n, source));
        let ret_str = ret.map(|r| format!(" -> {r}")).unwrap_or_default();

        Some(SkeletonEntry::new(
            Section::Function,
            node,
            compact_ws(&format!("{name}{params}{ret_str}")).into_owned(),
        ))
    }

    fn extract_assignment(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let left = node.child(0)?;
        let name = node_text(left, source);
        if !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return None;
        }

        let value = {
            let mut found_eq = false;
            let mut val = None;
            for i in 0..node.child_count() {
                let c = node.child(i as u32).unwrap();
                if found_eq {
                    val = Some(c);
                    break;
                }
                if node_text(c, source) == "=" {
                    found_eq = true;
                }
            }
            val.map(|n| truncate(node_text(n, source), 60))
        };

        let val_str = value.map(|v| format!(" = {v}")).unwrap_or_default();
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            format!("{name}{val_str}"),
        ))
    }
}

impl LanguageExtractor for PythonExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        let entry = match node.kind() {
            "import_statement" | "import_from_statement" => self.extract_import(node, source),
            "class_definition" => self.extract_class(node, source),
            "function_definition" => self.extract_function(node, source),
            "decorated_definition" => {
                let inner = find_child(node, "class_definition")
                    .or_else(|| find_child(node, "function_definition"));
                match inner {
                    Some(i) if i.kind() == "class_definition" => {
                        self.extract_class(i, source).map(|mut entry| {
                            entry.line_start = node.start_position().row + 1;
                            entry
                        })
                    }
                    Some(_) => self.extract_function(node, source),
                    None => None,
                }
            }
            "expression_statement" => node
                .child(0)
                .filter(|c| c.kind() == "assignment")
                .and_then(|c| self.extract_assignment(c, source)),
            _ => None,
        };
        entry.into_iter().collect()
    }

    fn is_doc_comment(&self, _node: Node, _source: &[u8]) -> bool {
        false
    }

    fn import_separator(&self) -> &'static str {
        "."
    }

    fn is_module_doc(&self, node: Node, source: &[u8]) -> bool {
        if node.kind() != "expression_statement" {
            return false;
        }
        let Some(child) = node.child(0) else {
            return false;
        };
        child.kind() == "string" && node_text(child, source).starts_with("\"\"\"")
    }
}
