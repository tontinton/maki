use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use maki_agent::headless::{InteractiveHandle, InteractiveParams};
use maki_agent::prompt::ResolvedSlots;
use maki_agent::{AgentEvent, AgentInput, AgentMode, Envelope};
use maki_config::{AgentConfig, DefaultEffect, PermissionsConfig};
use maki_providers::model::Model;
use maki_providers::provider::Provider;
use maki_providers::{
    AgentError, CancellationToken, ContentBlock, Message, ModelInfo, ProviderEvent, RequestOptions,
    Role, StopReason, StreamResponse, Timeouts, TokenUsage,
};
use maki_storage::StateDir;
use maki_storage::paths::{log_path, session_dir};
use maki_storage::session_log::{build_session_tree, load_folder};
use serde_json::Value;
use tempfile::TempDir;

use maki_agent::tree_sink::fold_to_messages;

const MODEL_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
const EVENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROMPT_TEXT: &str = "hello";

struct ScriptedProvider {
    responses: Mutex<Vec<StreamResponse>>,
    cancel_aware: bool,
}

impl ScriptedProvider {
    fn new(responses: Vec<StreamResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            cancel_aware: false,
        }
    }

    fn cancel_aware(responses: Vec<StreamResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            cancel_aware: true,
        }
    }
}

impl Provider for ScriptedProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a Value,
        _: &'a flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a str>,
        cancel: CancellationToken,
    ) -> maki_providers::provider::BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            if self.cancel_aware {
                let deadline = std::time::Instant::now() + EVENT_DRAIN_TIMEOUT;
                while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                    smol::Timer::after(POLL_INTERVAL).await;
                }
            }
            let mut responses = self.responses.lock().unwrap();
            assert!(!responses.is_empty(), "ScriptedProvider: no more responses");
            Ok(responses.remove(0))
        })
    }

    fn list_models(
        &self,
    ) -> maki_providers::provider::BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn text_response(text: &str) -> StreamResponse {
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

fn default_model() -> Model {
    Model::from_spec(MODEL_SPEC).unwrap()
}

fn default_params(
    provider: Arc<dyn Provider>,
    state_dir: StateDir,
    initial_history: Vec<Message>,
) -> InteractiveParams {
    InteractiveParams {
        model: default_model(),
        config: AgentConfig::default(),
        permissions_config: PermissionsConfig {
            default: DefaultEffect::Allow,
            rules: vec![],
            ..Default::default()
        },
        timeouts: Timeouts::default(),
        prompt_slots: Arc::new(ResolvedSlots::default()),
        excluded_tools: vec![],
        mcp_handle: None,
        initial_wd: std::path::PathBuf::from("/tmp"),
        session_id: None,
        initial_history,
        yolo: true,
        system_prompt_override: None,
        append_system_prompt: None,
        workflow: false,
        provider_override: Some(provider),
        state_dir: Some(state_dir),
    }
}

fn send_prompt(handle: &InteractiveHandle, message: &str) {
    handle
        .input_tx
        .send(AgentInput {
            message: message.into(),
            mode: AgentMode::Build,
            images: vec![],
            preamble: vec![],
            thinking: Default::default(),
            fast: false,
            workflow: false,
            prompt: None,
        })
        .unwrap();
}

fn drain_until_done(handle: &InteractiveHandle) -> Vec<Envelope> {
    let deadline = std::time::Instant::now() + EVENT_DRAIN_TIMEOUT;
    let mut events = Vec::new();
    while std::time::Instant::now() < deadline {
        match handle.event_rx.recv_timeout(POLL_INTERVAL) {
            Ok(e) => {
                let is_done = matches!(e.event, AgentEvent::Done { .. } | AgentEvent::Error { .. });
                let is_cancelled = matches!(e.event, AgentEvent::CancelledPartial { .. });
                events.push(e);
                if is_done || is_cancelled {
                    break;
                }
            }
            Err(flume::RecvTimeoutError::Timeout) => continue,
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }
    events
}

fn assert_log_exists(base: &std::path::Path, session_id: &str) {
    let dir = session_dir(base, session_id);
    let log = log_path(&dir);
    assert!(log.exists(), "log.jsonl missing for {session_id}");
}

fn load_tree_messages(base: &std::path::Path, session_id: &str) -> Vec<Message> {
    let dir = session_dir(base, session_id);
    let loaded = load_folder(&dir, session_id).expect("load folder");
    let tree = build_session_tree(&loaded).expect("build tree");
    fold_to_messages(&tree)
}

#[test]
fn prompt_then_fork_creates_new_session_on_disk() {
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hi")]));
    let handle =
        maki_agent::headless::spawn_interactive(default_params(provider, dir.clone(), Vec::new()));

    send_prompt(&handle, PROMPT_TEXT);
    let events = drain_until_done(&handle);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, AgentEvent::Done { .. })),
        "expected Done event, got {events:?}"
    );

    assert_log_exists(tmp.path(), &handle.session_id);

    let sink = handle.tree_sink.as_ref().expect("tree sink present");
    sink.barrier().expect("barrier");
    let leaf = sink.leaf_position();
    let leaf_nref = leaf.node_ref().cloned().expect("non-root leaf");

    let new_id = maki_storage::new_session_id();
    let result = sink.fork(new_id.clone(), leaf_nref).expect("fork");
    assert_eq!(result.new_session_id, new_id);

    assert_log_exists(tmp.path(), &new_id);
    let messages = load_tree_messages(tmp.path(), &new_id);
    assert!(
        messages.iter().any(|m| m.role == Role::User
            && m.content.iter().any(|b| matches!(b,
                ContentBlock::Text { text } if text.contains(PROMPT_TEXT)))),
        "forked session should contain the user prompt"
    );
}

#[test]
fn interrupt_persists_partial_on_disk() {
    let provider = Arc::new(ScriptedProvider::cancel_aware(vec![text_response(
        "partial",
    )]));
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let handle = maki_agent::headless::spawn_interactive(default_params(provider, dir, Vec::new()));

    send_prompt(&handle, PROMPT_TEXT);
    let _ = handle.cancel_tx.send(());
    let events = drain_until_done(&handle);

    let partial = events.iter().find_map(|e| match e.event {
        AgentEvent::CancelledPartial { interrupted } => Some(interrupted),
        AgentEvent::Done { .. } => Some(false),
        _ => None,
    });
    let interrupted = partial.expect("expected cancel or done event");

    if let Some(sink) = &handle.tree_sink {
        sink.barrier().ok();
    }

    assert_log_exists(tmp.path(), &handle.session_id);
    let messages = load_tree_messages(tmp.path(), &handle.session_id);
    assert!(!messages.is_empty(), "messages should be persisted");

    if interrupted {
        let dir = session_dir(tmp.path(), &handle.session_id);
        let loaded = load_folder(&dir, &handle.session_id).expect("load folder");
        assert!(
            loaded.messages.iter().any(|m| m.interrupted),
            "expected an interrupted assistant node on disk"
        );
    }
}

#[test]
fn compact_after_multiple_turns_still_loadable() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_response("first response"),
        text_response("second response"),
    ]));
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let handle = maki_agent::headless::spawn_interactive(default_params(provider, dir, Vec::new()));

    send_prompt(&handle, "first prompt");
    drain_until_done(&handle);
    send_prompt(&handle, "second prompt");
    drain_until_done(&handle);

    let sink = handle.tree_sink.as_ref().expect("tree sink present");
    sink.barrier().expect("barrier");

    let result = sink.compact(None);
    assert!(result.is_ok(), "compact should succeed: {:?}", result);

    let messages = load_tree_messages(tmp.path(), &handle.session_id);
    assert!(!messages.is_empty(), "session still loadable after compact");
}
