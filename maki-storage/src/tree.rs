use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use uuid::Uuid;

use crate::sessions::SessionMeta;

const MSG_PREFIX: &str = "msg_";
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId(String);

impl MessageId {
    pub fn new() -> Self {
        let uuid = Uuid::now_v7();
        let encoded = bs58::encode(uuid.as_bytes()).into_string();
        Self(format!("{MSG_PREFIX}{encoded}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for MessageId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        if !raw.starts_with(MSG_PREFIX) {
            return Err(serde::de::Error::custom(format!(
                "missing {MSG_PREFIX} prefix"
            )));
        }
        Ok(Self(raw))
    }
}

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
}

impl std::fmt::Display for NodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Msg(id) => f.write_str(id.as_str()),
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

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TreeRecord {
    Header(Header),
    Message(MessageNode),
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
}
