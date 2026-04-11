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
use maki_agent::skill::Skill;
use maki_agent::{
    AgentConfig, CancelToken, Envelope, McpCommand, McpHandle, McpSnapshotReader, ToolOutput,
};

use self::cancel_map::CancelMap;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model};
use tracing::{info, warn};

use crate::app::App;

use self::agent_loop::AgentLoop;
use self::command_router::spawn_command_router;
pub(crate) use self::shared_queue::{QueuedMessage, SharedQueue};

const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
}

pub(crate) enum AgentCommand {
    Cancel { run_id: u64 },
    CancelAll,
}

pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: Arc<ArcSwap<Vec<Message>>>,
    pub(crate) tool_outputs: Arc<Mutex<HashMap<String, ToolOutput>>>,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) queue: Arc<SharedQueue>,
    task: smol::Task<()>,
}

impl AgentHandles {
    /// MCP is started once up front. The handle lives across agent respawns, only the agent
    /// loop task gets replaced.
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<Message>,
        skills: &Arc<[Skill]>,
        config: AgentConfig,
        permissions: &Arc<PermissionManager>,
        cwd: PathBuf,
        session_id: Option<String>,
    ) -> Self {
        let mcp_handle = smol::block_on(mcp::start(&cwd));
        spawn_agent_internal(
            model_slot,
            initial_history,
            skills,
            config,
            permissions,
            mcp_handle,
            session_id,
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
        app.shared_tool_outputs = Some(Arc::clone(&self.tool_outputs));
        app.queue.set_shared(Arc::clone(&self.queue));
    }

    pub(crate) fn cancel(self) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
    }

    pub(crate) fn send_mcp(&self, cmd: McpCommand) {
        if let Some(ref h) = self.mcp_handle {
            h.send(cmd);
        }
    }

    pub(crate) fn respawn(
        &mut self,
        history: Vec<Message>,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        skills: &Arc<[Skill]>,
        config: AgentConfig,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
    ) {
        let slot = model_slot.load();
        if let Err(e) = smol::block_on(slot.provider.reload_auth()) {
            warn!(error = %e, "failed to reload auth, continuing with existing credentials");
        }
        let new = spawn_agent_internal(
            model_slot,
            history,
            skills,
            config,
            permissions,
            self.mcp_handle.clone(),
            Some(app.state.session.id.clone()),
        );
        let old = mem::replace(self, new);
        old.cancel();
        self.apply_to_app(app);
    }

    pub(crate) fn shutdown(self, timeout: Duration) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
        let mcp_handle = self.mcp_handle;
        let task = self.task;
        drop((self.cmd_tx, self.agent_rx, self.answer_tx));
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

            if let Some(handle) = mcp_handle {
                shutdown_mcp(&handle).await;
            }
        });
    }
}

async fn shutdown_mcp(handle: &McpHandle) {
    let (ack_tx, ack_rx) = flume::bounded(1);
    handle.send(McpCommand::Shutdown { ack: ack_tx });
    let finished = futures_lite::future::or(
        async {
            let _ = ack_rx.recv_async().await;
            true
        },
        async {
            smol::Timer::after(MCP_SHUTDOWN_TIMEOUT).await;
            false
        },
    )
    .await;
    if !finished {
        warn!("MCP shutdown timed out after {MCP_SHUTDOWN_TIMEOUT:?}");
    }
}

fn spawn_agent_internal(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_history: Vec<Message>,
    skills: &Arc<[Skill]>,
    config: AgentConfig,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    session_id: Option<String>,
) -> AgentHandles {
    let (agent_tx, agent_rx) = flume::unbounded::<Envelope>();
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (queue, notify_rx) = SharedQueue::new();
    let shared_history: Arc<ArcSwap<Vec<Message>>> =
        Arc::new(ArcSwap::from_pointee(initial_history.clone()));
    let shared_tool_outputs: Arc<Mutex<HashMap<String, ToolOutput>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (init_trigger, init_cancel) = CancelToken::new();
    let cancel_map = Arc::new(Mutex::new(CancelMap::new(0, init_trigger)));

    spawn_command_router(cmd_rx, Arc::clone(&cancel_map));

    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        Arc::clone(skills),
        config,
        initial_history,
        Arc::clone(&shared_history),
        mcp_handle.clone(),
        Arc::clone(permissions),
        agent_tx,
        answer_rx,
        notify_rx,
        Arc::clone(&queue),
        cancel_map,
        init_cancel,
        session_id,
    );

    let task = smol::spawn(agent_loop.run());

    AgentHandles {
        cmd_tx,
        agent_rx,
        answer_tx,
        history: shared_history,
        tool_outputs: shared_tool_outputs,
        mcp_handle,
        queue,
        task,
    }
}
