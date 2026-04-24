use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::{debug, error, warn};

use crate::mcp::{McpHandle, UNKNOWN_MCP};
use crate::task_set::TaskSet;
use crate::tools::ToolContext;
use crate::tools::registry::{ToolInvocation, ToolRegistry};
use crate::{AgentError, AgentEvent, AgentMode, ToolDoneEvent, ToolOutput, ToolStartEvent};

const DOOM_LOOP_THRESHOLD: usize = 3;
const DOOM_LOOP_MESSAGE: &str = "You have called this tool with identical input 3 times in a row. You are stuck in a loop. Break out and try a different approach.";
const MCP_BLOCKED_IN_PLAN: &str = "MCP tools are not available in plan mode";
const UNKNOWN_TOOL_PREFIX: &str = "unknown tool";

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

/// Parse errors and unknown tools skip the start event so the UI never
/// shows a phantom spinner.
pub async fn run(
    registry: &ToolRegistry,
    mcp: Option<&McpHandle>,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
) -> ToolDoneEvent {
    let entry = registry.get(name);
    let tool_id: Arc<str> = entry
        .as_ref()
        .map(|e| Arc::from(e.tool.name()))
        .or_else(|| mcp.map(|m| m.interned_name(name)))
        .unwrap_or_else(|| Arc::from(UNKNOWN_MCP));
    let started = Instant::now();

    let done_error = |msg: String| ToolDoneEvent {
        id: id.clone(),
        tool: Arc::clone(&tool_id),
        output: ToolOutput::Plain(msg),
        is_error: true,
    };

    if let Some(entry) = entry {
        let invocation = match entry.tool.parse(input) {
            Ok(inv) => inv,
            Err(e) => {
                warn!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    input_preview = %crate::tools::schema::preview(&input.to_string()),
                    error = %e,
                    "tool input parse failed"
                );
                return done_error(e.to_string());
            }
        };

        if let AgentMode::Plan(plan_path) = &ctx.mode
            && let Some(target) = invocation.mutable_path()
            && target != plan_path.as_path()
        {
            warn!(
                tool = %name,
                target = %target.display(),
                plan = %plan_path.display(),
                "blocked write in plan mode"
            );
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }

        let header_result = invocation.start_header().await;
        let start = ToolStartEvent {
            id: id.clone(),
            tool: Arc::clone(&tool_id),
            summary: header_result.text(),
            render_header: header_result.snapshot(),
            annotation: invocation.start_annotation(),
            input: invocation.start_input(),
            output: invocation.start_output(),
        };
        ctx.emit_tool_start(start);

        if let Err(e) = enforce_permission(invocation.as_ref(), name, ctx, &id).await {
            return done_error(e);
        }

        let result = invocation.execute(ctx).await;

        let elapsed = started.elapsed();
        match result {
            Ok(output) => {
                debug!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    elapsed_ms = elapsed.as_millis() as u64,
                    "tool ok"
                );
                ToolDoneEvent {
                    id,
                    tool: tool_id,
                    output,
                    is_error: false,
                }
            }
            Err(message) => {
                warn!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %message,
                    "tool failed"
                );
                done_error(message)
            }
        }
    } else if mcp.is_some_and(|m| m.has_tool(name)) {
        // MCP tools skip parsing, so we assemble the start event manually.
        let start = ToolStartEvent {
            id: id.clone(),
            tool: Arc::clone(&tool_id),
            summary: format!("mcp: {name}"),
            render_header: None,
            annotation: None,
            input: None,
            output: None,
        };
        ctx.emit_tool_start(start);
        execute_mcp_tool(ctx, &id, tool_id, name, input).await
    } else {
        let msg = format!("{UNKNOWN_TOOL_PREFIX}: {name}");
        warn!(tool = %name, "unknown tool");
        done_error(msg)
    }
}

async fn enforce_permission(
    inv: &dyn ToolInvocation,
    name: &str,
    ctx: &ToolContext,
    id: &str,
) -> Result<(), String> {
    if let Some(scope) = inv.permission_scope() {
        ctx.permissions
            .enforce(
                name,
                &scope,
                &ctx.event_tx,
                ctx.user_response_rx.as_deref(),
                id,
                &ctx.cancel,
            )
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn execute_mcp_tool(
    ctx: &ToolContext,
    id: &str,
    tool_id: Arc<str>,
    tool_name: &str,
    input: &Value,
) -> ToolDoneEvent {
    let done = |output: String, is_error: bool| ToolDoneEvent {
        id: id.to_owned(),
        tool: Arc::clone(&tool_id),
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

/// Deduplicates doom-loop repeats, then runs remaining calls in parallel.
pub(super) async fn process_tool_calls(
    response: maki_providers::StreamResponse,
    recent_calls: &mut RecentCalls,
    mcp: Option<&McpHandle>,
    history: &mut super::history::History,
    event_tx: &crate::EventSender,
    ctx: &ToolContext,
) -> Result<(), AgentError> {
    let tool_uses: Vec<(String, String, Value)> = response
        .message
        .tool_uses()
        .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
        .collect();

    history.push(response.message);

    let mut immediate_errors: Vec<ToolDoneEvent> = Vec::new();
    let mut runnable: Vec<(String, String, Value)> = Vec::new();

    for (id, name, input) in tool_uses {
        debug!(
            tool = %name,
            id = %id,
            input_preview = %crate::tools::schema::preview(&input.to_string()),
            "parsing tool call"
        );
        if recent_calls.is_doom_loop(&name, &input) {
            warn!(tool = %name, "doom loop detected, skipping execution");
            immediate_errors.push(ToolDoneEvent::error(id.clone(), DOOM_LOOP_MESSAGE));
        } else {
            runnable.push((id, name.clone(), input.clone()));
        }
        recent_calls.record(name, &input);
    }

    for err in &immediate_errors {
        event_tx.try_send(AgentEvent::ToolDone(Box::new(err.clone())));
    }

    let mut set = TaskSet::new();
    for (id, name, input) in runnable {
        let event_tx_clone = ctx.event_tx.clone();
        let tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            ..ctx.clone()
        };
        let mcp_owned = mcp.cloned();
        set.spawn(async move {
            let done = run(
                ToolRegistry::native(),
                mcp_owned.as_ref(),
                id,
                &name,
                &input,
                &tool_ctx,
            )
            .await;
            event_tx_clone.try_send(AgentEvent::ToolDone(Box::new(done.clone())));
            done
        });
    }

    let results: Vec<ToolDoneEvent> = set
        .join_all()
        .await
        .into_iter()
        .filter_map(|r| match r {
            Ok(out) => Some(out),
            Err(e) => {
                error!(error = %e, "tool task panicked");
                None
            }
        })
        .collect();

    let mut all_results = results;
    all_results.extend(immediate_errors);
    let tool_msg = crate::types::tool_results(all_results);
    event_tx.send(AgentEvent::ToolResultsSubmitted {
        message: Box::new(tool_msg.clone()),
    })?;
    history.push(tool_msg);
    Ok(())
}

/// Test-only entry that skips native lookup, letting plan-mode and MCP tests
/// exercise the dispatch path without registering a fake native tool.
#[cfg(test)]
async fn dispatch_mcp(
    ctx: &ToolContext,
    id: &str,
    tool_name: &str,
    input: &Value,
) -> ToolDoneEvent {
    let tool_id = ctx
        .mcp
        .as_ref()
        .map(|m| m.interned_name(tool_name))
        .unwrap_or_else(|| Arc::from(UNKNOWN_MCP));
    execute_mcp_tool(ctx, id, tool_id, tool_name, input).await
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
    fn unknown_tool_returns_error_event() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let done = run(
                ToolRegistry::native(),
                None,
                "t1".into(),
                "nonexistent__tool",
                &serde_json::json!({}),
                &ctx,
            )
            .await;
            assert!(done.is_error);
            assert_eq!(done.tool.as_ref(), UNKNOWN_MCP);
            let text = done.output.as_text();
            assert!(text.starts_with(UNKNOWN_TOOL_PREFIX));
            assert!(text.contains("nonexistent__tool"));
        });
    }

    #[test]
    fn mcp_tool_blocked_in_plan_mode() {
        smol::block_on(async {
            let result = dispatch_mcp(
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
            let result = dispatch_mcp(
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

    /// Denies bash and verifies the marker file is never created.
    #[test]
    fn permission_denial_short_circuits_execute() {
        use std::sync::Arc;

        use maki_config::{Effect, PermissionRule, PermissionsConfig};
        use tempfile::TempDir;

        use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager};

        smol::block_on(async {
            let deny_all_bash = PermissionsConfig {
                allow_all: false,
                rules: vec![PermissionRule {
                    tool: crate::tools::BASH_TOOL_NAME.into(),
                    scope: None,
                    effect: Effect::Deny,
                }],
            };
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new(
                deny_all_bash,
                dir.path().to_path_buf(),
            ));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );

            let marker = dir.path().join("should_never_exist");
            let marker_str = marker.to_str().unwrap();

            let done = run(
                ToolRegistry::native(),
                None,
                "t1".into(),
                crate::tools::BASH_TOOL_NAME,
                &serde_json::json!({ "command": format!("touch {marker_str}") }),
                &ctx,
            )
            .await;

            assert!(done.is_error, "permission denial must produce error event");
            assert!(!marker.exists(), "tool executed despite permission denial");
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "error should be the permission-denied message, got: {}",
                done.output.as_text()
            );
        });
    }
}
