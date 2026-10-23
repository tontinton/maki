//! The one portable form of a session: what `maki session export` prints,
//! `maki session import` reads, and `maki.session.export` / `new` hand to and
//! take from plugins. Every one of them goes through here, so a plugin and a
//! server can never disagree on what a session is.
//!
//! It carries conversation, never authority: no system prompt, no tools, no
//! rules. A fresh frame is built on the first run, and whatever the origin's
//! prompt stated that no longer holds (cwd, date) is told then.

use std::collections::HashSet;
use std::mem;
use std::sync::Arc;

use maki_providers::{ContentBlock, Message, Role, TokenUsage};
use maki_storage::frame::{PromptFacts, StoredFrame};
use maki_storage::sessions::{StoredMode, StoredThinking, ThinkingParseError, normalize_title};
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::history::{HistorySnapshot, facts_told, sanitize_restored, strip_thinking};
use crate::session::{SessionHeader, StoredSession};

/// Versions the envelope only. Messages inside it grow additively forever,
/// so nothing here moves with the session log format.
const FORMAT: u32 = 1;
const CONTENT_KEY: &str = "content";

pub const NEWER_FORMAT: &str = "exported by a newer maki";
const NO_TOOL_RESULT: &str = "has no tool_result in the next user message";
const MISPLACED_BLOCK: &str = "block is not allowed in a message with role";
const DUPLICATE_TOOL_ID: &str = "duplicate tool id";
const MALFORMED_CONTEXT_UPDATE: &str =
    "a context_update must be a user message with one text block and no display_text";
const INVALID_THINKING: &str = "invalid thinking setting";

/// A session as a document. Unknown top-level fields are ignored, which
/// means dropped: an importer keeps nothing it does not know. Everything but
/// `messages` may be left out, so a converter can hand over just those.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Reading a higher one fails, since a newer envelope field that must
    /// not be ignored is exactly what bumps it.
    #[serde(default = "current_format", deserialize_with = "known_format")]
    pub format: u32,
    /// Stored as given and resolved when the session opens, like any resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<StoredMode>,
    /// Spelled like the thinking setting everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Always written on export. Left out on import, the host's default holds.
    #[serde(default)]
    pub fast: Option<bool>,
    /// Left out, the title is generated from the history as usual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The date, working directory, model, instructions and hints the model
    /// was last told. Data, not authority: it only decides what the first run
    /// tells the model has moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<PromptFacts>,
    /// Skipped when empty only so [`SessionExport`] can write its shared
    /// messages under the same key. An empty document still parses.
    #[serde(
        default,
        deserialize_with = "located_messages",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub messages: Vec<Message>,
}

fn current_format() -> u32 {
    FORMAT
}

fn known_format<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let found = u32::deserialize(deserializer)?;
    if found > FORMAT {
        return Err(de::Error::custom(format!(
            "{NEWER_FORMAT} (format {found}, this build reads up to {FORMAT})"
        )));
    }
    Ok(found)
}

/// Where in `messages` something is wrong, as the path a reader of the
/// document would follow: `messages[12].content[2]`.
fn location(message: usize, block: Option<usize>) -> String {
    match block {
        Some(block) => format!("messages[{message}].{CONTENT_KEY}[{block}]"),
        None => format!("messages[{message}]"),
    }
}

/// What resume cannot repair. Message shape errors (facts on the wrong
/// message) are caught earlier, by deserializing, and are located the same
/// way.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("{at}: tool_use {id:?} {NO_TOOL_RESULT}")]
    MissingToolResult { at: String, id: String },
    #[error("{at}: {block} {MISPLACED_BLOCK} {role}")]
    MisplacedBlock {
        at: String,
        block: &'static str,
        role: &'static str,
    },
    #[error("{at}: {DUPLICATE_TOOL_ID} {id:?}")]
    DuplicateToolId { at: String, id: String },
    #[error("{at}: {MALFORMED_CONTEXT_UPDATE}")]
    MalformedContextUpdate { at: String },
    /// A block a strict reader cannot take, though a load keeps it: one of
    /// a known type that does not parse, or one without a type.
    #[error("{at}: {detail}")]
    UnreadableBlock { at: String, detail: String },
    #[error("{INVALID_THINKING}: {0}")]
    Thinking(#[from] ThinkingParseError),
}

impl Transcript {
    /// Lays the document over `base`, the blank session its host would open
    /// (cwd, settings that outlive a turn), but does not store it. Whatever
    /// the document leaves out keeps the base's value. Spend comes from
    /// neither. A transcript carries no authority, and a host's standing
    /// permissions belong to the sessions the user opened, so a document
    /// with conversation drops the base's. One without is just a new
    /// session, so it keeps them the way a new tab does.
    ///
    /// Repairs exactly what a resume repairs and refuses the rest, so a plugin
    /// bug fails loudly instead of changing what the model saw. Thinking goes:
    /// it is bound to the origin's prefix, which no fresh frame can match.
    pub fn into_session(self, mut base: StoredSession) -> Result<StoredSession, TranscriptError> {
        let thinking = self
            .thinking
            .as_deref()
            .map(StoredThinking::parse_setting)
            .transpose()?;
        validate(&self.messages)?;
        let mut messages = self.messages;
        sanitize_restored(&mut messages);
        strip_thinking(&mut messages);

        if !messages.is_empty() {
            base.meta.yolo = None;
            base.meta.session_rules.clear();
        }
        base.replace_messages(messages);
        base.set_frame(
            self.facts
                .map(|facts| Arc::new(StoredFrame::facts_only(facts))),
        );
        if let Some(model) = self.model.filter(|model| !model.trim().is_empty()) {
            base.set_model(model);
        }
        let meta = &mut base.meta;
        meta.mode = self.mode.or(meta.mode);
        meta.thinking = thinking.or(meta.thinking);
        meta.fast = self.fast.unwrap_or(meta.fast);
        base.token_usage = TokenUsage::default();
        match self.title.map(|title| normalize_title(&title)) {
            Some(title) if !title.is_empty() => base.set_title(title),
            _ => base.update_title_if_default(),
        }
        Ok(base)
    }
}

/// A session to export, its messages still shared with whoever holds them,
/// so reading a long live one copies nothing. Serializes as the
/// [`Transcript`] document.
pub struct SessionExport {
    pub header: SessionHeader,
    pub messages: Arc<Vec<Message>>,
    pub frame: Option<Arc<StoredFrame>>,
}

impl SessionExport {
    pub fn stored(session: StoredSession) -> Self {
        Self {
            header: SessionHeader::of(&session),
            frame: session.frame().cloned(),
            messages: Arc::new(session.take_messages()),
        }
    }

    /// A live session, whose transcript and frame run ahead of what its
    /// stored copy holds.
    pub fn live(header: SessionHeader, snapshot: &HistorySnapshot) -> Self {
        Self {
            header,
            messages: Arc::clone(&snapshot.messages),
            frame: snapshot.frame.clone(),
        }
    }

    /// The document without `messages`, which stay in [`Self::messages`].
    pub fn envelope(&self) -> Transcript {
        let header = &self.header;
        Transcript {
            format: FORMAT,
            model: (!header.model.is_empty()).then(|| header.model.clone()),
            mode: header.mode,
            thinking: header.thinking.map(|thinking| thinking.to_string()),
            fast: Some(header.fast),
            title: Some(header.title.clone()),
            facts: (self.frame.as_deref())
                .and_then(|frame| facts_told(frame, &self.messages).prompt),
            messages: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct ExportDocument<'a> {
    #[serde(flatten)]
    envelope: Transcript,
    messages: &'a [Message],
}

impl Serialize for SessionExport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ExportDocument {
            envelope: self.envelope(),
            messages: &self.messages,
        }
        .serialize(serializer)
    }
}

fn validate(messages: &[Message]) -> Result<(), TranscriptError> {
    let mut tool_uses = HashSet::new();
    let mut tool_results = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message.is_context_update() && !message.is_host_context_update() {
            return Err(TranscriptError::MalformedContextUpdate {
                at: location(index, None),
            });
        }
        let next = messages.get(index + 1);
        for (block_index, block) in message.content.iter().enumerate() {
            let at = location(index, Some(block_index));
            if let Some(detail) = block.strict_error() {
                return Err(TranscriptError::UnreadableBlock { at, detail });
            }
            if required_role(block).is_some_and(|role| message.role != role) {
                return Err(TranscriptError::MisplacedBlock {
                    at,
                    block: block.into(),
                    role: (&message.role).into(),
                });
            }
            let (seen, id) = match block {
                ContentBlock::ToolUse { id, .. } => (&mut tool_uses, id),
                ContentBlock::ToolResult { tool_use_id, .. } => (&mut tool_results, tool_use_id),
                _ => continue,
            };
            if !seen.insert(id.as_str()) {
                return Err(TranscriptError::DuplicateToolId { at, id: id.clone() });
            }
            // A trailing call is closed like on resume.
            if matches!(block, ContentBlock::ToolUse { .. })
                && next.is_some_and(|next| !answers(next, id))
            {
                return Err(TranscriptError::MissingToolResult { at, id: id.clone() });
            }
        }
    }
    Ok(())
}

fn answers(message: &Message, id: &str) -> bool {
    message.role == Role::User
        && message.content.iter().any(
            |block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
        )
}

/// `None` for blocks either role may carry, unknown ones included.
fn required_role(block: &ContentBlock) -> Option<Role> {
    match block {
        ContentBlock::ToolUse { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. } => Some(Role::Assistant),
        ContentBlock::ToolResult { .. } => Some(Role::User),
        _ => None,
    }
}

/// Parses each message, and each of its blocks, on its own, so a shape error
/// names the message and block it is in. Going through `Value` copies each
/// text string once more than a plain load does. Image data is not copied
/// again.
fn located_messages<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Message>, D::Error> {
    Vec::<Value>::deserialize(deserializer)?
        .into_iter()
        .enumerate()
        .map(|(index, raw)| parse_message(index, raw))
        .collect::<Result<_, _>>()
        .map_err(de::Error::custom)
}

fn parse_message(index: usize, mut raw: Value) -> Result<Message, String> {
    let blocks = match raw.get_mut(CONTENT_KEY) {
        Some(Value::Array(blocks)) => mem::take(blocks),
        _ => Vec::new(),
    };
    let mut message =
        Message::deserialize(raw).map_err(|e| format!("{}: {e}", location(index, None)))?;
    message.content = blocks
        .into_iter()
        .enumerate()
        .map(|(block, raw)| {
            <ContentBlock as Deserialize>::deserialize(raw)
                .map_err(|e| format!("{}: {e}", location(index, Some(block))))
        })
        .collect::<Result<_, _>>()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Display;

    use maki_providers::{ImageMediaType, ImageSource};
    use maki_storage::frame::FactsUpdate;
    use maki_storage::sessions::{StoredEffect, StoredRule};
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::agent::History;

    const CWD: &str = "/here";
    const ORIGIN_CWD: &str = "/origin";
    const MODEL: &str = "anthropic/claude-opus-5";
    const UNKNOWN_MODEL: &str = "nowhere/model-from-2024";
    const TITLE: &str = "moved session";
    const MULTILINE_TITLE: &str = "moved\n  session";
    const LONG_TITLE: &str =
        "a title far longer than any picker row can show without cutting it somewhere";
    const PROMPT: &str = "fix the bug";
    const ANSWER: &str = "done";
    const THINKING: &str = "high";
    const OTHER_THINKING: &str = "off";
    const HOST_CWD: &str = "/host/project";
    const SIGNATURE: &str = "sig";
    const TOOL_ID: &str = "t1";
    const TOOL_NAME: &str = "read";
    const DATE: &str = "2026-10-02";
    const STALE_DATE: &str = "2026-09-30";
    const ORIGIN_SYSTEM: &str = "the origin's system prompt";
    const ORIGIN_FINGERPRINT: &str = "0123456789abcdef";
    const MCP_TOOL: &str = "srv__tool";
    const PLAN_PATH: &str = "/plans/origin.md";
    const UNKNOWN_BLOCK_TYPE: &str = "server_tool_use";
    const UNKNOWN_FIELD: &str = "origin_host";
    const UPDATE_TEXT: &str = "<context-update>Date is now 2026-10-02.</context-update>";
    const NOT_CARRIED: &str = "an import must not carry what belongs to the origin machine";
    const PERMISSIONS_MSG: &str =
        "only a document without conversation keeps the tab's permissions";

    fn user(text: &str) -> Message {
        Message::user(text.into())
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    fn thinking() -> ContentBlock {
        ContentBlock::Thinking {
            thinking: PROMPT.into(),
            signature: Some(SIGNATURE.into()),
        }
    }

    fn signed_call(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: TOOL_NAME.into(),
            input: json!({ "path": null }),
            thought_signature: Some(SIGNATURE.into()),
        }
    }

    fn call(id: &str) -> Message {
        assistant(vec![ContentBlock::tool_use(id, TOOL_NAME, json!({}))])
    }

    fn result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(id, ANSWER, false)],
            display_text: Some(String::new()),
            ..Default::default()
        }
    }

    fn unknown_block() -> ContentBlock {
        ContentBlock::Unknown(json!({ "type": UNKNOWN_BLOCK_TYPE, "id": TOOL_ID }))
    }

    fn image() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from("aGk=")),
        }
    }

    fn date_update(date: &str) -> Message {
        Message::context_update(
            UPDATE_TEXT.into(),
            FactsUpdate {
                date: Some(date.into()),
                ..FactsUpdate::default()
            },
        )
    }

    fn facts() -> PromptFacts {
        PromptFacts {
            date: STALE_DATE.into(),
            cwd: ORIGIN_CWD.into(),
            model: MODEL.into(),
            instructions: "be nice".into(),
            hints: BTreeMap::from([("memory/after_instructions".into(), "tags: a".into())]),
        }
    }

    fn conversation() -> Vec<Message> {
        vec![
            user(PROMPT),
            assistant(vec![thinking(), signed_call(TOOL_ID)]),
            Message {
                content: vec![
                    ContentBlock::tool_result(TOOL_ID, ANSWER, false),
                    image(),
                    unknown_block(),
                ],
                ..result(TOOL_ID)
            },
            date_update(DATE),
            assistant(vec![
                ContentBlock::RedactedThinking {
                    data: SIGNATURE.into(),
                },
                text(ANSWER),
                unknown_block(),
            ]),
        ]
    }

    /// A session the way the origin machine holds it, with everything that
    /// must stay behind set to something an import would notice.
    fn origin_session() -> StoredSession {
        let mut session = StoredSession::new(MODEL, ORIGIN_CWD);
        session.replace_messages(conversation());
        session.set_frame(Some(Arc::new(StoredFrame::new(
            ORIGIN_SYSTEM.into(),
            ORIGIN_FINGERPRINT.into(),
            Some(facts()),
            vec![json!({ "name": MCP_TOOL })],
        ))));
        session.set_title(TITLE.into());
        session.meta.mode = Some(StoredMode::Plan);
        session.meta.plan_path = Some(PLAN_PATH.into());
        session.meta.plan_written = true;
        session.meta.thinking = Some(StoredThinking::parse_setting(THINKING).unwrap());
        session.meta.fast = true;
        session.meta.workflow = true;
        session.meta.yolo = Some(true);
        session.meta.session_rules = rules();
        session.token_usage = TokenUsage {
            input: 1000,
            output: 100,
            ..TokenUsage::default()
        };
        session
    }

    fn rules() -> Vec<StoredRule> {
        vec![StoredRule {
            tool: TOOL_NAME.into(),
            scope: None,
            effect: StoredEffect::Allow,
        }]
    }

    fn blank() -> StoredSession {
        StoredSession::new("", CWD)
    }

    fn import(transcript: Transcript) -> StoredSession {
        transcript.into_session(blank()).unwrap()
    }

    /// A focused tab's blank session: its settings, and permissions a
    /// plugin-made session must not pick up.
    fn host_base() -> StoredSession {
        let mut base = StoredSession::new(MODEL, HOST_CWD);
        base.meta.mode = Some(StoredMode::Plan);
        base.meta.thinking = Some(StoredThinking::parse_setting(THINKING).unwrap());
        base.meta.fast = true;
        base.meta.workflow = true;
        base.meta.yolo = Some(true);
        base.meta.session_rules = rules();
        base
    }

    /// What `maki session export` prints, read back.
    fn exported(session: StoredSession) -> Transcript {
        let document = serde_json::to_value(SessionExport::stored(session)).unwrap();
        serde_json::from_value(document).unwrap()
    }

    fn messages_only(messages: Vec<Message>) -> Transcript {
        Transcript {
            messages,
            ..Transcript::default()
        }
    }

    #[track_caller]
    fn assert_refused_at(error: impl Display, at: &str, problem: &str) {
        let error = error.to_string();
        assert!(error.starts_with(&format!("{at}: ")), "{error}");
        assert!(error.contains(problem), "{error}");
    }

    /// Fork stays the trusted path. Export and import agree with it on
    /// everything but what a fresh frame cannot keep.
    #[test]
    fn round_trip_loses_only_thinking() {
        let original = exported(origin_session());
        let reexported = exported(import(original.clone()));

        let mut expected = original;
        strip_thinking(&mut expected.messages);
        assert_eq!(reexported, expected);
    }

    /// The CLI prints the shared form, so reading it back as a transcript
    /// and writing that again must give the same document.
    #[test]
    fn a_session_export_serializes_as_its_transcript() {
        let shared = serde_json::to_value(SessionExport::stored(origin_session())).unwrap();
        let owned = serde_json::to_value(exported(origin_session())).unwrap();
        assert_eq!(shared, owned);
    }

    /// The origin's prompt is not carried, only what it said, with the
    /// transcript's updates already applied.
    #[test]
    fn export_carries_what_the_model_last_held() {
        assert_eq!(
            exported(origin_session()).facts,
            Some(PromptFacts {
                date: DATE.into(),
                ..facts()
            })
        );
    }

    #[test_case(Some(facts()) ; "facts")]
    #[test_case(None ; "no_facts")]
    fn facts_become_a_frame_without_a_prefix(facts: Option<PromptFacts>) {
        let session = import(Transcript {
            facts: facts.clone(),
            ..messages_only(vec![user(PROMPT)])
        });
        assert_eq!(
            session.frame().map(Arc::as_ref),
            facts.map(StoredFrame::facts_only).as_ref()
        );
    }

    #[test_case(|s: &StoredSession| s.frame().unwrap().prefix().is_none() && s.frame().unwrap().mcp_tools.is_empty() ; "frame")]
    #[test_case(|s: &StoredSession| s.meta.session_rules.is_empty() ; "rules")]
    #[test_case(|s: &StoredSession| s.meta.yolo.is_none() ; "yolo")]
    #[test_case(|s: &StoredSession| !s.meta.workflow ; "workflow")]
    #[test_case(|s: &StoredSession| s.token_usage == TokenUsage::default() ; "usage")]
    #[test_case(|s: &StoredSession| s.meta.plan_path.is_none() && !s.meta.plan_written ; "plan_file")]
    #[test_case(|s: &StoredSession| !s.messages().iter().flat_map(|m| &m.content).any(ContentBlock::is_thinking) ; "thinking")]
    #[test_case(|s: &StoredSession| !s.messages().iter().flat_map(|m| &m.content).any(|b| matches!(b, ContentBlock::ToolUse { thought_signature: Some(_), .. })) ; "signatures")]
    fn not_carried(left_behind: fn(&StoredSession) -> bool) {
        let session = import(exported(origin_session()));
        assert!(left_behind(&session), "{NOT_CARRIED}");
    }

    #[test]
    fn settings_and_title_are_carried() {
        let session = import(exported(origin_session()));
        assert_eq!(session.title, TITLE);
        assert_eq!(session.cwd, CWD);
        assert_eq!(session.meta.mode, Some(StoredMode::Plan));
        assert_eq!(session.meta.thinking.unwrap().to_string(), THINKING);
        assert!(session.meta.fast);
    }

    #[test]
    fn what_the_document_leaves_out_comes_from_the_base_but_permissions() {
        let session = messages_only(vec![user(PROMPT)])
            .into_session(host_base())
            .unwrap();
        assert_eq!(session.cwd, HOST_CWD);
        assert_eq!(session.model, MODEL);
        assert_eq!(session.meta.mode, Some(StoredMode::Plan));
        assert_eq!(session.meta.thinking.unwrap().to_string(), THINKING);
        assert!(session.meta.fast);
        assert!(session.meta.workflow);
        assert!(session.meta.yolo.is_none(), "{NOT_CARRIED}");
        assert!(session.meta.session_rules.is_empty(), "{NOT_CARRIED}");
    }

    /// A blank document is a new session like a new tab, so the tab's
    /// standing permissions hold. One that repairs to nothing is blank too.
    #[test_case(vec![user(PROMPT)], false ; "conversation_drops_them")]
    #[test_case(Vec::new(), true ; "blank_keeps_them")]
    #[test_case(vec![Message { content: Vec::new(), ..user(PROMPT) }], true ; "emptied_by_repair_keeps_them")]
    fn permissions_stay_only_with_a_blank_document(messages: Vec<Message>, kept: bool) {
        let session = messages_only(messages).into_session(host_base()).unwrap();
        assert_eq!(session.meta.yolo.is_some(), kept, "{PERMISSIONS_MSG}");
        assert_eq!(
            !session.meta.session_rules.is_empty(),
            kept,
            "{PERMISSIONS_MSG}"
        );
    }

    /// A model this build does not know is stored as given, and resolved
    /// when the session opens like on any resume.
    #[test]
    fn document_settings_win_over_the_base() {
        let session = Transcript {
            model: Some(UNKNOWN_MODEL.into()),
            mode: Some(StoredMode::Build),
            thinking: Some(OTHER_THINKING.into()),
            fast: Some(false),
            ..messages_only(vec![user(PROMPT)])
        }
        .into_session(host_base())
        .unwrap();
        assert_eq!(session.cwd, HOST_CWD);
        assert_eq!(session.model, UNKNOWN_MODEL);
        assert_eq!(session.meta.mode, Some(StoredMode::Build));
        assert_eq!(session.meta.thinking.unwrap().to_string(), OTHER_THINKING);
        assert!(!session.meta.fast);
        assert!(session.meta.workflow);
    }

    /// A title given is kept the way `set_title` keeps one, on one line. A
    /// blank one is no title at all, so one is generated from the prompt.
    #[test_case(None, PROMPT ; "absent_is_generated")]
    #[test_case(Some(""), PROMPT ; "empty_is_generated")]
    #[test_case(Some(" \n\t "), PROMPT ; "blank_is_generated")]
    #[test_case(Some(MULTILINE_TITLE), TITLE ; "multiline_is_one_line")]
    #[test_case(Some(LONG_TITLE), LONG_TITLE ; "long_is_kept")]
    fn import_title_is_normalized(title: Option<&str>, expected: &str) {
        let session = import(Transcript {
            title: title.map(String::from),
            ..messages_only(vec![user(PROMPT)])
        });
        assert_eq!(session.title, expected);
    }

    #[test_case(None, MODEL ; "absent_inherits")]
    #[test_case(Some(""), MODEL ; "empty_inherits")]
    #[test_case(Some("  "), MODEL ; "blank_inherits")]
    #[test_case(Some(UNKNOWN_MODEL), UNKNOWN_MODEL ; "given_is_kept")]
    fn import_model_falls_back_to_the_base_when_blank(model: Option<&str>, expected: &str) {
        let session = Transcript {
            model: model.map(String::from),
            ..messages_only(vec![user(PROMPT)])
        }
        .into_session(host_base())
        .unwrap();
        assert_eq!(session.model, expected);
    }

    #[test_case(vec![user(PROMPT), call(TOOL_ID)] ; "pending_tool_use")]
    #[test_case(vec![user(PROMPT), result(TOOL_ID), user(PROMPT)] ; "orphan_result")]
    #[test_case(vec![user(PROMPT), call(TOOL_ID), Message { content: vec![ContentBlock::tool_result(TOOL_ID, ANSWER, false), ContentBlock::tool_result("orphan", ANSWER, false)], ..result(TOOL_ID) }] ; "partial_orphan")]
    fn sanitized_like_resume(messages: Vec<Message>) {
        let resumed = History::restored(messages.clone());
        let imported = import(messages_only(messages));
        assert_eq!(imported.messages(), resumed.as_slice());
    }

    #[test_case(vec![user(PROMPT), call(TOOL_ID), user(PROMPT), assistant(vec![text(ANSWER)])], NO_TOOL_RESULT, "messages[1].content[0]" ; "tool_use_without_result")]
    #[test_case(vec![user(PROMPT), call(TOOL_ID), call("t2"), result(TOOL_ID)], NO_TOOL_RESULT, "messages[1].content[0]" ; "result_not_in_the_next_message")]
    #[test_case(vec![Message { content: vec![ContentBlock::tool_use(TOOL_ID, TOOL_NAME, json!({}))], ..user(PROMPT) }], MISPLACED_BLOCK, "messages[0].content[0]" ; "tool_use_from_user")]
    #[test_case(vec![Message { content: vec![text(PROMPT), thinking()], ..user(PROMPT) }], MISPLACED_BLOCK, "messages[0].content[1]" ; "thinking_from_user")]
    #[test_case(vec![user(PROMPT), assistant(vec![ContentBlock::tool_result(TOOL_ID, ANSWER, false)])], MISPLACED_BLOCK, "messages[1].content[0]" ; "tool_result_from_assistant")]
    #[test_case(vec![user(PROMPT), call(TOOL_ID), result(TOOL_ID), call(TOOL_ID), result(TOOL_ID)], DUPLICATE_TOOL_ID, "messages[3].content[0]" ; "duplicate_tool_id")]
    #[test_case(vec![user(PROMPT), Message { role: Role::Assistant, ..date_update(DATE) }], MALFORMED_CONTEXT_UPDATE, "messages[1]" ; "context_update_from_assistant")]
    fn invalid_history_is_refused(messages: Vec<Message>, problem: &str, at: &str) {
        let error = messages_only(messages).into_session(blank()).unwrap_err();
        assert_refused_at(error, at, problem);
    }

    /// A newer build's block loads as unknown, and either role may carry it.
    #[test]
    fn unknown_block_types_are_imported_in_either_role() {
        let block = json!({ "type": UNKNOWN_BLOCK_TYPE, "id": TOOL_ID });
        let document = json!({ "messages": [
            { "role": "user", "content": [block] },
            { "role": "assistant", "content": [block] },
        ] });
        let session = import(serde_json::from_value(document).unwrap());

        let unknown = [ContentBlock::Unknown(block)];
        assert_eq!(session.messages()[0].content, unknown);
        assert_eq!(session.messages()[1].content, unknown);
    }

    #[test]
    fn bad_thinking_setting_is_refused() {
        let error = Transcript {
            thinking: Some(PROMPT.into()),
            ..messages_only(vec![user(PROMPT)])
        }
        .into_session(blank())
        .unwrap_err();
        assert!(error.to_string().starts_with(INVALID_THINKING));
    }

    /// What a message or block must hold is the provider types' rule, tested
    /// there. Here only that the refusal says where it is.
    #[test_case(json!({ "role": "user", "content": [], "kind": "context_update" }), "messages[0]" ; "malformed_message")]
    #[test_case(json!({ "role": "user", "content": [{ "type": "text", "text": PROMPT }, { "text": PROMPT }] }), "messages[0].content[1]" ; "unreadable_block")]
    fn a_refusal_names_where_it_is(message: Value, at: &str) {
        let document = json!({ "messages": [message] });
        let error = serde_json::from_value::<Transcript>(document)
            .map_err(|e| e.to_string())
            .and_then(|transcript| {
                (transcript.into_session(blank()).map(drop)).map_err(|e| e.to_string())
            })
            .unwrap_err();
        assert!(error.starts_with(&format!("{at}: ")), "{error}");
    }

    #[test]
    fn newer_format_is_refused() {
        let document = json!({ "format": FORMAT + 1, "messages": [] });
        let error = serde_json::from_value::<Transcript>(document).unwrap_err();
        assert!(error.to_string().contains(NEWER_FORMAT), "{error}");
    }

    /// A converter may hand over just the messages, and an importer keeps
    /// nothing it does not know.
    #[test]
    fn messages_alone_are_a_transcript_and_unknown_fields_are_dropped() {
        let document = json!({
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": PROMPT }] }],
            UNKNOWN_FIELD: { "host": CWD },
        });
        let read: Transcript = serde_json::from_value(document).unwrap();
        let expected = Transcript {
            format: FORMAT,
            ..messages_only(vec![user(PROMPT)])
        };
        assert_eq!(read, expected);
    }
}
