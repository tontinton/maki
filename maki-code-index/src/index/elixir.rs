use tree_sitter::Node;

use super::common::{
    LanguageExtractor, Section, SkeletonEntry, compact_ws, find_child, line_range, node_text,
};

pub(crate) struct ElixirExtractor;

const DEF: &str = "def";
const DEFP: &str = "defp";
const DEFMODULE: &str = "defmodule";

const IMPORT_KEYWORDS: &[&str] = &["alias", "import", "require", "use"];

const DOC_ATTRS: &[&str] = &["doc", "moduledoc", "typedoc", "spec"];

impl ElixirExtractor {
    fn call_target<'a>(&self, node: Node, source: &'a [u8]) -> Option<&'a str> {
        if node.kind() != "call" {
            return None;
        }
        let target = node.child_by_field_name("target")?;
        Some(node_text(target, source))
    }

    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let target_name = self.call_target(node, source)?;
        if !IMPORT_KEYWORDS.contains(&target_name) {
            return None;
        }
        let args = find_child(node, "arguments")?;
        let first_arg = args.child(0)?;
        let module_path = node_text(first_arg, source);
        let segments: Vec<String> = module_path.split('.').map(String::from).collect();
        Some(SkeletonEntry::new_import_with_keyword(
            node,
            Some(target_name.to_string()),
            vec![segments],
        ))
    }

    fn extract_module(&self, node: Node, source: &[u8]) -> Vec<SkeletonEntry> {
        match self.call_target(node, source) {
            Some(DEFMODULE) => {}
            _ => return Vec::new(),
        };
        let args = match find_child(node, "arguments") {
            Some(a) => a,
            None => return Vec::new(),
        };
        let name = match args.child(0) {
            Some(c) => node_text(c, source),
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        let mut methods = Vec::new();
        if let Some(block) = find_child(node, "do_block") {
            let mut cursor = block.walk();
            for child in block.children(&mut cursor) {
                if let Some(imp) = self.extract_import(child, source) {
                    imports.push(imp);
                    continue;
                }
                if self.is_def_call(child, source).is_some()
                    && let Some(sig) = self.def_sig(child, source)
                {
                    let lr =
                        line_range(child.start_position().row + 1, child.end_position().row + 1);
                    methods.push(compact_ws(&format!("{sig} {lr}")).into_owned());
                }
            }
        }
        imports.push(
            SkeletonEntry::new(Section::Class, node, format!("defmodule {name}"))
                .with_children(methods),
        );
        imports
    }
    fn def_sig(&self, node: Node, source: &[u8]) -> Option<String> {
        let args = find_child(node, "arguments")?;
        let first = args.child(0)?;
        match first.kind() {
            "call" | "identifier" | "binary_operator" => Some(node_text(first, source).to_string()),
            _ => None,
        }
    }

    fn is_def_call<'a>(&self, node: Node, source: &'a [u8]) -> Option<&'a str> {
        let target_name = self.call_target(node, source)?;
        (target_name == DEF || target_name == DEFP).then_some(target_name)
    }

    fn extract_standalone_fn(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        self.is_def_call(node, source)?;
        let sig = self.def_sig(node, source)?;
        Some(SkeletonEntry::new(
            Section::Function,
            node,
            compact_ws(&sig).into_owned(),
        ))
    }

    fn extract_module_attr(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let name = self.attr_name(node, source)?;
        if DOC_ATTRS.contains(&name.as_str()) {
            return None;
        }
        if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
            return None;
        }
        Some(SkeletonEntry::new(
            Section::Constant,
            node,
            format!("@{name}"),
        ))
    }

    fn attr_name(&self, node: Node, source: &[u8]) -> Option<String> {
        if node.kind() != "unary_operator" {
            return None;
        }
        let op = node.child_by_field_name("operator")?;
        if node_text(op, source) != "@" {
            return None;
        }
        let operand = node.child_by_field_name("operand")?;
        match operand.kind() {
            "identifier" | "alias" => Some(node_text(operand, source).to_string()),
            "call" => {
                let target = operand.child_by_field_name("target")?;
                Some(node_text(target, source).to_string())
            }
            _ => None,
        }
    }
}

impl LanguageExtractor for ElixirExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        if node.kind() == "call" {
            if let Some(entry) = self.extract_import(node, source) {
                return vec![entry];
            }
            let module_entries = self.extract_module(node, source);
            if !module_entries.is_empty() {
                return module_entries;
            }
            if let Some(entry) = self.extract_standalone_fn(node, source) {
                return vec![entry];
            }
        }
        if let Some(entry) = self.extract_module_attr(node, source) {
            return vec![entry];
        }
        Vec::new()
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        if node.kind() == "comment" {
            return true;
        }
        self.attr_name(node, source)
            .is_some_and(|name| DOC_ATTRS.contains(&name.as_str()))
    }

    fn import_separator(&self) -> &'static str {
        "."
    }
}
