//! The one shape `maki.session.messages()` returns. The UI and the headless
//! drivers both build it from the same live history, so a plugin sees the
//! same rows wherever it runs.

use std::sync::Arc;

use maki_agent::SharedMessages;
use maki_providers::{ContentBlock, Message, Role};
use serde::Serialize;

use crate::EventHandle;

pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";

/// Where a row's text came from, which `role` alone cannot say: an answer
/// the human gave through the `question` tool arrives as a tool result, and
/// an observation a plugin injected arrives as a user message.
pub const KIND_TYPED: &str = "typed";
pub const KIND_ANSWER: &str = "answer";
pub const KIND_OBSERVATION: &str = "observation";
pub const KIND_SAID: &str = "said";

/// Cap on one row's text. A pasted file is not worth copying through the
/// Lua bridge in full, and a caller that wants the whole thing has `read`.
const TEXT_MAX_BYTES: usize = 16 * 1024;

#[derive(Serialize)]
pub struct MessageView {
    /// [`ROLE_USER`] or [`ROLE_ASSISTANT`].
    pub role: &'static str,
    /// [`KIND_TYPED`], [`KIND_ANSWER`], [`KIND_OBSERVATION`], or
    /// [`KIND_SAID`] for the assistant's own text.
    pub kind: &'static str,
    pub text: String,
    /// Whether {text} was cut to fit.
    pub truncated: bool,
}

/// What a caller asked for. `limit` counts back from the newest row, so the
/// answer is always the tail of the conversation, oldest first.
#[derive(Default)]
pub struct MessagesQuery {
    pub limit: Option<usize>,
    /// [`ROLE_USER`] or [`ROLE_ASSISTANT`]; both when absent.
    pub role: Option<String>,
}

fn row(role: &'static str, kind: &'static str, text: &str) -> MessageView {
    let kept = truncate_bytes(text, TEXT_MAX_BYTES);
    MessageView {
        role,
        kind,
        text: kept.to_owned(),
        truncated: kept.len() < text.len(),
    }
}

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Ids of `question` tool calls, so their results can be reported as
/// something the human said rather than as tool output.
fn question_tool_use_ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter(|msg| matches!(msg.role, Role::Assistant))
        .flat_map(Message::tool_uses)
        .filter(|(_, name, _)| *name == maki_agent::tools::QUESTION_TOOL_NAME)
        .map(|(id, _, _)| id)
        .collect()
}

/// Flattens a conversation into rows of text. Tool calls and their results
/// are left out (a plugin that wants them has the tool events), except for
/// `question` answers, which the human authored.
pub fn views(messages: &[Message], query: &MessagesQuery) -> Vec<MessageView> {
    let question_ids = question_tool_use_ids(messages);
    let wants = |role: &str| query.role.as_deref().is_none_or(|want| want == role);
    let mut rows: Vec<MessageView> = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User if wants(ROLE_USER) => {
                let kind = if msg.is_observation() {
                    KIND_OBSERVATION
                } else {
                    KIND_TYPED
                };
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            rows.push(row(ROLE_USER, kind, text));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error: false,
                        } if question_ids.contains(&tool_use_id.as_str())
                            && !content.trim().is_empty() =>
                        {
                            rows.push(row(ROLE_USER, KIND_ANSWER, content));
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant if wants(ROLE_ASSISTANT) => {
                for block in &msg.content {
                    if let ContentBlock::Text { text } = block
                        && !text.trim().is_empty()
                    {
                        rows.push(row(ROLE_ASSISTANT, KIND_SAID, text));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(limit) = query.limit
        && rows.len() > limit
    {
        rows.drain(..rows.len() - limit);
    }
    rows
}

pub fn to_json(messages: &[Message], query: &MessagesQuery) -> serde_json::Value {
    serde_json::json!(views(messages, query))
}

/// Backs `maki.session.messages` for the single session headless drivers
/// (`maki -p`, sdk mode), which read the agent's live mirror.
pub struct HeadlessMessages {
    pub id: String,
    pub history: SharedMessages,
}

impl HeadlessMessages {
    pub fn install(self, handle: &EventHandle) {
        let history = Arc::clone(&self.history);
        let id = self.id;
        handle.install_session_messages(Box::new(move |want, query| {
            // One session here, so any other id names a tab that is not ours.
            if let Some(want) = want
                && want != id
            {
                return Err(format!("session {want} not live"));
            }
            Ok(to_json(&history.load().messages, query))
        }));
    }
}

#[cfg(test)]
mod tests {
    use maki_providers::{ContentBlock, Message, Role};
    use serde_json::json;

    use super::*;

    const QUESTION_ID: &str = "q1";

    fn conversation() -> Vec<Message> {
        vec![
            Message::user("rebase my PRs and push".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "I'll resolve the conflict first.".into(),
                    },
                    ContentBlock::tool_use(
                        QUESTION_ID,
                        maki_agent::tools::QUESTION_TOOL_NAME,
                        json!({}),
                    ),
                    ContentBlock::tool_use("r1", "read", json!({})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: QUESTION_ID.into(),
                        content: "yes, you have full access".into(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "r1".into(),
                        content: "file contents a plugin must not read as the user".into(),
                        is_error: false,
                    },
                ],
                ..Default::default()
            },
            Message::observation("[host noticed something]".into()),
            Message::user("go ahead".into()),
        ]
    }

    /// The rows a plugin builds reviewer context out of: what the human
    /// typed, what it answered through the `question` tool, and the
    /// assistant's own last text. Tool output is none of those.
    #[test]
    fn rows_carry_role_and_where_the_text_came_from() {
        let rows = views(&conversation(), &MessagesQuery::default());
        let shape: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| (r.role, r.kind, r.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (ROLE_USER, KIND_TYPED, "rebase my PRs and push"),
                (
                    ROLE_ASSISTANT,
                    KIND_SAID,
                    "I'll resolve the conflict first."
                ),
                (ROLE_USER, KIND_ANSWER, "yes, you have full access"),
                (ROLE_USER, KIND_OBSERVATION, "[host noticed something]"),
                (ROLE_USER, KIND_TYPED, "go ahead"),
            ]
        );
    }

    /// `limit` answers with the tail, still oldest first, so the last row is
    /// always the most recent thing said.
    #[test]
    fn limit_and_role_narrow_the_window() {
        let messages = conversation();
        let rows = views(
            &messages,
            &MessagesQuery {
                limit: Some(2),
                role: Some(ROLE_USER.to_owned()),
            },
        );
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["[host noticed something]", "go ahead"]);
        assert!(rows.iter().all(|r| r.role == ROLE_USER));
    }

    #[test]
    fn a_pasted_wall_of_text_is_cut_and_says_so() {
        let messages = vec![Message::user("x".repeat(TEXT_MAX_BYTES * 2))];
        let rows = views(&messages, &MessagesQuery::default());
        assert_eq!(rows[0].text.len(), TEXT_MAX_BYTES);
        assert!(rows[0].truncated);
    }
}
