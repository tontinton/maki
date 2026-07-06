use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::{debug, error, warn};

use crate::mcp::{McpHandle, UNKNOWN_MCP};
use crate::task_set::TaskSet;
use crate::tools::registry::{ToolInvocation, ToolRegistry};
use crate::tools::{LocalToolFn, ToolContext};
use crate::{AgentError, AgentEvent, ToolDoneEvent, ToolOutput, ToolStartEvent};

#[derive(Clone, Copy)]
pub enum Emit {
    Notify,
    Silent,
}

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
    emit: Emit,
) -> ToolDoneEvent {
    if let Some(local) = ctx.local_tools.get(name) {
        return run_local_tool(local, id, name, input, ctx, emit);
    }
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
        output: ToolOutput::Plain(msg.into()),
        is_error: true,
        annotation: None,
        written_path: None,
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

        if let Some(target) = invocation.mutable_path() {
            let is_plan_target = ctx.mode.plan_path().is_some_and(|pp| target == pp);
            if !is_plan_target {
                if ctx.mode.plan_path().is_some() {
                    warn!(
                        tool = %name,
                        target = %target.display(),
                        "blocked write in plan mode"
                    );
                    return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
                }
                if let Some(reason) = ctx.permissions.boundary_block_reason(target) {
                    return done_error(reason);
                }
            }
        }

        let header_result = invocation.start_header().await;
        let start = ToolStartEvent {
            id: id.clone(),
            tool: Arc::clone(&tool_id),
            summary: header_result.text(),
            render_header: header_result.snapshot(),
            annotation: invocation.start_annotation(),
            input: None,
            raw_input: Some(input.clone()),
            output: invocation.start_output(ctx),
        };
        if matches!(emit, Emit::Notify) {
            let _ = ctx.event_tx.send(AgentEvent::ToolStart(Box::new(start)));
        }

        invocation.start(ctx).await;

        if let Err(e) = enforce_permission(invocation.as_ref(), name, ctx, &id).await {
            return done_error(e);
        }

        let result = invocation.execute(ctx).await;

        let elapsed = started.elapsed();
        match result.output {
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
                    annotation: result.annotation,
                    written_path: result.written_path,
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
            raw_input: Some(input.clone()),
            output: None,
        };
        if matches!(emit, Emit::Notify) {
            let _ = ctx.event_tx.send(AgentEvent::ToolStart(Box::new(start)));
        }
        execute_mcp_tool(ctx, &id, tool_id, name, input).await
    } else {
        let msg = format!("{UNKNOWN_TOOL_PREFIX}: {name}");
        warn!(tool = %name, "unknown tool");
        done_error(msg)
    }
}

fn run_local_tool(
    local: &LocalToolFn,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    emit: Emit,
) -> ToolDoneEvent {
    let tool_id: Arc<str> = Arc::from(name);
    if matches!(emit, Emit::Notify) {
        let start = ToolStartEvent {
            id: id.clone(),
            tool: Arc::clone(&tool_id),
            summary: name.to_owned(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: Some(input.clone()),
            output: None,
        };
        let _ = ctx.event_tx.send(AgentEvent::ToolStart(Box::new(start)));
    }
    let (output, is_error) = match local(input) {
        Ok(output) => (output, false),
        Err(e) => {
            warn!(tool = %name, error = %e, "local tool failed");
            (e, true)
        }
    };
    ToolDoneEvent {
        id,
        tool: tool_id,
        output: ToolOutput::Plain(output.into()),
        is_error,
        annotation: None,
        written_path: None,
    }
}

async fn enforce_permission(
    inv: &dyn ToolInvocation,
    name: &str,
    ctx: &ToolContext,
    id: &str,
) -> Result<(), String> {
    if let Some(scopes) = inv.permission_scopes().await {
        ctx.permissions
            .enforce(
                name,
                &scopes,
                &ctx.event_tx,
                ctx.user_response_rx.as_deref(),
                id,
                &ctx.cancel,
                ctx.mode.plan_path(),
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
        output: ToolOutput::Plain(output.into()),
        is_error,
        annotation: None,
        written_path: None,
    };

    if ctx.mode.plan_path().is_some() {
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
    let perm_scopes = crate::tools::PermissionScopes::single(perm_scope);

    if let Err(e) = ctx
        .permissions
        .enforce(
            &perm_tool,
            &perm_scopes,
            &ctx.event_tx,
            ctx.user_response_rx.as_deref(),
            id,
            &ctx.cancel,
            ctx.mode.plan_path(),
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
    let mut spawned_ids: Vec<String> = Vec::new();
    for (id, name, input) in runnable {
        spawned_ids.push(id.clone());
        let event_tx_clone = ctx.event_tx.clone();
        let tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            ..ctx.clone()
        };
        let mcp_owned = mcp.cloned();
        set.spawn(async move {
            let done = run(
                &tool_ctx.registry,
                mcp_owned.as_ref(),
                id,
                &name,
                &input,
                &tool_ctx,
                Emit::Notify,
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
        .zip(spawned_ids)
        .map(|(r, id)| match r {
            Ok(out) => out,
            Err(e) => {
                error!(error = %e, "tool task panicked");
                ToolDoneEvent::error(id, format!("internal error: tool panicked: {e}"))
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
    use std::sync::Arc;

    use maki_config::{Effect, PermissionRule, PermissionsConfig};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager};
    use crate::tools::registry::ToolSource;
    use crate::tools::test_support::{GUARDED_TOOL_NAME, GuardedMock};

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

    fn local_ctx(
        name: &str,
        f: impl Fn(&Value) -> Result<String, String> + Send + Sync + 'static,
    ) -> ToolContext {
        let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
        let mut map = std::collections::HashMap::new();
        map.insert(name.to_owned(), Arc::new(f) as LocalToolFn);
        ctx.local_tools = Arc::new(map);
        ctx
    }

    #[test]
    fn local_tool_shadows_registry_and_maps_errors() {
        smol::block_on(async {
            let ctx = local_ctx("batch", |input| Ok(format!("local:{}", input["path"])));
            let done = run(
                ToolRegistry::native(),
                None,
                "t1".into(),
                "batch",
                &serde_json::json!({"path": "/a"}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), r#"local:"/a""#);

            let ctx = local_ctx("boom", |_| Err("nope".into()));
            let done = run(
                ToolRegistry::native(),
                None,
                "t2".into(),
                "boom",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), "nope");
        });
    }

    #[test]
    fn local_tool_notify_emits_tool_start_with_raw_input() {
        smol::block_on(async {
            let (tx, rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(tx, 0);
            let mut ctx =
                crate::tools::test_support::stub_ctx_with(&AgentMode::Build, Some(&event_tx), None);
            let mut map = std::collections::HashMap::new();
            map.insert(
                "local_echo".to_owned(),
                Arc::new(|input: &Value| Ok(input.to_string())) as LocalToolFn,
            );
            ctx.local_tools = Arc::new(map);

            let input = serde_json::json!({"path": "/a"});
            let done = run(
                ToolRegistry::native(),
                None,
                "t1".into(),
                "local_echo",
                &input,
                &ctx,
                Emit::Notify,
            )
            .await;
            assert!(!done.is_error);

            let envelope = rx
                .try_recv()
                .expect("ToolStart must be emitted before the tool completes");
            let AgentEvent::ToolStart(start) = envelope.event else {
                panic!("expected ToolStart, got {:?}", envelope.event);
            };
            assert_eq!(start.tool.as_ref(), "local_echo");
            assert_eq!(start.summary, "local_echo");
            assert_eq!(start.raw_input, Some(input));
        });
    }

    #[test]
    fn unknown_tool_returns_error_event() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let done = run(
                &ctx.registry,
                None,
                "t1".into(),
                "nonexistent__tool",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
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

    #[test]
    fn permission_denial_short_circuits_execute() {
        smol::block_on(async {
            let deny_cfg = PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: GUARDED_TOOL_NAME.into(),
                    scope: None,
                    effect: Effect::Deny,
                }],
                ..Default::default()
            };
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new(deny_cfg, dir.path().to_path_buf()));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );

            let registry = ToolRegistry::new();
            registry
                .register(Arc::new(GuardedMock), ToolSource::Native)
                .unwrap();

            let done = run(
                &registry,
                None,
                "t1".into(),
                GUARDED_TOOL_NAME,
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error, "permission denial must produce error event");
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "error should be the permission-denied message, got: {}",
                done.output.as_text()
            );
        });
    }

    const START_PROBE_NAME: &str = "start_probe";

    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::tools::{
        BoxFuture, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
        PermissionScopes, Tool, ToolExecResult,
    };

    #[derive(Default)]
    struct StartProbe {
        started: Arc<AtomicBool>,
        executed: Arc<AtomicBool>,
    }

    struct StartProbeInvocation {
        started: Arc<AtomicBool>,
        executed: Arc<AtomicBool>,
    }

    impl ToolInvocation for StartProbeInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("probe".into()))
        }
        fn start<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
            self.started.store(true, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        }
        fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
            Box::pin(std::future::ready(Some(PermissionScopes::single(
                "probe".into(),
            ))))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            self.executed.store(true, Ordering::SeqCst);
            Box::pin(async {
                ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            })
        }
    }

    impl Tool for StartProbe {
        fn name(&self) -> &str {
            START_PROBE_NAME
        }
        fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
            "start probe".into()
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(StartProbeInvocation {
                started: Arc::clone(&self.started),
                executed: Arc::clone(&self.executed),
            }))
        }
    }

    /// A denied tool should still get its preview, but never its `execute`.
    #[test]
    fn start_runs_before_permission_denial_blocks_execute() {
        smol::block_on(async {
            let deny_cfg = PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: START_PROBE_NAME.into(),
                    scope: None,
                    effect: Effect::Deny,
                }],
                ..Default::default()
            };
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new(deny_cfg, dir.path().to_path_buf()));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );

            let probe = StartProbe::default();
            let (started, executed) = (Arc::clone(&probe.started), Arc::clone(&probe.executed));
            let registry = ToolRegistry::new();
            registry
                .register(Arc::new(probe), ToolSource::Native)
                .unwrap();

            let done = run(
                &registry,
                None,
                "t1".into(),
                START_PROBE_NAME,
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error, "denial must error");
            assert!(
                started.load(Ordering::SeqCst),
                "start must run before permission enforcement"
            );
            assert!(
                !executed.load(Ordering::SeqCst),
                "execute must not run after denial"
            );
        });
    }
}
