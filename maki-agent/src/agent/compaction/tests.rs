use std::sync::Mutex;

use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason, StreamResponse,
    TokenUsage,
};
use serde_json::Value;
use test_case::test_case;

use super::*;
use crate::AgentConfig;

struct MockProvider {
    responses: Mutex<Vec<StreamResponse>>,
}

impl MockProvider {
    fn new(responses: Vec<StreamResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Provider for MockProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a Value,
        _: &'a flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a str>,
        _: maki_providers::CancellationToken,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async {
            let mut responses = self.responses.lock().unwrap();
            assert!(!responses.is_empty(), "MockProvider: no more responses");
            Ok(responses.remove(0))
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
        Box::pin(async { unimplemented!() })
    }
}

fn default_model() -> Model {
    Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
}

fn small_context_model(context_window: u32) -> Model {
    let mut model = default_model();
    model.context_window = context_window;
    model
}

fn text_response(stop_reason: StopReason) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response".into(),
            }],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(stop_reason),
    }
}

#[test]
fn compact_appends_summary_and_continues() {
    smol::block_on(async {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user(FIRST_PROMPT.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
        ]);

        compact(
            &*provider,
            &model,
            &mut history,
            &EventSender::new(raw_tx, 0),
        )
        .await
        .unwrap();

        let msgs = history.active_branch();
        assert!(
            msgs.len() >= 2,
            "compaction must keep at least the narrative and a continuation turn"
        );
        assert!(matches!(msgs[0].role, Role::User));
    });
}

const FIRST_PROMPT: &str = "first";
const KEEP_ME: &str = "keep me";
const NO_TOOL_RESULT: &str = "no tool result";
const ONLY: &str = "only";
const PLAIN_REPLY: &str = "plain reply";
const HELLO: &str = "hello";
const BRANCH_NARRATIVE: &str = "abandoned branch was about X";
const ABANDONED_USER_MSG: &str = "abandoned question";
const LANDING_USER_MSG: &str = "landing question";

#[test_case(159_999, 0,       0,       0,      200_000, false ; "below_threshold")]
#[test_case(160_000, 0,       0,       0,      200_000, true  ; "at_threshold")]
#[test_case(100,     0,       0,       0,      100,     true  ; "tiny_context_window")]
#[test_case(5_000,   165_000, 10_000,  0,      200_000, true  ; "cached_tokens_count_toward_overflow")]
#[test_case(100_000, 0,       0,       80_000, 200_000, true  ; "output_tokens_count_toward_overflow")]
#[test_case(262_144, 0,       0,       0,      262_144, true  ; "equal_context_and_max_output")]
fn overflow_detection(
    input: u32,
    cache_read: u32,
    cache_creation: u32,
    output: u32,
    ctx_window: u32,
    expected: bool,
) {
    let model = small_context_model(ctx_window);
    let usage = TokenUsage {
        input,
        output,
        cache_read,
        cache_creation,
    };
    assert_eq!(
        is_overflow(&usage, &model, AgentConfig::default().compaction_buffer),
        expected
    );
}

#[test]
fn strip_images_replaces_with_placeholder() {
    use maki_providers::{ImageMediaType, ImageSource};
    use std::sync::Arc;
    let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
    let mut messages = vec![Message::user_with_images(HELLO.into(), vec![source])];
    strip_images(&mut messages);
    assert_eq!(messages[0].content.len(), 2);
    assert!(
        matches!(&messages[0].content[0], ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
    );
    assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == HELLO));
}

#[test]
fn strip_thinking_removes_thinking_blocks() {
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("sig".into()),
            },
            ContentBlock::Text { text: HELLO.into() },
            ContentBlock::RedactedThinking {
                data: "opaque".into(),
            },
        ],
        ..Default::default()
    }];
    strip_thinking(&mut messages);
    assert_eq!(messages[0].content.len(), 1);
    assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == HELLO));
}

#[test]
fn truncate_oldest_round_removes_single_user_message() {
    let mut messages = vec![
        Message::user(FIRST_PROMPT.into()),
        Message::user("second".into()),
    ];
    truncate_oldest_round(&mut messages);
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "second"));
}

#[test]
fn truncate_oldest_round_removes_assistant_tool_pair() {
    let mut messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "output".into(),
                is_error: false,
            }],
            ..Default::default()
        },
        Message::user(KEEP_ME.into()),
    ];
    truncate_oldest_round(&mut messages);
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == KEEP_ME));
}

#[test]
fn truncate_oldest_round_removes_assistant_without_matching_tool_result() {
    let mut messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            ..Default::default()
        },
        Message::user(NO_TOOL_RESULT.into()),
    ];
    truncate_oldest_round(&mut messages);
    assert_eq!(messages.len(), 1);
    assert!(
        matches!(&messages[0].content[0], ContentBlock::Text { text } if text == NO_TOOL_RESULT)
    );
}

#[test]
fn truncate_oldest_round_noop_on_single_message() {
    let mut messages = vec![Message::user(ONLY.into())];
    truncate_oldest_round(&mut messages);
    assert_eq!(messages.len(), 1);
}

#[test]
fn truncate_oldest_round_removes_plain_assistant() {
    let mut messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: PLAIN_REPLY.into(),
            }],
            ..Default::default()
        },
        Message::user(KEEP_ME.into()),
    ];
    truncate_oldest_round(&mut messages);
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == KEEP_ME));
}

#[test]
fn truncate_oldest_round_consecutive_assistants_drains_until_user() {
    let mut messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: PLAIN_REPLY.into(),
            }],
            ..Default::default()
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "output".into(),
                is_error: false,
            }],
            ..Default::default()
        },
        Message::user(KEEP_ME.into()),
    ];
    truncate_oldest_round(&mut messages);
    assert!(!messages.is_empty());
    assert!(matches!(messages[0].role, Role::User));
}

fn narrative_response(text: &str) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(StopReason::EndTurn),
    }
}

#[test]
fn branch_summary_appends_narrative_in_place() {
    smol::block_on(async {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(MockProvider::new(vec![narrative_response(
                BRANCH_NARRATIVE,
            )]));
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user(LANDING_USER_MSG.into()),
            Message::user(ABANDONED_USER_MSG.into()),
        ]);

        let parent = history.test_find_msg_by_content(LANDING_USER_MSG).unwrap();
        let leaf = history.test_leaf_ref().unwrap();
        history.test_rewind_leaf_to(parent.clone());

        branch_summary(
            &*provider,
            &model,
            &mut history,
            parent,
            leaf,
            &EventSender::new(raw_tx, 0),
            &CancelToken::none(),
        )
        .await
        .unwrap();

        let ctx = history.active_branch();
        assert!(
            ctx.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text == BRANCH_NARRATIVE
            ))),
            "branch-summary narrative must fold in place"
        );
        assert!(
            !ctx.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text == ABANDONED_USER_MSG
            ))),
            "abandoned message must not appear in the fold"
        );
    });
}

#[test]
fn branch_summary_aborts_on_cancel_leaves_no_record() {
    smol::block_on(async {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(MockProvider::new(vec![narrative_response(
                BRANCH_NARRATIVE,
            )]));
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user(LANDING_USER_MSG.into()),
            Message::user(ABANDONED_USER_MSG.into()),
        ]);

        let parent = history.test_find_msg_by_content(LANDING_USER_MSG).unwrap();
        let leaf = history.test_leaf_ref().unwrap();
        history.test_rewind_leaf_to(parent.clone());

        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();

        let result = branch_summary(
            &*provider,
            &model,
            &mut history,
            parent,
            leaf,
            &EventSender::new(raw_tx, 0),
            &cancel,
        )
        .await;

        assert!(result.is_err(), "cancelled summary must return an error");
        let ctx = history.active_branch();
        assert!(
            !ctx.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text == BRANCH_NARRATIVE
            ))),
            "no narrative appended after cancel"
        );
    });
}
