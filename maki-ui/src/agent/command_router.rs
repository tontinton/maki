use std::sync::Arc;

use tracing::warn;

use maki_agent::CancelMap;

use super::AgentCommand;
use super::cancel_map::RunCancelMap;

pub(super) fn spawn_command_router(
    cmd_rx: flume::Receiver<AgentCommand>,
    cancel_map: Arc<RunCancelMap>,
    subagent_cancels: Arc<CancelMap<String>>,
) {
    smol::spawn(async move {
        while let Ok(cmd) = cmd_rx.recv_async().await {
            match cmd {
                AgentCommand::Cancel { run_id } => {
                    if !cancel_map.cancel_or_precancel(run_id) {
                        warn!(run_id, "cancel matched no active run");
                    }
                }
                AgentCommand::CancelAll => {
                    cancel_map.cancel_all();
                    subagent_cancels.cancel_all();
                }
                AgentCommand::CancelSubagent { tool_use_id } => {
                    if !subagent_cancels.cancel_or_precancel(tool_use_id.clone()) {
                        warn!(tool_use_id = %tool_use_id, "subagent cancel matched no active session");
                    }
                }
            }
        }
    })
    .detach();
}
