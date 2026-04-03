use tree_sitter::Node;

use super::common::{
    FIELD_TRUNCATE_THRESHOLD, LanguageExtractor, Section, SkeletonEntry, compact_ws, find_child,
    line_range, node_text, prefixed,
};

pub(crate) struct KotlinExtractor;

impl KotlinExtractor {
    fn modifiers_text<'a>(&self, node: Node<'a>, source: &'a [u8]) -> String {
        let Some(mods) = find_child(node, "modifiers") else {
            return String::new();
        };
        let mut cursor = mods.walk();
        let mut parts: Vec<&str> = Vec::new();
        for child in mods.children(&mut cursor) {
            match child.kind() {
                "annotation" => {}
                _ => parts.push(node_text(child, source)),
            }
        }
        parts.join(" ")
    }

    fn type_params_text<'a>(&self, node: Node<'a>, source: &'a [u8]) -> &'a str {
        find_child(node, "type_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("")
    }

    fn delegation_text(&self, node: Node, source: &[u8]) -> String {
        find_child(node, "delegation_specifiers")
            .map(|n| format!(" : {}", node_text(n, source)))
            .unwrap_or_default()
    }

    fn extract_import(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let qi = find_child(node, "qualified_identifier")?;
        let path: Vec<String> = {
            let mut cursor = qi.walk();
            qi.children(&mut cursor)
                .filter(|c| c.kind() == "identifier")
                .map(|c| node_text(c, source).to_string())
                .collect()
        };
        // Handle "import ... as Alias" or "import ...*"
        let raw = node_text(node, source);
        let mut paths = vec![path];
        if raw.contains(".*")
            && let Some(last) = paths[0].last_mut()
        {
            *last = format!("{last}.*");
        }
        Some(SkeletonEntry::new_import(node, paths))
    }

    fn extract_package(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let pkg = find_child(node, "qualified_identifier")
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_else(|| {
                node_text(node, source)
                    .trim_start_matches("package")
                    .trim()
                    .to_string()
            });
        Some(SkeletonEntry::new(Section::Module, node, pkg))
    }

    fn extract_class(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let tparams = self.type_params_text(node, source);
        let ctor = find_child(node, "primary_constructor")
            .map(|n| {
                find_child(n, "class_parameters")
                    .map(|p| node_text(p, source))
                    .unwrap_or("")
            })
            .unwrap_or("");
        let supers = self.delegation_text(node, source);

        let mut kw_cursor = node.walk();
        let kw = node
            .children(&mut kw_cursor)
            .find(|c| !c.is_named() && matches!(node_text(*c, source), "class" | "interface"))
            .map(|c| node_text(c, source))
            .unwrap_or("class");
        let kw = if mods.contains("enum") {
            "enum class"
        } else {
            kw
        };
        let label = prefixed(&mods, format_args!("{kw} {name}{tparams}{ctor}{supers}"));
        let children = self.class_members(node, source);
        Some(
            SkeletonEntry::new(Section::Class, node, compact_ws(&label).into_owned())
                .with_children(children),
        )
    }

    fn class_members(&self, node: Node, source: &[u8]) -> Vec<String> {
        let body_node =
            find_child(node, "class_body").or_else(|| find_child(node, "enum_class_body"));
        let Some(body) = body_node else {
            return Vec::new();
        };
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "function_declaration" => {
                    if let Some(sig) = self.fn_sig(child, source) {
                        let lr = line_range(
                            child.start_position().row + 1,
                            child.end_position().row + 1,
                        );
                        members.push(format!("{sig} {lr}"));
                    }
                }
                "property_declaration" => {
                    if members.len() < FIELD_TRUNCATE_THRESHOLD
                        && let Some(text) = self.property_text(child, source)
                    {
                        let lr = line_range(
                            child.start_position().row + 1,
                            child.end_position().row + 1,
                        );
                        members.push(format!("{text} {lr}"));
                    }
                }
                "companion_object" => {
                    let inner_members = self.companion_members(child, source);
                    members.extend(inner_members);
                }
                "enum_entry" => {
                    let entry_name = find_child(child, "identifier")
                        .map(|n| node_text(n, source))
                        .unwrap_or("_");
                    members.push(entry_name.to_string());
                }
                _ => {}
            }
        }
        members
    }

    fn companion_members(&self, node: Node, source: &[u8]) -> Vec<String> {
        let Some(body) = find_child(node, "class_body") else {
            return Vec::new();
        };
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "function_declaration" {
                if let Some(sig) = self.fn_sig(child, source) {
                    let lr =
                        line_range(child.start_position().row + 1, child.end_position().row + 1);
                    members.push(format!("{sig} {lr}"));
                }
            } else if child.kind() == "property_declaration"
                && let Some(text) = self.property_text(child, source)
            {
                let lr = line_range(child.start_position().row + 1, child.end_position().row + 1);
                members.push(format!("{text} {lr}"));
            }
        }
        members
    }

    fn extract_object(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let supers = self.delegation_text(node, source);
        let label = prefixed(&mods, format_args!("object {name}{supers}"));
        let children = self.class_members(node, source);
        Some(
            SkeletonEntry::new(Section::Class, node, compact_ws(&label).into_owned())
                .with_children(children),
        )
    }

    fn fn_sig(&self, node: Node, source: &[u8]) -> Option<String> {
        let mods = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let tparams = self.type_params_text(node, source);
        let params = find_child(node, "function_value_parameters")
            .map(|n| node_text(n, source))
            .unwrap_or("()");
        let ret = find_child(node, "type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        Some(
            compact_ws(&prefixed(
                &mods,
                format_args!("fun {tparams}{name}{params}{ret}"),
            ))
            .into_owned(),
        )
    }

    fn extract_function(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let sig = self.fn_sig(node, source)?;
        Some(SkeletonEntry::new(Section::Function, node, sig))
    }

    fn property_text(&self, node: Node, source: &[u8]) -> Option<String> {
        let mods = self.modifiers_text(node, source);
        let raw = node_text(node, source);
        let kw = if raw.trim_start().starts_with("val") || mods.contains("val") {
            "val"
        } else {
            "var"
        };
        let var_decl = find_child(node, "variable_declaration")?;
        let vname = find_child(var_decl, "identifier")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        let ty = find_child(var_decl, "type")
            .map(|n| format!(": {}", node_text(n, source)))
            .unwrap_or_default();
        Some(compact_ws(&prefixed(&mods, format_args!("{kw} {vname}{ty}"))).into_owned())
    }

    fn extract_property(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let var_decl = find_child(node, "variable_declaration")?;
        let vname = find_child(var_decl, "identifier")
            .map(|n| node_text(n, source))
            .unwrap_or("_");
        let is_const = find_child(node, "modifiers")
            .map(|mods| {
                let mut c = mods.walk();
                mods.children(&mut c)
                    .any(|ch| ch.kind() == "property_modifier" && node_text(ch, source) == "const")
            })
            .unwrap_or(false);
        if !is_const && !vname.chars().all(|c| c.is_uppercase() || c == '_') {
            return None;
        }
        let text = self.property_text(node, source)?;
        Some(SkeletonEntry::new(Section::Constant, node, text))
    }

    fn extract_type_alias(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        let vis = self.modifiers_text(node, source);
        let name = node
            .child_by_field_name("type")
            .map(|n| node_text(n, source))?;
        let tparams = self.type_params_text(node, source);
        let rhs = {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .filter(|c| c.kind() == "type")
                .last()
                .map(|n| node_text(n, source))
                .unwrap_or("_")
        };
        let label = prefixed(&vis, format_args!("typealias {name}{tparams} = {rhs}"));
        Some(SkeletonEntry::new(
            Section::Type,
            node,
            compact_ws(&label).into_owned(),
        ))
    }
}

impl LanguageExtractor for KotlinExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], _attrs: &[Node]) -> Vec<SkeletonEntry> {
        match node.kind() {
            "import" => self.extract_import(node, source).into_iter().collect(),
            "package_header" => self.extract_package(node, source).into_iter().collect(),
            "statement" => {
                let mut cursor = node.walk();
                node.children(&mut cursor)
                    .flat_map(|child| self.extract_declaration(child, source))
                    .collect()
            }
            k if is_decl(k) => self.extract_declaration(node, source).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool {
        node.kind() == "multiline_comment" && node_text(node, source).starts_with("/**")
    }

    fn import_separator(&self) -> &'static str {
        "."
    }
}

impl KotlinExtractor {
    fn extract_declaration(&self, node: Node, source: &[u8]) -> Option<SkeletonEntry> {
        match node.kind() {
            "class_declaration" => self.extract_class(node, source),
            "object_declaration" => self.extract_object(node, source),
            "function_declaration" => self.extract_function(node, source),
            "property_declaration" => self.extract_property(node, source),
            "type_alias" => self.extract_type_alias(node, source),
            _ => None,
        }
    }
}

fn is_decl(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "object_declaration"
            | "function_declaration"
            | "property_declaration"
            | "type_alias"
    )
}

#[cfg(test)]
mod tests {
    use crate::{Language, index_source};

    const KOTLIN_SOURCE: &[u8] = b"
package com.example.app

import kotlin.collections.List
import java.io.File

const val MAX_SIZE = 100
val SOME_CONST = \"value\"
var notAConst = 0

typealias StringList = List<String>

data class User(val name: String, val age: Int) : Comparable<User> {
    fun greet(): String = \"Hello $name\"
    val label: String = name
}

interface Greeter {
    fun greet(): String
}

object Singleton : Greeter {
    override fun greet() = \"Hello\"
}

fun topLevel(x: Int, y: Int): Int = x + y

suspend fun asyncFn(): Unit {}

enum class Color {
    RED, GREEN, BLUE
}
";

    #[test]
    fn kotlin_skeleton_smoke() {
        let result = index_source(KOTLIN_SOURCE, Language::Kotlin).unwrap();
        assert!(result.contains("mod:"), "missing mod section: {result}");
        assert!(
            result.contains("com.example.app"),
            "missing package: {result}"
        );
        assert!(result.contains("imports:"), "missing imports: {result}");
        assert!(
            result.contains("kotlin.collections"),
            "missing kotlin import: {result}"
        );
        assert!(result.contains("consts:"), "missing consts: {result}");
        assert!(result.contains("MAX_SIZE"), "missing MAX_SIZE: {result}");
        assert!(result.contains("types:"), "missing types: {result}");
        assert!(result.contains("StringList"), "missing typealias: {result}");
        assert!(result.contains("classes:"), "missing classes: {result}");
        assert!(result.contains("User"), "missing User class: {result}");
        assert!(result.contains("greet"), "missing greet method: {result}");
        assert!(
            result.contains("Singleton"),
            "missing Singleton object: {result}"
        );
        assert!(result.contains("fns:"), "missing fns: {result}");
        assert!(result.contains("topLevel"), "missing topLevel fn: {result}");
        assert!(result.contains("asyncFn"), "missing asyncFn: {result}");
    }
}
