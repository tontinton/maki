//! One interception point for tool calls, shared by every route dispatch
//! knows: registry tools, MCP tools, and host (ACP client) tools alike.
//!
//! It lives on the [`ToolRegistry`](super::registry::ToolRegistry) rather than
//! on the [`Tool`](super::Tool) trait, so a tool is hookable because dispatch
//! reached it, not because whoever wrote it remembered to ask.

use std::time::Instant;

use serde_json::Value;

use super::CallOrigin;
use super::registry::BoxFuture;
use crate::cancel::CancelToken;
use maki_config::Permission;

/// Fields of the value a [`HookStage::Output`] hook sees and returns.
pub const OUTPUT_TEXT: &str = "text";
pub const OUTPUT_IS_ERROR: &str = "is_error";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum HookStage {
    /// Before the input is parsed and before permission rules resolve, so
    /// rules judge the call as the hook left it. Any later and a hook could
    /// smuggle in an argument the rules never approved.
    Input,
    /// After the call finished, on the text it produced (failures included),
    /// before that text becomes history.
    Output,
}

impl HookStage {
    /// Order matters: stages index array slots via `as usize`.
    pub const ALL: [Self; 2] = [Self::Input, Self::Output];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// What hooking a call would hand whoever wrote the hook. Rewriting an input
/// decides what the tool then does, so the host prices that before it lets
/// untrusted code near it.
///
/// There is no "free" variant on purpose. A tool declaring no capability has
/// not told us it exercises none, only that nothing asks: `batch`, `task` and
/// `code_execution` declare nothing and reach every other tool. So a tool
/// landing tomorrow is never silently free to layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Authority {
    /// Exactly the capability the tool declares; a layer needs that one.
    Capability(Permission),
    /// Reach nobody declared: a tool naming no capability, an MCP server tool,
    /// an ACP client tool, search. A layer needs every capability, because no
    /// narrower answer would be honest.
    Unbounded,
}

pub struct HookCall<'a> {
    pub tool: &'a str,
    pub tool_id: &'a str,
    pub session_id: Option<&'a str>,
    pub origin: CallOrigin,
    pub authority: Authority,
    /// The call's own cancellation, so a hook that runs elsewhere (the Lua
    /// thread) dies with the call rather than outliving the reply channel.
    pub cancel: &'a CancelToken,
    /// When the chain has to be dead by, whatever it is waiting on.
    pub deadline: Instant,
}

/// How a hook answered. `Unchanged` also covers every way a hook can fail: a
/// hook is an opinion about a call, never a precondition for making it, so a
/// broken one costs exactly what no hook costs.
pub enum Verdict {
    Unchanged,
    Replaced(Value),
    Denied(String),
}

pub trait ToolHook: Send + Sync + 'static {
    /// Synchronous and allocation free, because dispatch asks it for every
    /// call: a stage nobody wrapped never leaves the calling thread.
    fn wraps(&self, tool: &str, stage: HookStage) -> bool;

    /// `value` is the call's input at [`HookStage::Input`], and
    /// `{ [OUTPUT_TEXT]: string, [OUTPUT_IS_ERROR]: bool }` at
    /// [`HookStage::Output`].
    fn run<'a>(
        &'a self,
        stage: HookStage,
        value: Value,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, Verdict>;
}
