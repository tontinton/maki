//! Bridges maki's permission config to the optional [`maki_cedar`] engine.
//!
//! When `permissions.toml` sets `engine = "cedar"`, [`CedarBackend::build`]
//! produces the engine from a local policy file, a synced `cedar-agent`
//! store, or both. The result plugs into [`super::PermissionManager`] as an
//! extra decision source: a Cedar `forbid` denies, a `permit` allows, and no
//! match defers to maki's own rules and defaults.

use std::sync::Arc;
use std::time::Duration;

use maki_cedar::{AgentClient, CedarDecision, CedarEngine};
use maki_config::{CedarSettings, PolicyEngineKind};
use maki_storage::paths::expand_tilde;
use tracing::{info, warn};

/// How often to re-fetch from a `cedar-agent` when the config leaves it at 0.
const DEFAULT_SYNC_SECS: u64 = 300;
const AGENT_TIMEOUT: Duration = Duration::from_secs(10);

/// The Cedar decision path wired into the permission manager.
pub enum CedarBackend {
    /// Policies evaluated in-process (local file and/or synced agent policies).
    Local(Arc<CedarEngine>),
    /// Every call asks a remote `cedar-agent` to decide.
    PerCall(Arc<AgentClient>),
}

impl CedarBackend {
    /// Wrap an already-built engine, for tests that inject a policy set.
    #[cfg(test)]
    pub(crate) fn from_engine(engine: Arc<CedarEngine>) -> Self {
        Self::Local(engine)
    }
    /// Build the backend selected by `settings`, or `None` when the builtin
    /// engine is in force. A misconfiguration logs and returns `None` so maki
    /// falls back to its own rules rather than failing closed on every tool.
    pub fn build(settings: &CedarSettings) -> Option<Self> {
        if settings.engine != PolicyEngineKind::Cedar {
            return None;
        }
        if settings.agent_per_call {
            let url = settings.agent_url.as_deref()?;
            return match AgentClient::new(url, AGENT_TIMEOUT) {
                Ok(c) => {
                    info!(url, "cedar: per-call authorization via cedar-agent");
                    Some(Self::PerCall(Arc::new(c)))
                }
                Err(e) => {
                    warn!(error = %e, "cedar: agent client build failed, using builtin rules");
                    None
                }
            };
        }

        let policies = load_policies(settings)?;
        let schema = load_schema(settings);
        match CedarEngine::from_source(&policies, schema.as_deref()) {
            Ok(e) => {
                info!("cedar: local policy evaluation active");
                Some(Self::Local(Arc::new(e)))
            }
            Err(e) => {
                warn!(error = %e, "cedar: policy compile failed, using builtin rules");
                None
            }
        }
    }

    /// Decide one `(tool_key, scope)` pair. `PerCall` blocks on the agent,
    /// mapping an error to `NoDecision` so a network fault defers to maki's
    /// rules instead of denying every call.
    pub fn decide(&self, tool_key: &str, scope: Option<&str>) -> CedarDecision {
        match self {
            Self::Local(engine) => engine.decide(tool_key, scope),
            Self::PerCall(client) => match smol::block_on(client.is_authorized(tool_key, scope)) {
                Ok(true) => CedarDecision::Allow,
                Ok(false) => CedarDecision::Deny("denied by cedar-agent".to_string()),
                Err(e) => {
                    warn!(error = %e, tool = tool_key, "cedar-agent query failed, deferring");
                    CedarDecision::NoDecision
                }
            },
        }
    }
}

/// Combine a local policy file with any policies synced from `cedar-agent`.
/// At least one source must yield policy text.
fn load_policies(settings: &CedarSettings) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(path) = &settings.policy_file {
        let path = expand_tilde(path);
        match std::fs::read_to_string(&path) {
            Ok(s) => parts.push(s),
            Err(e) => warn!(path = %path.display(), error = %e, "cedar: cannot read policy_file"),
        }
    }

    if let Some(url) = &settings.agent_url {
        match AgentClient::new(url, AGENT_TIMEOUT) {
            Ok(client) => match smol::block_on(client.fetch_policies()) {
                Ok(s) => {
                    let secs = if settings.sync_interval_secs == 0 {
                        DEFAULT_SYNC_SECS
                    } else {
                        settings.sync_interval_secs
                    };
                    info!(
                        url,
                        refresh_secs = secs,
                        "cedar: synced policies from agent"
                    );
                    parts.push(s);
                }
                Err(e) => warn!(url, error = %e, "cedar: policy sync failed"),
            },
            Err(e) => warn!(url, error = %e, "cedar: agent client build failed"),
        }
    }

    if parts.is_empty() {
        warn!("cedar: engine selected but no policy_file or agent_url yielded policies");
        return None;
    }
    Some(parts.join("\n"))
}

fn load_schema(settings: &CedarSettings) -> Option<String> {
    let path = expand_tilde(settings.schema_file.as_ref()?);
    match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cedar: cannot read schema_file");
            None
        }
    }
}
