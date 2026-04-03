use tree_sitter::{Node, Tree};

pub(crate) fn node_at_offset(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let mut node = tree.root_node();
    if offset >= node.end_byte() {
        return None;
    }
    loop {
        let mut found_child = false;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.start_byte() <= offset && offset < child.end_byte() {
                node = child;
                found_child = true;
                break;
            }
        }
        if !found_child {
            break;
        }
    }
    Some(node)
}

pub(crate) fn find_ancestor_any<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut current = node;
    loop {
        if kinds.contains(&current.kind()) {
            return Some(current);
        }
        current = current.parent()?;
    }
}

pub(crate) fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

pub(crate) fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}

pub(crate) fn context_line(content: &[u8], byte_offset: usize) -> String {
    let line_start = content[..byte_offset]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_end = content[byte_offset..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| byte_offset + p)
        .unwrap_or(content.len());
    String::from_utf8_lossy(&content[line_start..line_end])
        .trim()
        .to_string()
}

pub(crate) fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
