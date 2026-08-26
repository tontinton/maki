use std::borrow::Cow;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, error, warn};

use crate::mcp::{McpSession, TOOL_SEARCH_TOOL_NAME, UNKNOWN_MCP};
use crate::task_set::TaskSet;
use crate::tools::registry::{RegisteredTool, ToolInvocation};
use crate::tools::{CallOrigin, LocalToolFn, ToolContext, truncate_bytes};
use crate::{AgentError, AgentEvent, ToolDoneEvent, ToolOutput, ToolStartEvent};
use maki_config::ToolKey;

const DOOM_LOOP_THRESHOLD: usize = 3;
const DOOM_LOOP_MESSAGE: &str = "You have called this tool with identical input 3 times in a row. You are stuck in a loop. Break out and try a different approach.";
const MCP_BLOCKED_IN_PLAN: &str = "MCP tools are not available in plan mode";
const UNKNOWN_TOOL_PREFIX: &str = "unknown tool";
const MCP_PERM_SCOPE_MAX_BYTES: usize = 200;

const SOURCE_LOCAL: &str = "local";
const SOURCE_MCP: &str = "mcp";
const SOURCE_UNKNOWN: &str = "unknown";
const BASH_TOOL: &str = "bash";
const BASH_COMMAND_FIELD: &str = "command";
const GIT_COMMIT: &str = "git commit";
const GH_PR_CREATE: &str = "gh pr create";

const ERROR_CANCELLED: &str = "cancelled";
const ERROR_TIMEOUT: &str = "timeout";
const ERROR_DENIED: &str = "permission_denied";
const ERROR_NOT_FOUND: &str = "not_found";
const ERROR_INVALID_INPUT: &str = "invalid_input";
const ERROR_OTHER: &str = "error";

/// A telemetry counter is not worth an unbounded diff; past this,
/// `similar` returns a coarser but still valid one.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

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

/// Every tool call in maki lands here (native, Lua, MCP, subagents, batch
/// children), which makes it the one place telemetry has to wrap.
pub async fn run(
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let resolved = resolve(ctx, name);
    if !maki_otel::enabled() {
        return run_inner(resolved, id, input, ctx, origin).await;
    }
    let name = resolved.name;
    let source = resolved.route.source();
    let started = Instant::now();
    let done = run_inner(resolved, id, input, ctx, origin).await;
    report(&done, name, &source, input, started.elapsed());
    done
}

/// Where a name goes for one context. Dispatch and telemetry read this one
/// answer, so a name can never be reported as one thing and run as another.
enum Route<'a> {
    Local(&'a LocalToolFn),
    Native(RegisteredTool),
    ToolSearch(&'a McpSession),
    Mcp(&'a McpSession, Arc<str>),
    Unknown,
}

impl Route<'_> {
    /// Registry tools report the plugin behind them, because "native" alone
    /// tells whoever reads the metric nothing.
    fn source(&self) -> Cow<'static, str> {
        match self {
            Self::Native(entry) => entry.source.as_log_field(),
            Self::Local(_) => Cow::Borrowed(SOURCE_LOCAL),
            Self::Mcp(..) => Cow::Borrowed(SOURCE_MCP),
            Self::ToolSearch(_) => Cow::Borrowed(TOOL_SEARCH_TOOL_NAME),
            Self::Unknown => Cow::Borrowed(SOURCE_UNKNOWN),
        }
    }
}

/// A canonical name paired with where it goes. Only [`resolve`] builds one, so
/// no caller can act on a name it forgot to canonicalize.
struct Resolved<'a> {
    name: &'a str,
    route: Route<'a>,
}

/// Precedence, highest first: client (ACP) tools, the registry, then MCP.
/// The context is the only input, because resolving against one session and
/// executing against another is how a tool escapes its audience.
fn resolve<'a>(ctx: &'a ToolContext, name: &'a str) -> Resolved<'a> {
    // Names coming back from model JSON (batch children, `call_tool`, the
    // interpreter bridge) never passed through streaming.rs, so clean up here.
    let name = super::streaming::canonical_tool_name(name);
    let route = if let Some(local) = ctx.local_tools.get(name) {
        Route::Local(local)
    } else if let Some(entry) = ctx.registry.get(name) {
        Route::Native(entry)
    } else if let Some(mcp) = ctx.mcp.as_ref() {
        match mcp.resolve(name) {
            Some(qualified) => Route::Mcp(mcp, qualified),
            None if name == TOOL_SEARCH_TOOL_NAME => Route::ToolSearch(mcp),
            None => Route::Unknown,
        }
    } else {
        Route::Unknown
    };
    Resolved { name, route }
}

/// Parse errors and unknown tools skip the start event so the UI never
/// shows a phantom spinner.
async fn run_inner(
    resolved: Resolved<'_>,
    id: String,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let name = resolved.name;
    let entry = match resolved.route {
        Route::Local(local) => return run_local_tool(local, id, name, input, ctx, origin).await,
        Route::ToolSearch(mcp) => return run_tool_search(mcp, id, input, ctx, origin),
        Route::Mcp(mcp, qualified) => {
            emit_raw_start(
                ctx,
                origin,
                &id,
                &qualified,
                format!("mcp: {qualified}"),
                input,
            );
            return execute_mcp_tool(ctx, mcp, &id, qualified, input, origin).await;
        }
        Route::Unknown => {
            warn!(tool = %name, "unknown tool");
            return ToolDoneEvent {
                id,
                tool: Arc::from(UNKNOWN_MCP),
                output: ToolOutput::Plain(format!("{UNKNOWN_TOOL_PREFIX}: {name}").into()),
                is_error: true,
                annotation: None,
                written_path: None,
            };
        }
        Route::Native(entry) => entry,
    };
    let tool_id: Arc<str> = Arc::from(entry.tool.name());
    let started = Instant::now();

    let done_error = |msg: String| ToolDoneEvent {
        id: id.clone(),
        tool: Arc::clone(&tool_id),
        output: ToolOutput::Plain(msg.into()),
        is_error: true,
        annotation: None,
        written_path: None,
    };

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
    if origin.is_model() {
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
}

/// MCP, local, and search tools never go through invocation parsing,
/// so there is no parsed input to show; the UI gets the raw JSON instead.
fn emit_raw_start(
    ctx: &ToolContext,
    origin: CallOrigin,
    id: &str,
    tool: &Arc<str>,
    summary: String,
    input: &Value,
) {
    if !origin.is_model() {
        return;
    }
    let start = ToolStartEvent {
        id: id.to_owned(),
        tool: Arc::clone(tool),
        summary,
        render_header: None,
        annotation: None,
        input: None,
        raw_input: Some(input.clone()),
        output: None,
    };
    let _ = ctx.event_tx.send(AgentEvent::ToolStart(Box::new(start)));
}

/// Runs without a permission gate: search only reveals names the deferred
/// catalog already showed the model.
fn run_tool_search(
    mcp: &McpSession,
    id: String,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let tool_id: Arc<str> = Arc::from(TOOL_SEARCH_TOOL_NAME);
    let query = input["query"].as_str().unwrap_or_default();
    emit_raw_start(ctx, origin, &id, &tool_id, query.to_owned(), input);
    let (output, is_error) = match mcp.search_tools(query, origin) {
        Ok(out) => (out, false),
        Err(e) => (e, true),
    };
    ToolDoneEvent {
        id,
        tool: tool_id,
        output: ToolOutput::Markdown(output.into()),
        is_error,
        annotation: None,
        written_path: None,
    }
}

async fn run_local_tool(
    local: &LocalToolFn,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let tool_id: Arc<str> = Arc::from(name);
    emit_raw_start(ctx, origin, &id, &tool_id, name.to_owned(), input);
    let tool_ctx = ToolContext {
        tool_use_id: Some(id.clone()),
        ..ctx.clone()
    };
    let (output, is_error) = match local(input.clone(), tool_ctx).await {
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

/// Enforce permission for a native tool. MCP tools bypass this — they go
/// through `execute_mcp_tool` which handles permission checking internally.
///
/// Returns an error if `name` contains dots (not a valid native tool name).
async fn enforce_permission(
    inv: &dyn ToolInvocation,
    name: &str,
    ctx: &ToolContext,
    id: &str,
) -> Result<(), String> {
    if name.contains('.') {
        return Err(format!(
            "enforce_permission called with dotted name: {name}"
        ));
    }
    if let Some(scopes) = inv.permission_scopes().await {
        let tool_key = ToolKey::native(name);
        ctx.permissions
            .enforce(
                &tool_key,
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
    mcp: &McpSession,
    id: &str,
    tool: Arc<str>,
    input: &Value,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let done = |output: String, is_error: bool| ToolDoneEvent {
        id: id.to_owned(),
        tool: Arc::clone(&tool),
        output: ToolOutput::Plain(output.into()),
        is_error,
        annotation: None,
        written_path: None,
    };

    if ctx.mode.plan_path().is_some() {
        return done(MCP_BLOCKED_IN_PLAN.into(), true);
    }

    let perm_tool = match ToolKey::parse(&tool) {
        Ok(k) => k,
        Err(e) => {
            return done(format!("invalid MCP tool key '{tool}': {e}"), true);
        }
    };
    let perm_scope = truncate_bytes(&input.to_string(), MCP_PERM_SCOPE_MAX_BYTES);
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

    // A permitted call counts as loading the tool, so its definition joins the
    // next request; a denied one must not load anything.
    mcp.mark_loaded(&tool, origin);
    match mcp.call_tool(&tool, input).await {
        Ok(text) => done(text, false),
        Err(e) => done(e.to_string(), true),
    }
}

/// Deduplicates doom-loop repeats, then runs remaining calls in parallel.
pub(super) async fn process_tool_calls(
    response: maki_providers::StreamResponse,
    recent_calls: &mut RecentCalls,
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
        set.spawn(async move {
            let done = run(id, &name, &input, &tool_ctx, CallOrigin::Model).await;
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

/// Low-cardinality buckets, because a raw error message would give the
/// collector a new attribute value on every call.
fn classify_error(text: &str) -> &'static str {
    let text = text.to_ascii_lowercase();
    if text.contains("cancel") {
        ERROR_CANCELLED
    } else if text.contains("timed out") || text.contains("timeout") {
        ERROR_TIMEOUT
    } else if text.contains("permission denied") || text.contains("not allowed") {
        ERROR_DENIED
    } else if text.contains("no such file") || text.contains("not found") {
        ERROR_NOT_FOUND
    } else if text.contains("invalid") || text.contains("expected") {
        ERROR_INVALID_INPUT
    } else {
        ERROR_OTHER
    }
}

fn changed_lines(before: &str, after: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    let diff = similar::TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(before, after);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// The same heuristic Claude Code uses: look at what the shell was asked to
/// do, not at what it printed.
fn git_activity(name: &str, input: &Value) {
    if name != BASH_TOOL {
        return;
    }
    let Some(command) = input.get(BASH_COMMAND_FIELD).and_then(Value::as_str) else {
        return;
    };
    if command.contains(GIT_COMMIT) {
        maki_otel::emit::commit_created();
    }
    if command.contains(GH_PR_CREATE) {
        maki_otel::emit::pull_request_created();
    }
}

fn report(done: &ToolDoneEvent, name: &str, source: &str, input: &Value, took: Duration) {
    let error_text = done.is_error.then(|| done.output.as_text());
    let tool_input = maki_otel::logs_tool_details().then(|| input.to_string());
    maki_otel::emit::tool_result(&maki_otel::emit::ToolResult {
        tool_name: name,
        tool_source: source,
        success: !done.is_error,
        duration: took,
        error_type: error_text.as_deref().map(classify_error),
        tool_input: tool_input.as_deref(),
    });
    if let ToolOutput::Diff { before, after, .. } = &done.output {
        let (added, removed) = changed_lines(before, after);
        maki_otel::emit::lines_of_code(added, removed);
    }
    if !done.is_error {
        git_activity(name, input);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use maki_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::mcp::test_support::stub_session;
    use crate::mcp::tool_names;
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager};
    use crate::tools::interpreter_bridge::context_tools;
    use crate::tools::registry::{ToolRegistry, ToolSource};
    use crate::tools::test_support::{
        GUARDED_TOOL_NAME, GuardedMock, mock_tool, stub_ctx, stub_ctx_with_permissions,
    };
    use crate::tools::{
        BoxFuture, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
        PermissionScopes, Tool, ToolAudience, ToolExecResult,
    };

    const TEST_ID: &str = "t1";
    const PROBE_WIRE: &str = "srv__probe";
    const PROBE_QUALIFIED: &str = "srv.probe";
    const OTHER_WIRE: &str = "srv__other";
    const OTHER_QUALIFIED: &str = "srv.other";
    const SHADOWED_NAME: &str = "batch";
    const CLIENT_NAME: &str = "client_probe";
    const TEST_PLUGIN: &str = "test";
    const TEST_PLUGIN_SOURCE: &str = "lua:test";
    const START_PROBE_NAME: &str = "start_probe";
    /// Allowed by the shared stub permissions; nothing is ever written here.
    const TEST_ROOT: &str = "/tmp";
    const PLAN_PATH: &str = "/tmp/plan.md";

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
        map.insert(
            name.to_owned(),
            crate::tools::local_tool(move |input, _ctx| {
                let result = f(&input);
                Box::pin(async move { result })
            }),
        );
        ctx.local_tools = Arc::new(map);
        ctx
    }

    async fn dispatch(ctx: &ToolContext, name: &str, input: &Value) -> ToolDoneEvent {
        run(TEST_ID.into(), name, input, ctx, CallOrigin::Model).await
    }

    async fn dispatch_nested(ctx: &ToolContext, name: &str, input: &Value) -> ToolDoneEvent {
        run(TEST_ID.into(), name, input, ctx, CallOrigin::Nested).await
    }

    fn with_mcp(mut ctx: ToolContext, mcp: &McpSession) -> ToolContext {
        ctx.mcp = Some(mcp.clone());
        ctx
    }

    /// Publishes the tools with empty descriptions: search matches on the name.
    fn stub_mcp(qualified: &[&str]) -> McpSession {
        let tools: Vec<_> = qualified.iter().map(|name| (*name, "")).collect();
        stub_session(&tools)
    }

    fn mcp_ctx(mcp: &McpSession) -> ToolContext {
        with_mcp(stub_ctx(&AgentMode::Build), mcp)
    }

    fn registered(tool: Arc<dyn Tool>) -> Arc<ToolRegistry> {
        let registry = ToolRegistry::new();
        register(&registry, tool);
        Arc::new(registry)
    }

    fn register(registry: &ToolRegistry, tool: Arc<dyn Tool>) {
        registry
            .register(
                tool,
                ToolSource::Lua {
                    plugin: TEST_PLUGIN.into(),
                },
            )
            .unwrap();
    }

    fn registry_with(names: &[&str]) -> Arc<ToolRegistry> {
        let registry = ToolRegistry::new();
        for name in names {
            register(&registry, mock_tool(name, ToolAudience::all()));
        }
        Arc::new(registry)
    }

    fn denying_ctx(tool: ToolKey) -> ToolContext {
        let config = PermissionsConfig {
            rules: vec![PermissionRule {
                tool,
                scope: None,
                effect: Effect::Deny,
            }],
            ..Default::default()
        };
        let permissions = Arc::new(PermissionManager::new(
            config,
            PathBuf::from(TEST_ROOT),
            Arc::default(),
        ));
        stub_ctx_with_permissions(&AgentMode::Build, permissions)
    }

    #[test]
    fn local_tool_shadows_registry_and_maps_errors() {
        smol::block_on(async {
            let mut ctx = local_ctx(SHADOWED_NAME, |input| {
                Ok(format!("local:{}", input["path"]))
            });
            ctx.registry = registry_with(&[SHADOWED_NAME]);
            let done = dispatch(&ctx, SHADOWED_NAME, &serde_json::json!({"path": "/a"})).await;
            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), r#"local:"/a""#);

            let ctx = local_ctx("boom", |_| Err("nope".into()));
            let done = dispatch(&ctx, "boom", &serde_json::json!({})).await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), "nope");
        });
    }

    #[test]
    fn functions_prefixed_name_dispatches_to_canonical_tool() {
        smol::block_on(async {
            let ctx = local_ctx("ok", |_| Ok("ran".into()));
            let done = dispatch(&ctx, "functions.ok", &serde_json::json!({})).await;
            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), "ran");
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
                crate::tools::local_tool(|input, _ctx| {
                    let out = input.to_string();
                    Box::pin(async move { Ok(out) })
                }),
            );
            ctx.local_tools = Arc::new(map);

            let input = serde_json::json!({"path": "/a"});
            let done = dispatch(&ctx, "local_echo", &input).await;
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
    fn tool_search_routes_and_loads_matches() {
        smol::block_on(async {
            let mcp = stub_mcp(&[PROBE_QUALIFIED]);
            let done = dispatch(
                &mcp_ctx(&mcp),
                TOOL_SEARCH_TOOL_NAME,
                &serde_json::json!({"query": "probe"}),
            )
            .await;
            assert!(!done.is_error, "got: {}", done.output.as_text());
            assert_eq!(done.tool.as_ref(), TOOL_SEARCH_TOOL_NAME);
            assert!(done.output.as_text().contains(PROBE_WIRE));

            let mut tools = serde_json::json!([]);
            mcp.extend_tools(&mut tools);
            assert!(
                tool_names(&tools).contains(&PROBE_WIRE),
                "searched tool must join the next request"
            );
        });
    }

    #[test_case(serde_json::json!({"query": "  "}) ; "blank_query")]
    #[test_case(serde_json::json!({}) ; "missing_query")]
    fn tool_search_bad_query_is_error_event(input: Value) {
        smol::block_on(async {
            let done = dispatch(
                &mcp_ctx(&stub_mcp(&[PROBE_QUALIFIED])),
                TOOL_SEARCH_TOOL_NAME,
                &input,
            )
            .await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), crate::mcp::SEARCH_EMPTY_QUERY);
        });
    }

    #[test]
    fn calling_deferred_mcp_tool_marks_it_loaded() {
        smol::block_on(async {
            let mcp = stub_mcp(&[PROBE_QUALIFIED]);
            let done = dispatch(&mcp_ctx(&mcp), PROBE_WIRE, &serde_json::json!({})).await;
            assert_eq!(done.tool.as_ref(), PROBE_QUALIFIED, "must route to MCP");

            let mut tools = serde_json::json!([]);
            mcp.extend_tools(&mut tools);
            assert_eq!(
                tool_names(&tools),
                vec![PROBE_WIRE],
                "called tool must join the next request"
            );
        });
    }

    /// `McpSession::new` rebuilds the loaded set from the `ToolUse` blocks in
    /// history, which hold no nested call, so loading one here would make the
    /// live tool array differ from the resumed one.
    #[test_case(PROBE_WIRE, serde_json::json!({}), PROBE_QUALIFIED ; "tool_call")]
    #[test_case(TOOL_SEARCH_TOOL_NAME, serde_json::json!({"query": "probe"}), TOOL_SEARCH_TOOL_NAME ; "tool_search")]
    fn nested_call_reaches_mcp_without_loading_anything(name: &str, input: Value, routed: &str) {
        smol::block_on(async {
            let mcp = stub_mcp(&[PROBE_QUALIFIED]);
            let done = dispatch_nested(&mcp_ctx(&mcp), name, &input).await;
            assert_eq!(done.tool.as_ref(), routed, "must route to MCP");

            let mut tools = serde_json::json!([]);
            mcp.extend_tools(&mut tools);
            assert_eq!(
                tool_names(&tools),
                vec![TOOL_SEARCH_TOOL_NAME],
                "a nested call must not change the next request"
            );
        });
    }

    #[test]
    fn denied_mcp_call_does_not_load_definition() {
        smol::block_on(async {
            let mcp = stub_mcp(&[PROBE_QUALIFIED]);
            let ctx = with_mcp(denying_ctx(ToolKey::parse(PROBE_QUALIFIED).unwrap()), &mcp);
            let done = dispatch(&ctx, PROBE_WIRE, &serde_json::json!({})).await;
            assert!(done.is_error);
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "got: {}",
                done.output.as_text()
            );

            let mut tools = serde_json::json!([]);
            mcp.extend_tools(&mut tools);
            assert_eq!(
                tool_names(&tools),
                vec![TOOL_SEARCH_TOOL_NAME],
                "denied call must not load the definition"
            );
        });
    }

    #[test]
    fn local_tool_named_tool_search_shadows_mcp_search() {
        smol::block_on(async {
            let mcp = stub_mcp(&[PROBE_QUALIFIED]);
            let ctx = with_mcp(
                local_ctx(TOOL_SEARCH_TOOL_NAME, |_| Ok("local wins".into())),
                &mcp,
            );
            let done = dispatch(
                &ctx,
                TOOL_SEARCH_TOOL_NAME,
                &serde_json::json!({"query": "probe"}),
            )
            .await;
            assert_eq!(done.output.as_text(), "local wins");
        });
    }

    #[test]
    fn telemetry_source_names_the_plugin_for_registry_tools() {
        let mut ctx = mcp_ctx(&stub_mcp(&[PROBE_QUALIFIED, OTHER_QUALIFIED]));
        ctx.registry = registry_with(&[PROBE_WIRE]);
        assert_eq!(resolve(&ctx, PROBE_WIRE).route.source(), TEST_PLUGIN_SOURCE);
        assert_eq!(resolve(&ctx, OTHER_WIRE).route.source(), SOURCE_MCP);
    }

    #[test]
    fn resolve_prefers_local_over_registry_and_registry_over_mcp() {
        let mcp = stub_mcp(&[PROBE_QUALIFIED]);
        let mut ctx = with_mcp(local_ctx(PROBE_WIRE, |_| Ok(String::new())), &mcp);
        ctx.registry = registry_with(&[PROBE_WIRE]);

        assert!(matches!(resolve(&ctx, PROBE_WIRE).route, Route::Local(_)));

        ctx.local_tools = Arc::default();
        assert!(matches!(resolve(&ctx, PROBE_WIRE).route, Route::Native(_)));

        ctx.registry = Arc::new(ToolRegistry::new());
        assert!(matches!(resolve(&ctx, PROBE_WIRE).route, Route::Mcp(..)));
    }

    /// A registry entry is an audience decision about that name, so any name the
    /// registry holds stays unbound, be it one MCP publishes or one a client
    /// tool shadows. Deferred MCP tools do bind: hidden from the tool array is
    /// not the same as uncallable.
    #[test]
    fn context_tools_bind_only_names_the_registry_does_not_hold() {
        let ctx = stub_ctx(&AgentMode::Build);
        assert!(
            context_tools(&ctx).is_empty(),
            "without MCP and client tools there is nothing to bind"
        );

        let mcp = stub_mcp(&[PROBE_QUALIFIED, OTHER_QUALIFIED]);
        let mut ctx = with_mcp(local_ctx(CLIENT_NAME, |_| Ok(String::new())), &mcp);
        assert_eq!(
            context_tools(&ctx),
            [CLIENT_NAME, OTHER_WIRE, PROBE_WIRE, TOOL_SEARCH_TOOL_NAME]
        );

        ctx.registry = registry_with(&[PROBE_WIRE, CLIENT_NAME]);
        assert_eq!(context_tools(&ctx), [OTHER_WIRE, TOOL_SEARCH_TOOL_NAME]);
    }

    /// The model only fixes names it recognizes, so it hears back what it sent.
    #[test_case(None, "nonexistent.tool" ; "without_mcp")]
    #[test_case(Some(PROBE_QUALIFIED), OTHER_WIRE ; "unpublished_wire_name")]
    fn unknown_tool_errors_and_echoes_the_name_verbatim(published: Option<&str>, name: &str) {
        smol::block_on(async {
            let mcp = published.map(|tool| stub_mcp(&[tool]));
            let ctx = match &mcp {
                Some(mcp) => mcp_ctx(mcp),
                None => stub_ctx(&AgentMode::Build),
            };
            let done = dispatch(&ctx, name, &serde_json::json!({})).await;
            assert!(done.is_error);
            assert_eq!(done.tool.as_ref(), UNKNOWN_MCP);
            let text = done.output.as_text();
            assert!(text.starts_with(UNKNOWN_TOOL_PREFIX), "got: {text}");
            assert!(text.contains(name), "got: {text}");
        });
    }

    #[test]
    fn mcp_tool_blocked_in_plan_mode() {
        smol::block_on(async {
            let plan = AgentMode::Plan(PathBuf::from(PLAN_PATH));
            let ctx = with_mcp(stub_ctx(&plan), &stub_mcp(&[PROBE_QUALIFIED]));
            let done = dispatch(&ctx, PROBE_WIRE, &serde_json::json!({})).await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), MCP_BLOCKED_IN_PLAN);
        });
    }

    #[test]
    fn permission_denial_short_circuits_execute() {
        smol::block_on(async {
            let mut ctx = denying_ctx(ToolKey::native(GUARDED_TOOL_NAME));
            ctx.registry = registered(Arc::new(GuardedMock));

            let done = dispatch(&ctx, GUARDED_TOOL_NAME, &serde_json::json!({})).await;

            assert!(done.is_error, "permission denial must produce error event");
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "error should be the permission-denied message, got: {}",
                done.output.as_text()
            );
        });
    }

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
            let mut ctx = denying_ctx(ToolKey::native(START_PROBE_NAME));
            let probe = StartProbe::default();
            let (started, executed) = (Arc::clone(&probe.started), Arc::clone(&probe.executed));
            ctx.registry = registered(Arc::new(probe));

            let done = dispatch(&ctx, START_PROBE_NAME, &serde_json::json!({})).await;

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

#[cfg(test)]
mod telemetry_tests {
    use test_case::test_case;

    use super::*;

    const BEFORE: &str = "a\nb\nc\n";
    const AFTER: &str = "a\nB\nc\nd\n";

    #[test_case("operation was cancelled", ERROR_CANCELLED; "cancelled")]
    #[test_case("command timed out after 120s", ERROR_TIMEOUT; "timed_out")]
    #[test_case("permission denied: bash", ERROR_DENIED; "denied")]
    #[test_case("no such file or directory", ERROR_NOT_FOUND; "missing_file")]
    #[test_case("invalid input: expected a string", ERROR_INVALID_INPUT; "invalid")]
    #[test_case("boom", ERROR_OTHER; "fallback")]
    fn errors_bucket_into_low_cardinality_types(text: &str, expected: &str) {
        assert_eq!(classify_error(text), expected);
    }

    #[test]
    fn diffs_count_added_and_removed_lines() {
        assert_eq!(changed_lines(BEFORE, AFTER), (2, 1));
        assert_eq!(changed_lines(BEFORE, BEFORE), (0, 0));
    }
}
