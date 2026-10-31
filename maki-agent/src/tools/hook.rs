//! One interception point for tool calls, shared by every route dispatch
//! knows: registry tools, MCP tools, and host (ACP client) tools alike.
//!
//! It lives on the [`ToolRegistry`](super::registry::ToolRegistry) rather than
//! on the [`Tool`](super::Tool) trait, so a tool is hookable because dispatch
//! reached it, not because whoever wrote it remembered to ask.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::timeout_at;

use super::registry::{BoxFuture, ToolReach};
use super::{CallOrigin, ToolContext, ToolOutputMode};
use crate::cancel::CancelToken;
use crate::permissions::LayerAnswer;
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
    /// an ACP client tool, search. A layer needs every capability a tool can
    /// reach, because no narrower answer would be honest. `agent` is not one:
    /// every call already runs inside a turn.
    Unbounded,
    /// Steering what the agent does next, up to turns nobody typed. A layer
    /// needs every permission, `agent` included.
    Total,
}

impl From<Option<ToolReach>> for Authority {
    fn from(reach: Option<ToolReach>) -> Self {
        reach.map_or(Self::Unbounded, |reach| Self::Capability(reach.permission))
    }
}

pub struct HookCall<'a> {
    pub tool: &'a str,
    pub tool_id: &'a str,
    /// The group the tool files itself under (`read`, `edit`, `execute`,
    /// ...), so a layer wrapping every tool can skip the harmless ones.
    pub tool_kind: Option<&'a str>,
    /// Only set for [`HookStage::Output`], so a layer judging a result never
    /// has to stash the call that produced it.
    pub input: Option<&'a Value>,
    pub session_id: Option<&'a str>,
    pub origin: CallOrigin,
    pub output_mode: ToolOutputMode,
    pub authority: Authority,
    /// [`HookStage::Output`] on the receipt of a call that moved to the
    /// background. Its final result fires the stage again, with this unset.
    pub receipt: bool,
    /// The call's own cancellation, so a hook that runs elsewhere (the Lua
    /// thread) dies with the call rather than outliving the reply channel.
    pub cancel: &'a CancelToken,
    /// When the chain has to be dead by, whatever it is waiting on.
    pub deadline: Instant,
}

/// How a hook answered. `Unchanged` also covers every way a hook can fail: a
/// hook is an opinion about a call, never a precondition for making it, so a
/// broken one costs exactly what no hook costs.
#[derive(Debug)]
pub enum Verdict {
    Unchanged,
    Replaced(Value),
    Denied(String),
    /// Show the call to the user whatever the rules and yolo say, with
    /// `reason` on the prompt. It can only make things stricter, so a deny
    /// rule still wins. `input` is a rewrite that came along, if any. Only
    /// [`HookStage::Input`] may ask, and everywhere else this reads as
    /// [`Verdict::Unchanged`].
    Ask {
        reason: String,
        input: Option<Value>,
    },
}

/// Why a chain ended without a verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum ChainCut {
    Cancelled,
    Lapsed,
}

/// The one wait every hook chain takes. The host on the other end enforces
/// `deadline` and has the last word there: a chain it already ran deserves
/// its verdict however late it arrives. This only ends the wait on a host
/// that never answers (one that is gone or mid-reload), `grace` past the
/// deadline, or past now for a deadline that had already lapsed.
pub async fn bounded<T>(
    cancel: &CancelToken,
    deadline: Instant,
    grace: Duration,
    chain: impl Future<Output = T>,
) -> Result<T, ChainCut> {
    let latest = deadline.max(Instant::now()) + grace;
    match timeout_at(latest.into(), cancel.race(chain)).await {
        Ok(answered) => answered.map_err(|_| ChainCut::Cancelled),
        Err(_) => Err(ChainCut::Lapsed),
    }
}

pub trait ToolHook: Send + Sync + 'static {
    /// Synchronous and allocation free, because dispatch asks it for every
    /// call: a stage nobody wrapped never leaves the calling thread.
    fn wraps(&self, tool: &str, stage: HookStage) -> bool;

    /// How long past a chain's deadline this host may still answer, see
    /// [`bounded`].
    fn answer_grace(&self) -> Duration;

    /// `value` is the call's input at [`HookStage::Input`], and
    /// `{ [OUTPUT_TEXT]: string, [OUTPUT_IS_ERROR]: bool }` at
    /// [`HookStage::Output`].
    fn run<'a>(
        &'a self,
        stage: HookStage,
        value: Value,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, Verdict>;

    /// Asked where the permission prompt would show, with the scopes it would
    /// ask about. `None` leaves the call to the prompt.
    fn prompt<'a>(
        &'a self,
        input: &'a Value,
        scopes: &'a [String],
        call: &'a HookCall<'a>,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Option<LayerAnswer>>;
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use test_case::test_case;
    use tokio::task::yield_now;

    use super::*;

    const SHORT_GRACE: Duration = Duration::from_millis(20);
    const LONG_GRACE: Duration = Duration::from_secs(60);
    const VERDICT: &str = "late but heard";

    /// A reply crossing threads is pending on the first poll even when the
    /// host already ran the chain, so a lapsed deadline alone must not end
    /// the wait: the host decides what a late chain is worth. The answering
    /// case gets a grace it never reaches, so a slow machine cannot fail it.
    #[test_case(true,  LONG_GRACE,  Ok(VERDICT)           ; "a_late_answer_is_heard")]
    #[test_case(false, SHORT_GRACE, Err(ChainCut::Lapsed) ; "silence_ends_after_the_grace")]
    #[tokio::test]
    async fn a_lapsed_deadline_waits_on_the_host(
        answers: bool,
        grace: Duration,
        expected: Result<&str, ChainCut>,
    ) {
        let started = Instant::now();
        let chain = async {
            if !answers {
                pending::<()>().await;
            }
            yield_now().await;
            VERDICT
        };

        let got = bounded(&CancelToken::none(), started, grace, chain).await;

        assert_eq!(got, expected);
        if !answers {
            assert!(started.elapsed() >= grace, "gave up inside the grace");
        }
    }
}
