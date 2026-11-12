use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use maki_agent::agent::{AgentHooks, History, RunContext, compact};
use maki_agent::tools::{RequestTools, ToolRegistry};
use maki_agent::{AgentConfig, CancelToken, EventSender};
use maki_lua::PluginHost;
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::retry::RetryPolicy;
use maki_providers::{
    AgentError, CHARS_PER_TOKEN, ContentBlock, ContextGauge, Message, Model, ModelInfo,
    ProviderEvent, RequestOptions, Role, StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use test_case::test_case;

const OVERFLOW_STATUS: u16 = 413;
const OVERFLOW_MESSAGE: &str = "prompt is too long";
const SUMMARY: &str = "summary";
const MODEL: &str = "anthropic/claude-sonnet-4-20250514";
const OLD: &str = "old instructions";
const LATEST: &str = "Preserve these instructions verbatim.\nIncluding unicode: π.";
const FAILED: &str = "a failed reload must not replace the instructions";
const CARRY_BUDGET: usize = 64 * 1024;
const SMALL_WINDOW: u32 = 20_000;
const SMALL_WINDOW_RESULT: usize = 5_000;
const SOURCE: &str = r#"
maki.api.register_tool({
  name = "remember",
  description = "Retain a named set of instructions",
  durable = "name",
  schema = { type = "object", properties = {
    name = { type = "string" },
    count = { type = "integer" },
  } },
  handler = function() return "unused" end,
})
"#;

struct SummaryProvider {
    overflow: AtomicBool,
}

impl Provider for SummaryProvider {
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
        Box::pin(async move {
            if self.overflow.swap(false, Ordering::Relaxed) {
                return Err(AgentError::api(OVERFLOW_STATUS, OVERFLOW_MESSAGE));
            }
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: SUMMARY.into(),
                    }],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn add_result(history: &mut History, id: &str, name: &str, text: &str, is_error: bool) {
    history.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(
            id,
            "remember",
            json!({ "name": name }),
        )],
        ..Default::default()
    });
    history.push(Message {
        role: Role::User,
        content: vec![ContentBlock::tool_result(id, text, is_error)],
        ..Default::default()
    });
}

fn summarize(registry: &ToolRegistry, history: &mut History, overflow: bool) {
    let model = Model::from_spec(MODEL).unwrap();
    summarize_for(&model, "", registry, history, overflow);
}

fn summarize_for(
    model: &Model,
    system: &str,
    registry: &ToolRegistry,
    history: &mut History,
    overflow: bool,
) {
    maki_lua::test_support::runtime().block_on(async {
        let config = AgentConfig::default();
        let cancel = CancelToken::none();
        let (tx, _rx) = flume::unbounded();
        compact(
            &SummaryProvider {
                overflow: AtomicBool::new(overflow),
            },
            model,
            history,
            &mut ContextGauge::default(),
            RunContext::fixed(
                system.to_owned(),
                RequestTools::assembled(json!([]), &config, model),
            ),
            None,
            &EventSender::new(tx, 0),
            &AgentHooks {
                registry,
                session_id: None,
                task_id: None,
                model,
                cancel: &cancel,
                context_size: 0,
            },
            &config,
            None,
            RetryPolicy::default(),
        )
        .await
        .unwrap();
    });
}

#[test_case(false ; "normal_summary")]
#[test_case(true ; "overflow_prunes_only_the_summary_input")]
fn a_declaration_retains_results_while_the_tool_stays_durable(overflow: bool) {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("durable", SOURCE).unwrap();
    let mut history = History::new(Vec::new());
    add_result(&mut history, "old", "release", OLD, false);
    add_result(&mut history, "other", "other", "other instructions", false);
    add_result(&mut history, "new", "release", LATEST, false);
    add_result(&mut history, "failed", "release", FAILED, true);
    add_result(&mut history, "empty", "release", "", false);
    let legacy = serde_json::to_value(history.as_slice()).unwrap();
    assert!(
        legacy
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("durable").is_none())
    );
    history = History::restored(serde_json::from_value(legacy).unwrap());
    summarize(&registry, &mut history, overflow);
    let expected = [
        ("remember:other", "other instructions"),
        ("remember:release", LATEST),
    ];
    for _ in 0..2 {
        assert_eq!(retained(&history), expected);
        assert!(
            !history
                .as_slice()
                .iter()
                .any(|message| message.first_text_content() == Some(FAILED))
        );
        let saved = serde_json::to_value(history.as_slice()).unwrap();
        history = History::restored(serde_json::from_value(saved).unwrap());
        summarize(&registry, &mut history, false);
    }
    summarize(&ToolRegistry::new(), &mut history, false);
    assert!(retained(&history).is_empty());
}

fn retained(history: &History) -> Vec<(&str, &str)> {
    history
        .as_slice()
        .iter()
        .filter_map(|message| Some((message.durable.as_deref()?, message.first_text_content()?)))
        .collect()
}

#[test]
fn carried_results_keep_the_newest_within_the_budget() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("durable", SOURCE).unwrap();
    let mut history = History::new(Vec::new());
    add_result(
        &mut history,
        "huge",
        "huge",
        &"h".repeat(CARRY_BUDGET + 1),
        false,
    );
    add_result(
        &mut history,
        "older",
        "older",
        &"o".repeat(CARRY_BUDGET / 2 + 1),
        false,
    );
    add_result(
        &mut history,
        "newer",
        "newer",
        &"n".repeat(CARRY_BUDGET / 2),
        false,
    );
    summarize(&registry, &mut history, false);
    let keys: Vec<_> = retained(&history).into_iter().map(|(key, _)| key).collect();
    assert_eq!(keys, ["remember:newer"]);
}

#[test_case("42" ; "wrong_declaration_type")]
#[test_case("\"missing\"" ; "missing_input_field")]
#[test_case("\"count\"" ; "non_string_input_field")]
fn durable_declarations_require_a_string_field(declaration: &str) {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = SOURCE.replace("durable = \"name\"", &format!("durable = {declaration}"));
    let error = host
        .load_source("durable", &source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("durable"), "{error}");
    assert!(registry.get("remember").is_none());
}

#[test]
fn carried_results_fit_a_small_context_window() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("durable", SOURCE).unwrap();
    let mut history = History::new(Vec::new());
    add_result(
        &mut history,
        "older",
        "older",
        &"o".repeat(SMALL_WINDOW_RESULT),
        false,
    );
    add_result(
        &mut history,
        "newer",
        "newer",
        &"n".repeat(SMALL_WINDOW_RESULT),
        false,
    );
    let mut model = Model::from_spec(MODEL).unwrap();
    model.context_window = SMALL_WINDOW;
    summarize_for(&model, "", &registry, &mut history, false);
    let keys: Vec<_> = retained(&history).into_iter().map(|(key, _)| key).collect();
    assert_eq!(keys, ["remember:newer"]);
}

#[test]
fn a_prefix_at_the_threshold_leaves_no_room_for_carried_results() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("durable", SOURCE).unwrap();
    let mut model = Model::from_spec(MODEL).unwrap();
    model.context_window = SMALL_WINDOW;
    let system = "s".repeat(SMALL_WINDOW as usize * CHARS_PER_TOKEN);
    let mut history = History::new(vec![Message::user(SUMMARY.into())]);
    summarize_for(&model, &system, &registry, &mut history, false);
    add_result(&mut history, "skill", "skill", LATEST, false);
    summarize_for(&model, &system, &registry, &mut history, false);
    assert!(retained(&history).is_empty());
}
