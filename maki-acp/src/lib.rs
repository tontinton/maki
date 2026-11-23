pub mod commands;
pub mod elicitation;
pub mod methods;
pub mod permissions;
mod router;
pub mod server;
pub mod translate;

pub use router::RELAYED_ENV;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use maki_agent::permissions::PluginRuleStore;
use maki_agent::prompt::ResolvedPrompts;
use maki_agent::{AgentConfig, CancelToken, Envelope, HostPrompt, SessionEndReason};
use maki_config::project::TrustMode;
use maki_config::{ModelPolicy, SessionDefaults, TrustConfig};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_storage::id::MakiId;

pub type SessionEndHook =
    Arc<dyn Fn(MakiId, SessionEndReason) -> BoxFuture<'static, ()> + Send + Sync>;

/// Sees every envelope of a session's stream, subagents' included, with the
/// session id. Must not block: it runs on the event pump.
pub type AgentEventHook = Arc<dyn Fn(&str, &Envelope) + Send + Sync>;

/// Called with the session id and the text of each prompt the agent is about
/// to get. Must not block: it runs on the request loop.
pub type TurnStartHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// What the launch folder's plugin host, `init.lua` and model give every
/// session this process runs.
pub struct ProjectStack {
    pub model: Model,
    pub config: AgentConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: Arc<ResolvedPrompts>,
    /// ACP exposes no toggles of its own, so the `always_*` knobs are the whole
    /// answer for every prompt this server runs.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    /// Folders the global `init.lua` answered for in advance. ACP never asks,
    /// so this is the only yes a cwd with no stored decision can get.
    pub trust_policy: Arc<TrustConfig>,
    pub yolo: bool,
    /// Called with the reason when an ACP session is replaced or the server
    /// exits. The hook answers with a future so its wait rides the executor
    /// instead of holding stdin for the whole `SessionEnd` grace period.
    pub on_session_end: Option<SessionEndHook>,
    /// Lets the plugin host see agent events, as the other drivers do.
    pub on_agent_event: Option<AgentEventHook>,
    /// Lets the plugin host see a turn start, which no agent event reports.
    pub on_turn_start: Option<TurnStartHook>,
}

pub struct AcpParams {
    pub stack: ProjectStack,
    /// The canonical folder `maki acp` was launched in. One process serves one
    /// folder.
    pub launch_dir: PathBuf,
    /// The env a child `maki acp` for another folder starts with: the process
    /// env from before any `.env` was loaded, so this folder's `.env` never
    /// leaks into another project.
    pub child_env: Vec<(OsString, OsString)>,
    /// `false` in a child started by the relay ([`RELAYED_ENV`]): it runs every
    /// session itself, whatever its folder, so relaying never recurses.
    pub relay: bool,
    /// How a session cwd with no stored trust decision is treated. `--trust`
    /// makes it [`TrustMode::Session`], which covers every cwd the client picks.
    pub trust_mode: TrustMode,
    pub storage: StateDir,
    /// `false` keeps every tool call blocking. Otherwise calls can move to the
    /// background and their results ride the next prompt. The server is long
    /// lived, so it never waits for them: closing the session stops them.
    pub background: bool,
    pub host_prompt: HostPrompt,
    /// `false` (`--no-commands`) skips custom command discovery, so only
    /// `/compact` and MCP prompts are advertised.
    pub commands: bool,
    /// Firing it shuts the server down as end of input does, without waiting
    /// for the client to close stdin.
    pub stop: CancelToken,
}

pub async fn run(params: AcpParams) -> color_eyre::Result<()> {
    server::serve(params).await
}
