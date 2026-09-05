//! `maki.agent` exposes subagent primitives to Lua plugins. Policy (retries,
//! validation, concurrency) lives in the task plugin, not here.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_lock::Mutex as AsyncMutex;
use futures::future::{Either, select};
use maki_agent::agent::tool_dispatch;
use maki_agent::cancel::{CancelMap, CancelSlot, CancelToken};
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::registry::ToolRegistry;
use maki_agent::tools::schema::sanitize_tool_input_schema;
use maki_agent::tools::{
    CallOrigin, Deadline, DescriptionContext, LocalTool, LocalTools, RequestTools, ToolAudience,
    ToolContext, ToolFilter, ToolLive,
};
use maki_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, DoneReason,
    EMPTY_RESPONSE_MARKER, Envelope, EventSender, History, McpSession, RunLedger, SubagentInfo,
    ToolDoneEvent,
};
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use maki_providers::model::ModelTier;
use maki_providers::provider;
use maki_providers::{ContentBlock, Model, ModelError, Role, ThinkingConfig, TokenUsage, add_cost};
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::StoredThinking;
use mlua::{Function, IntoLuaMulti, Lua, Result as LuaResult, Table, Value as LuaValue};
use serde_json::Value as JsonValue;
use tracing::info;

use crate::api::tool::{audiences_to_lua, parse_audience};
use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::{json_to_lua, lua_to_json, lua_tool_result};
use crate::api::util::ctx::{AgentContext, LuaCtx};
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::runtime::CANCELLED_MSG;

const SESSION_CLOSED_ERR: &str = "session closed";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
const SCOPE_TABLE_ERR: &str = "scope must be { session = \"<id>\" }";
const SCOPE_SESSION_MISMATCH_ERR: &str = "scope.session must be the caller's own session id";

fn resolve_model_from_ctx(ctx: &AgentContext, tier: Option<&str>) -> Result<Model, String> {
    let Some(tier_str) = tier else {
        return Ok(Model::clone(&ctx.model));
    };
    let requested: ModelTier = tier_str.parse().map_err(|e: ModelError| e.to_string())?;
    let effective = requested.min(ctx.model.tier);
    if effective == ctx.model.tier {
        return Ok(Model::clone(&ctx.model));
    }
    let slug = &ctx.model.provider;
    maki_providers::model_registry::spec_for_tier(slug, effective)
        .or_else(|| maki_providers::model_registry::spec_for_tier_any(effective))
        .filter(|spec| ctx.model_policy.allows(spec))
        .and_then(|s| Model::from_spec(&s).ok())
        .map(Ok)
        .unwrap_or_else(|| {
            Model::from_tier_with_policy(slug, effective, &ctx.model_policy)
                .map_err(|e| e.to_string())
        })
}

fn model_to_lua_table(lua: &Lua, model: &Model) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("id", model.id.clone())?;
    tbl.set("tier", model.tier.to_string())?;
    tbl.set("provider", model.provider.to_string())?;
    tbl.set("spec", model.spec())?;
    Ok(tbl)
}

fn dispatch_ctx<'a>(ctx: &'a LuaCtx, method: &str) -> Result<&'a AgentContext, String> {
    ctx.agent()
        .ok_or_else(|| ctx.cap_err(&format!("maki.agent.{method}")))
}

/// Forwards subagent events to the parent, stamped with the subagent identity.
/// Usage takes two paths: live on the tool header while the run goes on (last
/// turn's tokens plus the run's summed cost), and one total per run on
/// `usage_tx`, which `prompt` waits for.
async fn relay_session_events(
    sub_rx: flume::Receiver<Envelope>,
    parent_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    usage_tx: flume::Sender<TokenUsage>,
    live_sink: Option<flume::Sender<ToolLive>>,
) {
    let mut cost = None;
    while let Ok(mut envelope) = sub_rx.recv_async().await {
        match &envelope.event {
            AgentEvent::TurnComplete(turn) => {
                add_cost(&mut cost, turn.cost);
                if let Some(sink) = &live_sink {
                    let _ = sink.send(ToolLive::Usage(turn.usage.format_sum_cost(cost)));
                }
            }
            AgentEvent::Done { usage, .. } => {
                let _ = usage_tx.send(*usage);
                continue;
            }
            AgentEvent::Error { .. }
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolPending { .. }
            | AgentEvent::SubagentHistory { .. } => continue,
            _ => {}
        }
        envelope.subagent = subagent_info.get().cloned();
        let _ = parent_tx.send_envelope(envelope);
    }
}

/// Look up the model that the current agent is using, or pick a cheaper one.
/// You might want a cheaper model for simple subtasks (summaries, classification)
/// without hard-coding a model name.
///
/// The returned table has fields: `id` (string), `tier` (string),
/// `provider` (string), `spec` (string).
///
/// @param ctx LuaCtx Agent context.
/// @param opts table? Optional fields:
///   `tier` (string?) - target tier, e.g. `"fast"`, `"mid"`, `"best"`. Clamped to
///     the parent tier so you cannot escalate.
///   `spec` (string?) - exact model spec string, e.g. `"claude-3-5-haiku-20241022"`.
///     Takes precedence over `tier`.
/// @return (table?, string?) Model table on success, or `(nil, err)` on failure.
/// @example
/// local model, err = maki.agent.resolve_model(ctx, { tier = "fast" })
/// if err then error(err) end
/// print(model.spec, model.tier)
#[lua_fn]
async fn resolve_model(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "resolve_model"));
    let tier_str = opts
        .as_ref()
        .and_then(|t| t.get::<Option<String>>("tier").ok().flatten());
    let spec_str = opts
        .as_ref()
        .and_then(|t| t.get::<Option<String>>("spec").ok().flatten());

    let model = match spec_str {
        Some(ref spec) => try_pair!(Model::from_spec_with_policy(spec, &agent.model_policy)),
        None => try_pair!(resolve_model_from_ctx(agent, tier_str.as_deref())),
    };
    Ok((Some(model_to_lua_table(&lua, &model)?), None))
}

/// Build a system prompt from a built-in template. Environment variables like
/// `{cwd}` are substituted automatically. Use this when you need a ready-made
/// prompt for a subagent session.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Required fields:
///   `prompt_id` (string) - one of `"research"`, `"general"`, `"system"`.
/// Optional fields:
///   `instructions` (string|boolean?) - extra text appended to the prompt.
///     `true` loads instructions from the project `.maki/instructions` file.
///     `false` or nil omits them.
/// @return (string?, string?) The assembled prompt string, or `(nil, err)` on failure.
/// @example
/// local prompt, err = maki.agent.system_prompt(ctx, {
///   prompt_id = "research",
///   instructions = true,
/// })
/// if err then error(err) end
#[lua_fn]
async fn system_prompt(
    _lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Table,
) -> LuaResult<Pair<String>> {
    let slots = Arc::clone(&try_pair!(dispatch_ctx(&ctx, "system_prompt")).prompt_slots);
    // Nothing may hold the ctx borrow across the wait: a cancel hook firing
    // meanwhile needs `ctx:finish`, which takes it mutably.
    drop(ctx);
    let prompt_id_str: String = opts.get("prompt_id")?;
    let prompt_id = match prompt_id_str.as_str() {
        "research" => maki_agent::prompt::PromptId::Research,
        "general" => maki_agent::prompt::PromptId::General,
        "system" => maki_agent::prompt::PromptId::System,
        other => return Ok(err_pair(format!("unknown prompt_id: {other}"))),
    };

    let vars = maki_agent::template::env_vars();
    let instructions_val: LuaValue = opts.get("instructions")?;
    let instructions = match instructions_val {
        LuaValue::Boolean(true) => {
            let cwd = vars.apply("{cwd}").into_owned();
            smol::unblock(move || maki_agent::agent::load_instruction_text(&cwd)).await
        }
        LuaValue::Boolean(false) | LuaValue::Nil => String::new(),
        LuaValue::String(s) => s.to_str()?.to_owned(),
        _ => return Err(mlua::Error::runtime("instructions must be bool or string")),
    };

    let assembled = maki_agent::prompt::assemble(prompt_id, &slots, &instructions);
    Ok((Some(vars.apply(&assembled).into_owned()), None))
}

/// Get the list of tool definitions for a given audience. Pass the result
/// straight into `maki.agent.session()` or use it to inspect what tools are
/// available.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Required fields:
///   `audience` (string) - tool audience filter, e.g. `"general"`, `"subagent"`,
///     `"general_sub"`.
/// Optional fields:
///   `only` (string[]?) - include only these tool names.
///   `except` (string[]?) - exclude these tool names.
///   `workflow` (boolean?) - use workflow-mode descriptions. Default: `false`.
///   `spec` (string?) - evaluate capability exclusions against this model spec.
///   `mcp` (boolean?) - describe tools as if MCP is reachable. Default: `true`.
///     Pass what you pass to `maki.agent.session()`. Otherwise the descriptions
///     advertise MCP tools that the session has no way to call.
/// @return (table?, string?) Array of tool definition tables, or `(nil, err)` on failure.
/// @example
/// local defs, err = maki.agent.tools(ctx, {
///   audience = "general_sub",
///   except = { "bash", "write" },
/// })
/// if err then error(err) end
/// print(#defs .. " tools available")
#[lua_fn]
async fn tools(lua: Lua, ctx: mlua::UserDataRef<LuaCtx>, opts: Table) -> LuaResult<Pair<LuaValue>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "tools"));
    let audience_str: String = opts.get("audience")?;
    let audience = try_pair!(
        ToolAudience::parse_name(&audience_str)
            .ok_or_else(|| format!("unknown audience: {audience_str}"))
    );

    let only: Option<Vec<String>> = opts.get("only")?;
    let except: Option<Vec<String>> = opts.get("except")?;
    let workflow: bool = opts.get::<Option<bool>>("workflow")?.unwrap_or(false);
    let spec_str: Option<String> = opts.get("spec")?;
    let mcp_enabled: bool = opts.get::<Option<bool>>("mcp")?.unwrap_or(true);

    let parsed = spec_str
        .as_deref()
        .and_then(|spec| Model::from_spec_with_policy(spec, &agent.model_policy).ok());
    let model = parsed.as_ref().unwrap_or(&agent.model);

    let base = match (only, except) {
        (Some(o), _) => ToolFilter::Only(o),
        (_, Some(e)) => ToolFilter::AllExcept(e),
        _ => ToolFilter::All,
    };
    let disabled: Vec<&str> = agent
        .config
        .disabled_tools
        .iter()
        .map(String::as_str)
        .collect();
    let filter = base
        .excluding(&disabled)
        .excluding(maki_agent::tools::capability_exclusions(model));

    let vars = maki_agent::template::env_vars();
    let ctx_desc = DescriptionContext {
        filter: &filter,
        audience,
        workflow,
        mcp: mcp_enabled && agent.mcp.is_some(),
    };
    // Base definitions only: the session injects MCP definitions per
    // request, so baking them into a tools array would freeze the catalog.
    let defs = ToolRegistry::global().definitions(&vars, &ctx_desc, model.supports_tool_examples());

    Ok((Some(json_to_lua(&lua, &defs)?), None))
}

/// Every tool name this context can dispatch: registry tools, MCP tools
/// (deferred ones included), host tools (ACP client tools, a subagent's
/// `structured_output`) and `tool_search`. Reach for it when you expose tools
/// inside a sandbox and need the names to bind. `maki.api.get_tools()` covers
/// the registry alone and has no view of the session.
///
/// The list already accounts for this session's audience, the config's
/// `disabled_tools` and the model's capabilities. Read `audiences` to layer
/// your own policy on top. A sandbox wants `interpreter`.
///
/// Each name shows up once, described by the tool a call would really reach, so
/// a host tool that shadows a registry name reports its own audience rather
/// than the shadowed one's.
///
/// @param ctx LuaCtx Agent context.
/// @return (table?, string?) Array of `{ name, alias?, source, audiences, schema? }`,
///   or `(nil, err)` on failure. `source` is one of `"native"`, `"local"`,
///   `"mcp"`. `alias` is a safe identifier to bind, set only when `name` is not
///   one (say `srv__get-docs`). Dispatch `name` in every case. `schema` comes
///   with registry tools only.
/// @example
/// local tools, err = maki.agent.callable_tools(ctx)
/// if err then error(err) end
/// for _, t in ipairs(tools) do
///   print(t.source, t.alias or t.name)
/// end
#[lua_fn]
async fn callable_tools(lua: Lua, ctx: mlua::UserDataRef<LuaCtx>) -> LuaResult<Pair<Table>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "callable_tools"));
    let out = lua.create_table()?;
    for (i, tool) in tool_dispatch::callable(&agent.to_tool_context())
        .into_iter()
        .enumerate()
    {
        let t = lua.create_table()?;
        t.set("name", tool.name)?;
        if let Some(alias) = tool.alias {
            t.set("alias", alias)?;
        }
        t.set("source", tool.source)?;
        t.set("audiences", audiences_to_lua(&lua, tool.audience)?)?;
        if let Some(schema) = tool.schema {
            t.set("schema", json_to_lua(&lua, &schema)?)?;
        }
        out.set(i + 1, t)?;
    }
    Ok((Some(out), None))
}

/// Run a tool by name and wait for the result. This is how you call built-in
/// tools (like `read`, `bash`, `glob`) from Lua without going through the LLM.
///
/// Live events (streaming output, annotations, cumulative usage) are delivered
/// through optional callbacks while the tool runs.
///
/// @param ctx LuaCtx Agent context.
/// @param name string Tool name, e.g. `"bash"`, `"read"`.
/// @param input table|any Tool input (JSON-serializable). Must match the tool's `input_schema`.
/// @param opts table? Optional fields:
///   `timeout` (integer?) - deadline in seconds.
///   `on_live_buf` (function?) - called with a `BufHandle` for each live buffer
///     the tool publishes. Must not yield.
///   `on_annotation` (function?) - called with an annotation string for each
///     annotation event. Must not yield.
///   `on_usage` (function?) - called with a formatted cumulative token usage
///     string. Must not yield.
/// @return (string?, string?) Tool output text, or `(nil, err)` on failure.
/// @example
/// local out, err = maki.agent.call_tool(ctx, "bash", {
///   command = "ls -la",
///   timeout = 10,
/// })
/// if err then error(err) end
/// print(out)
#[lua_fn]
async fn call_tool(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    name: String,
    input: LuaValue,
    opts: Option<Table>,
) -> LuaResult<Pair<String>> {
    let input_json = lua_to_json(&lua, &input)?;
    let agent = try_pair!(dispatch_ctx(&ctx, "call_tool"));
    let mut tctx = agent.to_tool_context();
    let (mut on_buf, mut on_ann, mut on_usage, mut rx) = (None, None, None, None);
    if let Some(o) = opts {
        if let Some(secs) = o.get::<Option<u64>>("timeout")? {
            tctx.deadline = Deadline::after(Duration::from_secs(secs));
        }
        on_buf = o.get::<Option<Function>>("on_live_buf")?;
        on_ann = o.get::<Option<Function>>("on_annotation")?;
        on_usage = o.get::<Option<Function>>("on_usage")?;
        if on_buf.is_some() || on_ann.is_some() || on_usage.is_some() {
            let (tx, r) = flume::unbounded();
            tctx.live_sink = Some(tx);
            rx = Some(r);
        }
    }
    drop(ctx);
    if let Err(e) = tctx.deadline.check() {
        return Ok(err_pair(e));
    }
    let cbs = LiveCallbacks {
        tool: &name,
        on_buf,
        on_ann,
        on_usage,
    };
    let done = dispatch_racing_live(&tctx, &name, &input_json, rx, &cbs).await;
    // Same fallback the UI applies on tool completion, so a batch child's
    // header carries the annotation its standalone run would get.
    let annotation = done
        .annotation
        .clone()
        .or_else(|| (!done.is_error).then(|| done.output.annotation()).flatten());
    if let Some(a) = annotation {
        cbs.deliver(ToolLive::Annotation(a)).await;
    }
    match interpreter_bridge::flatten(&done) {
        Ok(text) => Ok((Some(text), None)),
        Err(err) => Ok((None, Some(err))),
    }
}

/// Create a new subagent session. The session inherits the parent model and
/// MCP handle unless you override them. You get back a `Session` object that
/// you can send messages to with `:prompt()`.
///
/// This is the main way to spin up a sub-conversation with its own history
/// and tool set.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Optional fields:
///   `model_spec` (string?) - model spec string to use instead of the parent model.
///   `system` (string?) - system prompt. Defaults to empty.
///   `tools` (table?) - tool definitions array (from `maki.agent.tools()`).
///   `local_tools` (table?) - map of `name -> spec` for Lua-backed tools. Each spec
///     requires `description` (string), `input_schema` (table), and
///     `handler` (function). The handler receives the input table and must return
///     `(string)` or `(nil, err)`. Optional `audiences` (string[]) gates who may
///     call it, the same way `maki.api.register_tool` does. The default is the
///     model alone, so a script cannot reach it through `code_execution`.
///   `name` (string?) - display name for logs and UI.
///   `audience` (string?) - tool audience for capability gating. Default: `"general_sub"`.
///   `mcp` (boolean?) - give the session access to MCP tools. Their
///     definitions are injected automatically each turn (deferred behind
///     `tool_search`), so don't put MCP definitions in `tools`. The session
///     starts with no loaded tools of its own. Default: `true`.
///   `thinking` (string|integer?) - thinking mode: `"off"`, `"adaptive"`, an
///     effort level (`"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`,
///     `"max"`), or a budget integer (token count). Inherits parent setting
///     if omitted.
///   `fast` (boolean?) - use fast mode. Inherits parent setting if omitted.
///   `scope` (table?) - `{ session = "<id>" }` detaches the session from the
///     call that spawned it: it survives the call returning and the turn
///     ending, instead of being cancelled the moment either does. `<id>`
///     must be the caller's own session (`ctx:session_id()`). Only cancel-all
///     and a targeted cancel of this session's tool call id can stop it from
///     here on; keep the returned handle (or its id) if you need to reach it
///     later. Omit for the default: tied to the call that spawned it.
/// @return (Session?, string?) Session handle, or `(nil, err)` on failure.
/// @example
/// local tools = maki.agent.tools(ctx, { audience = "general_sub" })
/// local sess, err = maki.agent.session(ctx, {
///   system = "You are a research assistant.",
///   tools = tools,
///   name = "researcher",
/// })
/// if err then error(err) end
/// local result = sess:prompt("Summarize this file.")
/// sess:close()
#[lua_fn]
async fn session(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Table,
) -> LuaResult<Pair<mlua::AnyUserData>> {
    let agent_ctx = try_pair!(dispatch_ctx(&ctx, "session")).clone();
    drop(ctx);
    let detached: bool = match opts.get::<LuaValue>("scope")? {
        LuaValue::Nil => false,
        LuaValue::Table(scope) => {
            let Ok(LuaValue::String(raw)) = scope.get::<LuaValue>("session") else {
                return Ok(err_pair(SCOPE_TABLE_ERR));
            };
            let session: MakiId = try_pair!(
                raw.to_str()?
                    .parse()
                    .map_err(|e: maki_storage::id::MakiIdParseError| e.to_string())
            );
            if agent_ctx.session_id.as_ref().map(SessionRef::id) != Some(session) {
                return Ok(err_pair(SCOPE_SESSION_MISMATCH_ERR));
            }
            true
        }
        _ => return Ok(err_pair(SCOPE_TABLE_ERR)),
    };
    let model_spec: Option<String> = opts.get("model_spec")?;
    let system: Option<String> = opts.get("system")?;
    let tools_val: Option<LuaValue> = opts.get("tools")?;
    let local_tools_tbl: Option<Table> = opts.get("local_tools")?;
    let name: Option<String> = opts.get("name")?;
    let thinking_val: Option<LuaValue> = opts.get("thinking")?;
    let audience = match opts.get::<Option<String>>("audience")? {
        Some(s) => {
            try_pair!(ToolAudience::parse_name(&s).ok_or_else(|| format!("unknown audience: {s}")))
        }
        None => DEFAULT_SESSION_AUDIENCE,
    };
    let fast: bool = opts
        .get::<Option<bool>>("fast")?
        .unwrap_or(agent_ctx.opts.fast);
    let mcp_enabled: bool = opts.get::<Option<bool>>("mcp")?.unwrap_or(true);

    let (model, provider): (Model, Arc<dyn provider::Provider>) = if let Some(ref spec) = model_spec
    {
        let mut m = try_pair!(Model::from_spec_with_policy(spec, &agent_ctx.model_policy));
        let p = try_pair!(provider::from_model_async(&mut m, agent_ctx.timeouts).await);
        (m, Arc::from(p))
    } else {
        (
            Model::clone(&agent_ctx.model),
            Arc::clone(&agent_ctx.provider),
        )
    };
    // A standalone task shows its model via SubagentInfo on the header;
    // a dispatching caller (batch) gets the same thing as a live annotation.
    if let Some(sink) = &agent_ctx.live_sink {
        let _ = sink.send(ToolLive::Annotation(model.spec()));
    }

    let mut tools_json: JsonValue = match tools_val {
        Some(val) => {
            let tools = lua_to_json(&lua, &val)?;
            if !tools.is_array() {
                return Err(mlua::Error::runtime("tools must be an array"));
            }
            tools
        }
        None => JsonValue::Array(vec![]),
    };

    let mut local_map: HashMap<String, LocalTool> = HashMap::new();
    if let Some(tbl) = local_tools_tbl {
        let defs = tools_json.as_array_mut().expect("checked above");
        for pair in tbl.pairs::<String, Table>() {
            let (name, spec) = pair?;
            let description = try_pair!(
                spec.get::<String>("description")
                    .map_err(|_| format!("local_tools.{name}: 'description' is required"))
            );
            let input_schema = lua_to_json(&lua, &spec.get::<LuaValue>("input_schema")?)?;
            let sanitized_schema = sanitize_tool_input_schema(input_schema);
            let handler = try_pair!(
                spec.get::<Function>("handler")
                    .map_err(|_| format!("local_tools.{name}: 'handler' is required"))
            );
            defs.push(serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": sanitized_schema,
            }));
            let audience =
                parse_audience(spec.get::<Option<Table>>("audiences")?, ToolAudience::MODEL)?;
            let weak = lua.weak();
            local_map.insert(
                name,
                maki_agent::tools::local_tool(audience, move |input, _ctx| {
                    let result = call_local_tool(&weak, &handler, &input);
                    Box::pin(async move { result })
                }),
            );
        }
    }

    let thinking = match thinking_val {
        Some(LuaValue::String(s)) => match StoredThinking::parse_setting(&s.to_str()?) {
            Ok(stored) => ThinkingConfig::from(stored),
            Err(e) => return Ok(err_pair(format!("invalid thinking: {e}"))),
        },
        Some(LuaValue::Integer(n)) => match u32::try_from(n) {
            Ok(tokens) if tokens > 0 => ThinkingConfig::Budget(tokens),
            _ => return Ok(err_pair(format!("invalid thinking budget: {n}"))),
        },
        Some(LuaValue::Number(n)) if n >= 1.0 && n <= f64::from(u32::MAX) => {
            ThinkingConfig::Budget(n as u32)
        }
        Some(LuaValue::Number(n)) => {
            return Ok(err_pair(format!("invalid thinking budget: {n}")));
        }
        Some(_) => return Err(mlua::Error::runtime("thinking must be string or number")),
        None => agent_ctx.opts.thinking,
    };

    let (sub_tx, sub_rx) = flume::unbounded::<Envelope>();
    let sub_event_tx = EventSender::new(sub_tx, agent_ctx.event_tx.run_id());
    let parent_tx = agent_ctx.event_tx.clone();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();

    let subagent_info: Arc<OnceLock<SubagentInfo>> = Arc::new(OnceLock::new());
    let (usage_tx, usage_rx) = flume::unbounded();

    smol::spawn(relay_session_events(
        sub_rx,
        parent_tx.clone(),
        Arc::clone(&subagent_info),
        usage_tx,
        agent_ctx.live_sink.clone(),
    ))
    .detach();

    // Register a cancel trigger so the child token does not fire on drop
    // and kill the subagent at birth. The fallback key gets its own id:
    // it keys `subagent_cancels`, so sharing the session id would make two
    // subagents running at once collide.
    let ui_id = agent_ctx
        .tool_use_id
        .clone()
        .unwrap_or_else(|| format!("session-{}", MakiId::generate()));
    // A detached child stands on its own token instead of the caller's: it
    // must outlive the call (and the turn) that spawned it, only stopped by
    // a targeted cancel or cancel-all from here on.
    let (child_trigger, child_cancel) = if detached {
        CancelToken::new()
    } else {
        agent_ctx.cancel.child()
    };
    // Several sessions can share one `ui_id`, so keep the slot and retire
    // only ours on close instead of clearing the whole key.
    let cancel_slot = agent_ctx
        .subagent_cancels
        .insert(ui_id.clone(), child_trigger);

    let name = name.unwrap_or_default();
    info!(name = %name, model = %model.id, "subagent session opened");

    // The array is the caller's, and the filter comes out of it, so whatever
    // the caller left out is also a name this session cannot dispatch or bind
    // inside its sandbox.
    let tools = RequestTools::assembled(tools_json, &agent_ctx.config, &model);

    let state = SessionState {
        params: AgentParams {
            provider,
            model,
            config: agent_ctx.config.clone(),
            tool_output_lines: maki_config::ToolOutputLines::default(),
            permissions: Arc::clone(&agent_ctx.permissions),
            session_id: agent_ctx.session_id.clone(),
            mailbox: None,
            timeouts: agent_ctx.timeouts,
            // Shared with the parent, not fresh: a lock that a subagent does
            // not take is no lock at all once two of them edit one file, and a
            // subagent starting with an empty read history would overwrite
            // whatever its parent read without ever being told it was stale.
            file_access: Arc::clone(&agent_ctx.file_access),
            prompt_slots: Arc::clone(&agent_ctx.prompt_slots),
            subagent_cancels: Arc::new(CancelMap::new()),
            ledger: RunLedger::child(&agent_ctx.ledger),
            registry: Arc::clone(maki_agent::tools::ToolRegistry::global_arc()),
            audience,
            model_policy: Arc::clone(&agent_ctx.model_policy),
        },
        system: system.unwrap_or_default(),
        tools,
        thinking,
        fast,
        mcp: agent_ctx
            .mcp
            .as_ref()
            .filter(|_| mcp_enabled)
            .map(McpSession::fresh),
        history: History::new(Vec::new()),
        sub_event_tx,
        child_cancel,
        answer_rx: Arc::new(AsyncMutex::new(answer_rx)),
        answer_tx: Some(answer_tx),
        parent_cancels: Arc::clone(&agent_ctx.subagent_cancels),
        ui_id,
        cancel_slot,
        parent_event_tx: parent_tx,
        subagent_info,
        local_tools: Arc::new(local_map),
        name,
        usage: TokenUsage::default(),
        usage_rx,
        start: Instant::now(),
        closed: false,
    };

    let sess = lua.create_userdata(LuaSession {
        inner: Arc::new(AsyncMutex::new(state)),
    })?;
    Ok((Some(sess), None))
}

lua_table! {
    /// Subagent primitives for plugins that need to talk to an LLM.
    ///
    /// This module gives you the building blocks: resolve which model to use,
    /// build a system prompt, list available tools, call a tool directly, or
    /// open a full session with its own conversation history.
    ///
    /// Policy like retries, validation, and concurrency lives in the calling
    /// plugin, not here.
    ///
    /// ```lua
    /// local tools = maki.agent.tools(ctx, { audience = "general_sub" })
    /// local sess = maki.agent.session(ctx, {
    ///   system = "You are a helpful assistant.",
    ///   tools = tools,
    /// })
    /// local r = sess:prompt("Hello!")
    /// print(r.text)
    /// sess:close()
    /// ```
    "maki.agent" => pub(crate) fn create_agent_table(), DOCS [
        resolve_model, system_prompt, tools, callable_tools, call_tool, session,
    ]
}

/// Must use `call_async`, not `call`: callbacks that yield (highlight,
/// markdown) hit the C-call boundary otherwise.
struct LiveCallbacks<'a> {
    tool: &'a str,
    on_buf: Option<Function>,
    on_ann: Option<Function>,
    on_usage: Option<Function>,
}

impl LiveCallbacks<'_> {
    async fn deliver(&self, ev: ToolLive) {
        let res = match ev {
            ToolLive::Buf(buf) => call_opt(&self.on_buf, BufHandle::foreign(buf)).await,
            ToolLive::Annotation(ann) => call_opt(&self.on_ann, ann).await,
            ToolLive::Usage(usage) => call_opt(&self.on_usage, usage).await,
        };
        if let Some(Err(e)) = res {
            tracing::warn!(tool = self.tool, error = %e, "call_tool callback failed");
        }
    }
}

async fn call_opt(f: &Option<Function>, arg: impl IntoLuaMulti) -> Option<LuaResult<()>> {
    match f {
        Some(f) => Some(f.call_async::<()>(arg).await),
        None => None,
    }
}

/// Like `interpreter_bridge::dispatch`, but keeps the full `ToolDoneEvent`
/// (the annotation lives there) and feeds live events to `cbs` while the
/// child runs.
async fn dispatch_racing_live(
    tctx: &ToolContext,
    name: &str,
    input: &JsonValue,
    rx: Option<flume::Receiver<ToolLive>>,
    cbs: &LiveCallbacks<'_>,
) -> ToolDoneEvent {
    let run = tool_dispatch::run(String::new(), name, input, tctx, CallOrigin::Nested);
    let Some(rx) = rx else {
        return run.await;
    };
    let mut run = pin!(run);
    loop {
        match select(run.as_mut(), pin!(rx.recv_async())).await {
            Either::Left((done, _)) => {
                while let Ok(ev) = rx.try_recv() {
                    cbs.deliver(ev).await;
                }
                return done;
            }
            Either::Right((Ok(ev), _)) => cbs.deliver(ev).await,
            // The sender is gone but no result arrived: just wait for the run.
            Either::Right((Err(_), _)) => return run.await,
        }
    }
}

struct SessionState {
    params: AgentParams,
    system: String,
    tools: RequestTools,
    thinking: ThinkingConfig,
    fast: bool,
    /// Fresh per session so `tool_search` loads never leak between a
    /// subagent and its parent.
    mcp: Option<McpSession>,
    history: History,
    sub_event_tx: EventSender,
    child_cancel: maki_agent::cancel::CancelToken,
    answer_rx: Arc<AsyncMutex<flume::Receiver<String>>>,
    answer_tx: Option<flume::Sender<String>>,
    parent_cancels: Arc<CancelMap<String>>,
    /// Stable identity for UI, cancel, and history. Falls back to a synthetic
    /// id for workflow-mode sessions (no model-issued tool call exists).
    /// Shared with any sibling session the same tool call opened.
    ui_id: String,
    /// Which registration under [`ui_id`](Self::ui_id) is ours.
    cancel_slot: CancelSlot,
    parent_event_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    local_tools: LocalTools,
    name: String,
    usage: TokenUsage,
    usage_rx: flume::Receiver<TokenUsage>,
    start: Instant,
    closed: bool,
}

impl SessionState {
    fn close(&mut self, failed: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.parent_cancels.retire(&self.ui_id, self.cancel_slot);
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let _ = self.parent_event_tx.send(AgentEvent::SubagentHistory {
            tool_use_id: self.ui_id.clone(),
            messages,
            failed,
        });
        info!(
            name = %self.name,
            duration_ms = self.start.elapsed().as_millis() as u64,
            input_tokens = self.usage.total_input(),
            output_tokens = self.usage.output,
            "subagent session closed",
        );
    }
}

struct LuaSession {
    inner: Arc<AsyncMutex<SessionState>>,
}

impl Drop for LuaSession {
    fn drop(&mut self) {
        match self.inner.try_lock() {
            Some(mut s) => s.close(false),
            // Prompt still in flight: close asynchronously so history
            // and cancel entry are never silently leaked.
            None => {
                let inner = Arc::clone(&self.inner);
                smol::spawn(async move { inner.lock().await.close(false) }).detach();
            }
        }
    }
}

/// Send a message to the subagent and wait for its full response. The agent
/// loop runs to completion, calling tools as needed. Conversation history is
/// kept across calls, so you can have a multi-turn conversation.
///
/// The returned table has fields: `text` (string), `duration_ms` (integer),
/// `input_tokens` (integer), `output_tokens` (integer). `text` is an empty
/// string when the subagent produced no text block (e.g. it only called
/// tools).
///
/// @param message string User message to send.
/// @return (table?, string?) Result table on success, or `(nil, err)` on
/// failure. A run cut short after streaming some text hands you both: the
/// error and a `{ text = <what it streamed> }` table.
/// @example
/// local r, err = sess:prompt("What files are in this project?")
/// if err then error(err) end
/// print(r.text)
/// print(r.input_tokens .. " input, " .. r.output_tokens .. " output tokens")
#[lua_fn]
async fn prompt(
    lua: Lua,
    this: mlua::UserDataRef<LuaSession>,
    message: String,
) -> LuaResult<Pair<Table>> {
    let inner = Arc::clone(&this.inner);
    drop(this);
    let mut guard = inner.lock().await;
    let s = &mut *guard;
    if s.closed {
        return Ok((None, Some(SESSION_CLOSED_ERR.to_owned())));
    }
    if s.subagent_info.get().is_none() {
        let _ = s.subagent_info.set(SubagentInfo {
            parent_tool_use_id: s.ui_id.clone(),
            name: s.name.clone(),
            prompt: Some(message.clone()),
            model: Some(s.params.model.spec()),
            answer_tx: s.answer_tx.take(),
        });
    }

    let history_len = s.history.len();
    let mut agent = Agent::new(
        s.params.clone(),
        AgentRunParams {
            history: &mut s.history,
            system: s.system.clone(),
            event_tx: s.sub_event_tx.clone(),
            tools: s.tools.clone(),
        },
    )
    .with_user_response_rx(Arc::clone(&s.answer_rx))
    .with_cancel(s.child_cancel.clone())
    .with_mcp(s.mcp.clone())
    .with_local_tools(Arc::clone(&s.local_tools));

    let input = AgentInput {
        message,
        mode: AgentMode::Build,
        images: Vec::new(),
        preamble: Vec::new(),
        thinking: s.thinking,
        fast: s.fast,
        workflow: false,
        prompt: None,
    };
    let result = agent.run(input).await;
    drop(agent);
    // Only this call's messages count: older turns may hold stale preamble
    // text, and the agent loop's empty-response retry leaves a synthetic
    // "(empty)" assistant marker that must not pass for a real response.
    // Auto-compaction can shrink the history mid-run, so clamp the start:
    // after a rewrite the tail is this call's output either way.
    let turn = &s.history.as_slice()[history_len.min(s.history.len())..];
    // A subagent can be cancelled on its own, and its caller should hear about
    // that instead of taking a half-finished answer for a real one, so cancel
    // reads like an error here even though the run ended normally.
    let cut_short = match &result {
        Err(e) => Some(e.to_string()),
        Ok(DoneReason::Cancelled) => Some(CANCELLED_MSG.to_owned()),
        Ok(_) => None,
    };
    if let Some(err) = cut_short {
        let partial = turn
            .iter()
            .filter(|m| matches!(m.role, Role::Assistant))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } if text != EMPTY_RESPONSE_MARKER => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tbl = if partial.is_empty() {
            None
        } else {
            let tbl = lua.create_table()?;
            tbl.set("text", partial)?;
            Some(tbl)
        };
        return Ok((tbl, Some(err)));
    }
    // Waiting here doubles as an ordering barrier: the relay reaches `Done` only
    // after every `TurnComplete`, so all our `ToolLive::Usage` messages sit in the
    // live channel before `dispatch_racing_live` drains it for the last time.
    match s.usage_rx.recv_async().await {
        Ok(usage) => s.usage += usage,
        Err(_) => tracing::warn!(
            name = %s.name,
            "subagent usage tracker stopped, token counts may lag"
        ),
    }

    let text = turn
        .iter()
        .rfind(|m| matches!(m.role, Role::Assistant))
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        });
    let text = text.map_or_else(String::new, str::to_owned);

    let tbl = lua.create_table()?;
    tbl.set("text", text)?;
    tbl.set("duration_ms", s.start.elapsed().as_millis() as u64)?;
    tbl.set("input_tokens", s.usage.total_input())?;
    tbl.set("output_tokens", s.usage.output)?;
    Ok((Some(tbl), None))
}

/// Close the session and flush its history back to the parent agent. You can
/// call this multiple times safely. If you forget, it runs automatically when
/// the session is garbage collected.
///
/// @param err string? Pass the failure reason when the run failed, so the session's UI item ends as errored even without a following tool result.
/// @return
#[lua_fn]
async fn close(
    _lua: Lua,
    this: mlua::UserDataRef<LuaSession>,
    err: Option<String>,
) -> LuaResult<()> {
    let inner = Arc::clone(&this.inner);
    drop(this);
    let mut s = inner.lock().await;
    s.close(err.is_some());
    Ok(())
}

lua_class! {
    /// A subagent session with its own conversation history.
    ///
    /// Create one with `maki.agent.session()`, then send messages with
    /// `:prompt()`. The session remembers previous turns, so you can have
    /// a multi-step conversation. Call `:close()` when you are done, or let
    /// garbage collection handle it.
    "maki.agent.Session" => LuaSession, SESSION_DOCS [prompt, close]
}

/// Weak Lua ref avoids a reference cycle when the session is stored in userdata.
fn call_local_tool(
    weak: &mlua::WeakLua,
    f: &Function,
    input: &JsonValue,
) -> Result<String, String> {
    let lua = weak.try_upgrade().ok_or("Lua runtime shut down")?;
    let arg = json_to_lua(&lua, input).map_err(|e| e.to_string())?;
    let values = f.call::<mlua::MultiValue>(arg).map_err(|e| e.to_string())?;
    lua_tool_result(values)
}

#[cfg(test)]
mod tests {
    use maki_agent::{DoneReason, TurnCompleteEvent};
    use maki_providers::Message;
    use serde_json::json;

    use super::*;

    fn call(src: &str, input: JsonValue) -> Result<String, String> {
        let lua = Lua::new();
        let f: Function = lua.load(src).eval().unwrap();
        call_local_tool(&lua.weak(), &f, &input)
    }

    #[test]
    fn local_tool_handler_result_conventions() {
        let input = json!({"x": "1"});
        assert_eq!(
            call("function(v) return 'ok:' .. v.x end", input.clone()),
            Ok("ok:1".into())
        );
        assert_eq!(
            call("function() return nil, 'bad' end", input.clone()),
            Err("bad".into())
        );
        assert_eq!(
            call("function() end", input.clone()),
            Err(crate::api::util::convert::NIL_TOOL_RESULT_ERR.into())
        );
        let raised = call("function() error('boom') end", input.clone()).unwrap_err();
        assert!(raised.contains("boom"), "got: {raised}");
        let wrong = call("function() return 42 end", input).unwrap_err();
        assert!(wrong.contains("expected string"), "got: {wrong}");
    }

    const RUN_ID: u64 = 7;
    const PARENT_ID: &str = "task-1";
    const IGNORED_ERROR: &str = "handled by the session caller";
    const DONE_USAGE: TokenUsage = tokens(150, 30);

    const fn tokens(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cache_creation: 0,
            cache_read: 0,
            cost: None,
        }
    }

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: RUN_ID,
        }
    }

    fn turn(usage: TokenUsage, cost: f64) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage,
            model: "test-model".into(),
            cost: Some(cost),
            context_size: None,
            context_window: 0,
        }))
    }

    #[test]
    fn relay_session_events_reports_live_usage_and_done_total() {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let subagent_info = Arc::new(OnceLock::new());
        subagent_info
            .set(SubagentInfo {
                parent_tool_use_id: PARENT_ID.into(),
                name: "research".into(),
                prompt: None,
                model: None,
                answer_tx: None,
            })
            .unwrap();
        let (usage_tx, usage_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();

        for event in [
            turn(tokens(100, 20), 0.25),
            turn(tokens(50, 10), 0.5),
            AgentEvent::Error {
                message: IGNORED_ERROR.into(),
            },
            AgentEvent::Done {
                usage: DONE_USAGE,
                cost: None,
                list_cost: None,
                context_size: 0,
                context_window: 0,
                num_turns: 2,
                reason: DoneReason::EndTurn,
            },
        ] {
            sub_tx.send(envelope(event)).unwrap();
        }
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID),
            subagent_info,
            usage_tx,
            Some(live_tx),
        ));

        let live = live_rx
            .drain()
            .map(|event| match event {
                ToolLive::Usage(usage) => usage,
                _ => panic!("relay must only publish usage"),
            })
            .collect::<Vec<_>>();
        let expected = [
            tokens(100, 20).format_sum_cost(Some(0.25)),
            tokens(50, 10).format_sum_cost(Some(0.75)),
        ];
        assert_eq!(live, expected);
        assert_eq!(usage_rx.try_recv(), Ok(DONE_USAGE));

        let forwarded = parent_rx.drain().collect::<Vec<_>>();
        assert_eq!(forwarded.len(), expected.len());
        assert!(forwarded.iter().all(|envelope| {
            matches!(envelope.event, AgentEvent::TurnComplete(_))
                && envelope
                    .subagent
                    .as_ref()
                    .is_some_and(|info| info.parent_tool_use_id == PARENT_ID)
        }));
    }
}
