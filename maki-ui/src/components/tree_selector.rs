//! Single-pane tree selector (§11) + non-destructive rewind landing rule (§4).
//!
//! Reads the `SessionTree` from `SessionState`: active path top-to-bottom, with
//! `⑂ N alternatives` collapse markers at branch points (siblings collapsed by
//! default). Hidden nodes are unlisted (§9 chrome). Tool-result carriers render
//! as `[T]` steps, never user prompts. Summaries are passive, non-selectable.

use std::collections::HashMap;

use crossterm::event::KeyEvent;
use maki_agent::FinalizedPartial;
use maki_providers::{ContentBlock, Message};
use maki_storage::session_log::{build_session_tree, load_folder};
use maki_storage::tree::{
    Flavor, MessageNode, NodeRef, OrderedRecord, Position as TreePosition, Role, SessionTree,
    TreeNode,
};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

const TITLE: &str = " Rewind ";
pub(crate) const NO_TURNS_MSG: &str = "Nothing to rewind to";
const GLYPH_USER: &str = "[U]";
const GLYPH_ASSISTANT: &str = "[A]";
const GLYPH_TOOL: &str = "[T]";
const GLYPH_SUMMARY: &str = "[\u{2211}]";
const BRANCH_MARKER: &str = "\u{2842}";
const PREVIEW_MAX_LEN: usize = 72;
const DETAIL_MAX_LEN: usize = 400;

#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
pub enum TreeSelectorOutcome {
    /// Land before a user prompt and prefill the composer with its text.
    RewindBefore { prompt_text: String },
    /// Land on a node (assistant / tool-result carrier) with no prefill.
    RewindOn,
    /// Block-boundary landing (§4): derive an interrupted sibling from the
    /// assistant node's content prefix, then land on it.
    RewindBoundary {
        parent: Option<NodeRef>,
        blocks: Vec<ContentBlock>,
    },
}

pub enum TreeSelectorAction {
    Consumed,
    Select(TreeSelectorOutcome),
    /// Undo-of-rewind (§4): only offered while the last record is a `Leaf`.
    Undo,
    /// Fork from the cursor node (§5, `f`): copy root→cursor into a new session.
    Fork(NodeRef),
    /// Branch-summary (§6, `s`): summarize the abandoned branch. `parent` is
    /// the landing node (current leaf); `fold_from_id` is the abandoned tip.
    /// Only offered while an undo-of-rewind target exists.
    BranchSummary {
        parent: NodeRef,
        fold_from_id: NodeRef,
    },
    Close,
}

/// One rendered row. Passive rows (summaries, branch markers) render but are
/// not selectable (§4/§11).
#[derive(Clone)]
pub enum TreeRow {
    Node {
        nref: NodeRef,
        flavor: Flavor,
        preview: String,
        detail: String,
        parent: Option<NodeRef>,
    },
    Summary {
        preview: String,
    },
    Branch {
        count: usize,
    },
}

impl TreeRow {
    #[cfg(test)]
    fn selectable(&self) -> bool {
        matches!(
            self,
            Self::Node {
                flavor: Flavor::UserPrompt | Flavor::ToolResultCarrier | Flavor::Assistant { .. },
                ..
            }
        )
    }
}

impl PickerItem for TreeRow {
    fn label(&self) -> &str {
        match self {
            Self::Node { preview, .. } => preview,
            Self::Summary { preview } => preview,
            Self::Branch { count } => {
                let _ = count;
                BRANCH_MARKER
            }
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Node { detail, .. } if !detail.is_empty() => Some(detail),
            _ => None,
        }
    }
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

pub struct TreeSelector {
    picker: ListPicker<TreeRow>,
    /// Snapshot built at open time. The selector is a read view; landing
    /// produces mutations enqueued via the app's storage writer.
    tree: Option<SessionTree>,
}

impl TreeSelector {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
            tree: None,
        }
    }

    /// Build the selector view from a freshly loaded `SessionTree`. Reloads
    /// `log.jsonl` to reflect on-disk truth (the caller barriers first).
    pub fn open(
        &mut self,
        loaded: &maki_storage::session_log::LoadedSession,
    ) -> Result<(), String> {
        let tree =
            build_session_tree(loaded).map_err(|e| format!("failed to build session tree: {e}"))?;
        let rows = build_rows(&tree);
        if rows.is_empty() {
            return Err(NO_TURNS_MSG.into());
        }
        // Tip first: Enter on the topmost row rewinds to the most recent
        // editable point.
        self.picker.open(rows, TITLE);
        self.tree = Some(tree);
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.tree = None;
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    /// Undo availability (§4): offered only while the most recent record is
    /// itself a `Leaf`. Computes the pre-rewind tip via `position_before_last_leaf`.
    pub fn undo_target(&self) -> Option<TreePosition> {
        undo_target_for_tree(self.tree.as_ref()?)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TreeSelectorAction {
        use crossterm::event::KeyCode;
        if key.code == KeyCode::Char('u') && self.undo_target().is_some() {
            return TreeSelectorAction::Undo;
        }
        if key.code == KeyCode::Char('b')
            && let Some(outcome) = self.boundary_outcome()
        {
            return TreeSelectorAction::Select(outcome);
        }
        if key.code == KeyCode::Char('f')
            && let Some(nref) = self.selected_node_ref()
        {
            return TreeSelectorAction::Fork(nref);
        }
        if key.code == KeyCode::Char('s')
            && let Some((parent, fold_from_id)) = self.branch_summary_target()
        {
            return TreeSelectorAction::BranchSummary {
                parent,
                fold_from_id,
            };
        }
        match self.picker.handle_key(key) {
            PickerAction::Consumed | PickerAction::Toggle(..) => TreeSelectorAction::Consumed,
            PickerAction::Close => TreeSelectorAction::Close,
            PickerAction::Select(_, row) => match self.landing(&row) {
                Some(outcome) => TreeSelectorAction::Select(outcome),
                None => TreeSelectorAction::Consumed,
            },
        }
    }

    /// The selected row's node ref (the fork cursor, §5). Passive rows
    /// (summaries, branch markers) are non-selectable.
    fn selected_node_ref(&self) -> Option<NodeRef> {
        let row = self.picker.selected_item()?;
        match row {
            TreeRow::Node { nref, .. } => Some(nref.clone()),
            _ => None,
        }
    }

    /// Branch-summary target (§6): the current leaf is the parent (landing
    /// node), the abandoned branch tip (`fold_from_id`) is the undo-of-rewind
    /// target. Only offered while an undo target exists — i.e. the last record
    /// is a `Leaf` pointing away from a still-preserved branch.
    fn branch_summary_target(&self) -> Option<(NodeRef, NodeRef)> {
        let tree = self.tree.as_ref()?;
        let parent = tree.leaf.node_ref()?.clone();
        let fold_from_id = undo_target_for_tree(tree).and_then(|p| p.node_ref().cloned())?;
        Some((parent, fold_from_id))
    }

    /// The selected assistant row's first non-empty boundary landing (§4).
    /// `None` if no row is highlighted, it's not an assistant node, or no
    /// boundary qualifies (empty filtered prefixes are not offered).
    fn boundary_outcome(&self) -> Option<TreeSelectorOutcome> {
        let tree = self.tree.as_ref()?;
        let row = self.picker.selected_item()?;
        let TreeRow::Node {
            nref,
            flavor: Flavor::Assistant { .. },
            parent,
            ..
        } = row
        else {
            return None;
        };
        let node = tree.nodes.get(nref).and_then(|n| match n {
            TreeNode::Message(m) => Some(m),
            _ => None,
        })?;
        let blocks = Self::boundary_landings(node).into_iter().next()?;
        Some(TreeSelectorOutcome::RewindBoundary {
            parent: parent.clone(),
            blocks,
        })
    }

    /// §4 landing rule, matching on the load-time `Flavor` (never re-derived).
    /// Passive rows (summary / branch marker) are non-selectable.
    fn landing(&self, row: &TreeRow) -> Option<TreeSelectorOutcome> {
        let TreeRow::Node { nref, flavor, .. } = row else {
            return None;
        };
        let tree = self.tree.as_ref()?;
        let node = tree.nodes.get(nref)?;
        match flavor {
            Flavor::UserPrompt => {
                let text = user_prompt_text(node)?;
                Some(TreeSelectorOutcome::RewindBefore { prompt_text: text })
            }
            Flavor::ToolResultCarrier => Some(TreeSelectorOutcome::RewindOn),
            Flavor::Assistant { .. } => Some(TreeSelectorOutcome::RewindOn),
            Flavor::Hidden => None,
        }
    }

    /// Block-boundary landing (§4): from an assistant node's content, produce a
    /// derived sibling with the filtered prefix `[0..k)`. Returns candidates
    /// for each boundary `k` whose filtered prefix is non-empty (§A.6).
    pub fn boundary_landings(node: &MessageNode) -> Vec<Vec<ContentBlock>> {
        let blocks = rehydrate(node);
        let mut out = Vec::new();
        for k in 1..=blocks.len() {
            if let FinalizedPartial::Node(prefix) =
                FinalizedPartial::from_completed_blocks(&blocks[..k])
            {
                out.push(prefix);
            }
        }
        out
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for TreeSelector {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

/// Rehydrate a node's inline `RawValue` content to `ContentBlock`s (§A.2).
fn rehydrate(node: &MessageNode) -> Vec<ContentBlock> {
    node.content
        .iter()
        .filter_map(|raw| serde_json::from_str::<ContentBlock>(raw.get()).ok())
        .collect()
}

/// The user-typed text for a user-prompt node — `display_text` if present,
/// else the first `Text` block. `Role::User` here is a rehydrate helper, not
/// the flavor match (flavor is the selector's source of truth, §4).
fn user_prompt_text(node: &TreeNode) -> Option<String> {
    let TreeNode::Message(m) = node else {
        return None;
    };
    let blocks = rehydrate(m);
    Message {
        role: Role::User,
        content: blocks,
        display_text: None,
    }
    .user_text()
    .map(str::to_owned)
}

/// Build the rendered row list: active path top-to-bottom (tip first in the
/// list), with branch markers and summary passive rows. Hidden nodes are
/// skipped (§9 — invisible in chat and selector).
fn build_rows(tree: &SessionTree) -> Vec<TreeRow> {
    let mut children_by_parent: HashMap<Option<NodeRef>, Vec<NodeRef>> = HashMap::new();
    for (nref, node) in &tree.nodes {
        if node_flavor_lookup(tree, nref) == Some(Flavor::Hidden) {
            continue;
        }
        children_by_parent
            .entry(node.parent_id())
            .or_default()
            .push(nref.clone());
    }

    let mut path: Vec<NodeRef> = Vec::new();
    let mut cur = tree.leaf.node_ref().cloned();
    while let Some(nref) = cur {
        if path.contains(&nref) {
            break;
        }
        let flavor = match node_flavor_lookup(tree, &nref) {
            Some(f) => f,
            None => break,
        };
        if flavor == Flavor::Hidden {
            break;
        }
        path.push(nref.clone());
        cur = tree.nodes.get(&nref).and_then(TreeNode::parent_id);
    }
    path.reverse();

    let mut rows: Vec<TreeRow> = Vec::new();
    for nref in &path {
        let Some(node) = tree.nodes.get(nref) else {
            continue;
        };
        let parent = node.parent_id();
        let depth = fork_depth(tree, &children_by_parent, parent.as_ref());
        let siblings: usize = children_by_parent
            .get(&parent)
            .map(|v| v.iter().filter(|s| *s != nref && !path.contains(s)).count())
            .unwrap_or(0);
        if siblings > 0 {
            rows.push(TreeRow::Branch { count: siblings });
        }

        match node {
            TreeNode::Summary(s) => {
                rows.push(TreeRow::Summary {
                    preview: format!(
                        "{}{} {}",
                        indent(depth),
                        GLYPH_SUMMARY,
                        truncate(&s.narrative, PREVIEW_MAX_LEN)
                    ),
                });
            }
            TreeNode::Message(m) => {
                let flavor = node_flavor_lookup(tree, nref).unwrap_or(Flavor::Hidden);
                rows.push(TreeRow::Node {
                    nref: nref.clone(),
                    flavor,
                    preview: node_preview(m, flavor, depth),
                    detail: node_detail(m),
                    parent,
                });
            }
        }
    }
    rows
}

fn node_flavor_lookup(tree: &SessionTree, nref: &NodeRef) -> Option<Flavor> {
    tree.flavors.get(nref).copied()
}

/// Indentation depth = number of fork points on the path from root to `parent`.
/// A fork point is a node with >1 non-hidden children. Linear conversations
/// have no fork points, so every row stays flat (no staircase, §11).
fn fork_depth(
    tree: &SessionTree,
    children: &HashMap<Option<NodeRef>, Vec<NodeRef>>,
    parent: Option<&NodeRef>,
) -> usize {
    let mut depth = 0;
    let mut cur = parent.cloned();
    let mut guard = std::collections::HashSet::new();
    while let Some(nref) = cur {
        if !guard.insert(nref.clone()) {
            break;
        }
        let is_fork = children
            .get(&Some(nref.clone()))
            .is_some_and(|v| v.len() > 1);
        if is_fork {
            depth += 1;
        }
        cur = tree.nodes.get(&nref).and_then(TreeNode::parent_id).clone();
    }
    depth
}

fn node_preview(node: &MessageNode, flavor: Flavor, depth: usize) -> String {
    let glyph = match flavor {
        Flavor::UserPrompt => GLYPH_USER,
        Flavor::ToolResultCarrier => GLYPH_TOOL,
        Flavor::Assistant { .. } => GLYPH_ASSISTANT,
        Flavor::Hidden => GLYPH_TOOL,
    };
    let body = match flavor {
        Flavor::UserPrompt => user_text(node).unwrap_or_default(),
        Flavor::ToolResultCarrier => tool_result_summary(node),
        Flavor::Assistant { .. } => assistant_text(node),
        Flavor::Hidden => String::new(),
    };
    format!(
        "{}{} {}",
        indent(depth),
        glyph,
        truncate(&body, PREVIEW_MAX_LEN)
    )
}

fn node_detail(node: &MessageNode) -> String {
    let body = match node.role {
        Role::User => user_text(node).unwrap_or_default(),
        Role::Assistant => assistant_text(node),
    };
    truncate(&body, DETAIL_MAX_LEN)
}

fn user_text(node: &MessageNode) -> Option<String> {
    let blocks = rehydrate(node);
    Message {
        role: Role::User,
        content: blocks,
        display_text: None,
    }
    .user_text()
    .map(str::to_owned)
}

fn assistant_text(node: &MessageNode) -> String {
    let mut out = String::new();
    for (i, block) in rehydrate(node).iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Thinking { thinking, .. } => {
                out.push_str("(reasoning) ");
                out.push_str(thinking);
            }
            ContentBlock::ToolUse { name, .. } => {
                out.push_str("(tool: ");
                out.push_str(name);
                out.push(')');
            }
            ContentBlock::ToolResult { content, .. } => {
                out.push_str("(result: ");
                out.push_str(content);
                out.push(')');
            }
            ContentBlock::Image { .. } => out.push_str("[image]"),
            ContentBlock::RedactedThinking { .. } => out.push_str("(redacted reasoning)"),
        }
    }
    out
}

fn tool_result_summary(node: &MessageNode) -> String {
    let mut names = Vec::new();
    for block in rehydrate(node) {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
            names.push(tool_use_id);
        }
    }
    if names.is_empty() {
        return "tool result".into();
    }
    format!("result for {}", names.join(", "))
}

fn truncate(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or("");
    if first_line.chars().count() <= max {
        return first_line.to_owned();
    }
    let target = max.saturating_sub(1);
    let mut out: String = first_line.chars().take(target).collect();
    out.push('\u{2026}');
    out
}

// Silence unused import lint: `load_folder` is re-exported for the app layer
// convenience; the selector itself builds from a `LoadedSession` the caller
// produces via it.
#[allow(unused_imports)]
use load_folder as _load_folder;

/// §4 landing-before rule: find the user-prompt node whose text matches
/// `prompt_text`, then return `Position::At(parent)` (Root if it's a top-level
/// node → next push is a new root).
pub(crate) fn landing_target_before(tree: &SessionTree, prompt_text: &str) -> TreePosition {
    for (nref, node) in &tree.nodes {
        let flavor = tree.flavors.get(nref).copied();
        if flavor != Some(Flavor::UserPrompt) {
            continue;
        }
        if user_prompt_text(node).as_deref() == Some(prompt_text) {
            return match node.parent_id() {
                Some(p) => TreePosition::At(p),
                None => TreePosition::Root,
            };
        }
    }
    TreePosition::Root
}

/// The last UserPrompt node on the active branch (tip → root walk). Returns
/// its text for the fast-path rewind (§11: Esc+Enter lands before the last
/// user message). `None` if there are no user prompts on the active branch.
pub(crate) fn last_user_prompt_text(tree: &SessionTree) -> Option<String> {
    let mut cur = tree.leaf.node_ref().cloned();
    while let Some(nref) = cur {
        if tree.flavors.get(&nref) == Some(&Flavor::UserPrompt) {
            let node = tree.nodes.get(&nref)?;
            return user_prompt_text(node);
        }
        cur = tree.nodes.get(&nref).and_then(TreeNode::parent_id);
    }
    None
}

/// §4 undo availability: returns the pre-rewind position while the most recent
/// order record is a `Leaf`; `None` otherwise (nothing to undo).
pub(crate) fn undo_target_for_tree(tree: &SessionTree) -> Option<TreePosition> {
    let last = tree.order.last()?;
    if !matches!(last, OrderedRecord::Leaf { .. }) {
        return None;
    }
    Some(maki_storage::session_log::position_before_last_leaf(
        &tree.order,
        &tree.nodes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_storage::tree::MessageId;
    use serde_json::value::RawValue;
    use test_case::test_case;

    fn raw(json: &str) -> Box<RawValue> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn passive_rows_are_non_selectable() {
        let row = TreeRow::Summary {
            preview: "x".into(),
        };
        assert!(!row.selectable());
        let row = TreeRow::Branch { count: 2 };
        assert!(!row.selectable());
    }

    #[test]
    fn boundary_landings_drop_tool_use_and_keep_signed_thinking() {
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: None,
            role: Role::Assistant,
            content: vec![
                raw(r#"{"type":"thinking","thinking":"reason","signature":"sig"}"#),
                raw(r#"{"type":"text","text":"answer"}"#),
                raw(r#"{"type":"tool_use","id":"t1","name":"bash","input":{}}"#),
            ],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let landings = TreeSelector::boundary_landings(&node);
        assert!(!landings.is_empty());
        for prefix in &landings {
            assert!(!prefix.is_empty());
            assert!(
                prefix
                    .iter()
                    .all(|b| !matches!(b, ContentBlock::ToolUse { .. })),
                "tool_use must be stripped"
            );
        }
    }

    #[test]
    fn boundary_landings_empty_when_only_unsigned_thinking() {
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: None,
            role: Role::Assistant,
            content: vec![raw(r#"{"type":"thinking","thinking":"reason"}"#)],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        assert!(TreeSelector::boundary_landings(&node).is_empty());
    }

    #[test_case(Flavor::UserPrompt ; "user_prompt")]
    #[test_case(Flavor::ToolResultCarrier ; "tool_carrier")]
    #[test_case(Flavor::Assistant { interrupted: false } ; "assistant")]
    fn selectable_flavors_select(flavor: Flavor) {
        let row = TreeRow::Node {
            nref: NodeRef::Msg(MessageId::new()),
            flavor,
            preview: "x".into(),
            detail: String::new(),
            parent: None,
        };
        assert!(row.selectable());
    }

    #[test]
    fn hidden_flavor_non_selectable() {
        let row = TreeRow::Node {
            nref: NodeRef::Msg(MessageId::new()),
            flavor: Flavor::Hidden,
            preview: "x".into(),
            detail: String::new(),
            parent: None,
        };
        assert!(!row.selectable());
    }

    #[test]
    fn build_rows_skips_hidden_nodes() {
        let mut nodes = HashMap::new();
        let visible = user_node("real", None);
        let visible_id = visible.id.clone();
        let mut hidden = user_node("secret", None);
        hidden.hidden = true;
        let hidden_id = hidden.id.clone();
        let vis_ref = NodeRef::Msg(visible_id);
        let hid_ref = NodeRef::Msg(hidden_id);
        nodes.insert(vis_ref.clone(), TreeNode::Message(visible.clone()));
        nodes.insert(hid_ref.clone(), TreeNode::Message(hidden));
        let mut flavors = HashMap::new();
        flavors.insert(vis_ref.clone(), Flavor::UserPrompt);
        flavors.insert(hid_ref.clone(), Flavor::Hidden);
        let tree = SessionTree {
            nodes,
            order: vec![OrderedRecord::Node(vis_ref.clone())],
            leaf: TreePosition::At(vis_ref.clone()),
            labels: Vec::new(),
            sub_msgs: Vec::new(),
            flavors,
        };
        let rows = build_rows(&tree);
        assert_eq!(rows.len(), 1);
        assert!(
            matches!(rows[0], TreeRow::Node { ref nref, .. } if *nref == vis_ref),
            "hidden node must not be listed"
        );
    }

    fn node_indent(row: &TreeRow) -> usize {
        let label = match row {
            TreeRow::Node { preview, .. } | TreeRow::Summary { preview } => preview,
            TreeRow::Branch { .. } => return 0,
        };
        label.chars().take_while(|c| *c == ' ').count() / 2
    }

    fn chain_tree() -> SessionTree {
        let u = user_node("hi", None);
        let uid = NodeRef::Msg(u.id.clone());
        let a = assistant_node(&uid);
        let aid = NodeRef::Msg(a.id.clone());
        let u2 = user_node("two", Some(aid.clone()));
        let u2id = NodeRef::Msg(u2.id.clone());
        let mut nodes = HashMap::new();
        nodes.insert(uid.clone(), TreeNode::Message(u));
        nodes.insert(aid.clone(), TreeNode::Message(a));
        nodes.insert(u2id.clone(), TreeNode::Message(u2));
        let mut flavors = HashMap::new();
        flavors.insert(uid.clone(), Flavor::UserPrompt);
        flavors.insert(aid.clone(), Flavor::Assistant { interrupted: false });
        flavors.insert(u2id.clone(), Flavor::UserPrompt);
        SessionTree {
            nodes,
            order: vec![OrderedRecord::Node(u2id.clone())],
            leaf: TreePosition::At(u2id),
            labels: Vec::new(),
            sub_msgs: Vec::new(),
            flavors,
        }
    }

    #[test]
    fn linear_chain_is_flat_no_staircase() {
        let tree = chain_tree();
        let rows = build_rows(&tree);
        assert_eq!(rows.len(), 3, "all three nodes listed");
        for row in &rows {
            assert_eq!(node_indent(row), 0, "no indentation on a linear branch");
        }
    }

    #[test]
    fn fork_indents_only_below_fork_point() {
        let mut tree = chain_tree();
        let aid = NodeRef::Msg(
            tree.nodes
                .iter()
                .find(|(_, n)| {
                    matches!(
                        n,
                        TreeNode::Message(MessageNode {
                            role: Role::Assistant,
                            ..
                        })
                    )
                })
                .map(|(_, n)| {
                    let TreeNode::Message(m) = n else {
                        unreachable!()
                    };
                    m.id.clone()
                })
                .unwrap(),
        );
        let sibling = user_node("alt", Some(aid.clone()));
        let sid = NodeRef::Msg(sibling.id.clone());
        tree.nodes.insert(sid.clone(), TreeNode::Message(sibling));
        tree.flavors.insert(sid.clone(), Flavor::UserPrompt);
        tree.leaf = TreePosition::At(sid);
        let rows = build_rows(&tree);
        let before_fork_indent = node_indent(&rows[0]);
        let active_sibling_indent = node_indent(rows.last().unwrap());
        assert_eq!(before_fork_indent, 0, "node above the fork stays flat");
        assert_eq!(active_sibling_indent, 1, "sibling under the fork indents");
    }

    fn user_node(_text: &str, parent: Option<NodeRef>) -> MessageNode {
        MessageNode {
            id: MessageId::new(),
            parent_id: parent,
            role: Role::User,
            content: vec![raw(r#"{"type":"text","text":"hi"}"#)],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        }
    }

    fn assistant_node(parent: &NodeRef) -> MessageNode {
        MessageNode {
            id: MessageId::new(),
            parent_id: Some(parent.clone()),
            role: Role::Assistant,
            content: vec![raw(r#"{"type":"text","text":"answer"}"#)],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        }
    }
}
