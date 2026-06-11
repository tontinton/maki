pub mod methods;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::prompt::ResolvedSlots;
use maki_agent::{AgentConfig, PermissionsConfig};
use maki_providers::Timeouts;
use maki_providers::model::Model;

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub mcp_handle: Option<maki_agent::McpHandle>,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub yolo: bool,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    smol::block_on(server::serve(params))
}
