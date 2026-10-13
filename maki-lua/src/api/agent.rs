//! `maki.agent` exposes subagent primitives to Lua plugins. Policy (retries,
//! validation, concurrency) lives in the task plugin, not here.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::future::{Either, select};
use maki_agent::agent::{LoadedInstructions, tool_dispatch};
use maki_agent::cancel::{CancelMap, CancelSlot, CancelToken};
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::registry::ToolRegistry;
use maki_agent::tools::schema::sanitize_tool_input_schema;
use maki_agent::tools::{
    CallOrigin, Deadline, DescriptionContext, LocalTool, LocalTools, RequestTools, ToolAudience,
    ToolContext, ToolFilter, ToolLive, ToolOutputMode,
};
use maki_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, AgentSeed,
    DETACHED_RUN_ID, DoneReason, EMPTY_RESPONSE_MARKER, EventSender, EventStreamGuard, History,
    InputSource, McpSession, RunContext, RunContextBuilder, RunLedger, SessionEvents,
    SubagentInbox, SubagentInfo, ToolDoneEvent, UNAVAILABLE_RESULT, event_stream,
};
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use maki_providers::model::ModelTier;
use maki_providers::provider;
use maki_providers::{
    ContentBlock, ContextGauge, Model, RequestOptions, Role, ThinkingConfig, add_cost,
};
use maki_storage::id::MakiId;
use maki_storage::now_epoch;
use maki_storage::sessions::StoredThinking;
use mlua::{
    Either as LuaEither, Function, IntoLuaMulti, Lua, Result as LuaResult, Table, UserDataRef,
    Value as LuaValue,
};
use serde_json::Value as JsonValue;
use smol::lock::Mutex as AsyncMutex;
use tracing::info;

use crate::api::session::{not_live, required_session};
use crate::api::tool::{audiences_to_lua, parse_audience};
use crate::api::ui::buf::BufHandle;
use crate::api::util::command::UiAction;
use crate::api::util::convert::{json_to_lua, lua_to_json, lua_tool_result};
use crate::api::util::ctx::{AgentContext, LuaCtx};
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::background::{BackgroundInfo, BackgroundSession, BackgroundSessions};
use crate::runtime::{CANCELLED_MSG, active_task_cancel, host_ui};

const SESSION_CLOSED_ERR: &str = "session closed";
const PROMPT_DROPPED_ERR: &str = "an `agent.user_message` layer dropped the prompt";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
const OUTPUT_MODE_ERR: &str = "output_mode must be 'full' or 'compact'";
const OPTS_REQUIRED_ERR: &str = "opts table is required";
const RUNNING_STATUS: &str = "running";
const IDLE_STATUS: &str = "idle";
const TASK_REQUIRED_ERR: &str = "maki.agent.session({ session = id }) and its prompts must run \
     in a task: maki.async.spawn, an autocmd callback or a tool handler";
const SCOPE_ERR: &str = "scope must be \"session\"";
const SCOPE_NESTED_ERR: &str = "scope = \"session\" is only valid on the main session";

fn resolve_model_from_ctx(ctx: &AgentContext, tier: Option<&str>) -> Result<Model, String> {
    let Some(tier_str) = tier else {
        return Ok(Model::clone(&ctx.model));
    };
    let requested = tier_str.parse::<ModelTier>().map_err(|e| e.to_string())?;
    let effective = requested.capped_at(ctx.model.tier);
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

fn required_opts(opts: Option<Table>) -> LuaResult<Table> {
    opts.ok_or_else(|| mlua::Error::runtime(OPTS_REQUIRED_ERR))
}

/// Settings left out come from the parent, and a thinking level the plugin
/// asks for can't go above the parent's. We clamp to the model once, right
/// here, so the status badge, `AgentInput` and the request all agree on what
/// this session really runs.
fn session_opts(
    thinking: Option<ThinkingConfig>,
    fast: Option<bool>,
    parent: RequestOptions,
    model: &Model,
) -> RequestOptions {
    RequestOptions {
        thinking: thinking.map_or(parent.thinking, |t| t.clamp_to(parent.thinking)),
        fast: fast.unwrap_or(parent.fast),
    }
    .clamped(model)
}

/// The ctx-less form borrows a live chat session's settings by id. We copy
/// what we need and let the seed go, because holding it would keep an ended
/// chat session looking live.
fn seed_ctx(
    opts: &Table,
    cancel: CancelToken,
) -> LuaResult<Result<(MakiId, AgentContext), String>> {
    Ok(required_session(Some(opts))?.and_then(|session| {
        let seed = AgentSeed::live(session).ok_or_else(|| not_live(session))?;
        Ok((session, AgentContext::from(&seed.tool_context(cancel))))
    }))
}

/// A background session reports to a sink nobody reads, so its turns would
/// never reach the chat's totals on their own. We send each one to the tab the
/// moment it is paid for. A quit or `/new` in the middle of a long prompt then
/// loses only the turn still streaming, just like the main chat. The registry
/// entry adds up the same turns, so `/tasks` matches the chat totals even in
/// the middle of a long prompt.
struct TurnBilling {
    session: MakiId,
    ui: flume::Sender<UiAction>,
    registry: BackgroundSessions,
    id: String,
}

/// Forwards subagent events to the parent, stamped with the subagent identity.
/// Usage goes live on the tool header while the run goes on (last turn's tokens
/// plus the run's summed cost). `Done` only pings `done_tx`, which tells
/// `prompt` that every turn of the run has passed through. A background session bills
/// every turn it sees, compactions and nested subagents too, each under the
/// model that ran it. Nested relays hand their turns up here instead of billing
/// them, and `Done` only repeats the run's total, so no turn gets paid twice.
async fn relay_session_events(
    mut sub_events: SessionEvents,
    parent_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    done_tx: flume::Sender<()>,
    live_sink: Option<flume::Sender<ToolLive>>,
    billing: Option<TurnBilling>,
) {
    let mut cost = None;
    while let Some(mut envelope) = sub_events.next().await {
        match &envelope.event {
            AgentEvent::TurnComplete(turn) => {
                add_cost(&mut cost, turn.cost);
                if let Some(sink) = &live_sink {
                    let _ = sink.send(ToolLive::Usage(turn.usage.format_sum_cost(cost)));
                }
                if let Some(billing) = &billing {
                    // Before the UI hears of it, so whoever sees the bill also
                    // sees it in `maki.agent.background()`.
                    billing.registry.update(&billing.id, |e| e.cost = cost);
                    let _ = billing.ui.try_send(UiAction::BackgroundUsage {
                        session: billing.session,
                        model: turn.model.clone(),
                        usage: turn
                            .usage
                            .billed_with_subsidised_list_cost(turn.cost, turn.subsidised_list_cost),
                    });
                }
            }
            AgentEvent::Done { .. } => {
                let _ = done_tx.send(());
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
/// Without a `ctx`, pass `session = id` in `opts` to get the tools a
/// background session of that chat would have.
///
/// @param ctx LuaCtx|table Agent context, or `opts` when called without one.
/// @param opts table? Required with a ctx. Fields:
///   `session` (string) - only without a ctx: id of a live chat session.
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
async fn tools(
    lua: Lua,
    ctx: LuaEither<UserDataRef<LuaCtx>, Table>,
    opts: Option<Table>,
) -> LuaResult<Pair<LuaValue>> {
    let (agent, opts) = match ctx {
        LuaEither::Left(ctx) => (
            try_pair!(dispatch_ctx(&ctx, "tools")).clone(),
            required_opts(opts)?,
        ),
        LuaEither::Right(opts) => (try_pair!(seed_ctx(&opts, CancelToken::none())?).1, opts),
    };
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
///   `output_mode` (string?) - "full" (default) returns the complete text.
///     "compact" returns what the model would get, with output limits and
///     RTK rewriting. Permissions and hooks apply in both modes, and hooks
///     see the mode as `ctx.output_mode`.
///   `on_live_buf` (function?) - called with a `BufHandle` for each live buffer
///     the tool publishes. Must not yield.
///   `on_annotation` (function?) - called with an annotation string for each
///     annotation event. Must not yield.
///   `on_usage` (function?) - called with a formatted cumulative token usage
///     string. Must not yield.
/// @return (string?, string?) Tool output text, or `(nil, err)` on failure.
///   Instruction files the child picks up (a subdirectory `AGENTS.md`) are
///   not in the text: they land on the calling tool's own result.
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
    tctx.output_mode = ToolOutputMode::Full;
    let (mut on_buf, mut on_ann, mut on_usage, mut rx) = (None, None, None, None);
    if let Some(o) = opts {
        if let Some(mode) = o.get::<Option<String>>("output_mode")? {
            let Some(mode) = ToolOutputMode::parse(&mode) else {
                return Ok(err_pair(OUTPUT_MODE_ERR));
            };
            tctx.output_mode = mode;
        }
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
    Ok(pair(interpreter_bridge::flatten(&done)))
}

/// Create a new subagent session. The session inherits the parent model and
/// MCP handle unless you override them. You get back a `Session` object that
/// you can send messages to with `:prompt()`.
///
/// This is the main way to spin up a sub-conversation with its own history
/// and tool set.
///
/// Without a `ctx`, `maki.agent.session({ session = id, ... })` opens a
/// background session. It borrows the model, permissions and MCP of the live
/// chat session `id` and is not tied to any turn. Call it and its `:prompt()`
/// from a task: a `maki.async.spawn` task, an autocmd callback or a tool
/// handler.
///
/// It ends at the first of these:
///
/// - the task that opened it ends (an autocmd callback when it returns, a tool
///   handler with its tool call)
/// - `:close()`
/// - the chat session ends (`/new`, closing the tab)
/// - the plugin unloads (`/reload`)
///
/// Open long-running monitors from `maki.async.spawn`. Turn ends, Esc and
/// rewinds leave them alone. Esc on a tool call still cancels a prompt that
/// the tool's handler is waiting on.
///
/// Nobody can answer its permission prompts, so a call that would prompt is
/// denied and the subagent is told why. Your rules, "allow for this session"
/// answers and YOLO still apply, so add allow rules for what it runs (see
/// [Permissions](/docs/permissions/)). It always runs in build mode. While the
/// chat is in plan mode its file-write tools are denied. Its thinking never
/// goes above the chat's current setting.
///
/// Its spend counts in the chat's totals after each turn, and `/tasks` lists
/// it but cannot stop it yet. Setting `plugins.<name>.enabled = false` stops a
/// plugin's monitors from the next `/reload`. Under `maki -p` and ACP there is
/// no live chat session, so opening one fails with "session not live". The
/// [maki.agent](#maki-agent) overview has a full monitor.
///
/// @param ctx LuaCtx|table Agent context, or `opts` when called without one.
/// @param opts table? Required with a ctx. Optional fields:
///   `session` (string) - required without a ctx: id of the live chat session
///     that owns it. Returns `(nil, "session not live: <id>")` once that
///     session has ended.
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
///     `"max"`), or a budget integer (token count). Inherits the parent
///     setting if omitted, and is capped at it otherwise.
///   `fast` (boolean?) - use fast mode. Inherits parent setting if omitted.
///   `scope` (string?) - `"session"` detaches the session from the call that
///     spawned it: it survives the call returning and the turn ending,
///     instead of being cancelled the moment either does. Esc on its chat
///     still cancels it. Keep the returned handle to `prompt` and `close`.
///     Omit for the default: tied to the call that spawned it. Invalid
///     inside a subagent.
/// @return (Session?, string?) Session handle, or `(nil, err)` on failure.
/// @example
/// local tools = maki.agent.tools(ctx, { audience = "general_sub" })
/// local sess, err = maki.agent.session(ctx, {
///   system = "You are a research assistant.",
///   tools = tools,
///   name = "researcher",
/// })
/// if err then error(err) end
///
/// -- Close before handling the error, so no path leaves the session open.
/// local result, prompt_err = sess:prompt("Summarize this file.")
/// sess:close()
/// if prompt_err then error(prompt_err) end
#[lua_fn]
async fn session(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    ctx: LuaEither<UserDataRef<LuaCtx>, Table>,
    opts: Option<Table>,
) -> LuaResult<Pair<mlua::AnyUserData>> {
    // The ctx borrow ends inside its arm, before any wait: a cancel hook
    // firing meanwhile needs `ctx:finish`, which takes it mutably.
    let (agent_ctx, opts, owner, detached) = match ctx {
        LuaEither::Left(ctx) => {
            let agent_ctx = try_pair!(dispatch_ctx(&ctx, "session")).clone();
            let opts = required_opts(opts)?;
            let detached: bool = match opts.get::<LuaValue>("scope")? {
                LuaValue::Nil => false,
                LuaValue::String(s) if s.to_str()?.as_ref() == "session" => true,
                _ => return Ok(err_pair(SCOPE_ERR)),
            };
            if detached && agent_ctx.task_id.is_some() {
                return Ok(err_pair(SCOPE_NESTED_ERR));
            }
            (agent_ctx, opts, None, detached)
        }
        LuaEither::Right(opts) => {
            let Some((task, _)) = active_task_cancel(&lua) else {
                return Ok(err_pair(TASK_REQUIRED_ERR));
            };
            // A child of the task's token would leave a watcher waiting on it
            // until it fires. An autocmd's token never fires, so every open
            // would leak one. The entry keeps this trigger instead, and the
            // entry goes away when the task ends.
            let (cancel, token) = CancelToken::new();
            let (session, agent_ctx) = try_pair!(seed_ctx(&opts, token)?);
            (agent_ctx, opts, Some((session, task, cancel)), false)
        }
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
    let fast: Option<bool> = opts.get("fast")?;
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

    // Numbers take the same route as strings: `parse_setting` already spells
    // out every accepted word and budget, so there is one grammar and one
    // error message instead of a second one per Lua number type.
    let requested_thinking = match thinking_val {
        None => None,
        Some(value) => {
            let setting = match &value {
                LuaValue::String(s) => s.to_str()?.to_owned(),
                LuaValue::Integer(n) => n.to_string(),
                LuaValue::Number(n) => n.to_string(),
                other => {
                    return Ok(err_pair(format!(
                        "thinking must be string or number, got {}",
                        other.type_name()
                    )));
                }
            };
            match StoredThinking::parse_setting(&setting) {
                Ok(stored) => Some(ThinkingConfig::from(stored)),
                Err(e) => return Ok(err_pair(format!("invalid thinking: {e}"))),
            }
        }
    };
    let opts = session_opts(requested_thinking, fast, agent_ctx.opts, &model);

    let (stream_guard, sub_events) = event_stream();
    let parent_tx = if detached {
        agent_ctx.event_tx.with_run_id(DETACHED_RUN_ID)
    } else {
        agent_ctx.event_tx.clone()
    };
    let sub_event_tx = stream_guard.sender(parent_tx.run_id());
    // A subagent can only ask the user if its parent can, and a detached
    // session outlives the turn that could answer it, so it never gets a
    // channel. Under `maki -p` or a background session nobody reads the
    // subagent's chat, so a fresh channel would leave every prompt waiting
    // until cancel. Without one, `enforce` denies the call right away.
    let (answer_tx, answer_rx) = (!detached && agent_ctx.user_response_rx.is_some())
        .then(flume::unbounded::<String>)
        .map(|(tx, rx)| (tx, Arc::new(AsyncMutex::new(rx))))
        .unzip();

    let subagent_info: Arc<OnceLock<SubagentInfo>> = Arc::new(OnceLock::new());
    let (done_tx, done_rx) = flume::unbounded();
    // Doubles as the task id tools see. The fallback gets its own id: it keys
    // `subagent_cancels`, where two subagents sharing the session id would
    // collide.
    let ui_id = agent_ctx
        .tool_use_id
        .clone()
        .unwrap_or_else(|| format!("session-{}", MakiId::generate()));
    let billing = owner.as_ref().and_then(|&(session, ..)| {
        Some(TurnBilling {
            session,
            ui: host_ui(&lua)?,
            registry: BackgroundSessions::of(&lua),
            id: ui_id.clone(),
        })
    });

    smol::spawn(relay_session_events(
        sub_events,
        parent_tx.clone(),
        Arc::clone(&subagent_info),
        done_tx,
        agent_ctx.live_sink.clone(),
        billing,
    ))
    .detach();

    // Registered before the session runs so the child token does not fire
    // on drop and kill the subagent at birth. A detached child stands on
    // its own token instead of the caller's: it must outlive the call (and
    // the turn) that spawned it, only stopped by a targeted cancel or
    // cancel-all from here on.
    let (child_trigger, child_cancel) = if detached {
        CancelToken::new()
    } else {
        agent_ctx.cancel.child()
    };
    // Several sessions can share one `ui_id`, so keep the slot and retire
    // only ours on close instead of clearing the whole key.
    let cancel_slot = agent_ctx
        .subagent_cancels
        .insert(ui_id.clone(), child_trigger, detached);

    let name = name.unwrap_or_default();
    let background = match owner {
        Some((session, task, cancel)) => {
            let registry = BackgroundSessions::of(&lua);
            try_pair!(registry.register(BackgroundSession {
                info: BackgroundInfo {
                    id: ui_id.clone(),
                    plugin,
                    name: name.clone(),
                    running: false,
                    started_at: now_epoch(),
                    cost: None,
                },
                session,
                task,
                _cancel: cancel,
            }));
            Some(Background {
                registry,
                session,
                thinking: requested_thinking,
                fast,
            })
        }
        None => None,
    };
    info!(
        name = %name,
        model = %model.id,
        background = background.is_some(),
        "subagent session opened"
    );

    // The array is the caller's, and the filter comes out of it, so whatever
    // the caller left out is also a name this session cannot dispatch or bind
    // inside its sandbox.
    let tools = RequestTools::assembled(tools_json, &agent_ctx.config, &model);
    let system = system.unwrap_or_default();
    let context: RunContextBuilder =
        Arc::new(move |_, _| RunContext::fixed(system.clone(), tools.clone()));

    let state = SessionState {
        params: AgentParams {
            provider,
            model,
            config: agent_ctx.config.clone(),
            tool_output_lines: maki_config::ToolOutputLines::default(),
            permissions: Arc::clone(&agent_ctx.permissions),
            session_id: agent_ctx.session_id.clone(),
            background_for: agent_ctx.background_for,
            task_id: Some(Arc::from(ui_id.as_str())),
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
        context,
        opts,
        mcp: agent_ctx
            .mcp
            .as_ref()
            .filter(|_| mcp_enabled)
            .map(McpSession::fresh),
        history: History::new(Vec::new()),
        gauge: ContextGauge::default(),
        loaded_instructions: LoadedInstructions::new(),
        sub_event_tx,
        stream_guard: Some(stream_guard),
        child_cancel,
        answer_rx,
        answer_tx,
        reauth: agent_ctx.reauth,
        inbox: Arc::new(SubagentInbox::default()),
        parent_cancels: Arc::clone(&agent_ctx.subagent_cancels),
        ui_id,
        cancel_slot,
        parent_event_tx: parent_tx,
        subagent_info,
        local_tools: Arc::new(local_map),
        name,
        dones: RunDones::new(done_rx),
        start: Instant::now(),
        closed: false,
        detached,
        background,
    };

    let background_id = state.background.as_ref().map(|_| state.ui_id.clone());
    let sess = lua.create_userdata(LuaSession {
        inner: Arc::new(AsyncMutex::new(state)),
        background_id,
    })?;
    Ok((Some(sess), None))
}

/// List the open background sessions of a chat session.
///
/// Each entry has these fields:
///   `id` (string) Session id.
///   `plugin` (string) Plugin that opened it.
///   `name` (string) Name given at open.
///   `status` (string) `"running"` during a `:prompt()`, otherwise `"idle"`.
///   `started_at` (integer) Unix seconds.
///   `cost` (number?) Dollars billed so far, updated after each turn. nil until
///     a priced turn ends.
///
/// @param opts table Required fields:
///   `session` (string) - id of the chat session.
/// @return (table?, string?) Array of entries, or `(nil, err)` on failure.
/// @example
/// for _, s in ipairs(maki.agent.background({ session = id })) do
///   print(s.plugin, s.name, s.status, os.time() - s.started_at)
/// end
#[lua_fn]
fn background(lua: &Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session = try_pair!(required_session(Some(&opts))?);
    let out = lua.create_table()?;
    for entry in BackgroundSessions::of(lua).list(session) {
        let status = if entry.running {
            RUNNING_STATUS
        } else {
            IDLE_STATUS
        };
        let t = lua.create_table()?;
        t.set("status", status)?;
        t.set("id", entry.id)?;
        t.set("plugin", &*entry.plugin)?;
        t.set("name", entry.name)?;
        t.set("started_at", entry.started_at)?;
        t.set("cost", entry.cost)?;
        out.push(t)?;
    }
    Ok((Some(out), None))
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
    ///
    /// A CI monitor for each chat session the user opens. It wakes the agent
    /// once when CI turns red and stays quiet until CI is green again. Each
    /// wake costs a full agent turn, and the model words the same failure
    /// differently every round, so comparing replies would wake it every time.
    /// `SessionFocusChanged` also fires after `/reload`, so the monitor comes
    /// back on its own.
    ///
    /// ```lua
    /// local tasks, failing = {}, {}
    ///
    /// local function start(sid)
    ///   if tasks[sid] then return end
    ///   tasks[sid] = maki.async.spawn(function()
    ///     while true do
    ///       maki.async.sleep(5 * 60 * 1000)
    ///       -- Prompts are denied here, so this needs an allow rule like `gh run list *`.
    ///       local sess = maki.agent.session({
    ///         session = sid,
    ///         name = "ci-monitor",
    ///         system = "Run `gh run list -L 3`. Reply exactly OK if all runs passed, else a 3-line failure summary.",
    ///         tools = (maki.agent.tools({ session = sid, audience = "general_sub", only = { "bash" } })),
    ///       })
    ///       -- nil once the chat session is gone. SessionEnd below stops the loop.
    ///       if sess then
    ///         local r, err = sess:prompt("Check CI now.")
    ///         sess:close()
    ///         local text = not err and r.text:match("^%s*(.-)%s*$")
    ///         if text == "OK" or text == "OK." then
    ///           failing[sid] = nil
    ///         elseif text and text ~= "" and not failing[sid] then
    ///           failing[sid] = true
    ///           maki.session.notify("[ci-monitor] " .. text, { session = sid, wake = true })
    ///         end
    ///       end
    ///     end
    ///   end)
    /// end
    ///
    /// maki.api.create_autocmd("SessionFocusChanged", {
    ///   callback = function(ev) start(ev.data.session_id) end,
    /// })
    /// maki.api.create_autocmd("SessionEnd", {
    ///   callback = function(ev)
    ///     local sid = ev.data.session_id
    ///     if tasks[sid] then tasks[sid]:cancel() end
    ///     tasks[sid], failing[sid] = nil, nil
    ///   end,
    /// })
    /// ```
    "maki.agent" => pub(crate) fn create_agent_table(plugin: Arc<str>), DOCS [
        resolve_model, system_prompt, tools, callable_tools, call_tool, session(plugin),
        background,
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
    /// The caller's prompt and tools, fixed for the session's life.
    context: RunContextBuilder,
    /// Already reconciled against `params.model`, so every reader agrees.
    opts: RequestOptions,
    /// Fresh per session so `tool_search` loads never leak between a
    /// subagent and its parent.
    mcp: Option<McpSession>,
    history: History,
    /// Travels with `history`: a subagent session spans many runs, and a gauge
    /// rebuilt per run would forget every measurement it made.
    gauge: ContextGauge,
    /// Fresh per session, not per prompt: the parent's set would hide files
    /// this model never saw, and a per-run set would inject the same file on
    /// every turn.
    loaded_instructions: LoadedInstructions,
    sub_event_tx: EventSender,
    /// Dropped on close, which ends the relay task. Tool contexts keep
    /// [`EventSender`] clones alive past the run, so the relay cannot key off
    /// sender disconnect.
    stream_guard: Option<EventStreamGuard>,
    child_cancel: maki_agent::cancel::CancelToken,
    answer_rx: Option<Arc<AsyncMutex<flume::Receiver<String>>>>,
    answer_tx: Option<flume::Sender<String>>,
    reauth: bool,
    /// Shared with the host through [`SubagentInfo`], so a user watching this
    /// session can queue messages that its next turn boundary picks up.
    inbox: Arc<SubagentInbox>,
    parent_cancels: Arc<CancelMap<String>>,
    /// Stable identity for UI, cancel, and history. Falls back to a synthetic
    /// id for workflow-mode sessions (no model-issued tool call exists).
    /// Shared with any sibling session the same tool call opened.
    ui_id: String,
    /// Which registration under [`ui_id`](Self::ui_id) is ours.
    cancel_slot: CancelSlot,
    parent_event_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    /// Set at creation from `scope = "session"`; echoed on every
    /// [`SubagentInfo`] so the UI knows to keep the chat cancellable after
    /// the spawning run ends.
    detached: bool,
    local_tools: LocalTools,
    name: String,
    dones: RunDones,
    start: Instant,
    closed: bool,
    background: Option<Background>,
}

/// Every run that ends `Ok` leaves exactly one `Done` on the relay's channel,
/// and one that errs or is dropped mid-flight leaves none. A prompt cut short
/// does not wait for its `Done`, and neither does one dropped while waiting.
/// Events carry no prompt tag, so the next prompt can't tell a leftover from
/// its own. It counts how many it is owed instead and reads past the old ones.
struct RunDones {
    rx: flume::Receiver<()>,
    unread: usize,
}

impl RunDones {
    fn new(rx: flume::Receiver<()>) -> Self {
        Self { rx, unread: 0 }
    }

    fn expect_one(&mut self) {
        self.unread += 1;
    }

    /// Safe to drop mid-wait, because a `Done` only stops being owed once it
    /// has been read.
    async fn caught_up(&mut self) -> Result<(), flume::RecvError> {
        while self.unread > 0 {
            self.rx.recv_async().await?;
            self.unread -= 1;
        }
        Ok(())
    }
}

struct Background {
    registry: BackgroundSessions,
    session: MakiId,
    /// What the plugin asked for at open. We keep it because every prompt caps
    /// it again against the chat as it is at that moment.
    thinking: Option<ThinkingConfig>,
    fast: Option<bool>,
}

impl Background {
    fn start_prompt(&self, id: &str) -> PromptSettle {
        self.registry.update(id, |e| e.running = true);
        PromptSettle {
            registry: self.registry.clone(),
            id: id.to_owned(),
        }
    }
}

/// Wraps up a background prompt when it drops. A cancelled task drops the
/// prompt future in the middle of the run, so the code after its `await` never
/// runs. Without this the entry would say "running" forever. It can drop while
/// Lua is tearing down the cancelled coroutine, so it only touches Rust state
/// and never calls into Lua.
struct PromptSettle {
    registry: BackgroundSessions,
    id: String,
}

impl Drop for PromptSettle {
    fn drop(&mut self) {
        self.registry.update(&self.id, |e| e.running = false);
    }
}

impl SessionState {
    fn close(&mut self, failed: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.stream_guard.take();
        self.parent_cancels.retire(&self.ui_id, self.cancel_slot);
        if let Some(background) = &self.background {
            background.registry.remove(&self.ui_id);
        }
        let usage = self.params.ledger.totals().usage;
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let _ = self.parent_event_tx.send(AgentEvent::SubagentHistory {
            tool_use_id: self.ui_id.clone(),
            messages,
            failed,
        });
        info!(
            name = %self.name,
            duration_ms = self.start.elapsed().as_millis() as u64,
            input_tokens = usage.total_input(),
            output_tokens = usage.output,
            "subagent session closed",
        );
    }
}

struct LuaSession {
    inner: Arc<AsyncMutex<SessionState>>,
    /// A background session's registry id, kept out here because a running
    /// prompt holds `inner` until it ends, and that can be never.
    background_id: Option<String>,
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
    // The session's own token fires only when the task that opened it ends.
    // Esc on a tool handler fires that handler's token, and the handler may not
    // be the opener. So a background prompt races the caller's token, from the
    // wait for the session lock to the end of the run, or the handler would sit
    // there until the host gives up on it. Only the caller's own stack knows
    // which task it is, so this has to happen before the first await.
    let caller_cancel = match (this.background_id.is_some(), active_task_cancel(&lua)) {
        (false, _) => None,
        (true, Some((_, cancel))) => Some(cancel),
        (true, None) => return Ok(err_pair(TASK_REQUIRED_ERR)),
    };
    let inner = Arc::clone(&this.inner);
    drop(this);
    let mut guard = match &caller_cancel {
        Some(cancel) => match cancel.race(inner.lock()).await {
            Ok(guard) => guard,
            Err(_) => return Ok(err_pair(CANCELLED_MSG)),
        },
        None => inner.lock().await,
    };
    let s = &mut *guard;
    if s.closed {
        return Ok((None, Some(SESSION_CLOSED_ERR.to_owned())));
    }
    // The user can lower the effort while a monitor sits idle, so we ask the
    // seed again on every prompt. Plan mode is left out on purpose. Each write
    // checks it instead, see `AgentSeed::background_write_block`.
    let watch = match (&s.background, caller_cancel) {
        (Some(background), Some(caller_cancel)) => {
            let Some(seed) = AgentSeed::live(background.session) else {
                return Ok(err_pair(not_live(background.session)));
            };
            s.opts = session_opts(
                background.thinking,
                background.fast,
                seed.chat_opts(),
                &s.params.model,
            );
            Some((caller_cancel, background.start_prompt(&s.ui_id)))
        }
        _ => None,
    };
    if s.subagent_info.get().is_none() {
        let _ = s.subagent_info.set(SubagentInfo {
            parent_tool_use_id: s.ui_id.clone(),
            name: s.name.clone(),
            prompt: Some(message.clone()),
            model: Some(s.params.model.spec()),
            opts: Some(s.opts),
            answer_tx: s.answer_tx.take(),
            inbox: Some(Arc::clone(&s.inbox)),
            detached: s.detached,
        });
    }

    // A cancel drops the run future, here in the race or with the whole task,
    // so the last run never got to clean up after itself. It may have left tool
    // calls without results, and the provider rejects those. The session lives
    // on, so we close them before going again.
    s.history.close_dangling_tool_calls(UNAVAILABLE_RESULT);
    let history_len = s.history.len();
    let mut agent = Agent::new(
        s.params.clone(),
        AgentRunParams {
            history: &mut s.history,
            gauge: &mut s.gauge,
            event_tx: s.sub_event_tx.clone(),
            context: Arc::clone(&s.context),
        },
    )
    .with_reauth(s.reauth)
    .with_interrupt_source(Arc::clone(&s.inbox) as Arc<dyn maki_agent::InterruptSource>)
    .with_loaded_instructions(s.loaded_instructions.clone())
    .with_cancel(s.child_cancel.clone())
    .with_mcp(s.mcp.clone())
    .with_local_tools(Arc::clone(&s.local_tools));
    if let Some(rx) = &s.answer_rx {
        agent = agent.with_user_response_rx(Arc::clone(rx));
    }

    let input = AgentInput {
        message,
        mode: AgentMode::Build,
        images: Vec::new(),
        preamble: Vec::new(),
        earlier: Vec::new(),
        thinking: s.opts.thinking,
        fast: s.opts.fast,
        workflow: false,
        prompt: None,
        source: InputSource::Plugin,
    };
    let run = agent.run(input);
    let finished = match &watch {
        Some((caller_cancel, _)) => caller_cancel.race(run).await.ok(),
        None => Some(run.await),
    };
    drop(agent);
    drop(watch);
    if let Some(Ok(_)) = finished {
        s.dones.expect_one();
    }
    let result = finished.unwrap_or(Ok(DoneReason::Cancelled));
    // Only this call's messages count: older turns may hold stale preamble
    // text, and the agent loop's empty-response retry leaves a synthetic
    // "(empty)" assistant marker that must not pass for a real response.
    // Auto-compaction can shrink the history mid-run, so clamp the start:
    // after a rewrite the tail is this call's output either way.
    let turn = &s.history.as_slice()[history_len.min(s.history.len())..];
    // A subagent can be cancelled on its own, and its caller should hear about
    // that instead of taking a half-finished answer for a real one, so cancel
    // reads like an error here even though the run ended normally. A dropped
    // prompt never reached the model, so an empty answer would be a lie.
    let cut_short = match &result {
        Err(e) => Some(e.to_string()),
        Ok(DoneReason::Cancelled) => Some(CANCELLED_MSG.to_owned()),
        Ok(DoneReason::Dropped) => Some(PROMPT_DROPPED_ERR.to_owned()),
        Ok(
            DoneReason::EndTurn
            | DoneReason::MaxTokens
            | DoneReason::MaxTurns
            | DoneReason::Compact,
        ) => None,
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
    // The relay reaches `Done` only after every `TurnComplete`, so once it has,
    // all our `ToolLive::Usage` messages sit in the live channel before
    // `dispatch_racing_live` drains it for the last time.
    if s.dones.caught_up().await.is_err() {
        tracing::warn!(name = %s.name, "subagent event relay stopped, live usage may lag");
    }
    let usage = s.params.ledger.totals().usage;

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
    tbl.set("input_tokens", usage.total_input())?;
    tbl.set("output_tokens", usage.output)?;
    Ok((Some(tbl), None))
}

/// Close the session and flush its history back to the parent agent. Calling
/// it more than once is safe.
///
/// Close on every path, error paths included. Dropping the session instead
/// leaves the work to the Lua garbage collector, which may never run while
/// the VM sits idle, and the subagent's event relay stays alive until it does.
///
/// @param err string? Pass the failure reason when the run failed, so the session's UI item ends as errored even without a following tool result.
/// On a background session, `close` cancels a running prompt rather than
/// waiting for it.
///
/// @return
#[lua_fn]
async fn close(
    lua: Lua,
    this: mlua::UserDataRef<LuaSession>,
    err: Option<String>,
) -> LuaResult<()> {
    let inner = Arc::clone(&this.inner);
    // Dropping the entry cancels the prompt, so the lock below frees up soon.
    // Taking the lock first would wait on a prompt that may never end. Worst
    // case is a `SessionEnd` callback: the host cancels the session's
    // background work only after the callbacks return, so it would wait on
    // itself and hold up every hook behind it.
    if let Some(id) = &this.background_id {
        BackgroundSessions::of(&lua).remove(id);
    }
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
    /// a multi-step conversation.
    ///
    /// Always call `:close()` when you are done, on error paths too. The
    /// garbage collector is a fallback that may never run while the VM sits
    /// idle, so a session you only drop can stay open for the rest of the run.
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
    use futures_lite::future::poll_once;
    use maki_agent::{DoneReason, TurnCompleteEvent};
    use maki_providers::{Message, TokenUsage};
    use serde_json::json;

    use super::*;
    use crate::runtime::{TaskCell, TaskScope};

    const PLUGIN: &str = "monitor";

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

    #[test]
    fn ctx_less_session_needs_a_task_and_a_live_session() {
        let lua = Lua::new();
        let agent = create_agent_table(&lua, Arc::from(PLUGIN)).unwrap();
        lua.globals().set("agent", agent).unwrap();
        let session = MakiId::generate();
        lua.globals().set("sid", session.to_string()).unwrap();
        let open = || {
            lua.load("return select(2, agent.session({ session = sid }))")
                .eval_async::<Option<String>>()
        };
        let task = TaskScope::new(&lua, TaskCell::new(CancelToken::none(), None, None));

        assert_eq!(
            smol::block_on(open()).unwrap().as_deref(),
            Some(TASK_REQUIRED_ERR)
        );
        assert_eq!(
            smol::block_on(task.scope_future(open())).unwrap(),
            Some(not_live(session))
        );
    }

    /// A prompt cut short leaves its run's `Done` unread. The next prompt must
    /// read past it and keep waiting for its own, even when it gets dropped
    /// halfway and asked again.
    #[test]
    fn next_prompt_waits_past_a_leftover_done() {
        let (tx, rx) = flume::unbounded();
        let mut dones = RunDones::new(rx);
        dones.expect_one();
        tx.send(()).unwrap();
        dones.expect_one();
        assert!(smol::block_on(poll_once(dones.caught_up())).is_none());

        tx.send(()).unwrap();
        assert_eq!(smol::block_on(dones.caught_up()), Ok(()));
        assert!(tx.is_empty());
    }

    const RUN_ID: u64 = 7;
    const PARENT_ID: &str = "task-1";
    const IGNORED_ERROR: &str = "handled by the session caller";
    const DONE_USAGE: TokenUsage = tokens(150, 30);
    const SESSION_MODEL: &str = "test-model";
    const NESTED_MODEL: &str = "nested-model";

    const fn tokens(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cache_creation: 0,
            cache_read: 0,
            cost: None,
        }
    }

    fn turn(model: &str, usage: TokenUsage, cost: f64) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage,
            model: model.into(),
            cost: Some(cost),
            subsidised_list_cost: None,
            context_size: None,
            context_window: 0,
        }))
    }

    fn subagent_info() -> Arc<OnceLock<SubagentInfo>> {
        let info = Arc::new(OnceLock::new());
        info.set(SubagentInfo {
            parent_tool_use_id: PARENT_ID.into(),
            name: "research".into(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx: None,
            inbox: None,
            detached: false,
        })
        .unwrap();
        info
    }

    /// Dropping the guard is what ends the relay: a Lua tool context keeps a
    /// sender alive past the run, so disconnect never comes. The forwarded
    /// count also pins down that the close marker itself stays behind, since
    /// passing it on would end the *parent's* stream at the first subagent.
    ///
    /// The first turn plays one handed up by a nested subagent, so it has to
    /// be billed under that subagent's model, not the session's.
    #[test]
    fn relay_session_events_reports_and_bills_each_turn_once() {
        let session = MakiId::generate();
        let (guard, sub_events) = event_stream();
        let sub_tx = guard.sender(RUN_ID);
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let (done_tx, done_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();
        let (ui_tx, ui_rx) = flume::unbounded();

        for event in [
            turn(NESTED_MODEL, tokens(100, 20), 0.25),
            turn(SESSION_MODEL, tokens(50, 10), 0.5),
            AgentEvent::Error {
                message: IGNORED_ERROR.into(),
                auth: false,
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
            sub_tx.send(event).unwrap();
        }
        drop(guard);

        smol::block_on(relay_session_events(
            sub_events,
            EventSender::new(parent_raw_tx, RUN_ID),
            subagent_info(),
            done_tx,
            Some(live_tx),
            Some(TurnBilling {
                session,
                ui: ui_tx,
                registry: BackgroundSessions::default(),
                id: PARENT_ID.into(),
            }),
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
        assert_eq!(done_rx.try_recv(), Ok(()));

        let billed = ui_rx
            .drain()
            .map(|action| match action {
                UiAction::BackgroundUsage {
                    session: billed_session,
                    model,
                    usage,
                } if billed_session == session => (model, usage),
                _ => panic!("relay must only bill this chat session"),
            })
            .collect::<Vec<_>>();
        let expected_billed = [
            (
                NESTED_MODEL.to_owned(),
                tokens(100, 20).billed_with_subsidised_list_cost(Some(0.25), None),
            ),
            (
                SESSION_MODEL.to_owned(),
                tokens(50, 10).billed_with_subsidised_list_cost(Some(0.5), None),
            ),
        ];
        assert_eq!(billed, expected_billed);

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
