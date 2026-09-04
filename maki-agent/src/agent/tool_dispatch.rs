use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tracing::{debug, error, warn};

use crate::mcp::{McpSession, TOOL_SEARCH_TOOL_NAME, UNKNOWN_MCP};
use crate::permissions::ReviewSource;
use crate::task_set::TaskSet;
use crate::tools::hook::{Authority, HookCall, HookStage, OUTPUT_IS_ERROR, OUTPUT_TEXT, Verdict};
use crate::tools::registry::{InstalledHook, RegisteredTool, ToolInvocation};
use crate::tools::{
    CallOrigin, Deadline, FileKey, LocalTool, LocalToolFn, ToolAudience, ToolContext,
    truncate_bytes,
};
use crate::{AgentError, AgentEvent, ToolDoneEvent, ToolOutput, ToolStartEvent};
use maki_config::ToolKey;
use maki_storage::id::SessionRef;

const DOOM_LOOP_THRESHOLD: usize = 3;
const DOOM_LOOP_MESSAGE: &str = "You have called this tool with identical input 3 times in a row. You are stuck in a loop. Break out and try a different approach.";
const UNKNOWN_TOOL_PREFIX: &str = "unknown tool";
const MCP_PERM_SCOPE_MAX_BYTES: usize = 200;

const SOURCE_NATIVE: &str = "native";
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

/// The window a chain gets when the call carries no deadline of its own.
/// Generous, because a layer may shell out before it decides, but a layer that
/// parks and never comes back has to end somewhere short of "when the user
/// gives up".
const HOOK_CHAIN_MAX: Duration = Duration::from_secs(60);

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
/// children), which makes it the one place telemetry has to wrap and the one
/// place [hooks] fire.
///
/// [hooks]: crate::tools::hook
pub async fn run(
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let resolved = resolve(ctx, name);
    let name = resolved.name;
    let hook = Hook::of(ctx, &resolved, origin);

    let verdict = match &hook {
        Some(hook) => hook.filter_input(&id, input).await,
        None => Verdict::Unchanged,
    };
    let input = match verdict {
        Verdict::Unchanged => Cow::Borrowed(input),
        Verdict::Replaced(value) => {
            debug!(tool = %name, "input hook rewrote the call");
            Cow::Owned(value)
        }
        Verdict::Denied(reason) => {
            warn!(tool = %name, reason = %reason, "input hook stopped the call");
            return ToolDoneEvent {
                id,
                tool: Arc::from(name),
                output: ToolOutput::Plain(reason.into()),
                is_error: true,
                annotation: None,
                written_path: None,
            };
        }
    };

    let telemetry = maki_otel::enabled().then(|| (resolved.route.source(), Instant::now()));
    let mut done = run_inner(resolved, id, &input, ctx, origin).await;
    if let Some(hook) = &hook {
        hook.filter_output(&mut done).await;
    }
    if let Some((source, started)) = telemetry {
        report(&done, name, &source, &input, started.elapsed());
    }
    done
}

/// The hook installed on this registry, bound to one call. `None` when nobody
/// installed one, or when the name routes nowhere to run.
///
/// The call id is handed to each stage instead of held, because `run_inner`
/// owns it in between.
struct Hook<'a> {
    installed: InstalledHook,
    ctx: &'a ToolContext,
    tool: &'a str,
    origin: CallOrigin,
    authority: Authority,
}

impl<'a> Hook<'a> {
    fn of(ctx: &'a ToolContext, resolved: &Resolved<'a>, origin: CallOrigin) -> Option<Self> {
        Some(Self {
            installed: ctx.registry.hook()?,
            ctx,
            tool: resolved.name,
            origin,
            authority: resolved.route.authority()?,
        })
    }

    async fn filter_input(&self, tool_id: &str, input: &Value) -> Verdict {
        if !self.installed.wraps(self.tool, HookStage::Input) {
            return Verdict::Unchanged;
        }
        let cancelled = Verdict::Denied(ERROR_CANCELLED.to_owned());
        self.fire(HookStage::Input, tool_id, input.clone(), cancelled)
            .await
    }

    /// Rewrites the finished event in place. Text and error flag move together,
    /// so a hook that cannot reach the text cannot flip the flag either.
    async fn filter_output(&self, done: &mut ToolDoneEvent) {
        if !self.installed.wraps(self.tool, HookStage::Output) {
            return;
        }
        let was_error = done.is_error;
        let Some(text) = done.output.filterable_text_mut() else {
            debug!(
                tool = %self.tool,
                "output hook skipped: this output renders from fields, not prose"
            );
            return;
        };
        let value = json!({ OUTPUT_TEXT: &*text, OUTPUT_IS_ERROR: was_error });
        let (rewritten, is_error) = match self
            .fire(HookStage::Output, &done.id, value, Verdict::Unchanged)
            .await
        {
            Verdict::Unchanged => return,
            // Nothing left to stop, so the reason becomes what the model reads.
            Verdict::Denied(reason) => (reason, true),
            Verdict::Replaced(value) => match value.get(OUTPUT_TEXT).and_then(Value::as_str) {
                Some(replaced) => (
                    replaced.to_owned(),
                    value
                        .get(OUTPUT_IS_ERROR)
                        .and_then(Value::as_bool)
                        .unwrap_or(was_error),
                ),
                None => {
                    warn!(
                        tool = %self.tool,
                        field = OUTPUT_TEXT,
                        "output hook replaced the output without a text field, leaving it alone"
                    );
                    return;
                }
            },
        };
        *text = rewritten;
        done.is_error = is_error;
        debug!(tool = %self.tool, "output hook rewrote the output");
    }

    /// Cancellation outranks a hook: nobody is left to read the verdict, so
    /// the wait ends with `on_cancel`.
    async fn fire(
        &self,
        stage: HookStage,
        tool_id: &str,
        value: Value,
        on_cancel: Verdict,
    ) -> Verdict {
        let call = HookCall {
            tool: self.tool,
            tool_id,
            session_id: self.ctx.session_id.as_ref().map(SessionRef::as_str),
            origin: self.origin,
            authority: self.authority,
            cancel: &self.ctx.cancel,
            deadline: self.window(),
        };
        self.ctx
            .cancel
            .race(self.installed.run(stage, value, &call))
            .await
            .unwrap_or(on_cancel)
    }

    /// Read when a stage fires, not once per call: the input chain and the tool
    /// spend from the same budget, so an output chain handed the entry-time
    /// answer would inherit a window the call already used up.
    fn window(&self) -> Instant {
        let cap = Instant::now() + HOOK_CHAIN_MAX;
        match self.ctx.deadline {
            Deadline::At(at) => at.min(cap),
            Deadline::None => cap,
        }
    }
}

/// Where a name goes for one context. Dispatch and telemetry read this one
/// answer, so a name can never be reported as one thing and run as another.
enum Route<'a> {
    Local(&'a LocalTool),
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

    /// What hooking this call would lend. Every route answers here, so a new
    /// one cannot reach a hook without naming its price. Only a declared
    /// capability narrows it: reading undeclared as free is what would let an
    /// unprivileged plugin steer `batch` or `code_execution` into any tool it
    /// likes.
    fn authority(&self) -> Option<Authority> {
        match self {
            Self::Native(entry) => Some(
                entry
                    .tool
                    .required_permission()
                    .map_or(Authority::Unbounded, Authority::Capability),
            ),
            Self::ToolSearch(_) | Self::Local(_) | Self::Mcp(..) => Some(Authority::Unbounded),
            // A rewrite cannot make the name exist, so firing here would hand
            // out a tool's authority without the tool.
            Self::Unknown => None,
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

/// One callable name, as [`resolve`] would route it.
pub struct Callable {
    /// The name to dispatch. Always what `resolve` was asked, never an alias.
    pub name: String,
    /// A name a host that binds tools as identifiers can use, set only when
    /// `name` is not one already (MCP servers publish `srv__get-docs`). Call
    /// `name`, bind `alias`.
    pub alias: Option<String>,
    pub source: &'static str,
    /// The audience of whatever will run, not of whatever shares its name.
    pub audience: ToolAudience,
    /// Registry tools only. MCP and host tools publish their schema to the
    /// model in the request's tool array, so repeating it here would buy an
    /// allocation per call and nothing else.
    pub schema: Option<Value>,
}

/// Every name this context can dispatch, deduplicated in [`resolve`]'s
/// precedence, so an entry always describes the tool that a call to that name
/// would actually reach. It lives beside `resolve` so the two cannot drift into
/// answering differently.
///
/// Filtered by the same filter that built the request's tool array, so a name
/// the model never saw is not one a script can reach either. What is left is
/// the caller's own policy, read off `audience` (a sandbox wants
/// `INTERPRETER`).
///
/// Recompute per call: MCP republishes its index whenever a server comes or goes.
pub fn callable(ctx: &ToolContext) -> Vec<Callable> {
    let filter = &ctx.tool_filter;
    let mut out: Vec<Callable> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    // A name belongs to the first source dispatch would reach, claimed before
    // any filter runs: a registry tool this audience may not call still owns its
    // name, or MCP would publish a way around it.
    let mut claim = |name: &str, audience: ToolAudience| {
        let first = claimed.insert(name.to_owned());
        first && audience.contains(ctx.audience)
    };
    let entry_of = |name: &str, source, audience, schema| Callable {
        name: name.to_owned(),
        alias: None,
        source,
        audience,
        schema,
    };

    let mut local: Vec<(&String, &LocalTool)> = ctx.local_tools.iter().collect();
    local.sort_by(|a, b| a.0.cmp(b.0));
    for (name, tool) in local {
        if claim(name, tool.audience) {
            out.push(entry_of(name, SOURCE_LOCAL, tool.audience, None));
        }
    }
    for entry in ctx.registry.iter().iter() {
        let audience = entry.tool.audience();
        if !claim(entry.name(), audience) || !filter.matches(entry.name()) {
            continue;
        }
        out.push(entry_of(
            entry.name(),
            SOURCE_NATIVE,
            audience,
            Some(entry.tool.schema()),
        ));
    }
    if let Some(mcp) = ctx.mcp.as_ref() {
        let mut names = mcp.wire_names();
        names.push(TOOL_SEARCH_TOOL_NAME.to_owned());
        names.sort();
        for name in names {
            // MCP has no audience system: a server is reachable or it is not,
            // and a session holding one already offers its tools to the model.
            if claim(&name, ToolAudience::all()) {
                out.push(entry_of(&name, SOURCE_MCP, ToolAudience::all(), None));
            }
        }
    }
    assign_aliases(&mut out);
    out
}

/// Fills in `alias` for names an identifier cannot hold. A collision (a server
/// publishing both `get-docs` and `get_docs`) leaves both aliases unset rather
/// than pointing one name at the other's tool.
fn assign_aliases(tools: &mut [Callable]) {
    let aliases: Vec<Option<String>> = tools.iter().map(|t| identifier_alias(&t.name)).collect();
    let mut claims: HashMap<String, usize> = HashMap::new();
    for claimant in tools
        .iter()
        .map(|t| t.name.clone())
        .chain(aliases.iter().flatten().cloned())
    {
        *claims.entry(claimant).or_default() += 1;
    }
    // An alias always claims itself once, and never its own name, or
    // `identifier_alias` would have declined it. A second claim is therefore
    // another tool's name or alias, and then neither of them may have it.
    for (tool, alias) in tools.iter_mut().zip(aliases) {
        if alias.as_deref().is_some_and(|a| claims[a] == 1) {
            tool.alias = alias;
        }
    }
}

fn identifier_alias(name: &str) -> Option<String> {
    let is_body = |c: char| c.is_ascii_alphanumeric() || c == '_';
    // A leading digit is not something substitution can fix without inventing a
    // character the model never saw.
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    name.chars().any(|c| !is_body(c)).then(|| {
        name.chars()
            .map(|c| if is_body(c) { c } else { '_' })
            .collect()
    })
}

/// Pure router: every arm owns its own start event, permission gate and
/// telemetry, so adding a source never means editing another one's path.
async fn run_inner(
    resolved: Resolved<'_>,
    id: String,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
    let name = resolved.name;
    match resolved.route {
        Route::Local(local) => run_local_tool(&local.handler, id, name, input, ctx, origin).await,
        Route::Native(entry) => run_native_tool(entry, id, name, input, ctx, origin).await,
        Route::ToolSearch(mcp) => run_tool_search(mcp, id, input, ctx, origin),
        Route::Mcp(mcp, qualified) => {
            execute_mcp_tool(ctx, mcp, &id, qualified, input, origin).await
        }
        Route::Unknown => {
            warn!(tool = %name, "unknown tool");
            ToolDoneEvent {
                id,
                tool: Arc::from(UNKNOWN_MCP),
                output: ToolOutput::Plain(format!("{UNKNOWN_TOOL_PREFIX}: {name}").into()),
                is_error: true,
                annotation: None,
                written_path: None,
            }
        }
    }
}

/// Parse errors skip the start event so the UI never shows a phantom spinner.
async fn run_native_tool(
    entry: RegisteredTool,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    origin: CallOrigin,
) -> ToolDoneEvent {
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

    // The one declaration a file-mutating tool makes, and the only place its
    // four consequences live: plan-mode block, boundary block, write
    // serialization, and the stale-read check. Keyed once so all four agree on
    // what "the same file" means.
    let mutated = invocation.mutable_path().map(FileKey::new);

    if let Some(key) = &mutated {
        let is_plan_target = ctx
            .mode
            .plan_path()
            .is_some_and(|plan| FileKey::new(plan) == *key);
        if !is_plan_target {
            if ctx.mode.plan_path().is_some() {
                warn!(
                    tool = %name,
                    target = %key.as_path().display(),
                    "blocked write in plan mode"
                );
                return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
            }
            if let Some(reason) = ctx.permissions.boundary_block_reason(key.as_path()) {
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

    if let Err(e) = enforce_permission(invocation.as_ref(), name, ctx, &id, input).await {
        return done_error(e);
    }

    // Taken after the permission gate, so a pending approval prompt never
    // holds a file lock, and around the whole handler, so it may `await`
    // freely without a sibling call on the same file interleaving its
    // read-modify-write. Every early return below drops it.
    //
    // The stale-read check belongs inside the lock: outside it, two siblings
    // both pass before either writes.
    let _guard = match &mutated {
        Some(key) => {
            let guard = ctx.file_access.acquire(key).await;
            if ctx.config.stale_read_check
                && let Err(message) = ctx.file_access.check_before_edit(key)
            {
                return done_error(message);
            }
            Some(guard)
        }
        None => None,
    };

    let result = invocation.execute(ctx).await;

    // Nothing else could have touched the file while the guard was held, so the
    // mtime is refreshed here instead of by each write plugin remembering to.
    // Only on success: a failed or half-finished write must leave the changed
    // mtime visible, so the next edit is correctly told the file moved.
    if result.output.is_ok()
        && let Some(key) = &mutated
    {
        ctx.file_access.record_read(key);
    }

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
    input: &Value,
) -> Result<(), String> {
    if name.contains('.') {
        return Err(format!(
            "enforce_permission called with dotted name: {name}"
        ));
    }
    let review = ReviewSource {
        input: Some(input),
        recent_user_messages: &ctx.recent_user_messages,
        timeouts: ctx.timeouts,
    };
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
                review,
            )
            .await
            .map_err(|e| e.to_string())?;
    } else {
        ctx.permissions
            .veto_review(name, review, &ctx.event_tx, &ctx.cancel)
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
    emit_raw_start(ctx, origin, id, &tool, format!("mcp: {tool}"), input);
    let done = |output: String, is_error: bool| ToolDoneEvent {
        id: id.to_owned(),
        tool: Arc::clone(&tool),
        output: ToolOutput::Plain(output.into()),
        is_error,
        annotation: None,
        written_path: None,
    };

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
            ReviewSource {
                input: Some(input),
                recent_user_messages: &ctx.recent_user_messages,
                timeouts: ctx.timeouts,
            },
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
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use maki_config::{Effect, Permission, PermissionRule, PermissionsConfig, ToolKey};
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::cancel::CancelToken;
    use crate::mcp::test_support::stub_session;
    use crate::mcp::tool_names;
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager};
    use crate::template::Vars;
    use crate::tools::registry::{ToolRegistry, ToolSource};
    use crate::tools::schema::{JsonPath, ToolInputErrorKind};
    use crate::tools::test_support::{
        GUARDED_TOOL_NAME, GuardedMock, mock_tool, stub_ctx, stub_ctx_with_permissions,
    };
    use crate::tools::{
        BoxFuture, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
        PermissionScopes, RequestTools, TOOL_NAME_FIELD, Tool, ToolAudience, ToolExecResult,
        ToolHook, local_tool,
    };

    const TEST_ID: &str = "t1";
    const PROBE_WIRE: &str = "srv__probe";
    const PROBE_QUALIFIED: &str = "srv.probe";
    const OTHER_WIRE: &str = "srv__other";
    const OTHER_QUALIFIED: &str = "srv.other";
    const SHADOWED_NAME: &str = "batch";
    const CLIENT_NAME: &str = "client_probe";
    const TEST_PLUGIN: &str = "test";
    const HOOK_TOOL_NAME: &str = "hook_probe";
    const HOOK_FIELD: &str = "command";
    const HOOK_PLAIN: &str = "ls";
    const HOOK_REWRITTEN_FROM: &str = "grep -r x .";
    const HOOK_REWRITTEN_TO: &str = "rg x";
    const HOOK_DENIED: &str = "sudo rm -rf /";
    const HOOK_DENY_REASON: &str = "not on my watch";
    const HOOK_OUTPUT_TEXT: &str = "trimmed";
    const HOOK_PERMISSION: Permission = Permission::Run;
    const HOOK_FIELD_TYPE: &str = "string";
    const HOOK_DIFF_COMMAND: &str = "apply";
    const HOOK_DIFF_PATH: &str = "/tmp/diffed.txt";
    const HOOK_DIFF_SUMMARY: &str = "1 file changed";
    const HOOK_ESCAPED_PATH: &str = "/tmp/not-the-plan.md";
    const TEST_PLUGIN_SOURCE: &str = "lua:test";
    const START_PROBE_NAME: &str = "start_probe";
    /// Allowed by the shared stub permissions; nothing is ever written here.
    const TEST_ROOT: &str = "/tmp";
    const PLAN_PATH: &str = "/tmp/plan.md";
    const HOOK_CALL_DEADLINE: Duration = Duration::from_secs(7);
    const HOOK_SLOW_COMMAND: &str = "slow";
    /// Real elapsed time inside the call, so the gap between the two stages'
    /// windows is a measurement rather than a race.
    const HOOK_SLOW_RUN: Duration = Duration::from_millis(20);

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
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.local_tools = Arc::new(HashMap::from([(
            name.to_owned(),
            local_tool(ToolAudience::all(), move |input, _ctx| {
                let result = f(&input);
                Box::pin(async move { result })
            }),
        )]));
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

    fn ruled_ctx(mode: &AgentMode, tool: ToolKey, effect: Effect) -> ToolContext {
        let config = PermissionsConfig {
            rules: vec![PermissionRule {
                tool,
                scope: None,
                effect,
            }],
            ..Default::default()
        };
        let permissions = Arc::new(PermissionManager::new(
            config,
            PathBuf::from(TEST_ROOT),
            Arc::default(),
        ));
        stub_ctx_with_permissions(mode, permissions)
    }

    fn denying_ctx(tool: ToolKey) -> ToolContext {
        ruled_ctx(&AgentMode::Build, tool, Effect::Deny)
    }

    fn build_ctx() -> ToolContext {
        stub_ctx(&AgentMode::Build)
    }

    /// Its permission scope, its write target and its output are all its
    /// input, which is how a test sees the input each stage of dispatch got.
    struct HookMock(Option<Permission>);

    struct HookMockInvocation(String);

    impl ToolInvocation for HookMockInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(HOOK_TOOL_NAME.into()))
        }
        fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
            Box::pin(std::future::ready(Some(PermissionScopes::single(
                self.0.clone(),
            ))))
        }
        fn mutable_path(&self) -> Option<&Path> {
            Some(Path::new(&self.0))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move {
                if self.0 == HOOK_SLOW_COMMAND {
                    smol::Timer::after(HOOK_SLOW_RUN).await;
                }
                Ok(output_of(&self.0)).into()
            })
        }
    }

    fn ran(command: &str) -> String {
        format!("ran {command}")
    }

    /// One command answers with a shape the UI renders from fields, the one
    /// kind of output a hook may not touch.
    fn output_of(command: &str) -> ToolOutput {
        if command == HOOK_DIFF_COMMAND {
            return ToolOutput::Diff {
                path: HOOK_DIFF_PATH.to_owned(),
                before: String::new(),
                after: String::new(),
                summary: HOOK_DIFF_SUMMARY.to_owned(),
            };
        }
        ToolOutput::Plain(ran(command).into())
    }

    /// Shared by the mock and the assertion, so the test cannot pass against
    /// some other error.
    fn missing_command() -> ParseError {
        ParseError {
            path: JsonPath::default(),
            kind: ToolInputErrorKind::Missing {
                expected: HOOK_FIELD_TYPE,
            },
        }
    }

    impl Tool for HookMock {
        fn name(&self) -> &str {
            HOOK_TOOL_NAME
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "hook mock".into()
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}})
        }
        fn required_permission(&self) -> Option<Permission> {
            self.0
        }
        fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            match input[HOOK_FIELD].as_str() {
                Some(command) => Ok(Box::new(HookMockInvocation(command.to_owned()))),
                None => Err(missing_command()),
            }
        }
    }

    /// One firing as the hook saw it.
    #[derive(Clone, Debug)]
    struct Seen {
        stage: HookStage,
        authority: Authority,
        tool: String,
        tool_id: String,
        session_id: Option<String>,
        origin: CallOrigin,
        value: Value,
        cancelled: bool,
        deadline: Instant,
    }

    #[derive(Clone, Copy)]
    enum Reply {
        Answers(fn(HookStage, &Value) -> Verdict),
        /// Never resolves, so only cancellation can end the wait.
        Pending,
    }

    /// Stands in for the Lua slot chain: records every firing and answers with
    /// whatever the test scripted.
    #[derive(Clone)]
    struct RecordingHook {
        seen: Arc<Mutex<Vec<Seen>>>,
        wrapped: &'static [HookStage],
        reply: Reply,
    }

    impl Default for RecordingHook {
        fn default() -> Self {
            Self {
                seen: Arc::default(),
                wrapped: &HookStage::ALL,
                reply: Reply::Answers(steer_the_call),
            }
        }
    }

    /// Rewrites, denies or defers on the way in, the way a plugin steering the
    /// model off one command onto another would.
    fn steer_the_call(stage: HookStage, value: &Value) -> Verdict {
        match stage {
            HookStage::Input => match value[HOOK_FIELD].as_str() {
                Some(HOOK_DENIED) => Verdict::Denied(HOOK_DENY_REASON.into()),
                Some(HOOK_REWRITTEN_FROM) => Verdict::Replaced(call_input(HOOK_REWRITTEN_TO)),
                _ => Verdict::Unchanged,
            },
            HookStage::Output => Verdict::Unchanged,
        }
    }

    impl RecordingHook {
        fn wrapping(wrapped: &'static [HookStage]) -> Self {
            Self {
                wrapped,
                ..Self::default()
            }
        }

        fn answering(answer: fn(HookStage, &Value) -> Verdict) -> Self {
            Self {
                reply: Reply::Answers(answer),
                ..Self::default()
            }
        }

        fn never_answering(wrapped: &'static [HookStage]) -> Self {
            Self {
                wrapped,
                reply: Reply::Pending,
                ..Self::default()
            }
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn stages(&self) -> Vec<(HookStage, Authority)> {
            self.seen().iter().map(|s| (s.stage, s.authority)).collect()
        }

        fn at(&self, stage: HookStage) -> Option<Seen> {
            self.seen().into_iter().find(|s| s.stage == stage)
        }
    }

    impl ToolHook for RecordingHook {
        fn wraps(&self, _tool: &str, stage: HookStage) -> bool {
            self.wrapped.contains(&stage)
        }

        fn run<'a>(
            &'a self,
            stage: HookStage,
            value: Value,
            call: &'a HookCall<'a>,
        ) -> BoxFuture<'a, Verdict> {
            self.seen.lock().unwrap().push(Seen {
                stage,
                authority: call.authority,
                tool: call.tool.to_owned(),
                tool_id: call.tool_id.to_owned(),
                session_id: call.session_id.map(str::to_owned),
                origin: call.origin,
                value: value.clone(),
                cancelled: call.cancel.is_cancelled(),
                deadline: call.deadline,
            });
            match self.reply {
                Reply::Answers(answer) => Box::pin(std::future::ready(answer(stage, &value))),
                Reply::Pending => Box::pin(std::future::pending()),
            }
        }
    }

    fn hooked_ctx(ctx: ToolContext) -> (ToolContext, RecordingHook) {
        hooked_with(ctx, None, RecordingHook::default())
    }

    fn plain_hooked_ctx(hook: RecordingHook) -> (ToolContext, RecordingHook) {
        hooked_with(build_ctx(), None, hook)
    }

    fn hooked_with(
        mut ctx: ToolContext,
        permission: Option<Permission>,
        hook: RecordingHook,
    ) -> (ToolContext, RecordingHook) {
        ctx.registry = registered(Arc::new(HookMock(permission)));
        ctx.registry.set_hook(hook.clone());
        (ctx, hook)
    }

    fn cancelled_token() -> CancelToken {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        token
    }

    fn call_input(command: &str) -> Value {
        serde_json::json!({ HOOK_FIELD: command })
    }

    fn both_stages(authority: Authority) -> Vec<(HookStage, Authority)> {
        HookStage::ALL.map(|stage| (stage, authority)).into()
    }

    /// The rewritten call is the one that runs and the one the rules judge.
    /// Were it the other way round, an `allow bash: git status` rule would be a
    /// way to run anything.
    #[test_case(build_ctx                                       , false ; "reaches_execute")]
    #[test_case(|| denying_ctx(ToolKey::native(HOOK_TOOL_NAME))  , true  ; "reaches_the_permission_prompt")]
    fn an_input_rewrite_is_the_call_that_runs(build: fn() -> ToolContext, is_error: bool) {
        smol::block_on(async {
            let (ctx, _hook) = hooked_ctx(build());
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_REWRITTEN_FROM)).await;

            let text = done.output.as_text();
            assert_eq!(done.is_error, is_error, "{text}");
            assert!(
                text.contains(HOOK_REWRITTEN_TO) && !text.contains(HOOK_REWRITTEN_FROM),
                "everything downstream names the rewritten command: {text}"
            );
        });
    }

    #[test]
    fn input_hook_denial_never_runs_the_tool() {
        smol::block_on(async {
            let (ctx, hook) = hooked_ctx(stub_ctx(&AgentMode::Build));
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_DENIED)).await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), HOOK_DENY_REASON);
            assert_eq!(
                hook.stages(),
                vec![(HookStage::Input, Authority::Unbounded)],
                "a stopped call has no output to hook"
            );
        });
    }

    /// Everything the model reads passes the output stage, so a hook that
    /// redacts or trims cannot be walked around by failing the call.
    #[test]
    fn a_refused_call_still_reaches_the_output_stage() {
        smol::block_on(async {
            let (ctx, hook) = hooked_ctx(denying_ctx(ToolKey::native(HOOK_TOOL_NAME)));
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            assert!(done.is_error);
            assert!(done.output.as_text().contains(PERMISSION_DENIED_PREFIX));
            assert_eq!(hook.stages(), both_stages(Authority::Unbounded));

            let firing = hook.at(HookStage::Output).expect("the output stage fired");
            assert_eq!(firing.value[OUTPUT_IS_ERROR], Value::Bool(true));
            let text = firing.value[OUTPUT_TEXT].as_str().unwrap_or_default();
            assert!(text.contains(PERMISSION_DENIED_PREFIX), "got: {text}");
        });
    }

    /// A name that routes nowhere lends no authority, so nothing fires.
    #[test]
    fn unknown_names_are_not_hooked() {
        smol::block_on(async {
            let (ctx, hook) = hooked_ctx(stub_ctx(&AgentMode::Build));
            let done = dispatch(&ctx, "nope", &call_input(HOOK_DENIED)).await;

            assert!(done.is_error);
            assert!(done.output.as_text().contains(UNKNOWN_TOOL_PREFIX));
            assert!(hook.stages().is_empty());
        });
    }

    fn mcp_route_ctx() -> ToolContext {
        mcp_ctx(&stub_mcp(&[PROBE_QUALIFIED]))
    }

    fn host_route_ctx() -> ToolContext {
        local_ctx(CLIENT_NAME, |_| Ok(String::new()))
    }

    /// Hooking in dispatch is what reaches a route maki did not write the code
    /// behind, and each one has to answer for what that lends. Only a declared
    /// capability narrows the price; everything else prices at the maximum.
    /// The name stays the one the model called, not whatever dispatch routes
    /// it to.
    #[test_case(build_ctx,      HOOK_TOOL_NAME,        Some(HOOK_PERMISSION), Authority::Capability(HOOK_PERMISSION) ; "a_checked_tool_lends_its_capability")]
    #[test_case(build_ctx,      HOOK_TOOL_NAME,        None,                  Authority::Unbounded                   ; "a_tool_declaring_nothing_declares_no_limit")]
    #[test_case(mcp_route_ctx,  TOOL_SEARCH_TOOL_NAME, None,                  Authority::Unbounded                   ; "search_declares_nothing_either")]
    #[test_case(mcp_route_ctx,  PROBE_WIRE,            None,                  Authority::Unbounded                   ; "an_mcp_tool_is_code_maki_does_not_own")]
    #[test_case(host_route_ctx, CLIENT_NAME,           None,                  Authority::Unbounded                   ; "a_host_tool_is_code_maki_does_not_own")]
    fn a_route_lends_the_authority_it_declares(
        build: fn() -> ToolContext,
        name: &str,
        permission: Option<Permission>,
        expected: Authority,
    ) {
        smol::block_on(async {
            let (ctx, hook) = hooked_with(build(), permission, RecordingHook::default());
            dispatch(&ctx, name, &call_input(HOOK_PLAIN)).await;

            let firing = hook.at(HookStage::Input).expect("the input stage fired");
            assert_eq!(firing.authority, expected);
            assert_eq!(firing.tool, name, "hooked under the name the model called");
        });
    }

    /// `wraps` is why an unwrapped slot costs nothing: a stage the hook
    /// declines never reaches `run` at all.
    #[test_case(&[HookStage::Input]  ; "input_only")]
    #[test_case(&[HookStage::Output] ; "output_only")]
    fn a_stage_the_hook_declines_never_fires(wrapped: &'static [HookStage]) {
        smol::block_on(async {
            let (ctx, hook) = plain_hooked_ctx(RecordingHook::wrapping(wrapped));
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), ran(HOOK_PLAIN));
            let fired: Vec<HookStage> = hook.seen().iter().map(|s| s.stage).collect();
            assert_eq!(fired, wrapped);
        });
    }

    /// Nobody is left reading the answer, so waiting on a verdict that never
    /// comes would only keep the call alive. Each stage keeps what it has: no
    /// input was judged, and an output already produced stands.
    #[test_case(&[HookStage::Input],  true,  ERROR_CANCELLED.to_owned() ; "input")]
    #[test_case(&[HookStage::Output], false, ran(HOOK_PLAIN)            ; "output")]
    fn a_cancelled_call_does_not_wait_for_a_verdict(
        wrapped: &'static [HookStage],
        is_error: bool,
        expected: String,
    ) {
        smol::block_on(async {
            let mut ctx = build_ctx();
            ctx.cancel = cancelled_token();
            let (ctx, _hook) = hooked_with(ctx, None, RecordingHook::never_answering(wrapped));

            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            assert_eq!(done.is_error, is_error);
            assert_eq!(done.output.as_text(), expected);
        });
    }

    /// A chain runs off this thread, so it only dies with the call it filters
    /// when it is handed that call's own token and an instant to be killed at.
    #[test]
    fn a_firing_carries_the_calls_cancellation_and_deadline() {
        smol::block_on(async {
            let at = Instant::now() + HOOK_CALL_DEADLINE;
            let mut ctx = build_ctx();
            ctx.deadline = Deadline::At(at);
            ctx.cancel = cancelled_token();
            let (ctx, hook) = hooked_ctx(ctx);

            dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            let firing = hook.at(HookStage::Input).expect("the input stage fired");
            assert!(firing.cancelled, "the call's own token, not a fresh one");
            assert_eq!(firing.deadline, at, "and no later than the call itself");
        });
    }

    /// A call with no deadline of its own still bounds each chain, or a layer
    /// that hangs hangs the call with it. Bounded from where the stage starts,
    /// too: the input chain and the tool spend from the same budget, and an
    /// output chain handed the entry-time answer would get whatever they left,
    /// which for a slow tool is nothing.
    #[test]
    fn a_call_without_a_deadline_bounds_each_stage_from_where_it_starts() {
        smol::block_on(async {
            let (ctx, hook) = hooked_ctx(build_ctx());
            let before = Instant::now();

            dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_SLOW_COMMAND)).await;

            let input = hook.at(HookStage::Input).expect("the input stage fired");
            let output = hook.at(HookStage::Output).expect("the output stage fired");
            assert!(
                input.deadline >= before && input.deadline <= Instant::now() + HOOK_CHAIN_MAX,
                "a deadline already past kills every chain"
            );
            assert!(
                output.deadline - input.deadline >= HOOK_SLOW_RUN,
                "the output chain inherited a window the call had already spent"
            );
        });
    }

    fn replace_the_output(stage: HookStage, _value: &Value) -> Verdict {
        match stage {
            HookStage::Input => Verdict::Unchanged,
            HookStage::Output => Verdict::Replaced(
                serde_json::json!({OUTPUT_TEXT: HOOK_OUTPUT_TEXT, OUTPUT_IS_ERROR: true}),
            ),
        }
    }

    fn replace_the_output_without_text(stage: HookStage, _value: &Value) -> Verdict {
        match stage {
            HookStage::Input => Verdict::Unchanged,
            HookStage::Output => Verdict::Replaced(serde_json::json!({OUTPUT_IS_ERROR: true})),
        }
    }

    fn deny_the_output(stage: HookStage, _value: &Value) -> Verdict {
        match stage {
            HookStage::Input => Verdict::Unchanged,
            HookStage::Output => Verdict::Denied(HOOK_DENY_REASON.into()),
        }
    }

    /// Text and error flag move together, and a hook that has run out of call
    /// to stop has only the text left to say so with.
    #[test_case(replace_the_output,              true,  HOOK_OUTPUT_TEXT.to_owned() ; "a_replacement_moves_text_and_flag")]
    #[test_case(replace_the_output_without_text, false, ran(HOOK_PLAIN)             ; "a_replacement_without_text_changes_neither")]
    #[test_case(deny_the_output,                 true,  HOOK_DENY_REASON.to_owned() ; "a_denial_becomes_the_result")]
    fn the_output_stage_decides_what_the_model_reads(
        answer: fn(HookStage, &Value) -> Verdict,
        is_error: bool,
        expected: String,
    ) {
        smol::block_on(async {
            let (ctx, _hook) = plain_hooked_ctx(RecordingHook::answering(answer));
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            assert_eq!(done.is_error, is_error);
            assert_eq!(done.output.as_text(), expected);
        });
    }

    /// An output the UI renders from fields carries no prose to lend, and
    /// editing it would desync the fields from the text.
    #[test]
    fn a_rendered_output_skips_the_output_stage() {
        smol::block_on(async {
            let (ctx, hook) = plain_hooked_ctx(RecordingHook::answering(deny_the_output));
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_DIFF_COMMAND)).await;

            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), HOOK_DIFF_SUMMARY);
            assert_eq!(
                hook.stages(),
                vec![(HookStage::Input, Authority::Unbounded)]
            );
        });
    }

    fn drop_the_field(stage: HookStage, _value: &Value) -> Verdict {
        match stage {
            HookStage::Input => Verdict::Replaced(serde_json::json!({})),
            HookStage::Output => Verdict::Unchanged,
        }
    }

    /// The rewrite lands before the schema check, so a shape the tool cannot
    /// parse is an ordinary parse error rather than something dispatch has to
    /// survive.
    #[test]
    fn a_rewrite_the_tool_cannot_parse_is_a_parse_error() {
        smol::block_on(async {
            let (ctx, _hook) = plain_hooked_ctx(RecordingHook::answering(drop_the_field));
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), missing_command().to_string());
        });
    }

    fn rewrite_the_target(stage: HookStage, _value: &Value) -> Verdict {
        match stage {
            HookStage::Input => Verdict::Replaced(call_input(HOOK_ESCAPED_PATH)),
            HookStage::Output => Verdict::Unchanged,
        }
    }

    /// The write gate reads its target off the rewritten input, so a hook
    /// cannot point a plan-mode write anywhere but the plan file. The
    /// untouched call is the control, otherwise the gate could be refusing for
    /// some unrelated reason.
    #[test_case(RecordingHook::answering(rewrite_the_target), crate::tools::PLAN_WRITE_RESTRICTED.to_owned() ; "rewritten_away_from_the_plan_file")]
    #[test_case(RecordingHook::default(),                     ran(PLAN_PATH)                                 ; "left_on_the_plan_file")]
    fn a_rewritten_write_target_is_still_plan_gated(hook: RecordingHook, expected: String) {
        smol::block_on(async {
            let plan = AgentMode::Plan(PathBuf::from(PLAN_PATH));
            let (ctx, _hook) = hooked_with(stub_ctx(&plan), None, hook);
            let done = dispatch(&ctx, HOOK_TOOL_NAME, &call_input(PLAN_PATH)).await;

            assert_eq!(done.output.as_text(), expected);
        });
    }

    /// A plugin keys its state on these, so a stage firing under another
    /// call's identity would write onto that other call.
    #[test]
    fn both_stages_carry_the_call_id_the_session_and_the_origin() {
        smol::block_on(async {
            let session = SessionRef::generate();
            let mut ctx = build_ctx();
            ctx.session_id = Some(session.clone());
            let (ctx, hook) = hooked_ctx(ctx);

            dispatch_nested(&ctx, HOOK_TOOL_NAME, &call_input(HOOK_PLAIN)).await;

            let seen = hook.seen();
            assert_eq!(seen.len(), HookStage::ALL.len(), "both stages fire");
            for firing in seen {
                assert_eq!(firing.tool_id, TEST_ID);
                assert_eq!(firing.session_id.as_deref(), Some(session.as_str()));
                assert_eq!(firing.origin, CallOrigin::Nested);
            }
        });
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
            ctx.local_tools = Arc::new(HashMap::from([(
                "local_echo".to_owned(),
                local_tool(ToolAudience::all(), |input: Value, _ctx| {
                    let out = input.to_string();
                    Box::pin(async move { Ok(out) })
                }),
            )]));

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

    fn callable_names(ctx: &ToolContext) -> Vec<String> {
        callable(ctx).into_iter().map(|c| c.name).collect()
    }

    /// A deferred MCP tool is missing from the request's tool array and is still
    /// a name the sandbox may bind.
    #[test]
    fn callable_lists_host_and_deferred_mcp_names() {
        let mcp = stub_mcp(&[PROBE_QUALIFIED, OTHER_QUALIFIED]);
        let ctx = with_mcp(local_ctx(CLIENT_NAME, |_| Ok(String::new())), &mcp);
        assert_eq!(
            callable_names(&ctx),
            [CLIENT_NAME, OTHER_WIRE, PROBE_WIRE, TOOL_SEARCH_TOOL_NAME]
        );
    }

    /// A shadowed name appears once, described by whatever `resolve` picks:
    /// listing it under the loser's audience is how a script gets handed a tool
    /// its own audience was denied.
    #[test]
    fn callable_describes_a_shadowed_name_by_what_dispatch_runs() {
        let mcp = stub_mcp(&[PROBE_QUALIFIED]);
        let mut ctx = with_mcp(local_ctx(PROBE_WIRE, |_| Ok(String::new())), &mcp);
        ctx.registry = registry_with(&[PROBE_WIRE]);

        let probe = |ctx: &ToolContext| {
            let all = callable(ctx);
            assert_eq!(all.iter().filter(|c| c.name == PROBE_WIRE).count(), 1);
            all.into_iter()
                .find(|c| c.name == PROBE_WIRE)
                .expect("the name is dispatchable")
        };
        assert_eq!(probe(&ctx).source, SOURCE_LOCAL);

        ctx.local_tools = Arc::default();
        let native = probe(&ctx);
        assert_eq!(native.source, SOURCE_NATIVE);
        assert!(native.schema.is_some(), "registry tools carry their schema");
    }

    /// The sandbox gets the same tools the request's array does. Otherwise a
    /// tool the user disabled, or one the host cannot service (ACP without
    /// form elicitation drops `question`), comes back through a script.
    #[test_case(&[PROBE_WIRE], &[]           ; "config_disabled")]
    #[test_case(&[],           &[PROBE_WIRE] ; "host_excluded")]
    fn callable_drops_what_the_requests_filter_dropped(disabled: &[&str], excluded: &[&str]) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = registry_with(&[PROBE_WIRE, OTHER_WIRE]);
        ctx.config.disabled_tools = disabled.iter().map(|n| (*n).to_owned()).collect();
        let tools = RequestTools::build(
            &ctx.registry,
            &Vars::new(),
            &ctx.model,
            &ctx.config,
            excluded,
            false,
            false,
        );
        ctx.tool_filter = Arc::clone(tools.filter());

        assert_eq!(tool_names(tools.definitions()), [OTHER_WIRE]);
        assert_eq!(callable_names(&ctx), [OTHER_WIRE]);
    }

    /// A host that trims the array it publishes (a Lua caller passing `except`)
    /// has answered for the sandbox too, because the filter comes off that same
    /// array.
    #[test]
    fn callable_drops_a_name_the_published_array_left_out() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = registry_with(&[PROBE_WIRE, OTHER_WIRE]);
        let tools = RequestTools::assembled(
            serde_json::json!([{ TOOL_NAME_FIELD: OTHER_WIRE }]),
            &ctx.config,
            &ctx.model,
        );
        ctx.tool_filter = Arc::clone(tools.filter());

        assert_eq!(callable_names(&ctx), [OTHER_WIRE]);
    }

    /// Losing on audience is not the same as freeing the name: MCP publishing
    /// the wire name of a gated registry tool must not become a way around it.
    #[test]
    fn mcp_cannot_republish_a_name_the_registry_gated() {
        let mut ctx = mcp_ctx(&stub_mcp(&[PROBE_QUALIFIED]));
        ctx.registry = registered(mock_tool(PROBE_WIRE, ToolAudience::MAIN));
        ctx.audience = ToolAudience::GENERAL_SUB;
        assert!(!callable_names(&ctx).contains(&PROBE_WIRE.to_owned()));
    }

    /// A host tool this session's audience excludes is not a callable name, even
    /// though `resolve` would route to it.
    #[test]
    fn callable_drops_names_this_audience_cannot_reach() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.local_tools = Arc::new(HashMap::from([(
            CLIENT_NAME.to_owned(),
            local_tool(ToolAudience::MAIN, |_, _| {
                Box::pin(async { Ok(String::new()) })
            }),
        )]));
        assert_eq!(callable_names(&ctx), [CLIENT_NAME]);

        ctx.audience = ToolAudience::GENERAL_SUB;
        assert!(callable_names(&ctx).is_empty());
    }

    #[test_case("srv.get_docs", "srv__get_docs", None                  ; "identifier_needs_no_alias")]
    #[test_case("srv.get-docs", "srv__get-docs", Some("srv__get_docs") ; "hyphen_becomes_underscore")]
    fn alias_is_set_only_when_the_name_is_not_an_identifier(
        qualified: &str,
        wire: &str,
        expected: Option<&str>,
    ) {
        let ctx = mcp_ctx(&stub_mcp(&[qualified]));
        let entry = callable(&ctx)
            .into_iter()
            .find(|c| c.name == wire)
            .expect("the published tool is callable");
        assert_eq!(entry.alias.as_deref(), expected);
    }

    /// Two names collapsing onto one alias would silently point a caller at the
    /// wrong tool, so neither gets one.
    #[test]
    fn colliding_aliases_are_dropped() {
        let ctx = mcp_ctx(&stub_mcp(&["srv.get-docs", "srv.get_docs"]));
        assert!(callable(&ctx).iter().all(|c| c.alias.is_none()));
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

    /// Plan mode puts MCP behind the user, it does not block it outright. An
    /// allow rule is the user's answer already, so the call goes through.
    #[test]
    fn mcp_tool_allowed_by_rule_in_plan_mode() {
        smol::block_on(async {
            let plan = AgentMode::Plan(PathBuf::from(PLAN_PATH));
            let ctx = with_mcp(
                ruled_ctx(
                    &plan,
                    ToolKey::parse(PROBE_QUALIFIED).unwrap(),
                    Effect::Allow,
                ),
                &stub_mcp(&[PROBE_QUALIFIED]),
            );
            let done = dispatch(&ctx, PROBE_WIRE, &serde_json::json!({})).await;
            // The stub transport fails every call, so a successful run surfaces
            // its error: the proof the call was neither plan-blocked nor
            // permission-denied and actually reached MCP.
            assert_eq!(done.tool.as_ref(), PROBE_QUALIFIED, "must route to MCP");
            let text = done.output.as_text();
            assert!(
                !text.starts_with(PERMISSION_DENIED_PREFIX)
                    && text != crate::tools::PLAN_WRITE_RESTRICTED,
                "plan mode must not block or deny the call, got: {text}"
            );
            let mut tools = serde_json::json!([]);
            ctx.mcp.as_ref().unwrap().extend_tools(&mut tools);
            assert!(
                tool_names(&tools).contains(&&PROBE_WIRE.to_owned()[..]),
                "a permitted plan-mode call must load the definition"
            );
        });
    }

    /// An MCP server can write without announcing it, so a plan-mode session
    /// asks first even where everything else is approved automatically. The
    /// stub has no channel to ask on, hence the denial below.
    #[test]
    fn mcp_tool_in_plan_mode_is_never_auto_approved() {
        smol::block_on(async {
            let plan = AgentMode::Plan(PathBuf::from(PLAN_PATH));
            let ctx = with_mcp(stub_ctx(&plan), &stub_mcp(&[PROBE_QUALIFIED]));
            let done = dispatch(&ctx, PROBE_WIRE, &serde_json::json!({})).await;
            assert!(done.is_error);
            let text = done.output.as_text();
            assert!(text.starts_with(PERMISSION_DENIED_PREFIX), "got: {text}");
            let mut tools = serde_json::json!([]);
            ctx.mcp.as_ref().unwrap().extend_tools(&mut tools);
            assert!(
                !tool_names(&tools).contains(&&PROBE_WIRE.to_owned()[..]),
                "an unapproved call must not load the definition"
            );
        });
    }

    #[test]
    fn mcp_tool_denied_by_rule_in_plan_mode() {
        smol::block_on(async {
            let plan = AgentMode::Plan(PathBuf::from(PLAN_PATH));
            let ctx = with_mcp(
                ruled_ctx(
                    &plan,
                    ToolKey::parse(PROBE_QUALIFIED).unwrap(),
                    Effect::Deny,
                ),
                &stub_mcp(&[PROBE_QUALIFIED]),
            );
            let done = dispatch(&ctx, PROBE_WIRE, &serde_json::json!({})).await;
            assert!(done.is_error, "plan mode must not bypass deny rules");
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "got: {}",
                done.output.as_text()
            );
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
