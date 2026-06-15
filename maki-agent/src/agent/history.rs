use std::sync::Arc;

use arc_swap::ArcSwap;
use maki_providers::{ContentBlock, Message, Role};
use tracing::warn;

const CANCEL_MARKER: &str = "[Cancelled by user]";
const UNAVAILABLE_RESULT: &str = "[Tool result not available]";

pub type SharedMessages = Arc<ArcSwap<Vec<Message>>>;

pub struct History {
    messages: Vec<Message>,
    mirror: Option<SharedMessages>,
}

impl History {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            mirror: None,
        }
    }

    pub fn restored(mut messages: Vec<Message>) -> Self {
        sanitize_restored(&mut messages);
        Self {
            messages,
            mirror: None,
        }
    }

    pub fn with_mirror(mut self, mirror: SharedMessages) -> Self {
        self.mirror = Some(mirror);
        self.publish();
        self
    }

    pub fn as_slice(&self) -> &[Message] {
        &self.messages
    }

    pub fn push(&mut self, msg: Message) {
        self.edit(|msgs| msgs.push(msg));
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.edit(|msgs| *msgs = messages);
    }

    pub fn truncate(&mut self, len: usize) {
        self.edit(|msgs| msgs.truncate(len));
    }

    pub fn into_vec(self) -> Vec<Message> {
        self.messages
    }

    fn edit(&mut self, f: impl FnOnce(&mut Vec<Message>)) {
        f(&mut self.messages);
        self.publish();
    }

    fn publish(&self) {
        let Some(mirror) = &self.mirror else { return };
        let mut snapshot = self.messages.clone();
        close_dangling_tool_calls(&mut snapshot, UNAVAILABLE_RESULT);
        mirror.store(Arc::new(snapshot));
    }
}

/// Restored sessions can have orphaned tool_results or unclosed tool_uses
/// (e.g. the process was killed mid-turn). The API returns 400 if it sees those.
fn sanitize_restored(messages: &mut Vec<Message>) {
    let len_before = messages.len();
    let mut i = 0;
    while i < messages.len() {
        if !matches!(messages[i].role, Role::User) {
            i += 1;
            continue;
        }

        let valid_ids: Vec<String> = if i > 0 && matches!(messages[i - 1].role, Role::Assistant) {
            messages[i - 1]
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        } else {
            Vec::new()
        };

        messages[i].content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                valid_ids.iter().any(|id| id == tool_use_id)
            }
            _ => true,
        });

        if messages[i].content.is_empty() {
            messages.remove(i);
        } else {
            i += 1;
        }
    }

    close_dangling_tool_calls(messages, UNAVAILABLE_RESULT);

    if messages.len() != len_before {
        warn!(
            before = len_before,
            after = messages.len(),
            "sanitized restored history"
        );
    }
}

fn close_dangling_tool_calls(messages: &mut Vec<Message>, note: &str) {
    let Some(last) = messages.last() else { return };
    if !matches!(last.role, Role::Assistant) || !last.has_tool_calls() {
        return;
    }
    let error_results: Vec<ContentBlock> = last
        .tool_uses()
        .map(|(id, _, _)| ContentBlock::ToolResult {
            tool_use_id: id.to_owned(),
            content: note.to_owned(),
            is_error: true,
        })
        .collect();
    messages.push(Message {
        role: Role::User,
        content: error_results,
        display_text: Some(String::new()),
    });
}

pub(crate) fn sanitize_cancelled_history(history: &mut History, rollback_len: usize) {
    if history.len() <= rollback_len {
        return;
    }
    history.edit(|msgs| {
        close_dangling_tool_calls(msgs, CANCEL_MARKER);
        msgs.push(Message::synthetic(CANCEL_MARKER.into()));
    });
}

#[cfg(test)]
mod tests {
    use maki_providers::{ContentBlock, Message, Role};
    use test_case::test_case;

    use super::*;

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == CANCEL_MARKER));
    }

    fn make_tool_use_msg(ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn make_tool_result_msg(ids: &[&str]) -> Message {
        Message {
            role: Role::User,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: "ok".into(),
                    is_error: false,
                })
                .collect(),
            display_text: Some(String::new()),
        }
    }

    fn make_mirror() -> SharedMessages {
        Arc::new(ArcSwap::from_pointee(Vec::new()))
    }

    #[track_caller]
    fn extract_error_ids(msg: &Message) -> Vec<&str> {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test_case(
        vec![Message::user("old".into())],
        1,
        1,
        false
        ; "no_new_messages_is_noop"
    )]
    #[test_case(
        vec![Message::user("hello".into())],
        0,
        2,
        true
        ; "user_only_appends_marker"
    )]
    #[test_case(
        vec![
            Message::user("hello".into()),
            Message { role: Role::Assistant, content: vec![ContentBlock::Text { text: "hi".into() }], ..Default::default() },
        ],
        0,
        3,
        true
        ; "complete_turn_appends_marker"
    )]
    fn sanitize_cancelled_history_cases(
        messages: Vec<Message>,
        rollback_len: usize,
        expected_len: usize,
        expect_cancel_marker: bool,
    ) {
        let mut history = History::new(messages);
        sanitize_cancelled_history(&mut history, rollback_len);
        assert_eq!(history.len(), expected_len);
        if expect_cancel_marker {
            assert_ends_with_cancel_marker(&history);
        }
    }

    #[test]
    fn sanitize_dangling_tool_use_adds_error_results() {
        let mut history = History::new(vec![
            Message::user("hello".into()),
            make_tool_use_msg(&["t1", "t2"]),
        ]);
        sanitize_cancelled_history(&mut history, 0);

        assert_eq!(extract_error_ids(&history.as_slice()[2]), ["t1", "t2"]);
        assert_ends_with_cancel_marker(&history);
    }

    #[test]
    fn mirror_sequential_mutations_always_consistent() {
        let mirror = make_mirror();
        let mut history = History::new(Vec::new()).with_mirror(Arc::clone(&mirror));

        for i in 0..10 {
            history.push(Message::user(format!("msg-{i}")));
            assert_eq!(mirror.load().len(), i + 1);
        }

        history.truncate(3);
        assert_eq!(mirror.load().len(), 3);

        history.replace(vec![Message::user("fresh".into())]);
        assert_eq!(mirror.load().len(), 1);

        history.push(make_tool_use_msg(&["t_final"]));
        assert_eq!(history.len(), 2, "history has 2");
        assert_eq!(mirror.load().len(), 3, "mirror has 3 (dangling closed)");
    }

    #[test]
    fn snapshot_closes_dangling_tool_uses_without_mutating_history() {
        let mirror = make_mirror();
        let history = History::new(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1", "t2"]),
        ])
        .with_mirror(Arc::clone(&mirror));

        assert_eq!(history.len(), 2, "history itself unchanged");

        let snap = mirror.load();
        assert_eq!(snap.len(), 3, "snapshot has extra closing message");

        let closing = &snap[2];
        assert!(matches!(closing.role, Role::User));
        assert_eq!(extract_error_ids(closing), ["t1", "t2"]);
        assert_eq!(closing.display_text.as_deref(), Some(""));
    }

    #[test]
    fn snapshot_not_dangling_when_tool_result_already_present() {
        let mirror = make_mirror();
        let mut history =
            History::new(vec![Message::user("go".into()), make_tool_use_msg(&["t1"])])
                .with_mirror(Arc::clone(&mirror));

        assert_eq!(mirror.load().len(), 3, "dangling before result");

        history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "file contents".into(),
                is_error: false,
            }],
            ..Default::default()
        });

        let snap = mirror.load();
        assert_eq!(snap.len(), 3, "no extra closing after real result");
    }

    #[test]
    fn into_vec_returns_inner_not_snapshot() {
        let mirror = make_mirror();
        let history = History::new(vec![Message::user("go".into()), make_tool_use_msg(&["t1"])])
            .with_mirror(Arc::clone(&mirror));

        assert_eq!(mirror.load().len(), 3, "snapshot has closing message");
        assert_eq!(history.into_vec().len(), 2, "into_vec returns raw messages");
    }

    #[test]
    fn sanitize_cancelled_on_mirrored_history() {
        let mirror = make_mirror();
        let mut history =
            History::new(vec![Message::user("go".into()), make_tool_use_msg(&["t1"])])
                .with_mirror(Arc::clone(&mirror));

        sanitize_cancelled_history(&mut history, 0);

        let snap = mirror.load();
        assert_eq!(snap.len(), history.len(), "mirror matches history length");

        let last = snap.last().unwrap();
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == CANCEL_MARKER));

        let tool_result_msg = &snap[snap.len() - 2];
        assert!(tool_result_msg.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { content, is_error: true, .. } if content == CANCEL_MARKER
        )));
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }

    #[test_case(
        vec![make_tool_result_msg(&["t1"])],
        0
        ; "orphan_at_start_removed"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            text_msg(Role::Assistant, "done"),
            make_tool_result_msg(&["orphan1", "orphan2"]),
        ],
        2
        ; "orphans_after_non_tool_assistant_removed"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1", "t2"]),
            make_tool_result_msg(&["t1", "t2"]),
        ],
        3
        ; "valid_pairing_preserved"
    )]
    #[test_case(
        vec![Message::user("go".into()), make_tool_use_msg(&["t1"])],
        3
        ; "dangling_tool_use_closed_with_synthetic_result"
    )]
    fn sanitize_restored_cases(messages: Vec<Message>, expected_len: usize) {
        let history = History::restored(messages);
        assert_eq!(history.len(), expected_len);
    }

    #[test]
    fn sanitize_restored_partial_orphan_keeps_matched_ids() {
        let history = History::restored(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1", "t2"]),
        ]);
        let results: Vec<&str> = history.as_slice()[2]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ["t1"]);
    }
}
