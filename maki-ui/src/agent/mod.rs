mod agent_loop;
mod cancel_map;
mod command_router;
pub(crate) mod shared_queue;

use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use maki_agent::mcp::config::McpServerInfo;
use maki_agent::permissions::PermissionManager;
use maki_agent::skill::Skill;
use maki_agent::{AgentConfig, CancelToken, Envelope, ToolOutput};

use self::cancel_map::CancelMap;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model};
use tracing::{info, warn};

use crate::app::App;

use self::agent_loop::AgentLoop;
use self::command_router::spawn_command_router;
pub(crate) use self::shared_queue::{QueuedMessage, SharedQueue};

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
}

pub(crate) enum AgentCommand {
    Cancel { run_id: u64 },
    CancelAll,
    ToggleMcp(String, bool),
}

#[derive(Clone, Default)]
pub(crate) struct McpState {
    pub(crate) disabled: Vec<String>,
    pub(crate) infos: Arc<ArcSwap<Vec<McpServerInfo>>>,
    pub(crate) pids: Arc<Mutex<Vec<u32>>>,
}

pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: Arc<ArcSwap<Vec<Message>>>,
    pub(crate) tool_outputs: Arc<Mutex<HashMap<String, ToolOutput>>>,
    pub(crate) mcp: McpState,
    pub(crate) queue: Arc<SharedQueue>,
    task: smol::Task<()>,
}

impl AgentHandles {
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

    #[allow(clippy::too_many_arguments)]
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
        let mcp = self.mcp.clone();
        let old = mem::replace(
            self,
            spawn_agent(model_slot, history, skills, config, permissions, mcp),
        );
        old.cancel();
        self.apply_to_app(app);
    }

    pub(crate) fn shutdown(self, timeout: Duration) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
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
        });
    }
}

pub(crate) fn toggle_disabled(disabled: &mut Vec<String>, name: &str, enabled: bool) {
    if enabled {
        disabled.retain(|s| s != name);
    } else if !disabled.contains(&name.to_owned()) {
        disabled.push(name.to_owned());
    }
}

pub(crate) fn spawn_agent(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_history: Vec<Message>,
    skills: &Arc<[Skill]>,
    config: AgentConfig,
    permissions: &Arc<PermissionManager>,
    mcp_state: McpState,
) -> AgentHandles {
    let (agent_tx, agent_rx) = flume::unbounded::<Envelope>();
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (toggle_tx, toggle_rx) = flume::unbounded::<(String, bool)>();
    let (queue, notify_rx) = SharedQueue::new();
    let shared_history: Arc<ArcSwap<Vec<Message>>> =
        Arc::new(ArcSwap::from_pointee(initial_history.clone()));
    let shared_tool_outputs: Arc<Mutex<HashMap<String, ToolOutput>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (init_trigger, init_cancel) = CancelToken::new();
    let cancel_map = Arc::new(Mutex::new(CancelMap::new(0, init_trigger)));

    spawn_command_router(cmd_rx, toggle_tx, Arc::clone(&cancel_map));

    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        Arc::clone(skills),
        config,
        initial_history,
        Arc::clone(&shared_history),
        Arc::clone(&mcp_state.infos),
        Arc::clone(&mcp_state.pids),
        mcp_state.disabled.clone(),
        Arc::clone(permissions),
        agent_tx,
        answer_rx,
        notify_rx,
        Arc::clone(&queue),
        toggle_rx,
        cancel_map,
        init_cancel,
    );

    let task = smol::spawn(agent_loop.run());

    AgentHandles {
        cmd_tx,
        agent_rx,
        answer_tx,
        history: shared_history,
        tool_outputs: shared_tool_outputs,
        mcp: mcp_state,
        queue,
        task,
    }
}
