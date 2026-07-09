use std::collections::HashMap;

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use uuid::Uuid;

use crate::sessions::SessionMeta;

const MSG_PREFIX: &str = "msg_";
const SUM_PREFIX: &str = "sum_";
const LFT_PREFIX: &str = "lft_";
const LBL_PREFIX: &str = "lbl_";
const LOG_VERSION: u32 = 3;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
}

impl Role {
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

macro_rules! prefixed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                let uuid = Uuid::now_v7();
                let encoded = bs58::encode(uuid.as_bytes()).into_string();
                Self(format!("{}{encoded}", $prefix))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                if !raw.starts_with($prefix) {
                    return Err(serde::de::Error::custom(format!(
                        "missing {} prefix",
                        $prefix
                    )));
                }
                Ok(Self(raw))
            }
        }
    };
}

prefixed_id!(
    /// Message node id (§0): maki-minted UUIDv7, prefix-validated at parse.
    MessageId,
    MSG_PREFIX
);
prefixed_id!(
    /// Summary node id (§A.1): generated, prefix-validated at parse.
    SummaryId,
    SUM_PREFIX
);
prefixed_id!(
    /// Leaf record id (§A.1): grep/debug only — nothing references it.
    LeafId,
    LFT_PREFIX
);
prefixed_id!(
    /// Label record id (§A.1): annotation, not parented into the tree.
    LabelId,
    LBL_PREFIX
);

/// Provider-generated tool-use id (e.g. `toolu_…`, `call_…`). Opaque: NO prefix
/// validation, or every sub_msg/render key would be silently skipped under
/// fail-soft (§A.0(1)).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolUseId(String);

impl ToolUseId {
    pub fn new(s: String) -> Option<Self> {
        (!s.is_empty()).then_some(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolUseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ToolUseId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolUseId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        if raw.is_empty() {
            return Err(serde::de::Error::custom("empty tool_use_id"));
        }
        Ok(Self(raw))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeRef {
    Msg(MessageId),
    Sum(SummaryId),
}

impl std::fmt::Display for NodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Msg(id) => f.write_str(id.as_str()),
            Self::Sum(id) => f.write_str(id.as_str()),
        }
    }
}

impl Serialize for NodeRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for NodeRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        if let Some(rest) = raw.strip_prefix(MSG_PREFIX) {
            Ok(Self::Msg(MessageId(format!("{MSG_PREFIX}{rest}"))))
        } else if let Some(rest) = raw.strip_prefix(SUM_PREFIX) {
            Ok(Self::Sum(SummaryId(format!("{SUM_PREFIX}{rest}"))))
        } else {
            Err(serde::de::Error::custom(format!(
                "unknown node ref prefix in {raw:?}"
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    Root,
    At(NodeRef),
}

impl Position {
    /// `None` for `Root` (§A.4 walk starts here).
    pub fn node_ref(&self) -> Option<&NodeRef> {
        match self {
            Self::Root => None,
            Self::At(nref) => Some(nref),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TreeRecord {
    Header(Header),
    Message(MessageNode),
    Leaf(LeafRecord),
    Summary(SummaryRecord),
    Label(LabelRecord),
    SubMsg(SubMsgRecord),
}

#[derive(Deserialize)]
struct RecordTag {
    t: String,
}

/// Ok(None) = unknown tag, tolerated as opaque (§14 forward compat).
/// Err = malformed known record (warn + skip).
pub fn parse_line(line: &str) -> Result<Option<TreeRecord>, serde_json::Error> {
    let tag: RecordTag = serde_json::from_str(line)?;
    Ok(match tag.t.as_str() {
        "header" => Some(TreeRecord::Header(serde_json::from_str(line)?)),
        "message" => Some(TreeRecord::Message(serde_json::from_str(line)?)),
        "leaf" => Some(TreeRecord::Leaf(serde_json::from_str(line)?)),
        "summary" => Some(TreeRecord::Summary(serde_json::from_str(line)?)),
        "label" => Some(TreeRecord::Label(serde_json::from_str(line)?)),
        "sub_msg" => Some(TreeRecord::SubMsg(serde_json::from_str(line)?)),
        _ => None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub session_id: String,
    pub cwd: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_from_node_id: Option<NodeRef>,
}

impl Header {
    pub fn new(session_id: &str, cwd: &str, created_at: u64) -> Self {
        Self {
            version: LOG_VERSION,
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            created_at,
            parent_session_id: None,
            created_from_node_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageNode {
    pub id: MessageId,
    pub parent_id: Option<NodeRef>,
    pub role: Role,
    pub content: Vec<Box<RawValue>>,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubMsgRecord {
    pub sub: ToolUseId,
    pub d: Box<RawValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeafRecord {
    pub id: LeafId,
    // None is SEMANTIC — the root position, "before the first message".
    // Never add skip_serializing_if here; absent tolerantly reads as None.
    pub target_node_id: Option<NodeRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryRecord {
    pub id: SummaryId,
    pub parent_id: NodeRef,
    pub narrative: String,
    #[serde(flatten)]
    pub kind: SummaryKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SummaryKind {
    Compaction {
        // First KEPT node — a message id at a valid cut (§6).
        fold_to_id: MessageId,
    },
    Branch {
        // Abandoned tip; provenance only, never walked, may dangle after fork (§5).
        fold_from_id: NodeRef,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelRecord {
    pub id: LabelId,
    pub target_node_id: NodeRef,
    pub name: String,
}

const BLOCK_TYPE_TOOL_RESULT: &str = "tool_result";

/// Peek a raw content block's `type` tag without importing `ContentBlock`
/// (crate cycle — `maki-storage` cannot depend on `maki-providers`, §A.10).
#[derive(Deserialize)]
struct BlockTag {
    r#type: String,
}

/// Per-node classification computed once at load (§A.0(1), §4). The landing
/// rule and selector match on this enum instead of re-deriving predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    UserPrompt,
    ToolResultCarrier,
    Hidden,
    Assistant { interrupted: bool },
}

/// Domain-layer tree node — only `Message`/`Summary` are walkable; `Leaf`/
/// `Label`/`SubMsg`/`Header` never enter the fold path (§A.0(2)).
#[derive(Debug, Clone)]
pub enum TreeNode {
    Message(MessageNode),
    Summary(SummaryRecord),
}

impl TreeNode {
    pub fn parent_id(&self) -> Option<NodeRef> {
        match self {
            Self::Message(m) => m.parent_id.clone(),
            Self::Summary(s) => Some(s.parent_id.clone()),
        }
    }
}

/// Append-order spine that leaf resolution and undo derivation run over
/// (§A.5). Kept appended by the writer path too, so §A.5 is computable live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedRecord {
    Node(NodeRef),
    Leaf { target: Option<NodeRef> },
}

/// The load output of `log.jsonl`: tree nodes, append order, side tables for
/// labels and subagent channels, with flavors computed once (§A.0(2), §A.7).
#[derive(Debug, Clone)]
pub struct SessionTree {
    pub nodes: HashMap<NodeRef, TreeNode>,
    pub order: Vec<OrderedRecord>,
    pub leaf: Position,
    pub labels: Vec<LabelRecord>,
    pub sub_msgs: Vec<SubMsgRecord>,
    pub flavors: HashMap<NodeRef, Flavor>,
}

impl SessionTree {
    pub fn node_flavor(node: &MessageNode) -> Flavor {
        if node.hidden {
            return Flavor::Hidden;
        }
        if node.role.is_user() {
            let any_tool_result = node.content.iter().any(|raw| {
                serde_json::from_str::<BlockTag>(raw.get())
                    .is_ok_and(|tag| tag.r#type == BLOCK_TYPE_TOOL_RESULT)
            });
            if any_tool_result {
                return Flavor::ToolResultCarrier;
            }
            return Flavor::UserPrompt;
        }
        Flavor::Assistant {
            interrupted: node.interrupted,
        }
    }
}

/// `TreeMutation` variants per §13. Every mutation is durable; the writer
/// never coalesces.
#[derive(Debug)]
pub enum TreeMutation {
    AppendMessage(MessageNode),
    AppendSubMsg(SubMsgRecord),
    AppendRender {
        tool_use_id: ToolUseId,
        frame: Vec<u8>,
    },
    SetMeta(MetaRecord),
    /// Appends a `Leaf`; also serves undo-of-rewind (§13).
    Rewind {
        target: Position,
    },
    AppendSummary(SummaryRecord),
    /// Fork the root→cursor path into a new session (§5, §A.8). The writer
    /// flushes buffered appends first, copies the on-path nodes, their renders,
    /// subagent transcripts, and the cursor snapshot, then acks. The new
    /// session may not be opened before the ack.
    Fork {
        new_session_id: String,
        from_node_id: NodeRef,
        ack: flume::Sender<Result<ForkResult, String>>,
    },
    /// fsync + ack oneshot (§13, §8 interrupt barrier).
    Barrier(flume::Sender<()>),
}

/// The writer's fork completion signal (§5). `Ok` carries the new sessions dir
/// so the UI can open it; `Err` surfaces a copy failure.
#[derive(Debug)]
pub struct ForkResult {
    pub new_session_id: String,
    pub parent_title: String,
}

/// `TreeEvent` variants per §13. Emitted after the write, before the batched
/// fsync — write-ordered, durability-batched. Unbounded and never dropped.
#[derive(Debug, Clone)]
pub enum TreeEvent {
    Append {
        node_id: Option<NodeRef>,
        kind: AppendKind,
    },
    Navigate {
        old_leaf: Position,
        new_leaf: Position,
    },
    Summary {
        node_id: SummaryId,
        kind: SummaryKind,
    },
    /// Fork completed (§5): the new session folder is durable. Carries the new
    /// id and the parent title for the `(fork of …)` label.
    Fork {
        new_session_id: String,
        parent_title: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendKind {
    Message,
    SubMsg,
    Render,
    Meta,
    Rewind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaRecord {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationMarker>,
    #[serde(flatten)]
    pub meta: SessionMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationMarker {
    pub source: String,
    pub msg_count: usize,
    pub out_count: usize,
    pub sub_msg_count: usize,
    pub at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn message_id_round_trips() {
        let id = MessageId::new();
        assert!(id.as_str().starts_with(MSG_PREFIX));
        let s = serde_json::to_string(&id).unwrap();
        let back: MessageId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn message_id_rejects_wrong_prefix() {
        let err = serde_json::from_str::<MessageId>("\"sum_abc\"").unwrap_err();
        assert!(err.to_string().contains(MSG_PREFIX));
    }

    #[test]
    fn tool_use_id_accepts_provider_strings() {
        for raw in ["\"toolu_01\"", "\"call_func\"", "\"call_func_2\""] {
            let id: ToolUseId = serde_json::from_str(raw).unwrap();
            assert!(!id.as_str().is_empty());
        }
    }

    #[test]
    fn tool_use_id_rejects_empty() {
        assert!(serde_json::from_str::<ToolUseId>("\"\"").is_err());
    }

    #[test]
    fn node_ref_round_trips_msg() {
        let id = MessageId::new();
        let nref = NodeRef::Msg(id);
        let s = serde_json::to_string(&nref).unwrap();
        let back: NodeRef = serde_json::from_str(&s).unwrap();
        assert_eq!(nref, back);
    }

    #[test]
    fn node_ref_rejects_unknown_prefix() {
        assert!(serde_json::from_str::<NodeRef>("\"lft_xyz\"").is_err());
    }

    #[test]
    fn role_wire_is_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        let r: Role = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(r, Role::Assistant);
    }

    #[test]
    fn parse_line_tolerates_unknown_tag() {
        let res = parse_line(r#"{"t":"future_thing","x":1}"#).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn parse_line_errors_on_malformed_known_tag() {
        assert!(parse_line(r#"{"t":"message"}"#).is_err());
    }

    #[test]
    fn tree_record_serializes_tag_first() {
        let id = MessageId::new();
        let node = MessageNode {
            id: id.clone(),
            parent_id: None,
            role: Role::User,
            content: Vec::new(),
            timestamp: 42,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let s = serde_json::to_string(&TreeRecord::Message(node)).unwrap();
        assert!(s.starts_with(r#"{"t":"message","#));
        let back = parse_line(&s).unwrap().unwrap();
        match back {
            TreeRecord::Message(m) => assert_eq!(m.id, id),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn header_version_is_three() {
        let h = Header::new("s1", "/cwd", 0);
        assert_eq!(h.version, LOG_VERSION);
    }

    #[test]
    fn summary_id_round_trips_and_rejects_other_prefix() {
        let id = SummaryId::new();
        assert!(id.as_str().starts_with(SUM_PREFIX));
        let s = serde_json::to_string(&id).unwrap();
        let back: SummaryId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
        assert!(serde_json::from_str::<SummaryId>("\"msg_x\"").is_err());
    }

    #[test]
    fn leaf_id_and_label_id_round_trip() {
        for id in [
            LeafId::new().as_str().to_string(),
            LabelId::new().as_str().to_string(),
        ] {
            assert!(
                serde_json::from_str::<LeafId>(&serde_json::to_string(&id).unwrap()).is_ok()
                    || serde_json::from_str::<LabelId>(&serde_json::to_string(&id).unwrap())
                        .is_ok()
            );
        }
    }

    #[test]
    fn node_ref_round_trips_sum() {
        let id = SummaryId::new();
        let nref = NodeRef::Sum(id);
        let s = serde_json::to_string(&nref).unwrap();
        let back: NodeRef = serde_json::from_str(&s).unwrap();
        assert_eq!(nref, back);
    }

    #[test]
    fn node_ref_rejects_leaf_prefix() {
        assert!(serde_json::from_str::<NodeRef>(r#""lft_xyz""#).is_err());
        assert!(serde_json::from_str::<NodeRef>(r#""lbl_xyz""#).is_err());
    }

    #[test]
    fn summary_record_compaction_round_trips() {
        let record = SummaryRecord {
            id: SummaryId::new(),
            parent_id: NodeRef::Msg(MessageId::new()),
            narrative: "summary".into(),
            kind: SummaryKind::Compaction {
                fold_to_id: MessageId::new(),
            },
            read_files: vec!["a.rs".into()],
            modified_files: Vec::new(),
        };
        let s = serde_json::to_string(&TreeRecord::Summary(record.clone())).unwrap();
        assert!(s.contains(r#""kind":"compaction""#));
        let back = parse_line(&s).unwrap().unwrap();
        match back {
            TreeRecord::Summary(parsed) => {
                assert_eq!(parsed.id, record.id);
                assert_eq!(parsed.narrative, "summary");
                assert!(matches!(parsed.kind, SummaryKind::Compaction { .. }));
                assert_eq!(parsed.read_files, vec!["a.rs"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn summary_record_branch_round_trips() {
        let fold_from = NodeRef::Msg(MessageId::new());
        let record = SummaryRecord {
            id: SummaryId::new(),
            parent_id: NodeRef::Msg(MessageId::new()),
            narrative: "branch".into(),
            kind: SummaryKind::Branch {
                fold_from_id: fold_from.clone(),
            },
            read_files: Vec::new(),
            modified_files: vec!["b.rs".into()],
        };
        let s = serde_json::to_string(&TreeRecord::Summary(record)).unwrap();
        assert!(s.contains(r#""kind":"branch""#));
        let back = parse_line(&s).unwrap().unwrap();
        match back {
            TreeRecord::Summary(parsed) => {
                assert!(matches!(
                    parsed.kind,
                    SummaryKind::Branch { fold_from_id } if fold_from_id == fold_from
                ));
                assert_eq!(parsed.modified_files, vec!["b.rs"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn leaf_record_round_trips_with_null_target() {
        let record = LeafRecord {
            id: LeafId::new(),
            target_node_id: None,
        };
        let s = serde_json::to_string(&TreeRecord::Leaf(record.clone())).unwrap();
        assert!(s.contains(r#""target_node_id":null"#));
        let back = parse_line(&s).unwrap().unwrap();
        match back {
            TreeRecord::Leaf(parsed) => assert!(parsed.target_node_id.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    fn raw_block(json: &str) -> Box<RawValue> {
        serde_json::from_str(json).unwrap()
    }

    #[test_case(vec![], Flavor::UserPrompt ; "empty_user_is_prompt")]
    #[test_case(vec![r#"{"type":"text","text":"hi"}"#], Flavor::UserPrompt ; "text_user_is_prompt")]
    #[test_case(
        vec![r#"{"type":"tool_result","tool_use_id":"x","content":"r"}"#],
        Flavor::ToolResultCarrier ; "tool_result_user_is_carrier"
    )]
    #[test_case(vec![], Flavor::Assistant { interrupted: false } ; "assistant_default")]
    fn flavor_classification(content: Vec<&str>, expected: Flavor) {
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: None,
            role: if matches!(expected, Flavor::UserPrompt | Flavor::ToolResultCarrier) {
                Role::User
            } else {
                Role::Assistant
            },
            content: content.into_iter().map(raw_block).collect(),
            timestamp: 0,
            run_id: None,
            interrupted: matches!(expected, Flavor::Assistant { interrupted: true }),
            hidden: false,
        };
        assert_eq!(SessionTree::node_flavor(&node), expected);
    }

    #[test]
    fn flavor_hidden_overrides_role() {
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: None,
            role: Role::User,
            content: vec![raw_block(r#"{"type":"text","text":"hi"}"#)],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: true,
        };
        assert_eq!(SessionTree::node_flavor(&node), Flavor::Hidden);
    }

    #[test]
    fn flavor_assistant_interrupted() {
        let node = MessageNode {
            id: MessageId::new(),
            parent_id: None,
            role: Role::Assistant,
            content: Vec::new(),
            timestamp: 0,
            run_id: None,
            interrupted: true,
            hidden: false,
        };
        assert_eq!(
            SessionTree::node_flavor(&node),
            Flavor::Assistant { interrupted: true }
        );
    }
}
