mod agent_loop;
mod cancel_map;
mod command_router;
pub(crate) mod shared_queue;

use std::collections::HashMap;
use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use maki_agent::mcp;
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentConfig, CancelMap, CancelToken, Envelope, McpCommand, McpConfigErrors, McpHandle,
    McpSnapshotReader, ToolOutput, ToolOutputLines,
};
use maki_lua::EventHandle;

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

pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) agent_tx: flume::Sender<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: Arc<ArcSwap<Vec<Message>>>,
    pub(crate) btw_system: Arc<ArcSwap<String>>,
    pub(crate) tool_outputs: Arc<Mutex<HashMap<String, ToolOutput>>>,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) mcp_config_errors: McpConfigErrors,
    pub(crate) queue: QueueSender,
    pub(crate) timeouts: maki_providers::Timeouts,
    task: smol::Task<()>,
}

impl AgentHandles {
    /// MCP is started once up front. The handle lives across agent respawns, only the agent
    /// loop task gets replaced.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<Message>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        cwd: PathBuf,
        session_id: Option<String>,
        timeouts: maki_providers::Timeouts,
        lua_handle: Option<EventHandle>,
    ) -> Self {
        let (mcp_handle, mcp_config_errors) = smol::block_on(mcp::start(&cwd));
        spawn_agent_internal(
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
        app.shared_tool_outputs = Some(Arc::clone(&self.tool_outputs));
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
        let slot = model_slot.load();
        if let Err(e) = smol::block_on(slot.provider.reload_auth()) {
            warn!(error = %e, "failed to reload auth, continuing with existing credentials");
        }
        let new = spawn_agent_internal(
            model_slot,
            history,
            config,
            tool_output_lines,
            permissions,
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Some(app.state.session.id.clone()),
            self.timeouts,
            lua_handle,
        );
        let old = mem::replace(self, new);
        // Repoint the app at the new queue before dropping `old`, otherwise the app keeps
        // the last old `QueueSender` alive and the old loop parks in `recv_notify` forever.
        self.apply_to_app(app);
        old.cancel();
    }

    pub(crate) fn shutdown(self, timeout: Duration) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
        let mcp_handle = self.mcp_handle;
        let task = self.task;
        drop((self.cmd_tx, self.agent_rx, self.answer_tx, self.queue));
        info!("waiting for agent to finish (timeout {timeout:?})");
        smol::block_on(async {
            let finished = futures_lite::future::or(
                async {
                    task.await;
                    true
                },
                async {
                    smol::Timer::after(timeout).await;
                    false
                },
            )
            .await;
            if !finished {
                warn!("agent did not finish within {timeout:?}, forcing shutdown");
            }

            if let Some(ref handle) = mcp_handle {
                handle.shutdown().await;
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent_internal(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_history: Vec<Message>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    session_id: Option<String>,
    timeouts: maki_providers::Timeouts,
    lua_handle: Option<EventHandle>,
) -> AgentHandles {
    let (agent_tx, agent_rx) = flume::unbounded::<Envelope>();
    let agent_tx_clone = agent_tx.clone();
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (queue_tx, queue_rx) = shared_queue::queue();
    let queue_rx = Arc::new(queue_rx);
    let shared_history: Arc<ArcSwap<Vec<Message>>> =
        Arc::new(ArcSwap::from_pointee(initial_history.clone()));
    let btw_system: Arc<ArcSwap<String>> = Arc::new(ArcSwap::from_pointee(String::new()));
    let shared_tool_outputs: Arc<Mutex<HashMap<String, ToolOutput>>> =
        Arc::new(Mutex::new(HashMap::new()));
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
        agent_tx,
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
        agent_tx: agent_tx_clone,
        answer_tx,
        history: shared_history,
        btw_system,
        tool_outputs: shared_tool_outputs,
        mcp_handle,
        mcp_config_errors,
        queue: queue_tx,
        timeouts,
        task,
    }
}
