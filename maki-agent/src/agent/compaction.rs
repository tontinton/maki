use std::env;

use maki_config::CompactionBuffer;
use maki_providers::{
    ContentBlock, Message, Model, RequestOptions, Role, StreamResponse, TokenUsage,
};
use tracing::info;

use super::history::History;
use super::streaming::stream_with_retry;
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender, TurnCompleteEvent};

pub(super) const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if unsure. Restore todo lists with todo_write. Save important context to memory before it's lost.";

const MINIMAL_CONTEXT_RATIO: f64 = 0.2;
const AGGRESSIVE_CONTEXT_RATIO: f64 = 0.4;
const NORMAL_BUDGET_RATIO: f64 = 0.15;
const AGGRESSIVE_BUDGET_RATIO: f64 = 0.10;
const MINIMAL_BUDGET_RATIO: f64 = 0.05;
const MIN_COMPACTION_BUDGET: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTier {
    Normal,
    Aggressive,
    Minimal,
}

impl CompactionTier {
    #[must_use]
    pub fn from_remaining_context(remaining: u32, context_window: u32) -> Self {
        if context_window == 0 {
            return Self::Normal;
        }
        let ratio = f64::from(remaining) / f64::from(context_window);
        if ratio < MINIMAL_CONTEXT_RATIO {
            Self::Minimal
        } else if ratio < AGGRESSIVE_CONTEXT_RATIO {
            Self::Aggressive
        } else {
            Self::Normal
        }
    }

    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn token_budget(self, context_window: u32) -> u32 {
        if context_window == 0 {
            return 0;
        }
        let ratio = match self {
            Self::Normal => NORMAL_BUDGET_RATIO,
            Self::Aggressive => AGGRESSIVE_BUDGET_RATIO,
            Self::Minimal => MINIMAL_BUDGET_RATIO,
        };
        ((f64::from(context_window) * ratio) as u32).max(MIN_COMPACTION_BUDGET)
    }
}

const IMAGE_PLACEHOLDER: &str = "[image]";

pub(super) async fn compact_history(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();
    let mut compaction_history: Vec<Message> = history.as_slice().to_vec();
    strip_images(&mut compaction_history);
    strip_thinking(&mut compaction_history);
    strip_old_tool_results(&mut compaction_history);

    let context_window = model.context_window;
    let current_usage = crate::agent::run::estimate_message_tokens(&compaction_history);
    let remaining = context_window.saturating_sub(current_usage);
    let tier = CompactionTier::from_remaining_context(remaining, context_window);
    let budget = tier.token_budget(context_window);

    let user_prompt = format!(
        "{}\n\nToken budget: {} tokens (tier: {:?}, remaining: {}/{} context)",
        crate::prompt::COMPACTION_USER,
        budget,
        tier,
        remaining,
        context_window
    );
    compaction_history.push(Message::user(user_prompt));

    let empty_tools = serde_json::json!([]);
    let max_attempts = 3;
    let mut last_error = None;

    for attempt in 0..max_attempts {
        match stream_with_retry(
            provider,
            model,
            &compaction_history,
            crate::prompt::COMPACTION_SYSTEM,
            &empty_tools,
            event_tx,
            cancel,
            RequestOptions::default(),
            None,
        )
        .await
        {
            Ok(response) => {
                if attempt > 0 {
                    info!(
                        attempt,
                        "compaction succeeded after truncating oldest rounds"
                    );
                }
                return Ok(finish_compact(
                    response,
                    history,
                    event_tx,
                    compact_start,
                    model,
                ));
            }
            Err(e) if e.is_context_overflow() && attempt < max_attempts - 1 => {
                last_error = Some(e);
                truncate_oldest_round(&mut compaction_history);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_error.unwrap())
}

fn finish_compact(
    response: StreamResponse,
    history: &mut History,
    event_tx: &EventSender,
    compact_start: std::time::Instant,
    model: &Model,
) -> TokenUsage {
    let _ = event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        context_size: Some(response.usage.output),
    })));

    let new_history = vec![
        Message::user("What did we do so far?".into()),
        response.message,
    ];
    history.replace(new_history);
    info!(
        model = %model.id,
        duration_ms = compact_start.elapsed().as_millis() as u64,
        "compaction completed"
    );

    response.usage
}

pub async fn compact(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
) -> Result<(), AgentError> {
    let cancel = CancelToken::none();
    let usage = compact_history(provider, model, history, event_tx, &cancel).await?;

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        stop_reason: None,
    })?;

    Ok(())
}

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, buffer: CompactionBuffer) -> bool {
    let usable = model
        .context_window
        .saturating_sub(buffer.resolve(model.context_window));
    usage.context_tokens() >= usable
}

fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            if matches!(block, ContentBlock::Image { .. }) {
                *block = ContentBlock::Text {
                    text: IMAGE_PLACEHOLDER.into(),
                };
            }
        }
    }
}

fn strip_thinking(messages: &mut [Message]) {
    for msg in messages {
        msg.content.retain(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
}

const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";
const KEEP_LAST_TOOL_RESULTS: usize = 3;

fn strip_old_tool_results(messages: &mut [Message]) {
    let total: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .count();

    let mut seen = 0;
    for msg in messages {
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                if seen < total.saturating_sub(KEEP_LAST_TOOL_RESULTS) {
                    *content = TOOL_RESULT_PLACEHOLDER.into();
                }
                seen += 1;
            }
        }
    }
}

fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let mut remove_count = 1;

    if matches!(messages.first().map(|m| &m.role), Some(Role::Assistant)) {
        let has_tool_calls = messages[0].has_tool_calls();
        if has_tool_calls {
            let next_has_tool_results = messages.get(1).is_some_and(|m| {
                matches!(m.role, Role::User)
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            });
            if next_has_tool_results {
                remove_count = 2;
            }
        }
    } else if matches!(messages.first().map(|m| &m.role), Some(Role::User))
        && matches!(messages.get(1).map(|m| &m.role), Some(Role::Assistant))
    {
        // Dropping a lone user message would leave assistant-first, which some providers reject.
        // Remove the assistant too to keep the conversation well-formed.
        remove_count = 2;
    }

    messages.drain(..remove_count);

    // After draining, the first message might still be an assistant (e.g. consecutive
    // assistant messages). Keep draining until the first message is user or we're empty.
    while messages.len() > 1 && matches!(messages.first().map(|m| &m.role), Some(Role::Assistant)) {
        let mut drop = 1;
        if matches!(messages.get(1).map(|m| &m.role), Some(Role::User)) {
            drop = 2;
        }
        messages.drain(..drop);
    }
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("MAKI_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use maki_storage::id::SessionRef;
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
            _: Option<&'a SessionRef>,
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

    #[test_case(0,     0, CompactionTier::Normal    ; "zero_context_window_defaults_normal")]
    #[test_case(0,   100, CompactionTier::Minimal    ; "empty_remaining_is_minimal")]
    #[test_case(10,  100, CompactionTier::Minimal    ; "low_ratio_minimal")]
    #[test_case(19,  100, CompactionTier::Minimal    ; "just_below_0_2")]
    #[test_case(20,  100, CompactionTier::Aggressive ; "at_0_2_boundary")]
    #[test_case(30,  100, CompactionTier::Aggressive ; "mid_ratio_aggressive")]
    #[test_case(39,  100, CompactionTier::Aggressive ; "just_below_0_4")]
    #[test_case(40,  100, CompactionTier::Normal     ; "at_0_4_boundary")]
    #[test_case(50,  100, CompactionTier::Normal     ; "high_ratio_normal")]
    #[test_case(199, 1000, CompactionTier::Minimal    ; "just_below_0_2_at_1000")]
    #[test_case(200, 1000, CompactionTier::Aggressive ; "at_0_2_boundary_1000")]
    #[test_case(399, 1000, CompactionTier::Aggressive ; "just_below_0_4_at_1000")]
    #[test_case(400, 1000, CompactionTier::Normal     ; "at_0_4_boundary_1000")]
    #[test_case(1999, 10000, CompactionTier::Minimal    ; "just_below_0_2_at_10000")]
    #[test_case(2000, 10000, CompactionTier::Aggressive ; "at_0_2_boundary_10000")]
    #[test_case(3999, 10000, CompactionTier::Aggressive ; "just_below_0_4_at_10000")]
    #[test_case(4000, 10000, CompactionTier::Normal     ; "at_0_4_boundary_10000")]
    fn compaction_tier_from_remaining(
        remaining: u32,
        context_window: u32,
        expected: CompactionTier,
    ) {
        assert_eq!(
            CompactionTier::from_remaining_context(remaining, context_window),
            expected
        );
    }

    #[test_case(CompactionTier::Normal,     100, 15 ; "normal_budget")]
    #[test_case(CompactionTier::Aggressive, 100, 10 ; "aggressive_budget")]
    #[test_case(CompactionTier::Minimal,    100,  5 ; "minimal_budget")]
    #[test_case(CompactionTier::Minimal,      1,  1 ; "tiny_window_minimum_budget")]
    #[test_case(CompactionTier::Minimal,      2,  1 ; "small_window_still_minimum")]
    #[test_case(CompactionTier::Normal,       0,  0 ; "zero_window_zero_budget")]
    fn compaction_tier_budget(tier: CompactionTier, context_window: u32, expected: u32) {
        assert_eq!(tier.token_budget(context_window), expected);
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
    fn compact_replaces_history_with_summary() {
        smol::block_on(async {
            let provider: std::sync::Arc<dyn Provider> =
                std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
            let model = default_model();
            let (raw_tx, _rx) = flume::unbounded();
            let mut history = History::new(vec![
                Message::user("first".into()),
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

            let msgs = history.as_slice();
            assert_eq!(msgs.len(), 2);
            assert!(matches!(msgs[0].role, Role::User));
            assert!(matches!(msgs[1].role, Role::Assistant));
        });
    }

    #[test_case(159_999, 0,       0,       0,      200_000, false ; "below_threshold")]
    #[test_case(160_000, 0,       0,       0,      200_000, true  ; "at_threshold")]
    #[test_case(100,     0,       0,       0,      100,     true  ; "tiny_context_window")]
    #[test_case(5_000,   165_000, 10_000,  0,      200_000, true  ; "cached_tokens_count_toward_overflow")]
    #[test_case(100_000, 0,       0,       80_000, 200_000, true  ; "output_tokens_count_toward_overflow")]
    #[test_case(262_144, 0,       0,       0,      262_144, true  ; "equal_context_and_max_output")]
    #[test_case(51_199,  0,       0,       0,      64_000,  false ; "small_window_below_scaled_threshold")]
    #[test_case(51_200,  0,       0,       0,      64_000,  true  ; "small_window_at_scaled_threshold")]
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

    #[test_case(CompactionBuffer::Tokens(10_000), 53_999, false ; "explicit_tokens_below")]
    #[test_case(CompactionBuffer::Tokens(10_000), 54_000, true  ; "explicit_tokens_honored")]
    #[test_case(CompactionBuffer::Percent(50),    32_000, true  ; "explicit_percent_at_threshold")]
    fn overflow_with_explicit_buffer(buffer: CompactionBuffer, input: u32, expected: bool) {
        let model = small_context_model(64_000);
        let usage = TokenUsage {
            input,
            ..Default::default()
        };
        assert_eq!(is_overflow(&usage, &model, buffer), expected);
    }

    #[test]
    fn strip_images_replaces_with_placeholder() {
        use maki_providers::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
        let mut messages = vec![Message::user_with_images("hello".into(), vec![source])];
        strip_images(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        );
        assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "hello"));
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
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            ],
            ..Default::default()
        }];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_old_tool_results_keeps_newest() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old result 1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old result 2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep 1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t4".into(),
                    content: "keep 2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t5".into(),
                    content: "keep 3".into(),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "keep me".into(),
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert_eq!(messages[0].content.len(), 6);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t1")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t2")
        );
        assert!(
            matches!(&messages[0].content[2], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 1" && tool_use_id == "t3")
        );
        assert!(
            matches!(&messages[0].content[3], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 2" && tool_use_id == "t4")
        );
        assert!(
            matches!(&messages[0].content[4], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 3" && tool_use_id == "t5")
        );
        assert!(
            matches!(&messages[0].content[5], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_removes_single_user_message() {
        let mut messages = vec![
            Message::user("first".into()),
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
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
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
            Message::user("no tool result".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "no tool result")
        );
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![Message::user("only".into())];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_plain_assistant() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_consecutive_assistants_drains_until_user() {
        // [User, Assistant(no tools), Assistant(tools), User(results)] drains 2,
        // leaving Assistant-first — keep draining until first is User.
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "plain reply".into(),
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
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert!(!messages.is_empty());
        assert!(matches!(messages[0].role, Role::User));
    }

    #[test]
    fn compact_produces_user_assistant_pair() {
        smol::block_on(async {
            let provider: std::sync::Arc<dyn Provider> =
                std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
            let model = default_model();
            let (raw_tx, _rx) = flume::unbounded();
            let mut history = History::new(vec![
                Message::user("first".into()),
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

            let msgs = history.as_slice();
            assert_eq!(msgs.len(), 2);
            assert!(matches!(msgs[0].role, Role::User));
            assert!(matches!(msgs[1].role, Role::Assistant));
            assert!(
                msgs[0]
                    .first_text_content()
                    .unwrap()
                    .contains("What did we do so far?")
            );
        });
    }
}
