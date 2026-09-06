pub mod elicitation;
pub mod methods;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::permissions::PluginRuleStore;
use maki_agent::prompt::ResolvedSlots;
use maki_agent::{AgentConfig, PermissionsConfig, SessionEndReason};
use maki_config::{ModelPolicy, SessionDefaults};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_storage::id::MakiId;
use smol::future::Boxed;

pub type SessionEndHook = Arc<dyn Fn(MakiId, SessionEndReason) -> Boxed<()> + Send + Sync>;

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub yolo: bool,
    /// ACP exposes no toggles of its own, so the `always_*` knobs are the whole
    /// answer for every prompt this server runs.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    /// Called with the reason when an ACP session is replaced or the server
    /// exits. The hook answers with a future so its wait rides the executor
    /// instead of holding stdin for the whole `SessionEnd` grace period.
    pub on_session_end: Option<SessionEndHook>,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    smol::block_on(server::serve(params))
}
