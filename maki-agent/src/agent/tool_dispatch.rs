use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, error, warn};

use crate::mcp::McpManager;
use crate::task_set::TaskSet;
use crate::tools::{ToolCall, ToolContext};
use crate::{AgentError, AgentEvent, AgentMode, ToolDoneEvent, ToolOutput, ToolStartEvent};

const DOOM_LOOP_THRESHOLD: usize = 3;
const DOOM_LOOP_MESSAGE: &str = "You have called this tool with identical input 3 times in a row. You are stuck in a loop. Break out and try a different approach.";
const MCP_BLOCKED_IN_PLAN: &str = "MCP tools are not available in plan mode";

#[derive(Clone)]
pub(crate) enum ResolvedCall {
    Native(ToolCall),
    Mcp { tool_name: String, input: Value },
}

impl ResolvedCall {
    pub(crate) fn start_event(&self, id: String, mcp: Option<&McpManager>) -> ToolStartEvent {
        match self {
            Self::Native(call) => call.start_event(id),
            Self::Mcp { tool_name, .. } => {
                let interned = mcp
                    .map(|m| m.interned_name(tool_name))
                    .unwrap_or("unknown_mcp");
                ToolStartEvent {
                    id,
                    tool: interned,
                    summary: format!("mcp: {tool_name}"),
                    annotation: None,
                    input: None,
                    output: None,
                }
            }
        }
    }

    pub(crate) async fn execute(&self, ctx: &ToolContext, id: String) -> ToolDoneEvent {
        match self {
            Self::Native(call) => call.execute(ctx, id).await,
            Self::Mcp { tool_name, input } => execute_mcp_tool(ctx, &id, tool_name, input).await,
        }
    }
}

pub(crate) fn resolve_tool(
    name: &str,
    input: &Value,
    mcp: Option<&McpManager>,
) -> Result<ResolvedCall, AgentError> {
    match ToolCall::from_api(name, input) {
        Ok(call) => Ok(ResolvedCall::Native(call)),
        Err(_) if mcp.is_some_and(|m| m.has_tool(name)) => Ok(ResolvedCall::Mcp {
            tool_name: name.to_owned(),
            input: input.clone(),
        }),
        Err(e) => Err(e),
    }
}

struct ParsedToolCall {
    id: String,
    call: ResolvedCall,
}

pub(super) struct RecentCalls(VecDeque<(String, u64)>);

impl RecentCalls {
    pub(super) fn new() -> Self {
        Self(VecDeque::new())
    }

    fn hash_input(input: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        input.to_string().hash(&mut h);
        h.finish()
    }

    fn is_doom_loop(&self, name: &str, input: &Value) -> bool {
        let hash = Self::hash_input(input);
        self.0.len() >= DOOM_LOOP_THRESHOLD - 1
            && self
                .0
                .iter()
                .rev()
                .take(DOOM_LOOP_THRESHOLD - 1)
                .all(|(n, h)| n == name && *h == hash)
    }

    fn record(&mut self, name: String, input: &Value) {
        self.0.push_back((name, Self::hash_input(input)));
        if self.0.len() > DOOM_LOOP_THRESHOLD {
            self.0.pop_front();
        }
    }
}

fn parse_tool_calls<'a>(
    tool_uses: impl Iterator<Item = (&'a str, &'a str, &'a Value)>,
    recent: &mut RecentCalls,
    mcp: Option<&McpManager>,
) -> (Vec<ParsedToolCall>, Vec<ToolDoneEvent>) {
    let mut parsed = Vec::new();
    let mut errors = Vec::new();

    for (id, name, input) in tool_uses {
        debug!(tool = %name, id = %id, raw_input = %input, "parsing tool call");
        if recent.is_doom_loop(name, input) {
            warn!(tool = %name, "doom loop detected, skipping execution");
            errors.push(ToolDoneEvent::error(id.to_owned(), DOOM_LOOP_MESSAGE));
        } else {
            match resolve_tool(name, input, mcp) {
                Ok(call) => parsed.push(ParsedToolCall {
                    id: id.to_owned(),
                    call,
                }),
                Err(e) => {
                    let msg = format!("failed to parse tool {name}: {e}");
                    warn!(tool = %name, error = %e, "failed to parse tool call");
                    errors.push(ToolDoneEvent::error(id.to_owned(), msg));
                }
            }
        }
        recent.record(name.to_owned(), input);
    }

    (parsed, errors)
}

async fn execute_tools(tool_calls: &[ParsedToolCall], ctx: &ToolContext) -> Vec<ToolDoneEvent> {
    let mut set = TaskSet::new();
    for parsed in tool_calls {
        let event_tx = ctx.event_tx.clone();
        let tool_ctx = ToolContext {
            tool_use_id: Some(parsed.id.clone()),
            ..ctx.clone()
        };
        let id = parsed.id.clone();
        let call = parsed.call.clone();
        set.spawn(async move {
            let output = call.execute(&tool_ctx, id).await;
            event_tx.try_send(AgentEvent::ToolDone(Box::new(output.clone())));
            output
        });
    }

    set.join_all()
        .await
        .into_iter()
        .enumerate()
        .map(|(i, r)| match r {
            Ok(output) => output,
            Err(e) => {
                error!(error = %e, "tool task panicked");
                ToolDoneEvent::error(tool_calls[i].id.clone(), "tool task panicked")
            }
        })
        .collect()
}

pub(crate) async fn execute_mcp_tool(
    ctx: &ToolContext,
    id: &str,
    tool_name: &str,
    input: &Value,
) -> ToolDoneEvent {
    let interned = ctx
        .mcp
        .as_ref()
        .map(|m| m.interned_name(tool_name))
        .unwrap_or("unknown_mcp");

    let done = |output: String, is_error: bool| ToolDoneEvent {
        id: id.to_owned(),
        tool: interned,
        output: ToolOutput::Plain(output),
        is_error,
    };

    if matches!(ctx.mode, AgentMode::Plan(_)) {
        return done(MCP_BLOCKED_IN_PLAN.into(), true);
    }

    let perm_tool = format!("mcp:{tool_name}");
    let perm_scope = {
        let json = input.to_string();
        if json.len() > 200 {
            format!("{}\u{2026}", &json[..200])
        } else {
            json
        }
    };

    if let Err(e) = ctx
        .permissions
        .enforce(
            &perm_tool,
            &perm_scope,
            &ctx.event_tx,
            ctx.user_response_rx.as_deref(),
            id,
            &ctx.cancel,
        )
        .await
    {
        return done(e.to_string(), true);
    }

    let Some(mcp) = &ctx.mcp else {
        return done(format!("MCP manager not available for {tool_name}"), true);
    };

    match mcp.call_tool(tool_name, input).await {
        Ok(text) => done(text, false),
        Err(e) => done(e.to_string(), true),
    }
}

pub(super) async fn process_tool_calls(
    response: maki_providers::StreamResponse,
    recent_calls: &mut RecentCalls,
    mcp: Option<&Arc<McpManager>>,
    history: &mut super::history::History,
    event_tx: &crate::EventSender,
    ctx: &ToolContext,
) -> Result<(), AgentError> {
    let (parsed, errors) = parse_tool_calls(
        response.message.tool_uses(),
        recent_calls,
        mcp.map(|m| m.as_ref()),
    );

    history.push(response.message);

    for p in &parsed {
        event_tx.send(AgentEvent::ToolStart(Box::new(
            p.call.start_event(p.id.clone(), mcp.map(|m| m.as_ref())),
        )))?;
    }

    for err in &errors {
        event_tx.try_send(AgentEvent::ToolDone(Box::new(err.clone())));
    }

    let mut results = execute_tools(&parsed, ctx).await;

    results.extend(errors);
    let tool_msg = crate::types::tool_results(results);
    event_tx.send(AgentEvent::ToolResultsSubmitted {
        message: Box::new(tool_msg.clone()),
    })?;
    history.push(tool_msg);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use test_case::test_case;

    use super::*;

    fn recent_calls(entries: &[(&str, Value)]) -> RecentCalls {
        let mut rc = RecentCalls::new();
        for (n, v) in entries {
            rc.record(n.to_string(), v);
        }
        rc
    }

    #[test_case("read", &[("read", "/a"), ("read", "/a")], true  ; "triggers_at_threshold")]
    #[test_case("read", &[("read", "/a")],                 false ; "below_threshold")]
    #[test_case("read", &[("read", "/a"), ("read", "/b")], false ; "different_input_breaks_chain")]
    #[test_case("grep", &[("glob", "/a"), ("glob", "/a")], false ; "different_tool_name")]
    #[test_case("bash", &[("bash", "/a"), ("bash", "/b"), ("bash", "/a")], false ; "interrupted_chain")]
    fn doom_loop_detection(name: &str, history: &[(&str, &str)], expected: bool) {
        let entries: Vec<_> = history
            .iter()
            .map(|(n, p)| (*n, serde_json::json!({"path": p})))
            .collect();
        let input = serde_json::json!({"path": "/a"});
        assert_eq!(recent_calls(&entries).is_doom_loop(name, &input), expected);
    }

    #[test]
    fn resolve_tool_returns_error_for_unknown_without_mcp() {
        let result = resolve_tool("unknown__tool", &serde_json::json!({}), None);
        assert!(result.is_err());
    }

    #[test]
    fn mcp_tool_blocked_in_plan_mode() {
        smol::block_on(async {
            let result = execute_mcp_tool(
                &crate::tools::test_support::stub_ctx(&AgentMode::Plan(PathBuf::from(
                    "/tmp/plan.md",
                ))),
                "t1",
                "myserver__mytool",
                &serde_json::json!({}),
            )
            .await;
            assert!(result.is_error);
            assert_eq!(result.output.as_text(), MCP_BLOCKED_IN_PLAN);
        });
    }

    #[test]
    fn mcp_tool_errors_without_mcp_manager() {
        smol::block_on(async {
            let result = execute_mcp_tool(
                &crate::tools::test_support::stub_ctx(&AgentMode::Build),
                "t1",
                "myserver__mytool",
                &serde_json::json!({}),
            )
            .await;
            assert!(result.is_error);
            assert!(result.output.as_text().contains("not available"));
        });
    }
}
