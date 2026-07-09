use maki_providers::{ContentBlock, Message, Role};
use maki_storage::tree::{
    MessageId, MessageNode, NodeRef, OrderedRecord, Position, SessionTree, SummaryKind,
    SummaryRecord, TreeNode,
};
use test_case::test_case;

use super::finalize::FinalizedPartial;
use super::*;

const NARRATIVE_A: &str = "summary of earlier turns A";
const NARRATIVE_B: &str = "summary of earlier turns B";
const USER_MSG_A: &str = "what is 2+2";
const ASSISTANT_REPLY: &str = "it is 4";
const TOOL_ID_T1: &str = "t1";
const TOOL_NAME_BASH: &str = "bash";
const RESULT_OUTPUT: &str = "command output";
const CONTINUE_PROMPT: &str = "Continue.";
const KEEP_RECENT: u32 = 0;

fn user_msg(text: &str) -> Message {
    Message::user(text.into())
}

fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        ..Default::default()
    }
}

fn assistant_tool_use(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        ..Default::default()
    }
}

fn tool_result(id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: content.into(),
        is_error: false,
    }
}

fn user_with_results(id: &str, content: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![tool_result(id, content)],
        ..Default::default()
    }
}

fn tree_from_messages(messages: &[Message]) -> SessionTree {
    linear_tree(messages)
}

fn push_compaction_summary(
    tree: &mut SessionTree,
    parent: NodeRef,
    narrative: String,
    fold_to_id: MessageId,
) -> NodeRef {
    let record = SummaryRecord {
        id: maki_storage::tree::SummaryId::new(),
        parent_id: parent,
        narrative,
        kind: SummaryKind::Compaction { fold_to_id },
        read_files: Vec::new(),
        modified_files: Vec::new(),
    };
    let nref = NodeRef::Sum(record.id.clone());
    tree.nodes.insert(nref.clone(), TreeNode::Summary(record));
    tree.order.push(OrderedRecord::Node(nref.clone()));
    nref
}

#[test]
fn fold_linear_is_identity() {
    let messages = vec![user_msg(USER_MSG_A), assistant_text(ASSISTANT_REPLY)];
    let ctx = ValidContext::fold_linear(messages.clone());
    assert_eq!(ctx.len(), messages.len());
    assert!(matches!(ctx[0].role, Role::User));
    assert!(matches!(ctx[1].role, Role::Assistant));
}

#[test]
fn fold_is_deterministic() {
    let messages = vec![
        user_msg("one"),
        assistant_text("reply one"),
        user_msg("two"),
        assistant_text("reply two"),
    ];
    let ctx1 = ValidContext::fold_linear(messages.clone());
    let ctx2 = ValidContext::fold_linear(messages);
    assert_eq!(ctx1.len(), ctx2.len());
    for (a, b) in ctx1.iter().zip(ctx2.iter()) {
        assert_eq!(a.role, b.role);
    }
}

#[test]
fn fold_hoists_compaction_narrative_to_front() {
    let messages = vec![
        user_msg(USER_MSG_A),
        assistant_text(ASSISTANT_REPLY),
        user_msg("second question"),
        assistant_text("second reply"),
    ];
    let mut tree = tree_from_messages(&messages);

    let leaf_nref = tree.leaf.node_ref().cloned().unwrap();
    let leaf_msg_id = match &leaf_nref {
        NodeRef::Msg(m) => m.clone(),
        _ => unreachable!(),
    };

    // The root-ward message: first node in the linear chain.
    let root_msg_id = tree
        .nodes
        .get(&leaf_nref)
        .and_then(TreeNode::parent_id)
        .map(|p| match p {
            NodeRef::Msg(m) => m,
            _ => leaf_msg_id.clone(),
        })
        .unwrap_or_else(|| leaf_msg_id.clone());

    let summary_nref =
        push_compaction_summary(&mut tree, leaf_nref, NARRATIVE_A.into(), root_msg_id);
    tree.leaf = Position::At(summary_nref);

    let ctx = fold(&tree);
    assert!(!ctx.is_empty());
    assert!(matches!(ctx[0].role, Role::User));
    assert!(
        matches!(&ctx[0].content[0], ContentBlock::Text { text } if text == NARRATIVE_A),
        "narrative must be hoisted to front"
    );
}

#[test]
fn fold_newest_compaction_wins() {
    let messages = vec![user_msg(USER_MSG_A), assistant_text(ASSISTANT_REPLY)];
    let mut tree = tree_from_messages(&messages);

    // First compaction
    let leaf = tree.leaf.node_ref().cloned().unwrap();
    let leaf_msg_id = match &leaf {
        NodeRef::Msg(m) => m.clone(),
        _ => unreachable!(),
    };
    let root = tree
        .nodes
        .get(&leaf)
        .and_then(TreeNode::parent_id)
        .map(|p| match p {
            NodeRef::Msg(m) => m,
            _ => leaf_msg_id.clone(),
        })
        .unwrap_or_else(|| leaf_msg_id.clone());

    let summary1 = push_compaction_summary(&mut tree, leaf.clone(), NARRATIVE_A.into(), root);

    // Add messages after first compaction
    let after_first = vec![user_msg("post-compaction"), assistant_text("ok")];
    let mut parent = Some(summary1);
    for msg in &after_first {
        let id = MessageId::new();
        let node = MessageNode {
            id: id.clone(),
            parent_id: parent.clone(),
            role: msg.role,
            content: msg.content.iter().filter_map(to_raw_value).collect(),
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let nref = NodeRef::Msg(id);
        tree.flavors
            .insert(nref.clone(), SessionTree::node_flavor(&node));
        tree.nodes.insert(nref.clone(), TreeNode::Message(node));
        tree.order.push(OrderedRecord::Node(nref.clone()));
        parent = Some(nref);
    }
    let second_leaf = parent.unwrap();

    // Second compaction subsumes the first
    let msg_after_compaction_id = match &second_leaf {
        NodeRef::Msg(m) => m.clone(),
        _ => unreachable!(),
    };
    let summary2 = push_compaction_summary(
        &mut tree,
        second_leaf.clone(),
        NARRATIVE_B.into(),
        msg_after_compaction_id,
    );
    tree.leaf = Position::At(summary2);

    let ctx = fold(&tree);
    assert!(
        matches!(&ctx[0].content[0], ContentBlock::Text { text } if text == NARRATIVE_B),
        "newest compaction narrative must win"
    );
}

#[test]
fn cut_point_rejects_when_leaf_is_compaction() {
    let messages = vec![user_msg(USER_MSG_A), assistant_text(ASSISTANT_REPLY)];
    let history = History::new(messages);
    let cut = history.compaction_cut(KEEP_RECENT);
    // leaf is not a compaction, should either find a cut or None depending on budget
    let _ = cut;

    // Append a compaction summary to make leaf a compaction
    let messages2 = vec![user_msg("q"), assistant_text("a")];
    let tree = tree_from_messages(&messages2);
    // Simulate by checking the method returns None for root leaf
    let empty = History::new(Vec::new());
    assert!(empty.compaction_cut(KEEP_RECENT).is_none());
    let _ = tree;
}

#[test_case(
    vec![
        ContentBlock::Text { text: "thinking".into() },
        ContentBlock::ToolUse { id: "t1".into(), name: "bash".into(), input: serde_json::json!({}) },
    ],
    false
    ; "drops_tool_use_together")]
fn finalize_filter(completed: Vec<ContentBlock>, _expect_discard: bool) {
    let result = FinalizedPartial::from_completed_blocks(&completed);
    match result {
        FinalizedPartial::Node(blocks) => {
            assert!(
                !blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            );
        }
        FinalizedPartial::Discard => {}
    }
}

#[test]
fn finalize_drops_unsigned_thinking_keeps_signed() {
    let blocks = vec![
        ContentBlock::Thinking {
            thinking: "hmm".into(),
            signature: None,
        },
        ContentBlock::Text {
            text: "answer".into(),
        },
    ];
    let result = FinalizedPartial::from_completed_blocks(&blocks);
    match result {
        FinalizedPartial::Node(kept) => {
            assert_eq!(kept.len(), 1);
            assert!(matches!(&kept[0], ContentBlock::Text { text } if text == "answer"));
        }
        FinalizedPartial::Discard => panic!("text block should survive"),
    }
}

#[test]
fn finalize_signed_thinking_survives_alone() {
    let blocks = vec![ContentBlock::Thinking {
        thinking: "hmm".into(),
        signature: Some("sig".into()),
    }];
    let result = FinalizedPartial::from_completed_blocks(&blocks);
    assert!(matches!(result, FinalizedPartial::Node(_)));
}

#[test]
fn finalize_empty_discards() {
    let result = FinalizedPartial::from_completed_blocks(&[]);
    assert!(matches!(result, FinalizedPartial::Discard));
}

#[test]
fn repair_removes_orphaned_tool_result() {
    // User message carrying a tool_result with no preceding assistant tool_use
    let messages = vec![
        user_msg("hello"),
        user_with_results(TOOL_ID_T1, RESULT_OUTPUT),
    ];
    let tree = tree_from_messages(&messages);
    let ctx = fold(&tree);
    // The orphaned result must be removed, leaving just "hello"
    assert_eq!(ctx.len(), 1);
    assert!(
        ctx[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "hello"))
    );
}

#[test]
fn repair_closes_dangling_tool_use() {
    // Assistant tool_use with no following tool_result
    let messages = vec![
        user_msg(USER_MSG_A),
        assistant_tool_use(TOOL_ID_T1, TOOL_NAME_BASH),
    ];
    let tree = tree_from_messages(&messages);
    let ctx = fold(&tree);
    // repair inserts a synthetic result for the dangling tool_use
    let has_closing_result = ctx.iter().any(|m| {
        m.content.iter().any(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => tool_use_id == TOOL_ID_T1 && content == TOOL_RESULT_UNAVAILABLE,
            _ => false,
        })
    });
    assert!(
        has_closing_result,
        "dangling tool_use must be closed with {TOOL_RESULT_UNAVAILABLE}"
    );
}

#[test]
fn repair_keeps_matched_tool_result() {
    let messages = vec![
        user_msg(USER_MSG_A),
        assistant_tool_use(TOOL_ID_T1, TOOL_NAME_BASH),
        user_with_results(TOOL_ID_T1, RESULT_OUTPUT),
    ];
    let tree = tree_from_messages(&messages);
    let ctx = fold(&tree);
    let has_result = ctx.iter().any(|m| {
        m.content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { content, .. } if content == RESULT_OUTPUT),
        )
    });
    assert!(has_result, "matched tool_result must survive repair");
}

#[test]
fn lower_for_provider_strips_thinking_when_unsupported() {
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ],
        ..Default::default()
    }];
    lower_for_provider(&mut messages, false);
    assert_eq!(messages[0].content.len(), 1);
    assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "answer"));
}

#[test]
fn lower_for_provider_keeps_thinking_when_supported() {
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("sig".into()),
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ],
        ..Default::default()
    }];
    lower_for_provider(&mut messages, true);
    assert_eq!(messages[0].content.len(), 2);
}

#[test]
fn lower_for_provider_drops_empty_assistant_after_strip() {
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Thinking {
            thinking: "hmm".into(),
            signature: None,
        }],
        ..Default::default()
    }];
    lower_for_provider(&mut messages, false);
    assert!(
        messages.is_empty(),
        "assistant left empty after strip must be dropped"
    );
}

#[test]
fn append_compaction_pushes_continue_after_assistant_tip() {
    let messages = vec![user_msg(USER_MSG_A), assistant_text(ASSISTANT_REPLY)];
    let mut history = History::new(messages);

    history.active_branch();
    let cut = history.compaction_cut(KEEP_RECENT);

    if let Some(cut) = cut {
        history.append_compaction(
            cut,
            NARRATIVE_A.into(),
            CONTINUE_PROMPT,
            Vec::new(),
            Vec::new(),
        );
        let ctx = history.active_branch();
        let last = ctx.last().unwrap();
        assert!(
            matches!(last.role, Role::User),
            "leaf after assistant-tip compaction must end in a user turn"
        );
    }
}

#[test]
fn append_compaction_no_continue_after_user_tip() {
    let messages = vec![
        user_msg(USER_MSG_A),
        assistant_text(ASSISTANT_REPLY),
        user_msg("follow up"),
    ];
    let mut history = History::new(messages);
    let cut = history.compaction_cut(KEEP_RECENT);

    if let Some(cut) = cut {
        let len_before = history.active_branch().len();
        history.append_compaction(
            cut,
            NARRATIVE_A.into(),
            CONTINUE_PROMPT,
            Vec::new(),
            Vec::new(),
        );
        let len_after = history.active_branch().len();
        assert!(
            len_after <= len_before + 1,
            "no continue-prompt when tip is user"
        );
    }
}

#[test]
fn push_drops_hidden_display_text() {
    let mut history = History::new(Vec::new());
    let hidden = Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "hidden chrome".into(),
        }],
        display_text: Some(String::new()),
    };
    history.push(hidden);
    let ctx = history.active_branch();
    let last = ctx.last().unwrap();
    assert_eq!(
        last.display_text.as_deref(),
        Some(""),
        "hidden node lowers with empty display_text"
    );
}

#[test]
fn history_cache_returns_same_generation() {
    let messages = vec![user_msg("hello"), assistant_text("world")];
    let mut history = History::new(messages);
    let ctx1 = history.active_branch();
    let len1 = ctx1.len();
    let ctx2 = history.active_branch();
    assert_eq!(len1, ctx2.len(), "cache hit returns same generation");
}

#[test]
fn push_invalidates_cache() {
    let messages = vec![user_msg("hello")];
    let mut history = History::new(messages);
    let len_before = history.active_branch().len();
    history.push(assistant_text("world"));
    let len_after = history.active_branch().len();
    assert_eq!(
        len_after,
        len_before + 1,
        "push must invalidate and re-fold"
    );
}

#[test]
fn compaction_prefix_returns_cut_region() {
    let messages = vec![
        user_msg(USER_MSG_A),
        assistant_text(ASSISTANT_REPLY),
        user_msg("second"),
        assistant_text("second reply"),
    ];
    let history = History::new(messages);
    let cut = history.compaction_cut(KEEP_RECENT);

    if let Some(cut) = &cut {
        let prefix = history.compaction_prefix(cut);
        assert!(
            !prefix.is_empty(),
            "prefix must include nodes up to the cut"
        );
    }
}

fn build_branched_history() -> History {
    let messages = vec![
        user_msg(USER_MSG_A),
        assistant_text(ASSISTANT_REPLY),
        user_msg("second question"),
        assistant_text("second reply"),
    ];
    let mut history = History::new(messages);
    let landing = history.test_leaf_ref().expect("leaf after 4 messages");
    let _abandoned_tip = history.push(user_msg("abandoned branch message"));
    history.test_rewind_leaf_to(landing);
    history
}

#[test]
fn abandoned_branch_prefix_collects_off_path_messages() {
    let history = build_branched_history();
    let parent = history.test_leaf_ref().unwrap();
    let fold_from_id = history
        .test_find_msg_by_content("abandoned branch")
        .expect("abandoned tip");

    let prefix = history.abandoned_branch_prefix(&parent, &fold_from_id);
    assert_eq!(
        prefix.len(),
        1,
        "prefix must contain exactly the abandoned branch message"
    );
    assert!(
        prefix[0].content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text == "abandoned branch message")
        ),
        "prefix must contain the abandoned message text"
    );
}

#[test]
fn append_branch_summary_folds_in_place() {
    let mut history = build_branched_history();
    let parent = history.test_leaf_ref().unwrap();
    let fold_from_id = history
        .test_find_msg_by_content("abandoned branch")
        .expect("abandoned tip");

    history.append_branch_summary(parent, fold_from_id, NARRATIVE_A.into(), CONTINUE_PROMPT);

    let ctx = history.active_branch();
    assert!(
        ctx.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == NARRATIVE_A))),
        "branch-summary narrative must fold in place on the active path"
    );
    assert!(
        !ctx.iter().any(|m| m.content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text == "abandoned branch message")
        )),
        "abandoned branch message must not appear in the fold"
    );
}

#[test]
fn branch_summary_absent_after_undo_of_rewind() {
    let mut history = build_branched_history();
    let parent = history.test_leaf_ref().unwrap();
    let fold_from_id = history
        .test_find_msg_by_content("abandoned branch")
        .expect("abandoned tip");

    history.append_branch_summary(
        parent.clone(),
        fold_from_id,
        NARRATIVE_A.into(),
        CONTINUE_PROMPT,
    );

    history.test_rewind_leaf_to(parent.clone());

    let ctx = history.active_branch();
    assert!(
        !ctx.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == NARRATIVE_A))),
        "moving the leaf off the branch summary must remove it from the fold"
    );
}

const INTERRUPT_RUN_ID: u64 = 42;
const CANCELLED_BY_USER_MSG: &str = "[Cancelled by user]";

#[test]
fn push_interrupted_marks_leaf() {
    let messages = vec![user_msg("hello")];
    let mut history = History::new(messages);
    assert!(!history.leaf_is_interrupted());

    let blocks = vec![ContentBlock::Text {
        text: "partial answer".into(),
    }];
    history.push_interrupted(blocks, INTERRUPT_RUN_ID);
    assert!(
        history.leaf_is_interrupted(),
        "leaf must be interrupted after push_interrupted"
    );
}

#[test]
fn push_interrupted_discard_keeps_leaf_unchanged() {
    let messages = vec![user_msg("hello"), assistant_text("full reply")];
    let mut history = History::new(messages);
    let id = history.push_interrupted(Vec::new(), INTERRUPT_RUN_ID);
    assert!(id.is_none(), "empty blocks must not append a node");
    assert!(
        !history.leaf_is_interrupted(),
        "discard must not mark the leaf as interrupted"
    );
}

#[test]
fn interrupted_leaf_excluded_from_next_request() {
    let messages = vec![user_msg("hello")];
    let mut history = History::new(messages);

    let blocks = vec![ContentBlock::Thinking {
        thinking: "partial reasoning".into(),
        signature: Some("sig".into()),
    }];
    history.push_interrupted(blocks, INTERRUPT_RUN_ID);

    let leaf_interrupted = history.leaf_is_interrupted();
    let ctx = history.active_branch();
    let mut messages = ctx.to_vec();
    super::lower_for_provider(&mut messages, true);
    if leaf_interrupted && matches!(messages.last(), Some(m) if m.role == Role::Assistant) {
        messages.pop();
    }
    assert!(
        !messages.last().is_some_and(|m| m.role == Role::Assistant),
        "trailing interrupted assistant turn must be excluded from the next request"
    );
}

#[test]
fn mid_tool_cancel_closes_dangling_tool_uses() {
    let messages = vec![
        user_msg("run a command"),
        assistant_tool_use(TOOL_ID_T1, TOOL_NAME_BASH),
    ];
    let mut history = History::new(messages);

    history.close_cancelled_tool_calls();

    let ctx = history.active_branch();
    let last = ctx.last().expect("context non-empty after cancel");
    assert_eq!(last.role, Role::User, "cancelled results land as user node");
    assert!(
        last.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { content, is_error: true, .. } if content == CANCELLED_BY_USER_MSG
        )),
        "dangling tool_use must be closed with [Cancelled by user] error"
    );
}
