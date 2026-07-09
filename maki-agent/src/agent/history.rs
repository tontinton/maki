use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;

use arc_swap::ArcSwap;
use maki_providers::{ContentBlock, Message, Role};
use maki_storage::tree::{
    Flavor, MessageId, MessageNode, NodeRef, OrderedRecord, Position, SessionTree, SummaryId,
    SummaryKind, SummaryRecord, TreeNode,
};
use serde_json::value::RawValue;
use tracing::warn;

/// Synthetic result text for a `tool_use` whose result never arrived (crash
/// mid-tool, rewind onto an assistant turn, walk-boundary truncation). Part of
/// the fold determinism invariant (§2): never changed casually.
pub const TOOL_RESULT_UNAVAILABLE: &str = "[Tool result not available]";

/// Cancelled-tool result text (mid-tool cancel path, §8).
const CANCELLED_BY_USER: &str = "[Cancelled by user]";

const CHARS_PER_TOKEN: usize = 4;

/// The only thing the request layer accepts (§A.0(4)). Constructed solely by
/// `fold` (assembly + repair + lowering), so an unrepaired history cannot reach
/// a provider by construction. Cache hits and UI hand-offs are an Arc bump.
pub struct ValidContext {
    messages: Arc<[Message]>,
}

impl ValidContext {
    fn new(messages: Vec<Message>) -> Self {
        Self {
            messages: Arc::from(messages),
        }
    }

    /// Build a `ValidContext` from a linear message vec by folding a linear
    /// tree (§A.4). The sole public constructor: it runs `fold` (assembly +
    /// repair), so an unrepaired history can never be minted (§A.0(4)).
    pub fn fold_linear(messages: Vec<Message>) -> Self {
        let tree = linear_tree(&messages);
        fold(&tree)
    }
}

impl Deref for ValidContext {
    type Target = [Message];

    fn deref(&self) -> &Self::Target {
        &self.messages
    }
}

impl Clone for ValidContext {
    fn clone(&self) -> Self {
        Self {
            messages: Arc::clone(&self.messages),
        }
    }
}

/// UI/plugin snapshot container (§A.7). `ArcSwapAny<Arc<[Message]>>` does not
/// compile (`RefCnt` is `Sized`-only), which is one reason the newtype exists.
pub type SharedContext = Arc<ArcSwap<ValidContext>>;

/// Tree-aware wrapper over `SessionTree` (§10). Holds the pure-fold cache keyed
/// by a generation counter bumped on every structural mutation; streaming
/// deltas mutate only an in-memory accumulator and leave `generation` (and thus the
/// cache) unchanged mid-stream (§A.7).
pub struct History {
    tree: SessionTree,
    generation: u64,
    cache: Option<(u64, Arc<ValidContext>)>,
    mirror: Option<SharedContext>,
}

impl History {
    pub fn new(messages: Vec<Message>) -> Self {
        Self::from_messages(messages)
    }

    pub fn restored(messages: Vec<Message>) -> Self {
        // `fold` runs the repair pass unconditionally, so a dirty persisted log
        // (orphaned results, dangling tool calls) can never reach a provider
        // (§A.4.1). No separate sanitize step is needed.
        Self::from_messages(messages)
    }

    /// C2 behaves linearly: each node parents onto the previous one. Builds a
    /// linear `SessionTree` from the message vec and folds it once for the cache.
    fn from_messages(messages: Vec<Message>) -> Self {
        let tree = linear_tree(&messages);
        let ctx = Arc::new(fold(&tree));
        Self {
            tree,
            generation: 0,
            cache: Some((0, Arc::clone(&ctx))),
            mirror: None,
        }
        .with_published(ctx)
    }

    pub fn with_mirror(mut self, mirror: SharedContext) -> Self {
        self.mirror = Some(mirror);
        self.publish();
        self
    }

    fn with_published(mut self, ctx: Arc<ValidContext>) -> Self {
        self.cache = Some((0, Arc::clone(&ctx)));
        if let Some(mirror) = &self.mirror {
            mirror.store(Arc::clone(&ctx));
        }
        self
    }

    /// Fold a `SessionTree`'s active branch into a `ValidContext` (§A.4). The
    /// only consumer outside `History` is the C3 UI, which rebuilds the chat
    /// scrollback from the active branch after a rewind commit (`Rewind`
    /// appends a `Leaf`; `fold` follows it). Infallible — cycles were rejected
    /// at open (§A.5).
    pub fn fold_tree(tree: &SessionTree) -> ValidContext {
        fold(tree)
    }

    /// The cached `active_branch(leaf)` (§10/§A.7). Returns the cached
    /// value (Arc bump) when `generation` is current, else re-folds and stores.
    pub fn active_branch(&mut self) -> ValidContext {
        if let Some((g, ctx)) = &self.cache
            && *g == self.generation
        {
            return (**ctx).clone();
        }
        let ctx = Arc::new(fold(&self.tree));
        self.cache = Some((self.generation, Arc::clone(&ctx)));
        if let Some(mirror) = &self.mirror {
            mirror.store(Arc::clone(&ctx));
        }
        (*ctx).clone()
    }

    /// Fold the active branch into the on-path `MessageNode`s (§A.4 walk),
    /// preserving node-level flags (`interrupted`, `run_id`, `hidden`) that
    /// `active_branch()` drops when lowering to `Message`. Used by the
    /// headless persistence path to durably append interrupted nodes.
    pub fn active_branch_nodes(&self) -> Vec<MessageNode> {
        fold_nodes(&self.tree)
    }

    /// Enqueue a message node: parents onto the current leaf position and
    /// becomes the new leaf (§4). A `Message` whose `display_text` is the
    /// empty-string sentinel is a hidden chrome turn (§9) → stored `hidden`.
    /// Bumps `generation`, invalidating the cache. Returns the new node id.
    pub fn push(&mut self, msg: Message) -> MessageId {
        let id = append_message_node(&mut self.tree, msg);
        self.bump();
        id
    }

    #[cfg(test)]
    pub fn test_rewind_leaf_to(&mut self, nref: NodeRef) {
        self.tree.leaf = Position::At(nref);
        self.bump();
    }

    #[cfg(test)]
    pub fn test_leaf_ref(&self) -> Option<NodeRef> {
        self.tree.leaf.node_ref().cloned()
    }

    #[cfg(test)]
    pub fn test_find_msg_by_content(&self, needle: &str) -> Option<NodeRef> {
        self.tree.nodes.iter().find_map(|(nref, node)| match node {
            TreeNode::Message(m) => m
                .content
                .iter()
                .any(|r| r.get().contains(needle))
                .then(|| nref.clone()),
            _ => None,
        })
    }

    /// Finalize a mid-stream interrupt (§8): append the completed blocks as
    /// an assistant `MessageNode` with `interrupted: true`, so the user can
    /// re-guide from the reasoning. Callers MUST pass blocks already filtered
    /// through [`finalize::FinalizedPartial`] (no ToolUse, no unsigned
    /// thinking). Sets the node's `run_id` so the barrier can flush it.
    pub fn push_interrupted(
        &mut self,
        blocks: Vec<ContentBlock>,
        run_id: u64,
    ) -> Option<MessageId> {
        if blocks.is_empty() {
            return None;
        }
        let msg = Message {
            role: Role::Assistant,
            content: blocks,
            ..Default::default()
        };
        let parent = self.tree.leaf.node_ref().cloned();
        let content: Vec<Box<RawValue>> = msg.content.iter().filter_map(to_raw_value).collect();
        let hidden = msg.display_text.as_deref() == Some("");
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: parent,
            role: msg.role,
            content,
            timestamp: maki_storage::now_epoch(),
            run_id: Some(run_id),
            interrupted: true,
            hidden,
        };
        let id = node.id.clone();
        let nref = NodeRef::Msg(id.clone());
        self.tree
            .flavors
            .insert(nref.clone(), SessionTree::node_flavor(&node));
        self.tree
            .nodes
            .insert(nref.clone(), TreeNode::Message(node));
        self.tree.order.push(OrderedRecord::Node(nref.clone()));
        self.tree.leaf = Position::At(nref);
        self.bump();
        Some(id)
    }

    /// Push a cancelled-tool result group as one hidden user node (§8). The
    /// interrupt-finalize wiring is C6; this keeps the cancelled-results path
    /// working as an ordinary hidden node.
    pub fn push_cancelled_results(&mut self, tool_use_ids: &[String]) {
        if tool_use_ids.is_empty() {
            return;
        }
        let content: Vec<ContentBlock> = tool_use_ids
            .iter()
            .map(|id| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: CANCELLED_BY_USER.into(),
                is_error: true,
            })
            .collect();
        self.push(Message {
            role: Role::User,
            content,
            display_text: Some(String::new()),
        });
    }

    /// On cancel, close dangling `tool_use`s as persisted hidden nodes
    /// carrying `[Cancelled by user]` results (§8). Walks the raw tree path
    /// (not the repaired fold) so that `repair`'s transient `[Tool result
    /// not available]` results don't mask cancel-time persist. Idempotent.
    pub fn close_cancelled_tool_calls(&mut self) {
        let dangling = dangling_tool_use_ids_in_tree(&self.tree);
        if dangling.is_empty() {
            return;
        }
        self.push_cancelled_results(&dangling);
    }

    /// Whether the current leaf is an `interrupted` assistant node (§8
    /// request-time guard). When true, the trailing turn is excluded from
    /// the next provider request so the context doesn't end in an assistant
    /// message some providers reject for continuation.
    pub fn leaf_is_interrupted(&self) -> bool {
        self.tree
            .leaf
            .node_ref()
            .and_then(|nr| self.tree.nodes.get(nr))
            .is_some_and(|n| matches!(n, TreeNode::Message(m) if m.interrupted))
    }

    /// Number of messages in the folded active branch.
    pub fn len(&mut self) -> usize {
        self.active_branch().len()
    }

    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    pub fn into_vec(self) -> Vec<Message> {
        fold(&self.tree).messages.to_vec()
    }

    pub fn has_recent_tool_results(&mut self, depth: usize) -> bool {
        let msgs = self.active_branch();
        let start = msgs.len().saturating_sub(depth);
        msgs[start..].iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
    }

    /// Select a compaction `CutPoint` over the active branch (§6), or `None`
    /// when nothing eligible folds or the leaf is already a compaction.
    pub fn compaction_cut(&self, keep_budget: u32) -> Option<CutPoint> {
        if matches!(self.tree.leaf, Position::Root) {
            return None;
        }
        if leaf_is_compaction(&self.tree) {
            return None;
        }
        CutPoint::select(&self.tree, self.tree.leaf.clone(), keep_budget)
    }

    /// The prefix messages to summarize: the active branch up to (but
    /// excluding) the cut node (the region `[root .. cut)` a compaction
    /// replaces, §6). Walks the tree directly so node identity is preserved.
    pub fn compaction_prefix(&self, cut: &CutPoint) -> Vec<Message> {
        let cut_id = cut.message_id();
        let mut path: Vec<&TreeNode> = Vec::new();
        let mut cur = self.tree.leaf.node_ref().cloned();
        while let Some(nref) = cur {
            let Some(node) = self.tree.nodes.get(&nref) else {
                break;
            };
            if let NodeRef::Msg(m) = &nref
                && m == cut_id
            {
                break;
            }
            path.push(node);
            cur = node.parent_id();
        }
        path.reverse();
        let mut out: Vec<Message> = path
            .into_iter()
            .map(|node| match node {
                TreeNode::Message(m) => {
                    let flavor = self
                        .tree
                        .flavors
                        .get(&NodeRef::Msg(m.id.clone()))
                        .copied()
                        .unwrap_or(Flavor::UserPrompt);
                    let display_text = if matches!(flavor, Flavor::Hidden) {
                        Some(String::new())
                    } else {
                        None
                    };
                    Message {
                        role: m.role,
                        content: rehydrate(m),
                        display_text,
                    }
                }
                TreeNode::Summary(s) => hidden_user_msg(s.narrative.clone()),
            })
            .collect();
        repair(&mut out);
        out
    }

    /// The abandoned branch messages for branch-summary (§6): walk from
    /// `fold_from_id` toward root until (but excluding) `parent`, collecting
    /// the abandoned branch. `parent` is the landing node the summary will be
    /// parented at; `fold_from_id` is the abandoned tip.
    pub fn abandoned_branch_prefix(
        &self,
        parent: &NodeRef,
        fold_from_id: &NodeRef,
    ) -> Vec<Message> {
        let mut path: Vec<&TreeNode> = Vec::new();
        let mut cur = Some(fold_from_id.clone());
        while let Some(nref) = cur {
            if &nref == parent {
                break;
            }
            let Some(node) = self.tree.nodes.get(&nref) else {
                break;
            };
            path.push(node);
            cur = node.parent_id();
        }
        path.reverse();
        let mut out: Vec<Message> = path
            .into_iter()
            .map(|node| match node {
                TreeNode::Message(m) => {
                    let flavor = self
                        .tree
                        .flavors
                        .get(&NodeRef::Msg(m.id.clone()))
                        .copied()
                        .unwrap_or(Flavor::UserPrompt);
                    let display_text = if matches!(flavor, Flavor::Hidden) {
                        Some(String::new())
                    } else {
                        None
                    };
                    Message {
                        role: m.role,
                        content: rehydrate(m),
                        display_text,
                    }
                }
                TreeNode::Summary(s) => hidden_user_msg(s.narrative.clone()),
            })
            .collect();
        repair(&mut out);
        out
    }

    /// at the current leaf and advance the leaf onto it (§6). If the
    /// pre-compaction tip is an assistant turn, push the hidden continue-prompt
    /// as a normal node so the branch ends in a user turn.
    pub fn append_compaction(
        &mut self,
        cut: CutPoint,
        narrative: String,
        continue_prompt: &str,
        read_files: Vec<String>,
        modified_files: Vec<String>,
    ) {
        let tip_is_assistant = self
            .tree
            .leaf
            .node_ref()
            .and_then(|nref| self.tree.flavors.get(nref))
            .is_some_and(|f| matches!(f, Flavor::Assistant { .. }));
        let record = SummaryRecord {
            id: SummaryId::new(),
            parent_id: self
                .tree
                .leaf
                .node_ref()
                .cloned()
                .unwrap_or_else(|| NodeRef::Msg(MessageId::new())),
            narrative,
            kind: SummaryKind::Compaction {
                fold_to_id: cut.message_id().clone(),
            },
            read_files,
            modified_files,
        };
        let nref = NodeRef::Sum(record.id.clone());
        self.tree
            .nodes
            .insert(nref.clone(), TreeNode::Summary(record));
        self.tree.order.push(OrderedRecord::Node(nref.clone()));
        self.tree.leaf = Position::At(nref);
        if tip_is_assistant {
            self.push(Message::synthetic(continue_prompt.into()));
        }
        self.bump();
    }

    /// Freeze the narrative into a `SummaryRecord` (branch kind) parented at
    /// `parent` and advance the leaf onto it (§6). The branch summary folds
    /// **in place** on the active path: the abandoned branch itself is never
    /// walked, `fold_from_id` records its tip as provenance only. If the tip
    /// (parent) is an assistant turn, push the hidden continue-prompt as a
    /// normal node so the branch ends in a user turn.
    pub fn append_branch_summary(
        &mut self,
        parent: NodeRef,
        fold_from_id: NodeRef,
        narrative: String,
        continue_prompt: &str,
    ) {
        let tip_is_assistant = self
            .tree
            .flavors
            .get(&parent)
            .is_some_and(|f| matches!(f, Flavor::Assistant { .. }));
        let record = SummaryRecord {
            id: SummaryId::new(),
            parent_id: parent.clone(),
            narrative,
            kind: SummaryKind::Branch { fold_from_id },
            read_files: Vec::new(),
            modified_files: Vec::new(),
        };
        let nref = NodeRef::Sum(record.id.clone());
        self.tree
            .nodes
            .insert(nref.clone(), TreeNode::Summary(record));
        self.tree.order.push(OrderedRecord::Node(nref.clone()));
        self.tree.leaf = Position::At(nref);
        if tip_is_assistant {
            self.push(Message::synthetic(continue_prompt.into()));
        }
        self.bump();
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        let ctx = Arc::new(fold(&self.tree));
        self.cache = Some((self.generation, Arc::clone(&ctx)));
        if let Some(mirror) = &self.mirror {
            mirror.store(ctx);
        }
    }

    fn publish(&mut self) {
        let ctx = Arc::new(fold(&self.tree));
        self.cache = Some((self.generation, Arc::clone(&ctx)));
        if let Some(mirror) = &self.mirror {
            mirror.store(ctx);
        }
    }
}

/// Append a `Message` to the tree as a `MessageNode`, parenting onto the
/// current leaf and advancing the leaf onto the new node (§4). Returns the id.
fn append_message_node(tree: &mut SessionTree, msg: Message) -> MessageId {
    let parent = tree.leaf.node_ref().cloned();
    let content = msg
        .content
        .iter()
        .filter_map(to_raw_value)
        .collect::<Vec<_>>();
    let hidden = msg.display_text.as_deref() == Some("");
    let node = MessageNode {
        id: MessageId::new(),
        parent_id: parent,
        role: msg.role,
        content,
        timestamp: maki_storage::now_epoch(),
        run_id: None,
        interrupted: false,
        hidden,
    };
    let id = node.id.clone();
    let nref = NodeRef::Msg(id.clone());
    tree.flavors
        .insert(nref.clone(), SessionTree::node_flavor(&node));
    tree.nodes.insert(nref.clone(), TreeNode::Message(node));
    tree.order.push(OrderedRecord::Node(nref.clone()));
    tree.leaf = Position::At(nref);
    id
}

/// `fold(active_branch(leaf))` — pure, infallible (cycles were rejected at
/// open, §A.5). Walks leaf→root, hoists the newest compaction narrative to the
/// FRONT, keeps `[fold_to .. leaf]`, runs the repair pass, and constructs the
/// sole `ValidContext`.
fn fold(tree: &SessionTree) -> ValidContext {
    let mut path: Vec<&TreeNode> = Vec::new();
    let mut seen: HashSet<NodeRef> = HashSet::new();
    let mut cur = tree.leaf.node_ref().cloned();
    let mut narrative: Option<String> = None;
    let mut stop_after: Option<MessageId> = None;
    let mut hit_stop = false;

    while let Some(nref) = cur {
        if !seen.insert(nref.clone()) {
            warn!(node = %nref, "cycle survived open-check in fold");
            break;
        }
        let Some(node) = tree.nodes.get(&nref) else {
            warn!(node = %nref, "broken parent chain in fold; serving reachable suffix");
            break;
        };
        if let TreeNode::Summary(summary) = node {
            // Newest compaction wins (encountered first walking leaf→root);
            // lossless because recompaction subsumes the prior narrative (§6).
            if narrative.is_none() {
                narrative = Some(summary.narrative.clone());
                if let SummaryKind::Compaction { fold_to_id } = &summary.kind {
                    stop_after = Some(fold_to_id.clone());
                }
            }
            cur = node.parent_id();
            continue;
        }
        path.push(node);
        if let Some(stop) = &stop_after
            && let NodeRef::Msg(m) = &nref
            && m == stop
        {
            hit_stop = true;
            break;
        }
        cur = node.parent_id();
    }

    if stop_after.is_some() && !hit_stop {
        warn!("compaction cut not on path; serving full walk");
    }

    path.reverse();

    let mut out: Vec<Message> = Vec::new();
    if let Some(n) = narrative {
        out.push(hidden_user_msg(n));
    }
    for node in path {
        match node {
            TreeNode::Message(m) => {
                let flavor = tree
                    .flavors
                    .get(&NodeRef::Msg(m.id.clone()))
                    .copied()
                    .unwrap_or(Flavor::UserPrompt);
                let display_text = if matches!(flavor, Flavor::Hidden) {
                    Some(String::new())
                } else {
                    None
                };
                let content = rehydrate(m);
                out.push(Message {
                    role: m.role,
                    content,
                    display_text,
                });
            }
            TreeNode::Summary(s) => out.push(hidden_user_msg(s.narrative.clone())),
        }
    }

    repair(&mut out);
    ValidContext::new(out)
}

/// Like `fold` but returns the on-path `MessageNode`s with their node-level
/// flags intact (`interrupted`, `run_id`, `hidden`). The headless persistence
/// path uses this to durably append interrupted nodes without losing the flag.
fn fold_nodes(tree: &SessionTree) -> Vec<MessageNode> {
    let mut path: Vec<&TreeNode> = Vec::new();
    let mut seen: HashSet<NodeRef> = HashSet::new();
    let mut cur = tree.leaf.node_ref().cloned();
    let mut stop_after: Option<MessageId> = None;
    let mut hit_stop = false;

    while let Some(nref) = cur {
        if !seen.insert(nref.clone()) {
            break;
        }
        let Some(node) = tree.nodes.get(&nref) else {
            break;
        };
        if let TreeNode::Summary(summary) = node {
            if stop_after.is_none()
                && let SummaryKind::Compaction { fold_to_id } = &summary.kind
            {
                stop_after = Some(fold_to_id.clone());
            }
            cur = node.parent_id();
            continue;
        }
        path.push(node);
        if let Some(stop) = &stop_after
            && let NodeRef::Msg(m) = &nref
            && m == stop
        {
            hit_stop = true;
            break;
        }
        cur = node.parent_id();
    }

    let _ = hit_stop;
    path.reverse();

    let mut out: Vec<MessageNode> = Vec::new();
    for node in path {
        if let TreeNode::Message(m) = node {
            out.push(m.clone());
        }
    }
    out
}

/// Lower a narrative to the provider's hidden-user-message convention (§9):
/// `Message { role: User, content: [Text{narrative}], display_text: Some("") }`.
/// The narrative is emitted verbatim; wrapper prose was frozen at creation.
fn hidden_user_msg(narrative: String) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: narrative }],
        display_text: Some(String::new()),
    }
}

/// Rehydrate a node's inline `RawValue` content to `ContentBlock`s (§A.2).
/// Fallible per block: external corruption is warned and dropped, never
/// unwrapped — repair then removes anything the drop orphaned (§A.4).
fn rehydrate(node: &MessageNode) -> Vec<ContentBlock> {
    node.content
        .iter()
        .filter_map(|raw| {
            serde_json::from_str::<ContentBlock>(raw.get())
                .map_err(|e| warn!(error = %e, "dropping unparseable content block"))
                .ok()
        })
        .collect()
}

fn to_raw_value(block: &ContentBlock) -> Option<Box<RawValue>> {
    serde_json::value::to_raw_value(block)
        .map_err(|e| warn!(error = %e, "failed to serialize content block"))
        .ok()
}

/// Deterministic API-validity pass (§A.4.1). Pure function of the assembled
/// message array; subsumes both `sanitize_restored` and
/// `close_dangling_tool_calls`. Rules, in order:
/// 1. orphaned `tool_result` (no match in immediately preceding assistant
///    message) removed; empty carrier dropped; orphaned tool images dropped.
/// 2. dangling `tool_use` (no following result) closed with a synthetic error.
fn repair(out: &mut Vec<Message>) {
    let len_before = out.len();
    let mut i = 0;
    while i < out.len() {
        if !matches!(out[i].role, Role::User) {
            i += 1;
            continue;
        }
        let valid_ids: Vec<String> = if i > 0 && matches!(out[i - 1].role, Role::Assistant) {
            out[i - 1]
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        } else {
            Vec::new()
        };
        let (mut had_results, mut kept_results) = (false, false);
        out[i].content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                had_results = true;
                let keep = valid_ids.iter().any(|id| id == tool_use_id);
                kept_results |= keep;
                keep
            }
            _ => true,
        });
        if had_results && !kept_results {
            out[i]
                .content
                .retain(|b| !matches!(b, ContentBlock::Image { .. }));
        }
        if out[i].content.is_empty() {
            out.remove(i);
        } else {
            i += 1;
        }
    }

    close_dangling_tool_uses(out);

    if out.len() != len_before {
        warn!(
            before = len_before,
            after = out.len(),
            "repaired folded context"
        );
    }
}

/// Tree-level dangling `tool_use` detection (§8): walks the active path
/// without `repair`, so cancel-time persist sees the true persisted state
/// and isn't masked by `repair`'s transient synthetic results.
fn dangling_tool_use_ids_in_tree(tree: &SessionTree) -> Vec<String> {
    let path = active_path(tree, tree.leaf.clone());
    let mut dangling = Vec::new();
    for i in 0..path.len() {
        let TreeNode::Message(m) = path[i] else {
            continue;
        };
        if !matches!(m.role, Role::Assistant) || m.content.is_empty() {
            continue;
        }
        let use_ids: Vec<String> = m
            .content
            .iter()
            .filter_map(|raw| {
                let block: ContentBlock = serde_json::from_str(raw.get()).ok()?;
                match block {
                    ContentBlock::ToolUse { id, .. } => Some(id),
                    _ => None,
                }
            })
            .collect();
        if use_ids.is_empty() {
            continue;
        }
        let already: HashSet<String> = path
            .get(i + 1)
            .and_then(|n| match n {
                TreeNode::Message(m) if matches!(m.role, Role::User) => Some(m),
                _ => None,
            })
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|raw| {
                        let block: ContentBlock = serde_json::from_str(raw.get()).ok()?;
                        match block {
                            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                            _ => None,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        dangling.extend(use_ids.into_iter().filter(|id| !already.contains(id)));
    }
    dangling
}

/// Rule 2 (§A.4.1): close dangling `tool_use`s with synthetic error results,
/// grouped into one hidden user message after the assistant turn (or merged
/// into an existing following user message carrying results).
fn close_dangling_tool_uses(out: &mut Vec<Message>) {
    let mut inserts: Vec<(usize, Vec<ContentBlock>)> = Vec::new();
    for i in 0..out.len() {
        if !matches!(out[i].role, Role::Assistant) || !out[i].has_tool_calls() {
            continue;
        }
        let use_ids: Vec<String> = out[i].tool_uses().map(|(id, _, _)| id.to_owned()).collect();
        let already: HashSet<&str> = out
            .get(i + 1)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let dangling: Vec<ContentBlock> = use_ids
            .iter()
            .filter(|id| !already.contains(id.as_str()))
            .map(|id| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: TOOL_RESULT_UNAVAILABLE.into(),
                is_error: true,
            })
            .collect();
        if dangling.is_empty() {
            continue;
        }
        if let Some(next) = out.get_mut(i + 1)
            && matches!(next.role, Role::User)
            && next
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            next.content.extend(dangling);
        } else {
            inserts.push((i + 1, dangling));
        }
    }
    while let Some((at, blocks)) = inserts.pop() {
        out.insert(
            at,
            Message {
                role: Role::User,
                content: blocks,
                display_text: Some(String::new()),
            },
        );
    }
}

/// Provider lowering (§9): strip thinking blocks a provider can't replay, then
/// drop any assistant message left empty after the strip. Deterministic per
/// provider; the predicate decides whether thinking survives.
pub fn lower_for_provider(messages: &mut Vec<Message>, can_replay_thinking: bool) {
    if can_replay_thinking {
        return;
    }
    for msg in messages.iter_mut() {
        msg.content.retain(|b| {
            !matches!(
                b,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
    messages.retain(|m| !matches!(m.role, Role::Assistant) || !m.content.is_empty());
}

/// Build a linear `SessionTree` from a message vec (C2 behaves linearly: each
/// node parents onto the previous one, last node is the leaf).
fn linear_tree(messages: &[Message]) -> SessionTree {
    let mut nodes = std::collections::HashMap::new();
    let mut order = Vec::new();
    let mut flavors = std::collections::HashMap::new();
    let mut parent: Option<NodeRef> = None;
    let mut leaf = Position::Root;
    for msg in messages {
        let content = msg
            .content
            .iter()
            .filter_map(to_raw_value)
            .collect::<Vec<_>>();
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: parent.clone(),
            role: msg.role,
            content,
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: msg.display_text.as_deref() == Some(""),
        };
        let nref = NodeRef::Msg(node.id.clone());
        flavors.insert(nref.clone(), SessionTree::node_flavor(&node));
        parent = Some(nref.clone());
        leaf = Position::At(nref.clone());
        order.push(OrderedRecord::Node(nref.clone()));
        nodes.insert(nref, TreeNode::Message(node));
    }
    SessionTree {
        nodes,
        order,
        leaf,
        labels: Vec::new(),
        sub_msgs: Vec::new(),
        flavors,
    }
}

/// The sole constructor of `interrupted: true` node content (§A.0(6), §A.6).
/// Both consumers (mid-stream interrupt, block-boundary landing) go through it,
/// so an unfiltered partial cannot be persisted.
pub mod finalize {
    use maki_providers::ContentBlock;

    pub enum FinalizedPartial {
        Node(Vec<ContentBlock>),
        Discard,
    }

    impl FinalizedPartial {
        /// `completed` must contain only COMPLETED blocks — an in-flight
        /// partial is stream-accumulator state, structurally indistinguishable
        /// from a complete block, so the CALLER owns that distinction.
        pub fn from_completed_blocks(completed: &[ContentBlock]) -> Self {
            let mut kept: Vec<ContentBlock> = Vec::new();
            for block in completed {
                match block {
                    // Drop every ToolUse — none has executed mid-stream, and a
                    // dangling persisted ToolUse is API-invalid (§A.6).
                    ContentBlock::ToolUse { .. } => continue,
                    // Unsigned thinking is rejected on replay; signed thinking
                    // and RedactedThinking survive, including alone.
                    ContentBlock::Thinking {
                        signature: None, ..
                    } => continue,
                    other => kept.push(other.clone()),
                }
            }
            if kept.is_empty() {
                Self::Discard
            } else {
                Self::Node(kept)
            }
        }
    }
}

/// Compaction cut (§6): the only source of a compaction's `fold_to`. Carries
/// the user-prompt-boundary and tip-ward-of-prior-cut proofs with it.
pub struct CutPoint(MessageId);

impl CutPoint {
    pub fn message_id(&self) -> &MessageId {
        &self.0
    }

    /// Walk tip-ward→root-ward accumulating estimated tokens until the
    /// keep-recent budget is reached, then cut at the nearest user-prompt node
    /// root-ward of that point (§6). Rejects cuts root-ward of any prior
    /// compaction's cut on the path, and rejects when the leaf itself is a
    /// compaction or nothing eligible is foldable.
    pub fn select(tree: &SessionTree, leaf: Position, keep_budget: u32) -> Option<Self> {
        let prior_cut = prior_compaction_cut(tree, &leaf);
        let path = active_path(tree, leaf);
        let mut budget: u32 = 0;
        let mut cut_floor: Option<usize> = None;
        for (i, node) in path.iter().enumerate() {
            budget = budget.saturating_add(estimated_tokens(node));
            if budget >= keep_budget {
                cut_floor = Some(i);
                break;
            }
        }
        let floor = cut_floor?;

        // Walk root-ward from the floor to the nearest user-prompt node.
        for node in &path[floor..] {
            if let TreeNode::Message(m) = node {
                let flavor = tree
                    .flavors
                    .get(&NodeRef::Msg(m.id.clone()))
                    .copied()
                    .unwrap_or(Flavor::UserPrompt);
                if matches!(flavor, Flavor::UserPrompt) {
                    // Reject cuts root-ward of a prior compaction's cut.
                    if let Some(prior) = &prior_cut
                        && !is_tipward_of(m, prior, tree)
                    {
                        return None;
                    }
                    return Some(Self(m.id.clone()));
                }
            }
        }
        None
    }
}

fn prior_compaction_cut(tree: &SessionTree, leaf: &Position) -> Option<MessageId> {
    let mut cur = leaf.node_ref().cloned();
    while let Some(nref) = cur {
        let Some(node) = tree.nodes.get(&nref) else {
            break;
        };
        if let TreeNode::Summary(s) = node
            && let SummaryKind::Compaction { fold_to_id } = &s.kind
        {
            return Some(fold_to_id.clone());
        }
        cur = node.parent_id();
    }
    None
}

/// `candidate` is tip-ward of (or equal to) `prior` on the path.
fn is_tipward_of(candidate: &MessageNode, prior: &MessageId, tree: &SessionTree) -> bool {
    let mut cur = Some(NodeRef::Msg(candidate.id.clone()));
    while let Some(nref) = cur {
        if let NodeRef::Msg(m) = &nref
            && m == prior
        {
            return true;
        }
        cur = tree.nodes.get(&nref).and_then(TreeNode::parent_id);
    }
    false
}

fn active_path(tree: &SessionTree, leaf: Position) -> Vec<&TreeNode> {
    let mut path = Vec::new();
    let mut cur = leaf.node_ref().cloned();
    while let Some(nref) = cur {
        let Some(node) = tree.nodes.get(&nref) else {
            break;
        };
        path.push(node);
        cur = node.parent_id();
    }
    path.reverse();
    path
}

fn leaf_is_compaction(tree: &SessionTree) -> bool {
    let Some(nref) = tree.leaf.node_ref() else {
        return false;
    };
    matches!(tree.nodes.get(nref), Some(TreeNode::Summary(s)) if matches!(s.kind, SummaryKind::Compaction { .. }))
}

fn estimated_tokens(node: &TreeNode) -> u32 {
    let bytes: usize = match node {
        TreeNode::Message(m) => m.content.iter().map(|r| r.get().len()).sum(),
        TreeNode::Summary(s) => s.narrative.len(),
    };
    (bytes.max(CHARS_PER_TOKEN) / CHARS_PER_TOKEN) as u32
}

#[cfg(test)]
mod tests;
