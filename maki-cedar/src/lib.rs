//! Optional Cedar-policy authorization backend for maki's permission system.
//!
//! maki's native rule engine stays the default. When configured, a
//! [`CedarEngine`] answers the same `(tool, scope)` question using a compiled
//! Cedar [`cedar_policy::PolicySet`], so a team can manage policy centrally
//! (a file, or a `cedar-agent` store) in Cedar's auditable language.
//!
//! The decision is three-valued to match maki: a Cedar `forbid` is a hard
//! [`CedarDecision::Deny`], a `permit` is [`CedarDecision::Allow`], and when no
//! policy matches the engine returns [`CedarDecision::NoDecision`] so maki
//! falls back to its own default (which usually means: prompt the user).

mod agent_client;
mod compile;
mod engine;

pub use agent_client::{AgentClient, AgentPolicy, DEFAULT_AGENT_PORT};
pub use compile::{DefaultModel, PermissionsModel, RuleModel, compile, schema};
pub use engine::{CedarDecision, CedarEngine};

use thiserror::Error;

/// The single principal every maki authorization request runs as.
pub const PRINCIPAL: &str = r#"Agent::"maki""#;
/// The single action: invoking a tool.
pub const ACTION: &str = r#"Action::"invoke""#;
/// The Cedar entity type for a tool resource.
pub const TOOL_TYPE: &str = "Tool";
/// Context key holding the requested scope string (e.g. a bash command).
pub const CTX_SCOPE: &str = "scope";
/// Context key holding the tool key string (e.g. `bash`, `server.tool`).
pub const CTX_TOOL: &str = "tool";

#[derive(Debug, Error)]
pub enum CedarError {
    #[error("failed to parse Cedar policies: {0}")]
    Policies(String),
    #[error("failed to parse Cedar schema: {0}")]
    Schema(String),
    #[error("invalid tool key {key:?}: {reason}")]
    ToolKey { key: String, reason: String },
    #[error("failed to build authorization request: {0}")]
    Request(String),
}
