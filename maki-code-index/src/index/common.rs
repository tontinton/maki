//! Shared skeleton formatting and tree-sitter helpers used by all language extractors.
//! `LanguageExtractor` trait defines the per-language hooks; `format_skeleton` groups entries
//! by `Section` (sorted by enum discriminant order, not source order) and renders them.
//! Imports get special treatment: same-root paths are consolidated (e.g. two `std::` uses merge).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write;

use tree_sitter::Node;

pub(crate) const FIELD_TRUNCATE_THRESHOLD: usize = 8;
const LINE_WRAP_THRESHOLD: usize = 120;

pub(crate) fn truncated_msg(total: usize) -> String {
    let omitted = total - FIELD_TRUNCATE_THRESHOLD;
    format!("[{omitted} more truncated]")
}

pub(crate) use crate::helpers::{find_child, node_text};

pub(crate) fn compact_ws(s: &str) -> Cow<'_, str> {
    let needs_compact = s
        .as_bytes()
        .windows(2)
        .any(|w| w[0].is_ascii_whitespace() && w[1].is_ascii_whitespace());
    if !needs_compact {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_ascii_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            prev_ws = false;
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

#[allow(dead_code)]
pub(crate) fn truncate(s: &str, max_chars: usize) -> Cow<'_, str> {
    if s.chars().count() <= max_chars {
        return Cow::Borrowed(s);
    }
    let boundary = s
        .char_indices()
        .nth(max_chars.saturating_sub(11))
        .map_or(s.len(), |(i, _)| i);
    Cow::Owned(format!("{}[truncated]", &s[..boundary]))
}

pub(crate) struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "[{}]", self.start)
        } else {
            write!(f, "[{}-{}]", self.start, self.end)
        }
    }
}

pub(crate) fn line_range(start: usize, end: usize) -> LineRange {
    LineRange { start, end }
}

#[cfg(feature = "lang-rust")]
pub(crate) fn has_test_attr(attrs: &[Node], source: &[u8]) -> bool {
    attrs.iter().any(|a| {
        let text = node_text(*a, source);
        text == "#[test]" || text == "#[cfg(test)]" || text.ends_with("::test]")
    })
}

pub(crate) fn doc_comment_start_line(
    node: Node,
    source: &[u8],
    extractor: &dyn LanguageExtractor,
) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if extractor.is_attr(p) {
            prev = p.prev_sibling();
            continue;
        }
        if extractor.is_doc_comment(p, source) {
            earliest = Some(p.start_position().row + 1);
            prev = p.prev_sibling();
        } else {
            break;
        }
    }
    earliest
}

pub(crate) fn detect_module_doc(
    root: Node,
    source: &[u8],
    extractor: &dyn LanguageExtractor,
) -> Option<(usize, usize)> {
    let mut cursor = root.walk();
    let mut start = None;
    let mut end = None;
    for child in root.children(&mut cursor) {
        if extractor.is_module_doc(child, source) {
            let line = child.start_position().row + 1;
            if start.is_none() {
                start = Some(line);
            }
            let end_pos = child.end_position();
            let end_line = if end_pos.column == 0 {
                end_pos.row
            } else {
                end_pos.row + 1
            };
            end = Some(end_line);
        } else if !extractor.is_attr(child) && !child.is_extra() {
            break;
        }
    }
    start.map(|s| (s, end.unwrap()))
}

#[cfg(feature = "lang-rust")]
pub(crate) fn relevant_attr_texts(attrs: &[Node], source: &[u8]) -> Vec<String> {
    attrs
        .iter()
        .filter_map(|a| {
            let text = node_text(*a, source);
            (text.contains("derive") || text.contains("cfg")).then(|| text.to_string())
        })
        .collect()
}

#[cfg(feature = "lang-rust")]
pub(crate) fn vis_prefix<'a>(node: Node<'a>, source: &'a [u8]) -> &'a str {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return node_text(child, source);
        }
    }
    ""
}

pub(crate) fn prefixed(vis: &str, rest: std::fmt::Arguments<'_>) -> String {
    if vis.is_empty() {
        format!("{rest}")
    } else {
        format!("{vis} {rest}")
    }
}

#[cfg(feature = "lang-rust")]
pub(crate) fn fn_signature(node: Node, source: &[u8]) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))?;
    let params = find_child(node, "parameters")
        .map(|n| node_text(n, source))
        .unwrap_or("()");
    let ret = node
        .child_by_field_name("return_type")
        .map(|n| {
            let t = node_text(n, source);
            if t.starts_with("->") {
                format!(" {t}")
            } else {
                format!(" -> {t}")
            }
        })
        .unwrap_or_default();
    Some(compact_ws(&format!("{name}{params}{ret}")).into_owned())
}

pub(crate) fn extract_enum_variants(body: Node, source: &[u8], variant_kind: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == variant_kind {
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or("_");
            values.push(name.to_string());
        }
    }
    values
}

pub(crate) fn extract_fields_truncated(
    body: Node,
    source: &[u8],
    field_kind: &str,
    format_field: impl Fn(Node, &[u8]) -> String,
) -> Vec<String> {
    let mut fields = Vec::new();
    let mut total = 0usize;
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == field_kind {
            total += 1;
            if total <= FIELD_TRUNCATE_THRESHOLD {
                fields.push(format_field(child, source));
            }
        }
    }
    if total > FIELD_TRUNCATE_THRESHOLD {
        fields.push(truncated_msg(total));
    }
    fields
}

pub(crate) struct BodyMemberRule<'a> {
    pub(crate) kind: &'a str,
    pub(crate) handler: BodyMemberHandler<'a>,
}

pub(crate) enum BodyMemberHandler<'a> {
    Method(&'a dyn Fn(Node, &[u8]) -> Option<String>),
    FieldTruncated {
        format_fn: &'a dyn Fn(Node, &[u8]) -> String,
        counter: &'a str,
    },
}

pub(crate) fn extract_body_members(
    body: Node,
    source: &[u8],
    rules: &[BodyMemberRule],
) -> Vec<String> {
    let mut members = Vec::new();
    let mut field_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        let kind = child.kind();
        let Some(rule) = rules.iter().find(|r| r.kind == kind) else {
            continue;
        };
        match &rule.handler {
            BodyMemberHandler::Method(f) => {
                if let Some(sig) = f(child, source) {
                    let lr =
                        line_range(child.start_position().row + 1, child.end_position().row + 1);
                    members.push(format!("{sig} {lr}"));
                }
            }
            BodyMemberHandler::FieldTruncated { format_fn, counter } => {
                let count = field_counts.entry(counter).or_insert(0);
                *count += 1;
                if *count <= FIELD_TRUNCATE_THRESHOLD {
                    let text = format_fn(child, source);
                    let lr =
                        line_range(child.start_position().row + 1, child.end_position().row + 1);
                    members.push(format!("{text} {lr}"));
                }
            }
        }
    }
    for count in field_counts.values() {
        if *count > FIELD_TRUNCATE_THRESHOLD {
            members.push(truncated_msg(*count));
        }
    }
    members
}

pub(crate) fn simple_import(
    node: Node,
    source: &[u8],
    prefixes: &[&str],
    sep: &str,
) -> Option<SkeletonEntry> {
    let text = node_text(node, source);
    let mut cleaned = text;
    for prefix in prefixes {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest;
            break;
        }
    }
    let cleaned = cleaned.trim_end_matches(';').trim();
    let paths = vec![cleaned.split(sep).map(String::from).collect()];
    Some(SkeletonEntry::new_import(node, paths))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)]
pub(crate) enum Section {
    Import,
    Module,
    Constant,
    Type,
    Trait,
    Impl,
    Function,
    Class,
    Macro,
    Test,
}

impl Section {
    pub(crate) fn header(self) -> &'static str {
        match self {
            Self::Import => "imports:",
            Self::Module => "mod:",
            Self::Constant => "consts:",
            Self::Type => "types:",
            Self::Trait => "traits:",
            Self::Impl => "impls:",
            Self::Function => "fns:",
            Self::Class => "classes:",
            Self::Macro => "macros:",
            Self::Test => "tests:",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ChildKind {
    #[default]
    Detailed,
    Brief,
}

#[derive(Debug, Clone)]
pub(crate) enum EntryData {
    Import {
        paths: Vec<Vec<String>>,
    },
    Item {
        text: String,
        children: Vec<String>,
        attrs: Vec<String>,
        child_kind: ChildKind,
    },
}

pub(crate) struct SkeletonEntry {
    pub(crate) section: Section,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) data: EntryData,
}

impl SkeletonEntry {
    pub(crate) fn new(section: Section, node: Node, text: String) -> Self {
        Self {
            section,
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            data: EntryData::Item {
                text,
                children: Vec::new(),
                attrs: Vec::new(),
                child_kind: ChildKind::default(),
            },
        }
    }

    pub(crate) fn new_import(node: Node, paths: Vec<Vec<String>>) -> Self {
        Self {
            section: Section::Import,
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            data: EntryData::Import { paths },
        }
    }

    pub(crate) fn text(&self) -> &str {
        match &self.data {
            EntryData::Item { text, .. } => text,
            EntryData::Import { .. } => "",
        }
    }

    pub(crate) fn with_children(mut self, new_children: Vec<String>) -> Self {
        match &mut self.data {
            EntryData::Item { children, .. } => *children = new_children,
            EntryData::Import { .. } => unreachable!("with_children called on import entry"),
        }
        self
    }

    pub(crate) fn with_attrs(mut self, new_attrs: Vec<String>) -> Self {
        match &mut self.data {
            EntryData::Item { attrs, .. } => *attrs = new_attrs,
            EntryData::Import { .. } => unreachable!("with_attrs called on import entry"),
        }
        self
    }

    pub(crate) fn with_child_kind(mut self, kind: ChildKind) -> Self {
        match &mut self.data {
            EntryData::Item { child_kind, .. } => *child_kind = kind,
            EntryData::Import { .. } => unreachable!("with_child_kind called on import entry"),
        }
        self
    }
}

pub(crate) trait LanguageExtractor {
    fn extract_nodes(&self, node: Node, source: &[u8], attrs: &[Node]) -> Vec<SkeletonEntry>;
    fn is_doc_comment(&self, node: Node, source: &[u8]) -> bool;

    fn is_test_node(&self, _node: Node, _source: &[u8], _attrs: &[Node]) -> bool {
        false
    }

    fn is_module_doc(&self, _node: Node, _source: &[u8]) -> bool {
        false
    }

    fn import_separator(&self) -> &'static str {
        "::"
    }

    fn is_attr(&self, _node: Node) -> bool {
        false
    }
    fn collect_preceding_attrs<'a>(&self, node: Node<'a>) -> Vec<Node<'a>> {
        let mut attrs = Vec::new();
        let mut prev = node.prev_sibling();
        while let Some(p) = prev {
            if self.is_attr(p) {
                attrs.push(p);
            } else {
                break;
            }
            prev = p.prev_sibling();
        }
        attrs.reverse();
        attrs
    }
}

pub(crate) fn format_skeleton(
    entries: &[SkeletonEntry],
    test_lines: &[usize],
    module_doc: Option<(usize, usize)>,
    import_sep: &str,
) -> String {
    let mut out = String::with_capacity(entries.len() * 80);

    if let Some((start, end)) = module_doc {
        let _ = writeln!(out, "module doc: {}", line_range(start, end));
    }

    let mut grouped: BTreeMap<Section, Vec<&SkeletonEntry>> = BTreeMap::new();
    for entry in entries {
        grouped.entry(entry.section).or_default().push(entry);
    }

    for (section, items) in &grouped {
        match section {
            Section::Import => format_imports(&mut out, items, import_sep),
            Section::Module => format_leaf_section(&mut out, section.header(), items),
            _ => format_section(&mut out, section.header(), items),
        }
    }

    if !test_lines.is_empty() {
        let min = *test_lines.iter().min().unwrap();
        let max = *test_lines.iter().max().unwrap();
        let sep = if out.is_empty() { "" } else { "\n" };
        let _ = writeln!(out, "{sep}tests: {}", line_range(min, max));
    }

    out
}

fn format_section(out: &mut String, header: &str, items: &[&SkeletonEntry]) {
    let sep = if out.is_empty() { "" } else { "\n" };
    let _ = writeln!(out, "{sep}{header}");
    for entry in items {
        let EntryData::Item {
            text,
            children,
            attrs,
            child_kind,
        } = &entry.data
        else {
            continue;
        };
        for attr in attrs {
            let _ = writeln!(out, "  {attr}");
        }
        let _ = writeln!(
            out,
            "  {} {}",
            text,
            line_range(entry.line_start, entry.line_end)
        );
        match child_kind {
            ChildKind::Brief if !children.is_empty() => {
                let names: Vec<&str> = children.iter().map(String::as_str).collect();
                for line in wrap_csv(&names, "    ") {
                    let _ = writeln!(out, "{line}");
                }
            }
            _ => {
                for child in children {
                    let _ = writeln!(out, "    {child}");
                }
            }
        }
    }
}

fn format_leaf_section(out: &mut String, header: &str, items: &[&SkeletonEntry]) {
    let sep = if out.is_empty() { "" } else { "\n" };
    let min = items.iter().map(|e| e.line_start).min().unwrap();
    let max = items.iter().map(|e| e.line_end).max().unwrap();
    let _ = writeln!(out, "{sep}{header} {}", line_range(min, max));
    let names: Vec<&str> = items.iter().map(|e| e.text()).collect();
    let indent = "  ";
    for line in wrap_csv(&names, indent) {
        let _ = writeln!(out, "{line}");
    }
}

fn wrap_csv(items: &[&str], indent: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::from(indent);
    for (i, item) in items.iter().enumerate() {
        let addition = if i == 0 {
            item.to_string()
        } else {
            format!(", {item}")
        };
        if i > 0 && current.len() + addition.len() > LINE_WRAP_THRESHOLD {
            lines.push(current);
            current = format!("{indent}{item}");
        } else {
            current.push_str(&addition);
        }
    }
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines
}

fn format_imports(out: &mut String, entries: &[&SkeletonEntry], import_sep: &str) {
    if entries.is_empty() {
        return;
    }

    let min_line = entries.iter().map(|e| e.line_start).min().unwrap();
    let max_line = entries.iter().map(|e| e.line_end).max().unwrap();

    let prefix = if out.is_empty() { "" } else { "\n" };
    let _ = writeln!(out, "{prefix}imports: {}", line_range(min_line, max_line));

    let mut trie = ImportTrie::default();
    for entry in entries {
        if let EntryData::Import { paths } = &entry.data {
            for path in paths {
                trie.insert(path);
            }
        }
    }
    let lines = trie.render(import_sep);
    for line in lines {
        let _ = writeln!(out, "  {line}");
    }
}

/// Finds the first occurrence of `sep` outside braces.
fn find_sep(text: &str, sep: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let sep_bytes = sep.as_bytes();
    let mut depth = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && bytes[i..].starts_with(sep_bytes) => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits `text` on `delim` at brace-depth 0, trimming each part.
fn split_top_level(text: &str, delim: u8) -> Vec<&str> {
    let mut results = Vec::new();
    let mut depth = 0u32;
    let mut start = 0;
    for (i, &b) in text.as_bytes().iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            c if c == delim && depth == 0 => {
                results.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        results.push(last);
    }
    results
}

pub(crate) fn expand_import(text: &str, sep: &str) -> Vec<Vec<String>> {
    let mut results: Vec<Vec<String>> = Vec::new();
    let mut stack: Vec<(Vec<String>, &str)> = vec![(Vec::new(), text.trim())];

    while let Some((prefix, remaining)) = stack.pop() {
        if remaining.is_empty() {
            if !prefix.is_empty() {
                results.push(prefix);
            }
            continue;
        }

        let Some(pos) = find_sep(remaining, sep) else {
            let mut path = prefix;
            path.push(remaining.to_string());
            results.push(path);
            continue;
        };

        let segment = &remaining[..pos];
        let rest = &remaining[pos + sep.len()..];

        let mut new_prefix = prefix;
        new_prefix.push(segment.to_string());

        if let Some(inner) = rest.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            let items = split_top_level(inner, b',');
            for item in &items[..items.len() - 1] {
                stack.push((new_prefix.clone(), item));
            }
            if let Some(last) = items.last() {
                stack.push((new_prefix, last));
            }
        } else {
            stack.push((new_prefix, rest));
        }
    }

    results
}

#[derive(Default)]
struct ImportTrie {
    children: BTreeMap<String, ImportTrie>,
    is_leaf: bool,
}

impl ImportTrie {
    fn insert(&mut self, segments: &[String]) {
        let mut node = self;
        for seg in segments {
            node = node.children.entry(seg.clone()).or_default();
        }
        node.is_leaf = true;
    }

    fn render(&self, sep: &str) -> Vec<String> {
        render_children(&self.children, sep)
    }
}

fn render_node(seg: &str, node: &ImportTrie, sep: &str) -> Vec<String> {
    if node.children.is_empty() {
        return vec![seg.to_string()];
    }

    let rendered = render_children(&node.children, sep);

    if node.is_leaf {
        let mut out = vec![seg.to_string()];
        for item in &rendered {
            out.push(format!("{seg}{sep}{item}"));
        }
        return out;
    }

    if rendered.len() == 1 {
        vec![format!("{seg}{sep}{}", rendered[0])]
    } else {
        vec![format!("{seg}{sep}{{{}}}", rendered.join(", "))]
    }
}

fn render_children(children: &BTreeMap<String, ImportTrie>, sep: &str) -> Vec<String> {
    children
        .iter()
        .flat_map(|(seg, node)| render_node(seg, node, sep))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn truncate_at_char_boundary() {
        let long = format!("{}{}", "a".repeat(55), "ü".repeat(10));
        let result = truncate(&long, 60);
        assert!(result.ends_with("[truncated]"));
        assert!(result.chars().count() <= 60);
    }

    fn build_trie(imports: &[&str], sep: &str) -> ImportTrie {
        let mut trie = ImportTrie::default();
        for &imp in imports {
            for segments in expand_import(imp, sep) {
                trie.insert(&segments);
            }
        }
        trie
    }

    #[test_case(&["std::io", "std::fs"],                          "::", &["std::{fs, io}"]                    ; "shared_prefix")]
    #[test_case(&["crate::a::X", "crate::a::Y", "crate::b::Z"], "::", &["crate::{a::{X, Y}, b::Z}"]          ; "deep_shared_prefix")]
    #[test_case(&["std::io::*", "std::io::Write"],                "::", &["std::io::{*, Write}"]           ; "wildcard")]
    #[test_case(&["java.util.List", "java.io.IOException"],       ".",  &["java.{io.IOException, util.List}"]  ; "dot_separator")]
    #[test_case(&["os", "std::io"],                               "::", &["os", "std::io"]                     ; "single_segment")]
    #[test_case(&["std::io"],                                     "::", &["std::io"]                            ; "single_import")]
    #[test_case(&["std::io", "std::io::Write"],                   "::", &["std::{io, io::Write}"]             ; "leaf_and_children")]
    fn trie_rendering(imports: &[&str], sep: &str, expected: &[&str]) {
        assert_eq!(build_trie(imports, sep).render(sep), expected);
    }
}
