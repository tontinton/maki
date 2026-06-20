use std::sync::Arc;

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
                    cancel_map.cancel_or_precancel(run_id);
                }
                AgentCommand::CancelAll => {
                    cancel_map.cancel_all();
                    subagent_cancels.cancel_all();
                }
                AgentCommand::CancelSubagent { tool_use_id } => {
                    subagent_cancels.cancel_or_precancel(tool_use_id);
                }
            }
        }
    })
    .detach();
}
