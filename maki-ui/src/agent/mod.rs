mod agent_loop;
mod cancel_map;
mod command_router;
pub(crate) mod shared_queue;

use std::mem;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentConfig, CancelMap, CancelToken, Envelope, McpCommand, McpConfigErrors, McpHandle,
    McpSnapshotReader, ToolOutputLines,
};
use maki_lua::EventHandle;
use maki_storage::id::SessionRef;

use self::cancel_map::new_run_cancel_map;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model};
use tracing::{info, warn};

use crate::app::App;

use self::agent_loop::AgentLoop;
use self::command_router::spawn_command_router;
pub(crate) use self::shared_queue::{QueueSender, QueuedMessage};

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
}

pub(crate) enum AgentCommand {
    Cancel { run_id: u64 },
    CancelAll,
    CancelSubagent { tool_use_id: String },
}

/// Input channels (`cmd_tx`, `answer_tx`, `queue`) are per-agent, so an old
/// loop can never steal new input. The output channel (`agent_tx`/`agent_rx`)
/// is per-tab: `respawn` reuses it, so anyone still holding a sender (a Lua
/// restore reply, a click, an old agent winding down) can always deliver.
/// Stale events are filtered by `run_id`, not by killing the channel.
pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) agent_tx: flume::Sender<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: Arc<ArcSwap<Vec<Message>>>,
    pub(crate) btw_system: Arc<ArcSwap<String>>,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) mcp_config_errors: McpConfigErrors,
    pub(crate) queue: QueueSender,
    pub(crate) timeouts: maki_providers::Timeouts,
    task: smol::Task<()>,
}

impl AgentHandles {
    /// MCP is shared across sessions and agent respawns; the event loop starts it
    /// once and shuts it down at exit. Only the agent loop task lives here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<Message>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        session_id: Option<SessionRef>,
        timeouts: maki_providers::Timeouts,
        lua_handle: Option<EventHandle>,
        mcp_handle: Option<McpHandle>,
        mcp_config_errors: McpConfigErrors,
    ) -> Self {
        spawn_agent_internal(
            flume::unbounded(),
            model_slot,
            initial_history,
            config,
            tool_output_lines,
            permissions,
            mcp_handle,
            mcp_config_errors,
            session_id,
            timeouts,
            lua_handle,
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
        app.cmd_tx = Some(self.cmd_tx.clone());
        app.shared_history = Some(Arc::clone(&self.history));
        app.btw_system = Some(Arc::clone(&self.btw_system));
        app.queue.set_shared(self.queue.clone());
        let restore_tx =
            maki_agent::EventSender::new(self.agent_tx.clone(), crate::app::RESTORE_RUN_ID);
        app.restore_event_tx = Some(restore_tx.clone());
        for chat in &mut app.chats {
            chat.set_restore_channel(app.lua_event_handle.clone(), Some(restore_tx.clone()));
        }
    }

    pub(crate) fn cancel(self) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
    }

    pub(crate) fn send_mcp(&self, cmd: McpCommand) {
        if let Some(ref h) = self.mcp_handle {
            h.send(cmd);
        }
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
        lua_handle: Option<EventHandle>,
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
            config,
            tool_output_lines,
            permissions,
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Some(SessionRef::from(app.state.session.id)),
            self.timeouts,
            lua_handle,
        );
        let old = mem::replace(self, new);
        // Repoint the app at the new queue before dropping `old`, otherwise the app keeps
        // the last old `QueueSender` alive and the old loop parks in `recv_notify` forever.
        self.apply_to_app(app);
        app.flush_restored_queue();
        old.cancel();
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
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    session_id: Option<SessionRef>,
    timeouts: maki_providers::Timeouts,
    lua_handle: Option<EventHandle>,
) -> AgentHandles {
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (queue_tx, queue_rx) = shared_queue::queue();
    let queue_rx = Arc::new(queue_rx);
    let shared_history: Arc<ArcSwap<Vec<Message>>> =
        Arc::new(ArcSwap::from_pointee(initial_history.clone()));
    let btw_system: Arc<ArcSwap<String>> = Arc::new(ArcSwap::from_pointee(String::new()));
    let (init_trigger, init_cancel) = CancelToken::new();
    let cancel_map = Arc::new(new_run_cancel_map(0, init_trigger));
    let subagent_cancels: Arc<CancelMap<String>> = Arc::new(CancelMap::new());

    spawn_command_router(
        cmd_rx,
        Arc::clone(&cancel_map),
        Arc::clone(&subagent_cancels),
    );

    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        config,
        tool_output_lines,
        initial_history,
        Arc::clone(&shared_history),
        Arc::clone(&btw_system),
        mcp_handle.clone(),
        Arc::clone(permissions),
        agent_tx.clone(),
        answer_rx,
        queue_rx,
        cancel_map,
        init_cancel,
        session_id,
        timeouts,
        lua_handle,
        subagent_cancels,
    );

    let task = smol::spawn(agent_loop.run());

    AgentHandles {
        cmd_tx,
        agent_rx,
        agent_tx,
        answer_tx,
        history: shared_history,
        btw_system,
        mcp_handle,
        mcp_config_errors,
        queue: queue_tx,
        timeouts,
        task,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use maki_agent::AgentEvent;
    use maki_config::PermissionsConfig;
    use maki_providers::provider::BoxFuture;
    use maki_providers::{AgentError, ModelInfo, ProviderEvent, RequestOptions, StreamResponse};

    use super::*;

    const LONG_TIMEOUT: Duration = Duration::from_secs(60);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const PROBE_TEXT: &str = "probe-through-old-sender";
    const RESTORED_TEXT: &str = "restored-queued-message";

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
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider: Arc::new(StubProvider),
        }));
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
        ));
        let handles = AgentHandles::spawn(
            &model_slot,
            Vec::new(),
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            None,
            maki_providers::Timeouts::default(),
            None,
            None,
            McpConfigErrors::new(PathBuf::new()),
        );
        (handles, model_slot, permissions)
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
            None,
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

        app.state.session.meta.queued_messages = vec![RESTORED_TEXT.into()];
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(
            app.run_id,
            run_id_before + 2,
            "each respawn must bump run_id exactly once"
        );
        assert!(app.state.session.meta.queued_messages.is_empty());

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
