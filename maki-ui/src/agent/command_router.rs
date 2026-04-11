use std::sync::{Arc, Mutex};

use super::AgentCommand;
use super::cancel_map::CancelMap;

pub(super) fn spawn_command_router(
    cmd_rx: flume::Receiver<AgentCommand>,
    cancel_map: Arc<Mutex<CancelMap>>,
) {
    smol::spawn(async move {
        while let Ok(cmd) = cmd_rx.recv_async().await {
            let mut map = cancel_map.lock().unwrap_or_else(|e| e.into_inner());
            match cmd {
                AgentCommand::Cancel { run_id } => map.cancel(run_id),
                AgentCommand::CancelAll => map.cancel_all(),
            }
        }
    })
    .detach();
}
