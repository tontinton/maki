use tree_sitter::Node;

use super::common::{LanguageExtractor, Section, SkeletonEntry, compact_ws, node_text};

pub(crate) struct MarkdownExtractor;

impl MarkdownExtractor {
    fn heading_level(node: Node, source: &[u8]) -> usize {
        match node.kind() {
            "atx_heading" => {
                // Count the # characters at the start of the raw text
                let text = node_text(node, source);
                text.chars().take_while(|&c| c == '#').count()
            }
            "setext_heading" => {
                let text = node_text(node, source);
                // `===` is level 1, `---` level 2 and other levels don't exist
                if text.trim_end().ends_with('=') { 1 } else { 2 }
            }
            _ => unreachable!(),
        }
    }

    fn collect_all_headings<'a>(
        entries: &mut Vec<(usize, SkeletonEntry)>,
        node: Node<'a>,
        source: &'a [u8],
    ) {
        if (node.kind() == "atx_heading" || node.kind() == "setext_heading")
            && let Some(heading_content) = node.child_by_field_name("heading_content")
        {
            let level = Self::heading_level(node, source);
            let text = node_text(heading_content, source).trim();
            // Prefix with level `#` to format like an atx heading
            let text = format!("{:#<1$} {text}", "", level);
            let compacted = compact_ws(&text).into_owned();
            let entry = SkeletonEntry::new(Section::Heading, node, compacted);
            entries.push((level, entry));
        }

        // Recurse into child nodes
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_all_headings(entries, child, source);
        }
    }
}

impl LanguageExtractor for MarkdownExtractor {
    fn extract_nodes_from_root(
        &self,
        root: Node,
        source: &[u8],
    ) -> (Vec<SkeletonEntry>, Vec<usize>) {
        let mut entries: Vec<(usize, SkeletonEntry)> = Vec::new();

        // Walk the entire tree to find all headings in document order
        Self::collect_all_headings(&mut entries, root, source);

        // For all entries, calculate the end position by finding the next entry
        // with the same or lower level, e.g. a `##` extends until the next `##`
        // or `#`, whichever happens first.
        //
        // If there is none then it extends until the end of the document.
        let doc_end = root.end_position().row + 1;
        for i in 0..entries.len() {
            entries[i].1.line_end = entries[i + 1..]
                .iter()
                .find(|(lvl, _)| *lvl <= entries[i].0)
                .map(|(_, e)| e.line_start - 1)
                .unwrap_or(doc_end);
        }

        let entries = entries.into_iter().map(|(_, e)| e).collect();

        (entries, Vec::new())
    }

    fn import_separator(&self) -> &'static str {
        ""
    }
}
