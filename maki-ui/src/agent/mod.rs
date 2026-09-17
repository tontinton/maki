mod agent_loop;
mod model_slots;
mod run_cancels;
pub(crate) mod shared_queue;

use std::mem;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentConfig, CancelMap, Envelope, HistorySnapshot, McpCommand, McpConfigErrors, McpHandle,
    McpSnapshotReader, SessionMailbox, SharedMessages, ToolOutputLines,
};
use maki_config::ModelPolicy;
use maki_lua::EventHandle;
use maki_storage::id::SessionRef;

use self::run_cancels::RunCancels;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model};
use tracing::{info, warn};

use crate::app::App;

use self::agent_loop::AgentLoop;
pub(crate) use self::model_slots::ModelSlots;
pub(crate) use self::shared_queue::{QueueSender, QueuedMessage};

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
}

/// Input channels (`answer_tx`, `queue`) are per-agent, so an old loop can
/// never steal new input. The output channel (`agent_tx`/`agent_rx`) is
/// per-tab: `respawn` reuses it, so anyone still holding a sender (a Lua
/// restore reply, a click, an old agent winding down) can always deliver.
/// Stale events are filtered by `run_id`, not by killing the channel.
pub(crate) struct AgentHandles {
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) agent_tx: flume::Sender<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: SharedMessages,
    pub(crate) btw_system: Arc<ArcSwap<String>>,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) mcp_config_errors: McpConfigErrors,
    pub(crate) queue: QueueSender,
    pub(crate) timeouts: maki_providers::Timeouts,
    cancels: Arc<RunCancels>,
    subagent_cancels: Arc<CancelMap<String>>,
    model_policy: Arc<ModelPolicy>,
    mailbox: Option<SessionMailbox>,
    task: smol::Task<()>,
}

impl AgentHandles {
    /// MCP is shared across sessions and agent respawns; the event loop starts it
    /// once and shuts it down at exit. Only the agent loop task lives here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<Message>,
        initial_context_size: u32,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        session_id: Option<SessionRef>,
        timeouts: maki_providers::Timeouts,
        lua_handle: EventHandle,
        mcp_handle: Option<McpHandle>,
        mcp_config_errors: McpConfigErrors,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        spawn_agent_internal(
            flume::unbounded(),
            model_slot,
            initial_history,
            initial_context_size,
            config,
            tool_output_lines,
            permissions,
            mcp_handle,
            mcp_config_errors,
            session_id,
            timeouts,
            lua_handle,
            model_policy,
        )
    }

    pub(crate) fn mcp_reader(&self) -> McpSnapshotReader {
        self.mcp_handle
            .as_ref()
            .map(McpHandle::reader)
            .unwrap_or_else(McpSnapshotReader::empty)
    }

    pub(crate) fn apply_to_app(&self, app: &mut App) {
        app.answer_tx = Some(self.answer_tx.clone());
        app.shared_history = Some(Arc::clone(&self.history));
        app.btw_system = Some(Arc::clone(&self.btw_system));
        app.queue.set_shared(self.queue.clone());
        let restore_tx =
            maki_agent::EventSender::new(self.agent_tx.clone(), crate::app::RESTORE_RUN_ID);
        app.restore_event_tx = Some(restore_tx.clone());
        for chat in &mut app.chats {
            chat.set_restore_channel(Some(restore_tx.clone()));
        }
    }

    /// Esc: stops {run_id} and everything queued behind it.
    pub(crate) fn cancel_run(&self, run_id: u64) {
        self.cancels.cancel(run_id);
    }

    pub(crate) fn cancel_subagent(&self, tool_use_id: String) {
        self.subagent_cancels.cancel(tool_use_id);
    }

    /// Respawn or shutdown: this loop is done, whatever it was in the middle of.
    pub(crate) fn cancel_all(&self) {
        self.cancels.cancel_all();
        self.subagent_cancels.cancel_all();
    }

    pub(crate) fn send_mcp(&self, cmd: McpCommand) {
        if let Some(ref h) = self.mcp_handle {
            h.send(cmd);
        }
    }

    pub(crate) fn claim_mailbox_wake(&self) -> Vec<Message> {
        self.mailbox
            .as_ref()
            .map(SessionMailbox::claim_wake)
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn respawn(
        &mut self,
        history: Vec<Message>,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
        lua_handle: EventHandle,
    ) {
        // The output channel survives the respawn, so this bump is the only
        // thing that makes the old loop's in-flight envelopes stale. It lives
        // here so no caller can respawn without it.
        app.run_id += 1;
        let slot = model_slot.load();
        if let Err(e) = smol::block_on(slot.provider.reload_auth()) {
            warn!(error = %e, "failed to reload auth, continuing with existing credentials");
        }
        let new = spawn_agent_internal(
            (self.agent_tx.clone(), self.agent_rx.clone()),
            model_slot,
            history,
            // A respawn carries the app's last reported count across, so the
            // next request is not left guessing at its own prompt.
            app.state.context_size,
            config,
            tool_output_lines,
            permissions,
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Some(SessionRef::from(app.state.session.id)),
            self.timeouts,
            lua_handle,
            Arc::clone(&self.model_policy),
        );
        let old = mem::replace(self, new);
        // Repoint the app at the new queue before dropping `old`, otherwise the app keeps
        // the last old `QueueSender` alive and the old loop parks in `recv_notify` forever.
        self.apply_to_app(app);
        app.flush_restored_queue();
        old.cancel_all();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Hand back the agent task, dropping every channel so the loop can
    /// wind down. The caller sends `CancelAll` first and then awaits all
    /// tabs at once via [`join_all`] instead of paying a serial timeout
    /// per tab.
    pub(crate) fn into_task(self) -> smol::Task<()> {
        self.task
    }
}

/// Wait for every agent task under one shared timeout, not one per task.
pub(crate) fn join_all(tasks: Vec<smol::Task<()>>, timeout: Duration) {
    info!(
        count = tasks.len(),
        "waiting for agents to finish (timeout {timeout:?})"
    );
    smol::block_on(async {
        let finished = futures_lite::future::or(
            async {
                for task in tasks {
                    task.await;
                }
                true
            },
            async {
                smol::Timer::after(timeout).await;
                false
            },
        )
        .await;
        if !finished {
            warn!("agents did not finish within {timeout:?}, forcing shutdown");
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent_internal(
    (agent_tx, agent_rx): (flume::Sender<Envelope>, flume::Receiver<Envelope>),
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_history: Vec<Message>,
    initial_context_size: u32,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    session_id: Option<SessionRef>,
    timeouts: maki_providers::Timeouts,
    lua_handle: EventHandle,
    model_policy: Arc<ModelPolicy>,
) -> AgentHandles {
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (queue_tx, queue_rx) = shared_queue::queue();
    let queue_rx = Arc::new(queue_rx);
    // Seeded empty because `AgentLoop::new` below publishes the real snapshot
    // synchronously, before any handle escapes.
    let shared_history: SharedMessages =
        Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
    let btw_system: Arc<ArcSwap<String>> = Arc::new(ArcSwap::from_pointee(String::new()));
    let cancels = RunCancels::new();
    let subagent_cancels: Arc<CancelMap<String>> = Arc::new(CancelMap::new());
    let mailbox = session_id
        .as_ref()
        .map(|session_id| SessionMailbox::register(session_id.id()));

    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        config,
        tool_output_lines,
        initial_history,
        initial_context_size,
        Arc::clone(&shared_history),
        Arc::clone(&btw_system),
        mcp_handle.clone(),
        Arc::clone(permissions),
        agent_tx.clone(),
        answer_rx,
        queue_rx,
        Arc::clone(&cancels),
        session_id,
        mailbox.clone(),
        timeouts,
        lua_handle,
        Arc::clone(&subagent_cancels),
        Arc::clone(&model_policy),
    );

    let task = smol::spawn(agent_loop.run());

    AgentHandles {
        agent_rx,
        agent_tx,
        answer_tx,
        history: shared_history,
        btw_system,
        mcp_handle,
        mcp_config_errors,
        queue: queue_tx,
        timeouts,
        cancels,
        subagent_cancels,
        model_policy,
        mailbox,
        task,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use maki_agent::{AgentEvent, AgentInput, AgentMode};
    use maki_config::{PermissionsConfig, ProjectConfig};
    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        AgentError, ModelInfo, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
    };

    use super::shared_queue::{QueueItem, QueuedInput};
    use super::*;

    const LONG_TIMEOUT: Duration = Duration::from_secs(60);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const PROBE_TEXT: &str = "probe-through-old-sender";
    const RESTORED_TEXT: &str = "restored-queued-message";
    const RESUMED_HISTORY_TEXT: &str = "resumed-conversation";
    const MODEL_A: &str = "model-a";
    const MODEL_B: &str = "model-b";
    const FIRST_PROMPT: &str = "first-turn";
    const SECOND_PROMPT: &str = "second-turn";
    const RECORDER_TOOL: &str = "recording-provider";
    const TURN_OVER: &str = "model recorded, nothing left to stream";
    const NO_CALL: &str = "the agent never reached the provider";
    const NO_TURN_END: &str = "the run never reported that it was over";
    const GATE_CLOSED: &str = "the in-flight call is no longer waiting to be released";

    struct StubProvider;

    impl Provider for StubProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a serde_json::Value,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(std::future::pending())
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn stub_spawn() -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        stub_spawn_with(Vec::new())
    }

    fn stub_spawn_with(
        initial_history: Vec<Message>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider: Arc::new(StubProvider),
        }));
        let (handles, permissions) = spawn_on(&model_slot, initial_history);
        (handles, model_slot, permissions)
    }

    fn spawn_on(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<Message>,
    ) -> (AgentHandles, Arc<PermissionManager>) {
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
            ProjectConfig::for_project(Path::new("/tmp")),
            Arc::default(),
        ));
        let handles = AgentHandles::spawn(
            model_slot,
            initial_history,
            0,
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            None,
            maki_providers::Timeouts::default(),
            EventHandle::disconnected_for_test(),
            None,
            McpConfigErrors::new(PathBuf::new()),
            Arc::new(ModelPolicy::default()),
        );
        (handles, permissions)
    }

    fn respawn(
        handles: &mut AgentHandles,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
    ) {
        handles.respawn(
            Vec::new(),
            model_slot,
            AgentConfig::default(),
            ToolOutputLines::default(),
            permissions,
            app,
            EventHandle::disconnected_for_test(),
        );
    }

    /// Reports which model the agent handed it, then ends the turn with a
    /// non-retryable error so no stream has to be faked. `gate` parks a call in
    /// flight until the test lets it go, one permit per call.
    struct RecordingProvider {
        calls: flume::Sender<String>,
        gate: Option<flume::Receiver<()>>,
    }

    impl Provider for RecordingProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a serde_json::Value,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            let id = model.id.clone();
            Box::pin(async move {
                let _ = self.calls.send(id);
                if let Some(ref gate) = self.gate {
                    gate.recv_async().await.expect(GATE_CLOSED);
                }
                Err(AgentError::Tool {
                    tool: RECORDER_TOOL.into(),
                    message: TURN_OVER.into(),
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn recorder() -> (Arc<dyn Provider>, flume::Receiver<String>) {
        let (calls, reported) = flume::unbounded();
        (Arc::new(RecordingProvider { calls, gate: None }), reported)
    }

    fn gated_recorder() -> (
        Arc<dyn Provider>,
        flume::Receiver<String>,
        flume::Sender<()>,
    ) {
        let (calls, reported) = flume::unbounded();
        let (release, gate) = flume::unbounded();
        (
            Arc::new(RecordingProvider {
                calls,
                gate: Some(gate),
            }),
            reported,
            release,
        )
    }

    fn slot(model_id: &str, provider: &Arc<dyn Provider>) -> Arc<ModelSlot> {
        Arc::new(ModelSlot {
            model: Model {
                id: model_id.into(),
                ..crate::components::test_model()
            },
            provider: Arc::clone(provider),
        })
    }

    fn push_prompt(handles: &AgentHandles, run_id: u64, text: &str) {
        handles.queue.push(QueueItem::Message(QueuedInput {
            text: text.into(),
            input: AgentInput {
                message: text.into(),
                mode: AgentMode::default(),
                images: Vec::new(),
                preamble: Vec::new(),
                thinking: ThinkingConfig::default(),
                fast: false,
                workflow: false,
                prompt: None,
            },
            run_id,
            displayed: true,
        }));
    }

    /// The failed turn's `Error` is what says the run is over. Queue the next
    /// prompt before it and the running turn swallows it as an interrupt, so
    /// the second request never happens.
    fn await_turn_end(handles: &AgentHandles) {
        loop {
            let envelope = handles
                .agent_rx
                .recv_timeout(LONG_TIMEOUT)
                .expect(NO_TURN_END);
            if matches!(envelope.event, AgentEvent::Error { .. }) {
                return;
            }
        }
    }

    fn drive_turn(
        handles: &AgentHandles,
        reported: &flume::Receiver<String>,
        run_id: u64,
        text: &str,
    ) -> String {
        push_prompt(handles, run_id, text);
        let model_id = reported.recv_timeout(LONG_TIMEOUT).expect(NO_CALL);
        await_turn_end(handles);
        model_id
    }

    /// The regression the per-runtime cell fixed. With one process-wide slot,
    /// picking a model in one tab dragged every other tab onto it.
    #[test]
    fn a_model_swap_in_one_tab_leaves_the_other_tab_alone() {
        let (provider_one, reported_one) = recorder();
        let (provider_two, reported_two) = recorder();
        let cell_one = Arc::new(ArcSwap::new(slot(MODEL_A, &provider_one)));
        let cell_two = Arc::new(ArcSwap::new(slot(MODEL_A, &provider_two)));
        let (handles_one, _permissions_one) = spawn_on(&cell_one, Vec::new());
        let (handles_two, _permissions_two) = spawn_on(&cell_two, Vec::new());

        cell_one.store(slot(MODEL_B, &provider_one));

        assert_eq!(
            drive_turn(&handles_one, &reported_one, 1, FIRST_PROMPT),
            MODEL_B
        );
        assert_eq!(
            drive_turn(&handles_two, &reported_two, 1, SECOND_PROMPT),
            MODEL_A,
            "a swap on one runtime's cell must not move another runtime's model"
        );
    }

    /// `respawn` hands the new loop the cell, not the model inside it. Captured
    /// by value, a `/model` pick after a `/new` or a session load would be
    /// ignored until something respawned the agent again.
    #[test]
    fn a_swap_after_respawn_still_reaches_the_new_agent() {
        let (provider, reported) = recorder();
        let cell = Arc::new(ArcSwap::new(slot(MODEL_A, &provider)));
        let (mut handles, permissions) = spawn_on(&cell, Vec::new());
        let mut app = crate::app::tests::test_app();
        respawn(&mut handles, &cell, &permissions, &mut app);

        cell.store(slot(MODEL_B, &provider));
        assert_eq!(
            drive_turn(&handles, &reported, app.run_id, FIRST_PROMPT),
            MODEL_B
        );
    }

    /// The loop reads its cell at the start of every run rather than keeping
    /// the model it spawned with, so a pick made mid-flight leaves the running
    /// turn alone and still shows up on the next one.
    #[test]
    fn a_swap_during_a_turn_only_applies_to_the_following_turn() {
        let (provider, reported, release) = gated_recorder();
        let cell = Arc::new(ArcSwap::new(slot(MODEL_A, &provider)));
        let (handles, _permissions) = spawn_on(&cell, Vec::new());

        push_prompt(&handles, 1, FIRST_PROMPT);
        let in_flight = reported.recv_timeout(LONG_TIMEOUT).expect(NO_CALL);
        cell.store(slot(MODEL_B, &provider));
        release.send(()).expect(GATE_CLOSED);
        await_turn_end(&handles);
        assert_eq!(in_flight, MODEL_A);

        push_prompt(&handles, 2, SECOND_PROMPT);
        let next = reported.recv_timeout(LONG_TIMEOUT).expect(NO_CALL);
        release.send(()).expect(GATE_CLOSED);
        await_turn_end(&handles);
        assert_eq!(
            next, MODEL_B,
            "a swap stored mid-flight must still land on the next turn"
        );
    }

    /// Senders captured before any respawn (Lua restore replies, clicks) must
    /// still reach the live receiver, and restored queue items must land in
    /// the freshly wired queue, not the one that just died.
    #[test]
    fn respawn_twice_keeps_channel_and_delivers_restored_queue() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let pre_gen1_sender =
            maki_agent::EventSender::new(handles.agent_tx.clone(), crate::app::RESTORE_RUN_ID);

        let mut app = crate::app::tests::test_app();
        let run_id_before = app.run_id;
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(app.run_id, run_id_before + 1);

        app.state.session_mut().meta.queued_messages = vec![RESTORED_TEXT.into()];
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(
            app.run_id,
            run_id_before + 2,
            "each respawn must bump run_id exactly once"
        );
        assert_eq!(
            app.queue.text_messages(),
            [RESTORED_TEXT],
            "the restored item lands in the new queue exactly once"
        );

        pre_gen1_sender
            .send(AgentEvent::TextDelta {
                text: PROBE_TEXT.into(),
            })
            .expect("pre-generation-1 sender must still deliver after two respawns");

        let mut probe_seen = false;
        let mut consumed_seen = false;
        while !(probe_seen && consumed_seen) {
            let envelope = handles
                .agent_rx
                .recv_timeout(LONG_TIMEOUT)
                .expect("probe or restored queue item never reached the tab channel");
            match envelope.event {
                AgentEvent::TextDelta { ref text } if text == PROBE_TEXT => probe_seen = true,
                AgentEvent::QueueItemConsumed { ref text, .. } => {
                    assert_eq!(text, RESTORED_TEXT);
                    assert_eq!(envelope.run_id, app.run_id);
                    consumed_seen = true;
                }
                _ => {}
            }
        }
    }

    /// If the seeded empty snapshot ever outlived `spawn`, the next checkpoint
    /// would adopt it and wipe a resumed conversation from disk.
    #[test]
    fn spawn_publishes_the_resumed_history_before_the_handles_escape() {
        let (handles, _model_slot, _permissions) =
            stub_spawn_with(vec![Message::user(RESUMED_HISTORY_TEXT.into())]);
        let snapshot = handles.history.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "the seeded empty snapshot must be replaced synchronously"
        );
        assert_eq!(snapshot.messages[0].user_text(), Some(RESUMED_HISTORY_TEXT));
    }

    #[test]
    fn respawn_publishes_the_new_history_into_the_app_mirror() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let mut app = crate::app::tests::test_app();
        handles.respawn(
            vec![Message::user(RESUMED_HISTORY_TEXT.into())],
            &model_slot,
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            &mut app,
            EventHandle::disconnected_for_test(),
        );

        let mirror = app
            .shared_history
            .as_ref()
            .expect("respawn wires the live mirror into the app");
        let snapshot = mirror.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "a checkpoint right after respawn must not see the seeded empty snapshot"
        );
        assert_eq!(snapshot.messages[0].user_text(), Some(RESUMED_HISTORY_TEXT));
    }

    #[test]
    fn join_all_returns_when_all_tasks_complete() {
        join_all(Vec::new(), LONG_TIMEOUT);
        join_all(
            (0..3).map(|_| smol::spawn(async {})).collect(),
            LONG_TIMEOUT,
        );
    }

    #[test]
    fn join_all_stuck_task_returns_after_shared_timeout() {
        let start = Instant::now();
        join_all(
            vec![
                smol::spawn(async {}),
                smol::spawn(futures_lite::future::pending::<()>()),
            ],
            SHORT_TIMEOUT,
        );
        assert!(start.elapsed() >= SHORT_TIMEOUT);
    }
}
