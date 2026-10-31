//! `maki.agent` exposes subagent primitives to Lua plugins. Policy (retries,
//! validation, concurrency) lives in the task plugin, not here.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::future::{Either, select};
use maki_agent::agent::{LoadedInstructions, tool_dispatch};
use maki_agent::cancel::{CancelMap, CancelSlot, CancelToken, CancelTrigger};
use maki_agent::permissions::{PermissionAnswer, TaggedAnswer};
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::registry::ToolRegistry;
use maki_agent::tools::schema::sanitize_tool_input_schema;
use maki_agent::tools::{
    BackgroundFor, CallOrigin, Deadline, DescriptionContext, LocalTool, LocalTools, RequestTools,
    ToolAudience, ToolContext, ToolFilter, ToolLive, ToolOutputMode,
};
use maki_agent::types::ALLOWED_BY;
use maki_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, AgentSeed, CallScope,
    DoneReason, EMPTY_RESPONSE_MARKER, EarlierInput, Envelope, EventSender, EventStreamGuard,
    History, ImageSource, InputSource, McpSession, RunContext, RunContextBuilder, RunLedger,
    RunTotals, SessionEvents, SessionMailbox, SessionStub, SubagentInbox, SubagentInfo, TaskHome,
    ToolDoneEvent, UNAVAILABLE_RESULT, event_stream,
};
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use maki_providers::model::ModelTier;
use maki_providers::model_registry;
use maki_providers::provider;
use maki_providers::{
    AgentError, ContentBlock, ContextGauge, Message, Model, RequestOptions, Role, ThinkingConfig,
    add_cost,
};
use maki_storage::id::MakiId;
use maki_storage::now_epoch;
use maki_storage::sessions::StoredThinking;
use mlua::{
    Either as LuaEither, Function, IntoLuaMulti, Lua, Result as LuaResult, Table, UserDataRef,
    Value as LuaValue,
};
use serde_json::Value as JsonValue;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::spawn_blocking;
use tracing::info;

use crate::api::session::{not_live, required_session};
use crate::api::tool::{audiences_to_lua, parse_audience};
use crate::api::ui::buf::BufHandle;
use crate::api::util::command::UiAction;
use crate::api::util::convert::{TableExt, json_to_lua, lua_to_json, lua_tool_result};
use crate::api::util::ctx::{AgentContext, LuaCtx};
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::background::{
    BackgroundInfo, BackgroundTask, BackgroundTasks, Fire, Kind, Lifetime, Stop, StopMark,
};
use crate::plugin_permissions::{Permission, PluginPermissions, require};
use crate::runtime::{CANCELLED_MSG, active_task_cancel, host_ui};

const SESSION_CLOSED_ERR: &str = "session closed";
/// Esc in the task's tab. Distinct from [`CANCELLED_MSG`], so a loop can tell
/// user intent from the end of its own task and not reopen.
pub(crate) const STOPPED_BY_USER_ERR: &str = "stopped by user";
const PROMPT_DROPPED_ERR: &str = "an `agent.user_message` layer dropped the prompt";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
const OUTPUT_MODE_ERR: &str = "output_mode must be 'full' or 'compact'";
const OPTS_REQUIRED_ERR: &str = "opts table is required";
const WORKING_STATUS: &str = "working";
const IDLE_STATUS: &str = "idle";
const TASK_SCOPE: &str = "task";
const SESSION_SCOPE: &str = "session";
const NO_REGISTRY_ERR: &str = "tool registry not available";
const MCP_REACH_ERR: &str =
    "mcp = true reaches whatever its servers do, so a session of your own takes every permission";
const TASK_REQUIRED_ERR: &str = "maki.agent.session({ session = id }) and its prompts must run \
     in a task: maki.async.spawn, an autocmd callback or a tool handler";

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
    model_registry::resolve_tier(Some(slug), effective)
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

/// A plugin denied `run` could hand `bash` to a model and let YOLO or an allow
/// rule run it, and in a session of the plugin's own nobody answers a prompt
/// at all. So a session pays for its reach the way a hook on the same tool
/// does: a declared capability costs itself, undeclared reach costs
/// everything. A name the registry does not know never dispatches, so it is
/// free.
fn delegated_reach(
    lua: &Lua,
    perms: &PluginPermissions,
    tools: &[JsonValue],
    mcp: bool,
) -> Result<(), String> {
    let holds_all = perms.holds_all_reach();
    if mcp && !holds_all {
        return Err(MCP_REACH_ERR.to_owned());
    }
    let registry = lua
        .app_data_ref::<Arc<ToolRegistry>>()
        .ok_or(NO_REGISTRY_ERR)?;
    for name in tools.iter().filter_map(|def| def["name"].as_str()) {
        let Some(registered) = registry.get(name) else {
            continue;
        };
        match registered.tool.reach().map(|reach| reach.permission) {
            Some(required) if !perms.is_allowed(required) => {
                return Err(format!(
                    "tool '{name}' needs '{required}', which this plugin does not hold"
                ));
            }
            None if !holds_all => {
                return Err(format!(
                    "tool '{name}' declares no capability, so a session of your own takes every permission"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Task scope remembers the opening task so its end can retire the entry.
/// Session scope forgets it: only the chat or the plugin ends that one.
fn parse_scope(scope: Option<String>, task: u64) -> LuaResult<Option<u64>> {
    match scope.as_deref() {
        None | Some(TASK_SCOPE) => Ok(Some(task)),
        Some(SESSION_SCOPE) => Ok(None),
        Some(other) => Err(mlua::Error::runtime(format!(
            "maki.agent.session: unknown scope {other:?}; expected {TASK_SCOPE:?} or {SESSION_SCOPE:?}"
        ))),
    }
}

/// Where a session's events go. A session opened on a tool ctx reports to the
/// tool's parent agent, until the call that opened it moves to the background:
/// from then on the parent's next run would drop its events as stale, so they
/// go to the chat it moved to. One a plugin opened by chat session id reports
/// to the chat's tab, which shows it as a task owned by that plugin. Headless
/// there is no tab, so `ui` is `None` and only the registry keeps the cost
/// for `maki.agent.list()`.
#[derive(Clone)]
enum Upstream {
    Parent {
        tx: EventSender,
        adoption: Option<Adoption>,
    },
    Chat {
        session: MakiId,
        plugin: Arc<str>,
        ui: Option<flume::Sender<UiAction>>,
        registry: BackgroundTasks,
        id: String,
    },
}

/// A ctx session under a model call: what it needs once that call moves.
#[derive(Clone)]
struct Adoption {
    call: Arc<CallScope>,
    ui: Option<flume::Sender<UiAction>>,
    registry: BackgroundTasks,
    /// The session's `ui_id`.
    id: String,
    /// The session's own plugin, which its registry entry names too.
    plugin: Arc<str>,
}

impl Adoption {
    /// A session that closed has nothing left to stop. A top-level `task`
    /// shares its id with the call, whose entry is not ours to retire.
    fn release(&self) {
        self.call.remove_session(&self.id);
        if self.call.home().is_some() && self.call.id() != self.id {
            self.registry.remove(&self.id);
        }
    }
}

/// Gives a session opened under a moved call an entry of its own, so its
/// adopted tab can be stopped by its id. A top-level `task` shares the call's
/// id, and the call's entry already covers it. An entry its chat refuses
/// fires the session as it drops, and its chat ending ends the session anyway.
///
/// A session that already ran has a chat in the UI, under the call's row.
/// The UI adopts it now rather than at its next event, or a quiet one would
/// look dead once the turn that started it ends.
pub(crate) fn adopt_session(
    registry: &BackgroundTasks,
    ui: Option<&flume::Sender<UiAction>>,
    home: &TaskHome,
    stub: SessionStub,
) {
    if stub.ui_id == home.id {
        return;
    }
    let entry = BackgroundTask {
        info: BackgroundInfo {
            id: stub.ui_id,
            plugin: Arc::clone(&stub.plugin),
            name: stub.name,
            kind: Kind::Agent,
            running: true,
            started_at: now_epoch(),
            cost: None,
        },
        session: home.chat,
        task: None,
        lifetime: Lifetime::Call,
        stop: StopMark::default(),
        cancel: Fire::Handle(stub.handle),
        mailbox: None,
    };
    if let Err(error) = registry.register(entry) {
        tracing::debug!(call = %home.id, %error, "session under a moved call outlived its chat");
        return;
    }
    if let (Some(ui), Some(info)) = (ui, stub.info.get()) {
        let _ = ui.try_send(UiAction::PluginTaskOpened {
            session: home.chat,
            plugin: stub.plugin,
            info: Box::new(info.clone()),
        });
    }
}

impl Upstream {
    fn send(&self, envelope: Envelope) {
        match self {
            Self::Parent { tx, .. } => {
                let _ = tx.send_envelope(envelope);
            }
            Self::Chat {
                session,
                plugin,
                ui: Some(ui),
                ..
            } => {
                let _ = ui.try_send(UiAction::PluginTaskEvent {
                    session: *session,
                    plugin: Arc::clone(plugin),
                    envelope: Box::new(envelope),
                });
            }
            Self::Chat { ui: None, .. } => {}
        }
    }

    /// The chat a ctx session's call moved to, once it has. `None` before the
    /// move and for a session that already reports to a chat.
    fn adopted(&self) -> Option<Self> {
        let Self::Parent {
            adoption: Some(adoption),
            ..
        } = self
        else {
            return None;
        };
        let home = adoption.call.home()?;
        Some(Self::Chat {
            session: home.chat,
            plugin: Arc::clone(&adoption.plugin),
            ui: adoption.ui.clone(),
            registry: adoption.registry.clone(),
            id: adoption.id.clone(),
        })
    }

    fn follow_move(&mut self) {
        if let Some(adopted) = self.adopted() {
            *self = adopted;
        }
    }

    /// A parent drops every run but its own as stale, so the event has to
    /// carry its run. The tab is not bound to a run at all.
    fn send_event(&self, event: AgentEvent, subagent: Option<SubagentInfo>) {
        let run_id = match self {
            Self::Parent { tx, .. } => tx.run_id(),
            Self::Chat { .. } => 0,
        };
        self.send(Envelope {
            event,
            subagent,
            run_id,
        });
    }

    /// A plugin task shows up in its chat as soon as it opens, so the user can
    /// find it in `/tasks` and stop it even before its first prompt.
    fn open_task(&self, info: SubagentInfo) {
        if let Self::Chat {
            session,
            plugin,
            ui: Some(ui),
            ..
        } = self
        {
            let _ = ui.try_send(UiAction::PluginTaskOpened {
                session: *session,
                plugin: Arc::clone(plugin),
                info: Box::new(info),
            });
        }
    }

    /// The transcript as of now. A plugin task writes it after every prompt,
    /// so a monitor that dies with the process still leaves what it did.
    fn send_history(&self, tool_use_id: &str, messages: Vec<Message>) {
        self.send_event(
            AgentEvent::SubagentHistory {
                tool_use_id: tool_use_id.to_owned(),
                messages,
            },
            None,
        );
    }
}

/// Forwards subagent events upstream, stamped with the subagent identity.
/// Usage goes live on the tool header while the run goes on (last turn's tokens
/// plus the run's summed cost). `Done` pings `done_tx`, which tells `prompt`
/// that every turn of the run has passed through. A parent agent hears neither
/// `Done` nor `Error`: it learns both from the tool result. A chat tab hears
/// both, since nothing else would tell its task the run ended, and hears
/// `StreamClosed` once the session is gone. The registry adds up every turn
/// it sees, compactions and nested subagents too, each under the model that
/// ran it. Nested relays hand their turns up here instead of billing them, and
/// `Done` only repeats the run's total, so no turn gets paid twice.
///
/// A ctx session switches to the chat between two events once its call moved,
/// so none is reordered. The parent's run is still live at the switch, so the
/// events it already got are not stale yet. Headless, nobody reads that chat,
/// so every ask sent there is answered as unattended instead of parking the
/// session for good.
async fn relay_session_events(
    mut sub_events: SessionEvents,
    mut upstream: Upstream,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    done_tx: flume::Sender<()>,
    live_sink: Option<flume::Sender<ToolLive>>,
) {
    let mut cost = None;
    while let Some(mut envelope) = sub_events.next().await {
        upstream.follow_move();
        if matches!(upstream, Upstream::Chat { ui: None, .. })
            && let Some(answer) = unattended_answer(&envelope.event)
        {
            if let Some(answers) = subagent_info.get().and_then(|i| i.answer_tx.as_ref()) {
                let _ = answers.send(answer);
            }
            continue;
        }
        let to_chat = matches!(upstream, Upstream::Chat { .. });
        match &envelope.event {
            AgentEvent::TurnComplete(turn) => {
                add_cost(&mut cost, turn.cost);
                if let Some(sink) = &live_sink {
                    let _ = sink.send(ToolLive::Usage(turn.usage.format_sum_cost(cost)));
                }
                // Updated before the tab hears of the turn, so whoever sees
                // the bill in the chat also sees it in `maki.agent.list()`.
                // A moved top-level `task` shares its id with the call, so
                // this bills the call's own entry.
                if let Upstream::Chat { registry, id, .. } = &upstream {
                    registry.update(id, |e| e.info.cost = cost);
                }
            }
            AgentEvent::Done { .. } => {
                let _ = done_tx.send(());
                if !to_chat {
                    continue;
                }
            }
            AgentEvent::Error { .. } if !to_chat => continue,
            AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolPending { .. }
            | AgentEvent::SubagentHistory { .. } => continue,
            _ => {}
        }
        envelope.subagent = subagent_info.get().cloned();
        upstream.send(envelope);
    }
    upstream.follow_move();
    if matches!(upstream, Upstream::Chat { .. }) {
        upstream.send_event(AgentEvent::StreamClosed, subagent_info.get().cloned());
    }
}

/// The answer to an ask that parks the run until the user replies, given in
/// place of a user nobody can reach. A permission is refused as unattended.
/// An auth retry rereads the credentials, and fails the run once the agent's
/// own retry budget is spent.
fn unattended_answer(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::PermissionRequest { id, .. } => {
            Some(TaggedAnswer::new(id, PermissionAnswer::Unattended).encode())
        }
        AgentEvent::AuthRequired => Some(String::new()),
        _ => None,
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
///     the parent tier so you cannot escalate. Resolved by
///     `maki.model.tier`, so a `/model` pick for the tier wins over the
///     session provider's curated default.
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
        .and_then(|t| t.opt::<String>("tier").ok().flatten());
    let spec_str = opts
        .as_ref()
        .and_then(|t| t.opt::<String>("spec").ok().flatten());

    let model = match spec_str {
        Some(ref spec) => try_pair!(Model::from_spec_with_policy(spec, &agent.model_policy)),
        None => try_pair!(resolve_model_from_ctx(agent, tier_str.as_deref())),
    };
    Ok((Some(model_to_lua_table(&lua, &model)?), None))
}

/// Build a system prompt from the parent's resolved prompt catalogue.
/// Plugin template and section replacements apply. Environment variables like
/// `{cwd}` are substituted automatically. Use this when you need a ready-made
/// prompt for a subagent session.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Required fields:
///   `prompt_id` (string) - one of `"research"`, `"general"`, `"system"`.
/// Optional fields:
///   `instructions` (string|boolean?) - text inserted at the template's
///     `{{instructions}}` marker.
///     `true` loads project instruction files.
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
    let (slots, instructions_config) = {
        let agent_ctx = try_pair!(dispatch_ctx(&ctx, "system_prompt"));
        (
            Arc::clone(&agent_ctx.prompt_slots),
            agent_ctx.config.instructions.clone(),
        )
    };
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
            spawn_blocking(move || {
                maki_agent::agent::load_instruction_text(&cwd, &instructions_config)
            })
            .await
            .map_err(mlua::Error::external)?
        }
        LuaValue::Boolean(false) | LuaValue::Nil => String::new(),
        LuaValue::String(s) => s.to_str()?.to_owned(),
        _ => return Err(mlua::Error::runtime("instructions must be bool or string")),
    };

    Ok((Some(slots.render(prompt_id, &vars, &instructions)), None))
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
///   `mcp` (boolean?) - describe tools as if MCP is reachable. Default: `false`.
///     Match what you pass to `maki.agent.session()`, or the descriptions
///     advertise MCP tools the session cannot call.
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

    let only: Option<Vec<String>> = opts.opt("only")?;
    let except: Option<Vec<String>> = opts.opt("except")?;
    let workflow: bool = opts.opt::<bool>("workflow")?.unwrap_or(false);
    let spec_str: Option<String> = opts.opt("spec")?;
    let mcp_enabled: bool = opts.opt::<bool>("mcp")?.unwrap_or(false);

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
    // A session opened from Lua is a subagent's, and a subagent never moves a
    // call itself, so it is not offered what only that needs.
    let defs = ToolRegistry::global().definitions_with_background(
        &vars,
        &ctx_desc,
        model.supports_tool_examples(),
        false,
    );

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
///     annotation event. Must not yield. A call a `permission.prompt` layer
///     allowed arrives as `"allowed by <plugin>"`. Without this callback that
///     mark goes on your own tool's row instead.
///   `on_usage` (function?) - called with a formatted cumulative token usage
///     string. Must not yield.
///   `on_movable` (function?) - called with `true` when the child registers
///     `ctx:on_background`, so the caller knows it may outlive the turn, and
///     `false` if it loses that. Must not yield.
///   `on_task` (function?) - called with the task id when the child runs as
///     a subagent, which `/tasks` then lists as a task of its own. Must not
///     yield.
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
    // Open even without callbacks: when a plugin answers the child's
    // permission prompt, the user still has to see it somewhere.
    let (live_tx, live_rx) = flume::unbounded();
    tctx.live_sink = Some(live_tx);
    let (mut on_buf, mut on_ann, mut on_usage, mut on_movable, mut on_task) =
        (None, None, None, None, None);
    if let Some(o) = opts {
        if let Some(mode) = o.opt::<String>("output_mode")? {
            let Some(mode) = ToolOutputMode::parse(&mode) else {
                return Ok(err_pair(OUTPUT_MODE_ERR));
            };
            tctx.output_mode = mode;
        }
        if let Some(secs) = o.opt::<u64>("timeout")? {
            tctx.deadline = Deadline::after(Duration::from_secs(secs));
        }
        on_buf = o.opt::<Function>("on_live_buf")?;
        on_ann = o.opt::<Function>("on_annotation")?;
        on_usage = o.opt::<Function>("on_usage")?;
        on_movable = o.opt::<Function>("on_movable")?;
        on_task = o.opt::<Function>("on_task")?;
    }
    let cbs = LiveCallbacks {
        tool: &name,
        caller: ToolContext::clone(agent),
        on_buf,
        on_ann,
        on_usage,
        on_movable,
        on_task,
    };
    drop(ctx);
    if let Err(e) = tctx.deadline.check() {
        return Ok(err_pair(e));
    }
    let done = dispatch_racing_live(&tctx, &name, &input_json, live_rx, &cbs).await;
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
/// handler. This form needs the `agent` plugin permission.
///
/// Nobody can answer its permission prompts, so a call that would prompt is
/// denied and the subagent is told why. Your rules, "allow for this session"
/// answers and YOLO still apply, so add allow rules for what it runs (see
/// [Permissions](/docs/permissions/)). It always runs in build mode. While the
/// chat is in plan mode its file-write tools are denied. Its thinking never
/// goes above the chat's current setting.
///
/// With or without a ctx, the plugin must hold what its tools reach. A prompt
/// is no gate under YOLO or an allow rule, and a background session has
/// nobody to prompt. A tool declaring a permission (`bash` with `run`, `read`
/// with `fs_read`) costs that one. A tool declaring none (`batch`,
/// `code_execution`), or `mcp = true` on a chat with MCP servers, costs every
/// permission except `agent`. `local_tools` cost nothing, they run as the
/// plugin. The bundled `task` hands over the whole chat, so it holds every
/// permission.
///
/// `scope` sets the lifetime. `"task"`, the default, ends the session with
/// the task that opened it: an autocmd callback when it returns, a tool
/// handler with its tool call, a `maki.async.spawn` task when it finishes.
/// `"session"` outlives the task. Both end at `:close()`, when the chat
/// session ends (`/new`, closing the tab) and when the plugin unloads
/// (`/reload`). Turn ends, Esc and rewinds leave it alone. Esc on a tool
/// call still cancels a prompt that the tool's handler is waiting on.
///
/// A polling monitor opens one per round inside a `maki.async.spawn` loop.
/// An event-driven one opens once with `scope = "session"`, keeps the handle
/// and prompts it from autocmd callbacks.
///
/// Opened by chat session id, it shows up in that chat as a task owned by
/// the plugin as soon as it opens: a transcript tab, a `plugin` field in
/// `/tasks` and `maki.task.list()`, `"idle"` before and between prompts, and
/// its spend in the chat's totals after each turn. Esc in its tab or Ctrl+S in
/// `/tasks` closes it, and the pending `:prompt()` returns `"stopped by user"`
/// rather than `"cancelled"`, so a loop can tell the user's stop from its own
/// task ending and stop rather than reopen. Permission prompts stay denied even
/// though the task has a tab: a prompt in an unfocused tab would stall a
/// monitor silently, a denial reaches the plugin as an error it can handle.
/// Setting `plugins.<name>.enabled = false` stops a plugin's monitors from
/// the next `/reload`. Under `maki -p` and ACP there is no live chat
/// session, so opening one fails with "session not live". The
/// [maki.agent](#maki-agent) overview has a full monitor.
///
/// @param ctx LuaCtx|table Agent context, or `opts` when called without one.
/// @param opts table? Required with a ctx. Optional fields:
///   `session` (string) - required without a ctx: id of the live chat session
///     that owns it. Returns `(nil, "session not live: <id>")` once that
///     session has ended.
///   `scope` (string?) - only without a ctx: `"task"` (default) or
///     `"session"`, see above.
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
///   `mcp` (boolean?) - give the session MCP tools. Their definitions are
///     injected each turn (deferred behind `tool_search`), so do not put MCP
///     definitions in `tools`. Default: `false`.
///   `thinking` (string|integer?) - thinking mode: `"off"`, `"adaptive"`, an
///     effort level (`"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`,
///     `"max"`), or a budget integer (token count). Inherits the parent
///     setting if omitted, and is capped at it otherwise.
///   `fast` (boolean?) - use fast mode. Inherits parent setting if omitted.
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
    #[ctx] perms: PluginPermissions,
    ctx: LuaEither<UserDataRef<LuaCtx>, Table>,
    opts: Option<Table>,
) -> LuaResult<Pair<mlua::AnyUserData>> {
    // The ctx borrow ends inside its arm, before any wait: a cancel hook
    // firing meanwhile needs `ctx:finish`, which takes it mutably.
    let (agent_ctx, opts, owner) = match ctx {
        LuaEither::Left(ctx) => (
            try_pair!(dispatch_ctx(&ctx, "session")).clone(),
            required_opts(opts)?,
            None,
        ),
        LuaEither::Right(opts) => {
            require(&lua, &perms, Permission::Agent)?;
            let Some((task, _)) = active_task_cancel(&lua) else {
                return Ok(err_pair(TASK_REQUIRED_ERR));
            };
            let task = parse_scope(opts.opt("scope")?, task)?;
            // A child of the task's token would leave a watcher waiting on it
            // until it fires. An autocmd's token never fires, so every open
            // would leak one. The entry keeps this trigger instead, and the
            // entry goes away when the task ends.
            let (cancel, token) = CancelToken::new();
            let (session, agent_ctx) = try_pair!(seed_ctx(&opts, token)?);
            (agent_ctx, opts, Some((session, task, cancel)))
        }
    };
    let model_spec: Option<String> = opts.opt("model_spec")?;
    let system: Option<String> = opts.opt("system")?;
    let tools_val: Option<LuaValue> = opts.opt("tools")?;
    let local_tools_tbl: Option<Table> = opts.opt("local_tools")?;
    let name: Option<String> = opts.opt("name")?;
    let thinking_val: Option<LuaValue> = opts.opt("thinking")?;
    let audience = match opts.opt::<String>("audience")? {
        Some(s) => {
            try_pair!(ToolAudience::parse_name(&s).ok_or_else(|| format!("unknown audience: {s}")))
        }
        None => DEFAULT_SESSION_AUDIENCE,
    };
    let fast: Option<bool> = opts.opt("fast")?;
    let mcp_enabled: bool = opts.opt::<bool>("mcp")?.unwrap_or(false);

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

    let mut tools_json = match tools_val {
        Some(val) => match lua_to_json(&lua, &val)? {
            JsonValue::Array(defs) => defs,
            _ => return Err(mlua::Error::runtime("tools must be an array")),
        },
        None => vec![],
    };
    let reaches_mcp = mcp_enabled && agent_ctx.mcp.is_some();
    try_pair!(delegated_reach(
        &lua,
        &perms.now(&lua),
        &tools_json,
        reaches_mcp
    ));

    let mut local_map: HashMap<String, LocalTool> = HashMap::new();
    if let Some(tbl) = local_tools_tbl {
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
            tools_json.push(serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": sanitized_schema,
            }));
            let audience = parse_audience(spec.opt::<Table>("audiences")?, ToolAudience::MODEL)?;
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
    let sub_event_tx = stream_guard.sender(agent_ctx.event_tx.run_id());
    let parent_tx = agent_ctx.event_tx.clone();
    // A subagent can only ask the user if its parent can. Under `maki -p` or
    // a background session nobody reads the subagent's chat, so a fresh
    // channel would leave every prompt waiting until cancel. Without one,
    // `enforce` denies the call right away.
    let (answer_tx, answer_rx) = agent_ctx
        .user_response_rx
        .is_some()
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
    let name = name.unwrap_or_default();
    let registry = BackgroundTasks::of(&lua);
    let ui = host_ui(&lua);
    let adoption = match (&owner, &agent_ctx.call) {
        (None, Some(call)) => Some(Adoption {
            call: Arc::clone(call),
            ui: ui.clone(),
            registry: registry.clone(),
            id: ui_id.clone(),
            plugin: Arc::clone(&plugin),
        }),
        _ => None,
    };
    let upstream = match &owner {
        Some((session, ..)) => Upstream::Chat {
            session: *session,
            plugin: Arc::clone(&plugin),
            ui,
            registry: registry.clone(),
            id: ui_id.clone(),
        },
        None => Upstream::Parent {
            tx: parent_tx,
            adoption: adoption.clone(),
        },
    };

    if owner.is_none()
        && let Some(sink) = &agent_ctx.live_sink
    {
        let _ = sink.send(ToolLive::Task(ui_id.clone()));
    }
    tokio::spawn(relay_session_events(
        sub_events,
        upstream.clone(),
        Arc::clone(&subagent_info),
        done_tx,
        agent_ctx.live_sink.clone(),
    ));

    // The session owns its trigger and the map only a handle: the run reaches
    // an unmoved session through the call's token anyway, and draining the
    // map at run end must leave a moved one running. A handle under an id
    // that is already cancelled fires at once.
    let (child_trigger, child_cancel) = agent_ctx.cancel.child();
    // Several sessions can share one `ui_id`, so keep the slot and retire
    // only ours on close instead of clearing the whole key.
    let cancel_slot = agent_ctx
        .subagent_cancels
        .insert_handle(ui_id.clone(), child_trigger.handle());
    if let Some(adoption) = &adoption {
        let stub = SessionStub {
            ui_id: ui_id.clone(),
            plugin: Arc::clone(&plugin),
            name: name.clone(),
            handle: child_trigger.handle(),
            info: Arc::clone(&subagent_info),
        };
        if let Some(stub) = adoption.call.add_session(stub)
            && let Some(home) = adoption.call.home()
        {
            adopt_session(&adoption.registry, adoption.ui.as_ref(), home, stub);
        }
    }

    let cancellation = Arc::new(SessionCancellation {
        parent_cancels: Arc::clone(&agent_ctx.subagent_cancels),
        ui_id,
        slot: cancel_slot,
        trigger: child_trigger,
        registry: registry.clone(),
        background: owner.is_some(),
        mailbox: SessionMailbox::register(MakiId::generate()),
        adoption,
        stop: StopMark::default(),
    });
    cancellation.end_mailbox_on_cancel(child_cancel.clone());
    let background = match owner {
        Some((session, task, cancel)) => {
            try_pair!(registry.register(BackgroundTask {
                info: BackgroundInfo {
                    id: cancellation.ui_id.clone(),
                    plugin,
                    name: name.clone(),
                    kind: Kind::Agent,
                    running: false,
                    started_at: now_epoch(),
                    cost: None,
                },
                session,
                task,
                lifetime: Lifetime::Plugin,
                stop: cancellation.stop.clone(),
                cancel: Fire::Owned(cancel),
                mailbox: None,
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
    let tools = RequestTools::assembled(JsonValue::Array(tools_json), &agent_ctx.config, &model);
    let system = system.unwrap_or_default();
    let context: RunContextBuilder =
        Arc::new(move |_, _| RunContext::fixed(system.clone(), tools.clone()));

    let inbox = Arc::new(SubagentInbox::default());
    let state = SessionState {
        params: AgentParams {
            provider,
            model,
            config: agent_ctx.config.clone(),
            tool_output_lines: maki_config::ToolOutputLines::default(),
            permissions: Arc::clone(&agent_ctx.permissions),
            session_id: agent_ctx.session_id.clone(),
            // Once the call moves nobody watches its writes, so they honour
            // plan mode from then on, as other background work does.
            background_for: match (&agent_ctx.background_for, &agent_ctx.call) {
                (BackgroundFor::None, Some(call)) => BackgroundFor::Call(Arc::clone(call)),
                (inherited, _) => inherited.clone(),
            },
            task_id: Some(Arc::from(cancellation.ui_id.as_str())),
            mailbox: Some(cancellation.mailbox.clone()),
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
        instruction_files: Arc::clone(&agent_ctx.instruction_files),
        sub_event_tx,
        stream_guard: Some(stream_guard),
        child_cancel,
        answer_rx,
        answer_tx,
        reauth: agent_ctx.reauth,
        cancellation: Arc::clone(&cancellation),
        upstream,
        subagent_info,
        local_tools: Arc::new(local_map),
        name,
        dones: RunDones::new(done_rx),
        start: Instant::now(),
        closed: false,
        background,
    };

    if state.background.is_some() {
        let info = SubagentInfo {
            parent_tool_use_id: state.cancellation.ui_id.clone(),
            name: state.name.clone(),
            prompt: None,
            model: Some(state.params.model.spec()),
            opts: Some(state.opts),
            answer_tx: None,
            inbox: Some(Arc::clone(&inbox)),
            parent_call: None,
        };
        state.upstream.open_task(info.clone());
        let _ = state.subagent_info.set(info);
    }

    let sess = lua.create_userdata(LuaSession {
        inner: Arc::new(AsyncMutex::new(state)),
        cancellation,
        inbox,
    })?;
    Ok((Some(sess), None))
}

/// List the sessions plugins opened on a chat session without a `ctx`, see
/// [maki.agent.session](#maki-agent-session).
///
/// Each entry has these fields:
///   `id` (string) Session id.
///   `session` (string) Chat session it belongs to.
///   `plugin` (string) Plugin that opened it.
///   `name` (string) Name given at open.
///   `status` (string) `"working"` during a `:prompt()`, otherwise `"idle"`.
///   `started_at` (integer) Unix seconds.
///   `cost` (number?) Dollars billed so far, updated after each turn. nil until
///     a priced turn ends.
///
/// @param opts table Required fields:
///   `session` (string) - id of the chat session.
/// @return (table?, string?) Array of entries, or `(nil, err)` on failure.
/// @example
/// for _, s in ipairs(maki.agent.list({ session = id })) do
///   print(s.plugin, s.name, s.status, os.time() - s.started_at)
/// end
#[lua_fn]
fn list(lua: &Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session = try_pair!(required_session(Some(&opts))?);
    let out = lua.create_table()?;
    for entry in BackgroundTasks::of(lua).list_agents(session) {
        let status = if entry.running {
            WORKING_STATUS
        } else {
            IDLE_STATUS
        };
        let t = lua.create_table()?;
        t.set("status", status)?;
        t.set("id", entry.id)?;
        t.set("session", session.to_string())?;
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
    "maki.agent" => pub(crate) fn create_agent_table(plugin: Arc<str>, perms: PluginPermissions), DOCS [
        resolve_model, system_prompt, tools, callable_tools, call_tool,
        session(plugin, perms), list,
    ]
}

/// Must use `call_async`, not `call`: callbacks that yield (highlight,
/// markdown) hit the C-call boundary otherwise.
struct LiveCallbacks<'a> {
    tool: &'a str,
    /// The tool that called `call_tool`. A child's "allowed by" mark goes on
    /// its row when there is no `on_annotation` to show it.
    caller: ToolContext,
    on_buf: Option<Function>,
    on_ann: Option<Function>,
    on_usage: Option<Function>,
    on_movable: Option<Function>,
    on_task: Option<Function>,
}

impl LiveCallbacks<'_> {
    async fn deliver(&self, ev: ToolLive) {
        let res = match ev {
            ToolLive::Buf(buf) => call_opt(&self.on_buf, BufHandle::foreign(buf)).await,
            ToolLive::Annotation(ann) => call_opt(&self.on_ann, ann).await,
            ToolLive::Usage(usage) => call_opt(&self.on_usage, usage).await,
            ToolLive::AllowedBy(plugin) if self.on_ann.is_some() => {
                call_opt(&self.on_ann, format!("{ALLOWED_BY} {plugin}")).await
            }
            ToolLive::AllowedBy(plugin) => {
                tool_dispatch::mark_allowed_by(&self.caller, plugin);
                None
            }
            ToolLive::Movable(movable) => call_opt(&self.on_movable, movable).await,
            ToolLive::Task(id) => call_opt(&self.on_task, id).await,
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
    rx: flume::Receiver<ToolLive>,
    cbs: &LiveCallbacks<'_>,
) -> ToolDoneEvent {
    let mut run = pin!(tool_dispatch::run(
        String::new(),
        name,
        input,
        tctx,
        CallOrigin::Nested
    ));
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
    instruction_files: Arc<Vec<String>>,
    sub_event_tx: EventSender,
    /// Dropped on close, which ends the relay task. Tool contexts keep
    /// [`EventSender`] clones alive past the run, so the relay cannot key off
    /// sender disconnect.
    stream_guard: Option<EventStreamGuard>,
    child_cancel: maki_agent::cancel::CancelToken,
    answer_rx: Option<Arc<AsyncMutex<flume::Receiver<String>>>>,
    answer_tx: Option<flume::Sender<String>>,
    reauth: bool,
    cancellation: Arc<SessionCancellation>,
    upstream: Upstream,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
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
    registry: BackgroundTasks,
    session: MakiId,
    /// What the plugin asked for at open. We keep it because every prompt caps
    /// it again against the chat as it is at that moment.
    thinking: Option<ThinkingConfig>,
    fast: Option<bool>,
}

impl Background {
    fn start_prompt(
        &self,
        id: &str,
        event_tx: EventSender,
        ledger: Arc<RunLedger>,
    ) -> PromptSettle {
        self.registry.update(id, |e| e.info.running = true);
        PromptSettle {
            registry: self.registry.clone(),
            id: id.to_owned(),
            event_tx,
            ledger,
            outcome: Some(cancelled_run()),
        }
    }
}

/// What a run ends on in its tab until it ends on its own.
fn cancelled_run() -> AgentEvent {
    AgentEvent::Error {
        message: CANCELLED_MSG.to_owned(),
        auth: false,
        totals: RunTotals::default(),
    }
}

/// Wraps up a background prompt when it drops. A cancelled task drops the
/// prompt future in the middle of the run, so the code after its `await` never
/// runs. Without this the entry would say "running" forever. It can drop while
/// Lua is tearing down the cancelled coroutine, so it only touches Rust state
/// and never calls into Lua.
///
/// The run only sends `Done` when it comes back `Ok`. A run that failed or got
/// dropped sends nothing, and its tab would keep its tools spinning until the
/// session closes. So we tell the tab `outcome` ourselves. It goes through the
/// relay, so it lands after the run's own events. It is never a `Done`, because
/// the relay counts every `Done` against the ones `prompt` waits for. It bills
/// what `ledger` holds when it drops, as a `Done` would, so a run cancelled
/// mid-turn still reports its spend.
struct PromptSettle {
    registry: BackgroundTasks,
    id: String,
    event_tx: EventSender,
    ledger: Arc<RunLedger>,
    outcome: Option<AgentEvent>,
}

impl Drop for PromptSettle {
    fn drop(&mut self) {
        self.registry.update(&self.id, |e| e.info.running = false);
        if let Some(mut outcome) = self.outcome.take() {
            if let AgentEvent::Error { totals, .. } = &mut outcome {
                *totals = self.ledger.totals();
            }
            let _ = self.event_tx.send(outcome);
        }
    }
}

impl SessionState {
    /// Why this session takes no more messages, if it does not.
    fn refusal(&self) -> Option<&'static str> {
        if self.closed {
            return Some(SESSION_CLOSED_ERR);
        }
        self.cancellation.refusal()
    }

    fn stopped_by_user(&self) -> bool {
        self.cancellation.stopped_by_user()
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.stream_guard.take();
        self.cancellation.cancel();
        let usage = self.params.ledger.totals().usage;
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let adopted = self.upstream.adopted();
        adopted
            .as_ref()
            .unwrap_or(&self.upstream)
            .send_history(&self.cancellation.ui_id, messages);
        info!(
            name = %self.name,
            duration_ms = self.start.elapsed().as_millis() as u64,
            input_tokens = usage.total_input(),
            output_tokens = usage.output,
            "subagent session closed",
        );
    }

    fn agent(&mut self, inbox: &Arc<SubagentInbox>) -> Agent<'_> {
        let agent = Agent::new(
            self.params.clone(),
            AgentRunParams {
                history: &mut self.history,
                gauge: &mut self.gauge,
                event_tx: self.sub_event_tx.clone(),
                context: Arc::clone(&self.context),
            },
        )
        .with_reauth(self.reauth)
        .with_interrupt_source(Arc::clone(inbox) as Arc<dyn maki_agent::InterruptSource>)
        .with_loaded_instructions(
            self.loaded_instructions.clone(),
            Arc::clone(&self.instruction_files),
        )
        .with_cancel(self.child_cancel.clone())
        .with_mcp(self.mcp.clone())
        .with_local_tools(Arc::clone(&self.local_tools));
        match &self.answer_rx {
            Some(rx) => agent.with_user_response_rx(Arc::clone(rx)),
            None => agent,
        }
    }
}

/// Auto-compaction can shrink the history mid-run, so the start is clamped.
/// After a rewrite the tail is this call's output either way.
fn messages_since(history: &History, start: usize) -> &[Message] {
    &history.as_slice()[start.min(history.len())..]
}

/// Why a run left its caller without an answer, or `None` if it answered. A
/// subagent can be cancelled on its own, and its caller should hear about that
/// instead of taking a half-finished answer for a real one, so cancel reads
/// like an error here even though the run ended normally. A dropped prompt
/// never reached the model, so an empty answer would be a lie.
fn cut_short_err(result: &Result<DoneReason, AgentError>, stopped_by_user: bool) -> Option<String> {
    match result {
        Err(e) => Some(e.to_string()),
        Ok(DoneReason::Cancelled) if stopped_by_user => Some(STOPPED_BY_USER_ERR.to_owned()),
        Ok(DoneReason::Cancelled) => Some(CANCELLED_MSG.to_owned()),
        Ok(DoneReason::Dropped) => Some(PROMPT_DROPPED_ERR.to_owned()),
        Ok(
            DoneReason::EndTurn
            | DoneReason::MaxTokens
            | DoneReason::MaxTurns
            | DoneReason::Compact,
        ) => None,
    }
}

/// The notices that wake the session's next turn, or `None` once none of its
/// moved calls is still out and nothing they reported is left unread. A sent
/// message wakes it with no notices while a moved call is still out. `Err`
/// when the session or the caller is cancelled first.
async fn next_wake(
    mailbox: &SessionMailbox,
    inbox: &SubagentInbox,
    session: &CancelToken,
    caller: Option<&CancelToken>,
) -> Result<Option<Vec<Message>>, String> {
    let sent = async {
        inbox.wait_nonempty().await;
        Some(Vec::new())
    };
    let wake = session.race(async {
        tokio::select! {
            biased;
            wake = mailbox.next_wake_before_answer() => wake,
            wake = sent => wake,
        }
    });
    match caller {
        Some(caller) => caller.race(wake).await?,
        None => wake.await,
    }
}

struct LuaSession {
    inner: Arc<AsyncMutex<SessionState>>,
    cancellation: Arc<SessionCancellation>,
    /// Shared with the host through [`SubagentInfo`], so a user watching this
    /// session can queue messages that its next turn boundary picks up. Kept
    /// outside the lock, which a running prompt holds for the whole run, so
    /// `send` can still reach it.
    inbox: Arc<SubagentInbox>,
}

struct SessionCancellation {
    parent_cancels: Arc<CancelMap<String>>,
    ui_id: String,
    slot: CancelSlot,
    /// Fires the session's token. `parent_cancels` holds only its handle.
    trigger: CancelTrigger,
    registry: BackgroundTasks,
    /// Opened by chat session id, so `registry` holds an entry under `ui_id`.
    background: bool,
    /// Where calls moved off this session's runs report, for the session's
    /// whole life, so a later prompt still hears of a call an earlier one moved.
    mailbox: SessionMailbox,
    adoption: Option<Adoption>,
    /// Esc in the task's tab, see [`BackgroundTasks::stop`].
    stop: StopMark,
}

impl SessionCancellation {
    /// See [`SubagentInfo::parent_call`].
    fn parent_call(&self) -> Option<String> {
        let call = self.adoption.as_ref()?.call.id();
        (call != self.ui_id).then(|| call.to_owned())
    }

    /// Why this session takes no more messages, closing aside. It needs no
    /// session lock, so `send` gets the same answer mid-prompt as between.
    fn refusal(&self) -> Option<&'static str> {
        if self.stopped_by_user() {
            return Some(STOPPED_BY_USER_ERR);
        }
        // Stopping the call that opened the session fires this token right
        // away, but the prompt it cut short takes a while to unwind. Its chat
        // session or its plugin may also have ended while the plugin kept the
        // handle.
        (self.trigger.is_cancelled() || (self.background && !self.registry.contains(&self.ui_id)))
            .then_some(CANCELLED_MSG)
    }

    fn stopped_by_user(&self) -> bool {
        self.stop.get() != Stop::None
    }

    fn cancel(&self) {
        self.parent_cancels.retire(&self.ui_id, self.slot);
        self.trigger.handle().fire();
        if self.background {
            self.registry.remove(&self.ui_id);
        }
        self.end_mailbox();
        if let Some(adoption) = &self.adoption {
            adoption.release();
        }
    }

    /// No run of this session reads its mailbox again, so the calls it moved
    /// stop instead of reporting to nobody.
    fn end_mailbox(&self) {
        self.registry.end_mailbox(&self.mailbox);
    }

    /// The session's token can fire with no prompt left to notice: the task
    /// that opened it ends between prompts, or the prompt waiting on it was
    /// dropped. Dropping the session fires it too, so this never outlives it.
    fn end_mailbox_on_cancel(&self, token: CancelToken) {
        let registry = self.registry.clone();
        let mailbox = self.mailbox.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            registry.end_mailbox(&mailbox);
        });
    }
}

impl Drop for SessionCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Drop for LuaSession {
    fn drop(&mut self) {
        self.cancellation.cancel();
        match self.inner.try_lock() {
            Ok(mut s) => s.close(),
            // Prompt still in flight: close asynchronously so history
            // and cancel entry are never silently leaked.
            Err(_) => {
                let inner = Arc::clone(&self.inner);
                tokio::spawn(async move { inner.lock().await.close() });
            }
        }
    }
}

/// What was sent while no run read the inbox, shown as consumed and handed
/// to the next run ahead of its own message, so its first reply reads it.
fn take_waiting(inbox: &SubagentInbox, event_tx: &EventSender) -> Vec<EarlierInput> {
    inbox
        .drain()
        .into_iter()
        .map(|input| {
            announce_consumed(event_tx, &input.message, &input.images);
            EarlierInput::from(input)
        })
        .collect()
}

fn announce_consumed(event_tx: &EventSender, text: &str, images: &[ImageSource]) {
    let _ = event_tx.send(AgentEvent::QueueItemConsumed {
        text: text.to_owned(),
        images: images.to_vec(),
    });
}

fn plugin_input(message: String, opts: RequestOptions) -> AgentInput {
    AgentInput {
        message,
        mode: AgentMode::Build,
        images: Vec::new(),
        preamble: Vec::new(),
        earlier: Vec::new(),
        thinking: opts.thinking,
        fast: opts.fast,
        workflow: false,
        prompt: None,
        source: InputSource::Plugin,
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
/// A tool call the user moves to the background (Ctrl+B in the session's tab)
/// keeps the prompt waiting. When the call finishes, the subagent gets its
/// result and runs another turn, and `text` is that last turn's. Closing the
/// session, or the end of the task that opened it, stops such calls.
///
/// @param message string User message to send.
/// @return (table?, string?) Result table on success, or `(nil, err)` on
/// failure. A run cut short after streaming some text hands you both: the
/// error and a `{ text = <what it streamed> }` table. `"cancelled"` means the
/// task that opened or prompted the session ended. `"stopped by user"` means
/// Esc in its tab closed a session opened by chat session id, and every
/// later prompt answers the same.
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
    let caller_cancel = match (this.cancellation.background, active_task_cancel(&lua)) {
        (false, _) => None,
        (true, Some((_, cancel))) => Some(cancel),
        (true, None) => return Ok(err_pair(TASK_REQUIRED_ERR)),
    };
    let inner = Arc::clone(&this.inner);
    let inbox = Arc::clone(&this.inbox);
    drop(this);
    let mut guard = match &caller_cancel {
        Some(cancel) => match cancel.race(inner.lock()).await {
            Ok(guard) => guard,
            Err(_) => return Ok(err_pair(CANCELLED_MSG)),
        },
        None => inner.lock().await,
    };
    let s = &mut *guard;
    if let Some(refusal) = s.refusal() {
        return Ok(err_pair(refusal));
    }
    // The user can lower the effort while a monitor sits idle, so we ask the
    // seed again on every prompt. Plan mode is left out on purpose. Each write
    // checks it instead, see `AgentSeed::background_write_block`.
    let mut watch = match (&s.background, caller_cancel) {
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
            Some((
                caller_cancel,
                background.start_prompt(
                    &s.cancellation.ui_id,
                    s.sub_event_tx.clone(),
                    Arc::clone(&s.params.ledger),
                ),
            ))
        }
        _ => None,
    };
    if s.background.is_none() && s.subagent_info.get().is_none() {
        let _ = s.subagent_info.set(SubagentInfo {
            parent_tool_use_id: s.cancellation.ui_id.clone(),
            name: s.name.clone(),
            prompt: Some(message.clone()),
            model: Some(s.params.model.spec()),
            opts: Some(s.opts),
            answer_tx: s.answer_tx.take(),
            inbox: Some(Arc::clone(&inbox)),
            parent_call: s.cancellation.parent_call(),
        });
    }
    // The run only reads the inbox after a turn, so a message sent while we
    // sat idle would cost a reply that ignores it plus one more turn. It rides
    // in front of the prompt instead.
    let earlier = take_waiting(&inbox, &s.sub_event_tx);
    // The tab opened with the session, so every prompt shows up the way a
    // queued message does.
    if s.background.is_some() {
        announce_consumed(&s.sub_event_tx, &message, &[]);
    }

    // A cancel drops the run future, here in the race or with the whole task,
    // so the last run never got to clean up after itself. It may have left tool
    // calls without results, and the provider rejects those. The session lives
    // on, so we close them before going again.
    s.history.close_dangling_tool_calls(UNAVAILABLE_RESULT);
    let history_len = s.history.len();
    let mut input = AgentInput {
        earlier,
        ..plugin_input(message, s.opts)
    };
    // A call moved off a run reports to the session's mailbox, and the caller
    // is owed the answer that comes after it. So the prompt waits for every
    // moved call, and each wake runs one more turn on the same history.
    let mut last_run_start;
    let cut_short = loop {
        last_run_start = s.history.len();
        let mut agent = s.agent(&inbox);
        let run = agent.run(input);
        let finished = match &watch {
            Some((caller_cancel, _)) => caller_cancel.race(run).await.ok(),
            None => Some(run.await),
        };
        drop(agent);
        // A run that lost the race keeps the `cancelled` it started with.
        if let (Some((_, settle)), Some(result)) = (&mut watch, &finished) {
            settle.outcome = result
                .as_ref()
                .err()
                .map(|e| AgentEvent::error(e, RunTotals::default()));
        }
        if let Some(Ok(_)) = finished {
            s.dones.expect_one();
        }
        // Before the run is judged, so a prompt Esc or an error cut short still
        // leaves what it streamed in the tab's transcript. A monitor nobody
        // closes would otherwise keep it in memory until the plugin unloads.
        if s.background.is_some() {
            s.upstream
                .send_history(&s.cancellation.ui_id, s.history.as_slice().to_vec());
        }
        let result = finished.unwrap_or(Ok(DoneReason::Cancelled));
        if let Some(err) = cut_short_err(&result, s.stopped_by_user()) {
            break Some(err);
        }
        let caller_cancel = watch.as_ref().map(|(caller_cancel, _)| caller_cancel);
        match next_wake(
            &s.cancellation.mailbox,
            &inbox,
            &s.child_cancel,
            caller_cancel,
        )
        .await
        {
            Ok(Some(notices)) => {
                if let Some((_, settle)) = &mut watch {
                    settle.outcome = Some(cancelled_run());
                }
                input = AgentInput {
                    preamble: notices,
                    earlier: take_waiting(&inbox, &s.sub_event_tx),
                    ..plugin_input(String::new(), s.opts)
                };
            }
            Ok(None) => break None,
            Err(_) => break cut_short_err(&Ok(DoneReason::Cancelled), s.stopped_by_user()),
        }
    };
    drop(watch);
    // Its own token fired, so no run of this session will read what its moved
    // calls report. A caller's cancel alone leaves them for the next prompt.
    if s.child_cancel.is_cancelled() {
        s.cancellation.end_mailbox();
    }
    if let Some(err) = cut_short {
        // Only this call's messages count: older turns may hold stale preamble
        // text, and the agent loop's empty-response retry leaves a synthetic
        // "(empty)" assistant marker that must not pass for a real response.
        let partial = messages_since(&s.history, history_len)
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

    let text = messages_since(&s.history, last_run_start)
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

/// Hand the subagent a message without waiting, the way a user typing in its
/// tab does. A running `:prompt()` picks it up at its next turn boundary, once
/// the current reply and its tool calls finish, and the model reads it as a
/// user interrupt. The tab draws it when it lands. Messages sent in a burst
/// land together.
///
/// With no prompt running, or one already past its last turn, the message
/// waits. The next `:prompt()` sends it in the same request as its own
/// message, just before it, so the first reply already reads it.
///
/// @param message string Message to deliver.
/// @return (boolean?, string?) `true`, or `(nil, err)` when the session takes
/// no more messages. The errors are the ones `:prompt()` refuses with. `true`
/// means queued, not delivered: a message still waiting when the session
/// closes is dropped.
/// @example
/// -- From another task, while `sess:prompt()` is still running.
/// local ok, err = sess:send("New finding: the bug is in parser.rs, focus there.")
/// if not ok then maki.log.warn(err) end
#[lua_fn]
fn send(_lua: &Lua, this: &LuaSession, message: String) -> LuaResult<Pair<bool>> {
    // A held lock means a prompt is running, and a task stopped mid-prompt
    // holds it until the prompt unwinds. Without the fallback its message
    // would queue for a run that never reads it.
    let refusal = match this.inner.try_lock() {
        Ok(s) => s.refusal(),
        Err(_) => this.cancellation.refusal(),
    };
    if let Some(refusal) = refusal {
        return Ok(err_pair(refusal));
    }
    // A sent message never picks the run's settings, so these are inert.
    this.inbox
        .push(plugin_input(message, RequestOptions::default()));
    Ok((Some(true), None))
}

/// Close the session and flush its history back to the parent agent. Calling
/// it more than once is safe.
///
/// Close on every path, error paths included. Dropping the session instead
/// leaves the work to the Lua garbage collector, which may never run while
/// the VM sits idle, and the subagent's event relay stays alive until it does.
///
/// Close cancels a running prompt before waiting to flush its history. That
/// prompt returns a cancelled error and any partial text.
///
/// @return
#[lua_fn]
async fn close(_lua: Lua, this: mlua::UserDataRef<LuaSession>) -> LuaResult<()> {
    this.cancellation.cancel();
    let inner = Arc::clone(&this.inner);
    drop(this);
    let mut s = inner.lock().await;
    s.close();
    Ok(())
}

/// The task id of this session. A session opened by chat session id goes by
/// this id in `maki.task.list()` and the `"TaskStopped"` event, so a plugin
/// can tell which of those are its own.
///
/// @return (string) Session task id.
#[lua_fn]
fn id(_lua: &Lua, this: &LuaSession) -> LuaResult<String> {
    Ok(this.cancellation.ui_id.clone())
}

lua_class! {
    /// A subagent session with its own conversation history.
    ///
    /// Create one with `maki.agent.session()`, then send messages with
    /// `:prompt()`. The session remembers previous turns, so you can have
    /// a multi-step conversation. `:send()` nudges a prompt that is still
    /// running.
    ///
    /// Always call `:close()` when you are done, on error paths too. The
    /// garbage collector is a fallback that may never run while the VM sits
    /// idle, so a session you only drop can stay open for the rest of the run.
    "maki.agent.Session" => LuaSession, SESSION_DOCS [prompt, send, close, id]
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
    use flume::Sender;
    use futures::FutureExt;
    use maki_agent::prompt::{PromptId, ResolvedPrompts};
    use maki_agent::tools::test_support::{stub_ctx_with, stub_seed};
    use maki_agent::{BackgroundNotice, DoneReason, SessionMailbox, TaskEnd, TurnCompleteEvent};
    use maki_config::ToolKey;
    use maki_providers::model::ThinkingSupport;
    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        AgentError, ContentBlock, Message, ModelInfo, ProviderEvent, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use maki_storage::id::SessionRef;
    use maki_storage::sessions::Effort;
    use mlua::AnyUserData;
    use serde_json::json;
    use test_case::test_case;
    use tokio::time::timeout;

    use super::*;
    use crate::runtime::{TaskCell, TaskScope};

    const PLUGIN: &str = "monitor";
    const SESSION_PROMPT: &str = "work until closed";
    const PARTIAL_TEXT: &str = "partial response";
    const SESSION_TIMEOUT: Duration = Duration::from_secs(5);
    const CLOSE_SESSION: &str = "function(sess) sess:close() end";
    const THINKING_BUDGET: u32 = 2048;
    const CALL_ID: &str = "toolu_call";
    const CALL_TOOL: &str = "task";
    const CALL_PLUGIN: &str = "batch";
    const REQUEST_ID: &str = "toolu_ask";
    const ASKING_TOOL: &str = "bash";
    const FIRST_REPLY: &str = "The build runs in the background.";
    const WOKEN_REPLY: &str = "The build passed.";
    const MOVED_CALL_ID: &str = "toolu_moved";
    const NOTICE_TEXT: &str = "Background bash toolu_moved (cargo build) finished:\n\nok";
    const SENT_MESSAGE: &str = "Wrap up now.";

    #[test_case(ThinkingConfig::Effort(Effort::Low), json!({"adaptive": {"enable_thinking": true}}), ThinkingConfig::Adaptive, ThinkingConfig::Effort(Effort::Low) ; "toggle_keeps_ancestor_effort_ceiling")]
    #[test_case(ThinkingConfig::Adaptive, json!({"adaptive": {"enable_thinking": true}}), ThinkingConfig::Adaptive, ThinkingConfig::Effort(Effort::Max) ; "explicit_adaptive_stays_unbounded")]
    #[test_case(ThinkingConfig::Budget(THINKING_BUDGET), json!({"adaptive": {"enable_thinking": true}}), ThinkingConfig::Budget(THINKING_BUDGET), ThinkingConfig::Budget(THINKING_BUDGET) ; "toggle_keeps_ancestor_numeric_ceiling")]
    #[test_case(ThinkingConfig::Budget(THINKING_BUDGET), json!({"max": {"reasoning_effort": "xhigh"}}), ThinkingConfig::Effort(Effort::Max), ThinkingConfig::Budget(THINKING_BUDGET) ; "native_budget_display_keeps_ancestor_numeric_ceiling")]
    #[test_case(ThinkingConfig::Effort(Effort::Low), json!({"max": {"reasoning_effort": "xhigh"}}), ThinkingConfig::Effort(Effort::Max), ThinkingConfig::Effort(Effort::Low) ; "snapping_up_does_not_raise_ancestor_ceiling")]
    #[test_case(ThinkingConfig::Effort(Effort::High), json!({"low": {"reasoning_effort": "low"}}), ThinkingConfig::Effort(Effort::Low), ThinkingConfig::Effort(Effort::High) ; "snapping_down_keeps_ancestor_intent")]
    fn native_thinking_display_preserves_nested_delegation_ceiling(
        parent: ThinkingConfig,
        fields: JsonValue,
        shown: ThinkingConfig,
        expected_grandchild: ThinkingConfig,
    ) {
        let mut model = Model::from_spec("llama-cpp/default").unwrap();
        model.thinking_override = Some(ThinkingSupport::Yes);
        let mut intermediate = model.clone();
        intermediate.thinking_fields = Some(Box::new(serde_json::from_value(fields).unwrap()));
        let root = RequestOptions {
            thinking: parent,
            fast: false,
        };

        for requested in [None, Some(ThinkingConfig::Adaptive)] {
            let child = session_opts(requested, None, root, &intermediate);
            assert_eq!(child.thinking, parent);
            assert_eq!(child.thinking.shown(&intermediate), shown);
            let grandchild = session_opts(
                Some(ThinkingConfig::Effort(Effort::Max)),
                None,
                child,
                &model,
            );
            assert_eq!(grandchild.thinking, expected_grandchild);
        }
    }

    struct PendingProvider(flume::Sender<()>);

    impl Provider for PendingProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a JsonValue,
            events: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                events
                    .send(ProviderEvent::TextDelta {
                        text: PARTIAL_TEXT.into(),
                    })
                    .unwrap();
                self.0.send(()).unwrap();
                std::future::pending().await
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    async fn native_session(lua: &Lua, ctx: &ToolContext) -> AnyUserData {
        lua.set_app_data(Arc::clone(ToolRegistry::global_arc()));
        let ctx = lua.create_userdata(LuaCtx::handler(ctx)).unwrap();
        let (session, error) = session(
            lua.clone(),
            Arc::from(PLUGIN),
            PluginPermissions::trusted(),
            LuaEither::Left(ctx.borrow().unwrap()),
            Some(lua.create_table().unwrap()),
        )
        .await
        .unwrap();
        assert!(error.is_none(), "{error:?}");
        session.unwrap()
    }

    #[test_case(false ; "close")]
    #[test_case(true ; "dispose")]
    #[tokio::test]
    async fn closing_a_session_leaves_its_siblings_running(dispose: bool) {
        let lua = Lua::new();
        let ctx = stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
        let first = native_session(&lua, &ctx).await;
        let sibling = native_session(&lua, &ctx).await;
        if dispose {
            drop(first.take::<LuaSession>().unwrap());
        } else {
            close(lua.clone(), first.borrow().unwrap()).await.unwrap();
            close(lua.clone(), first.borrow().unwrap()).await.unwrap();
        }
        let replacement = native_session(&lua, &ctx).await;
        if !dispose {
            close(lua.clone(), first.borrow().unwrap()).await.unwrap();
        }
        for session in [sibling, replacement] {
            assert!(
                !session
                    .borrow::<LuaSession>()
                    .unwrap()
                    .inner
                    .lock()
                    .await
                    .child_cancel
                    .is_cancelled()
            );
            close(lua.clone(), session.borrow().unwrap()).await.unwrap();
        }
    }

    #[test_case(false ; "close_during_prompt")]
    #[test_case(true ; "dispose_during_prompt")]
    #[tokio::test]
    async fn session_close_does_not_wait_for_the_prompt_lock(dispose: bool) {
        let lua = Lua::new();
        let (started_tx, started_rx) = flume::unbounded();
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, RUN_ID);
        let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(PARENT_ID));
        ctx.provider = Arc::new(PendingProvider(started_tx));
        timeout(SESSION_TIMEOUT, async {
                let sess = native_session(&lua, &ctx).await;
                let (result, ()) = futures::future::join(
                    prompt(lua.clone(), sess.borrow().unwrap(), SESSION_PROMPT.into()),
                    async {
                        started_rx.recv_async().await.unwrap();
                        if dispose {
                            drop(sess.take::<LuaSession>().unwrap());
                        } else {
                            let close: Function = lua.load(CLOSE_SESSION).eval().unwrap();
                            close.call_async::<()>(sess.clone()).await.unwrap();
                            close.call_async::<()>(sess.clone()).await.unwrap();
                        }
                    },
                )
                .await;
                let (partial, error) = result.unwrap();
                assert_eq!(error.as_deref(), Some(CANCELLED_MSG));
                assert!(
                    partial
                        .unwrap()
                        .get::<String>("text")
                        .unwrap()
                        .starts_with(PARTIAL_TEXT)
                );
                if !dispose {
                    let (_, error) =
                        prompt(lua.clone(), sess.borrow().unwrap(), SESSION_PROMPT.into())
                            .await
                            .unwrap();
                    assert_eq!(error.as_deref(), Some(SESSION_CLOSED_ERR));
                    let (_, error) =
                        send(&lua, &sess.borrow().unwrap(), SESSION_PROMPT.into()).unwrap();
                    assert_eq!(error.as_deref(), Some(SESSION_CLOSED_ERR));
                    assert!(started_rx.is_empty());
                }
                loop {
                    if let AgentEvent::SubagentHistory {
                        tool_use_id,
                        messages,
                    } = events.recv_async().await.unwrap().event
                    {
                        assert_eq!(tool_use_id, PARENT_ID);
                        assert!(messages.iter().any(|message| {
                            matches!(message.role, Role::Assistant)
                                && message.content.iter().any(|block| {
                                    matches!(block, ContentBlock::Text { text } if text.starts_with(PARTIAL_TEXT))
                                })
                        }));
                        break;
                    }
                }
                assert!(
                    !events
                        .try_iter()
                        .any(|event| matches!(event.event, AgentEvent::SubagentHistory { .. }))
                );
            })
        .await
        .expect("session close waited for the active prompt");
    }

    use crate::runtime::SpawnedTasks;
    use maki_agent::tools::ToolRegistry;

    const CHILD_REQUEST: &str = "Analyse the supplied evidence.";
    const CHILD_SYSTEM: &str = "You analyse evidence.\n{{instructions}}";
    const CHILD_INSTRUCTIONS: &str = "Keep {{identity}} literal.";
    const CHILD_RESPONSE: &str = "Analysis complete.";

    struct PromptRecorder(Sender<String>);

    impl Provider for PromptRecorder {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            system: &'a str,
            _: &'a JsonValue,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.0.send(system.to_owned()).unwrap();
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: CHILD_RESPONSE.into(),
                        }],
                        ..Default::default()
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test_case("research", PromptId::Research ; "research_child")]
    #[test_case("general", PromptId::General ; "general_child")]
    #[tokio::test]
    async fn child_session_receives_the_parent_prompt_policy(name: &str, id: PromptId) {
        let lua = Lua::new();
        lua.set_app_data(SpawnedTasks::default());
        lua.set_app_data(Arc::new(ToolRegistry::new()));
        let (sent, requests) = flume::unbounded();
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
        ctx.provider = Arc::new(PromptRecorder(sent));
        let mut prompts = ResolvedPrompts::default();
        prompts.set_template(id, CHILD_SYSTEM.into());
        ctx.prompt_slots = Arc::new(prompts);
        let ctx = lua.create_userdata(LuaCtx::handler(&ctx)).unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("prompt_id", name).unwrap();
        opts.set("instructions", CHILD_INSTRUCTIONS).unwrap();
        let (system, error) = system_prompt(lua.clone(), ctx.borrow().unwrap(), opts)
            .await
            .unwrap();
        assert!(error.is_none(), "{error:?}");
        let opts = lua.create_table().unwrap();
        opts.set("system", system.unwrap()).unwrap();
        let (sess, error) = session(
            lua.clone(),
            Arc::from("test"),
            PluginPermissions::trusted(),
            LuaEither::Left(ctx.borrow().unwrap()),
            Some(opts),
        )
        .await
        .unwrap();
        assert!(error.is_none(), "{error:?}");
        let sess = sess.unwrap();
        let (_, error) = prompt(lua.clone(), sess.borrow().unwrap(), CHILD_REQUEST.into())
            .await
            .unwrap();
        close(lua.clone(), sess.borrow().unwrap()).await.unwrap();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(
            requests.recv().unwrap(),
            format!("You analyse evidence.\n{CHILD_INSTRUCTIONS}")
        );
    }

    const NUDGE: &str = "new finding, hurry up";
    const INTERRUPT_OPEN: &str = "<user-interrupt>";

    /// Hands the texts of each request's closing user messages to the test,
    /// then holds the reply until the test lets it go.
    struct GatedProvider {
        requests: Sender<Vec<String>>,
        release: flume::Receiver<()>,
    }

    impl Provider for GatedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a JsonValue,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let mut user_turn: Vec<String> = messages
                    .iter()
                    .rev()
                    .take_while(|m| matches!(m.role, Role::User))
                    .filter_map(|m| m.first_text_content().map(str::to_owned))
                    .collect();
                user_turn.reverse();
                self.requests.send(user_turn).unwrap();
                self.release.recv_async().await.unwrap();
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: CHILD_RESPONSE.into(),
                        }],
                        ..Default::default()
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// A nudge sent ahead of a prompt rides in its first request, in front of
    /// it. One sent mid-prompt waits for the turn boundary and lands as a user
    /// interrupt in the next request. Either way the tab hears it through the
    /// same event a message typed there would raise.
    #[test_case(false ; "before_prompt")]
    #[test_case(true ; "while_prompt_runs")]
    #[tokio::test]
    async fn send_reaches_the_child(mid_prompt: bool) {
        let lua = Lua::new();
        let (requests_tx, requests) = flume::unbounded();
        let (release, release_rx) = flume::unbounded();
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, RUN_ID);
        let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(PARENT_ID));
        ctx.provider = Arc::new(GatedProvider {
            requests: requests_tx,
            release: release_rx,
        });
        let nudge = |sess: &AnyUserData| {
            let sent = send(&lua, &sess.borrow().unwrap(), NUDGE.into()).unwrap();
            assert_eq!(sent, (Some(true), None));
        };
        let run = async {
            let sess = native_session(&lua, &ctx).await;
            if !mid_prompt {
                nudge(&sess);
            }
            let (result, requests_seen) = futures::future::join(
                prompt(lua.clone(), sess.borrow().unwrap(), SESSION_PROMPT.into()),
                async {
                    let mut seen = vec![requests.recv_async().await.unwrap()];
                    if mid_prompt {
                        nudge(&sess);
                        release.send(()).unwrap();
                        seen.push(requests.recv_async().await.unwrap());
                    }
                    release.send(()).unwrap();
                    seen
                },
            )
            .await;
            close(lua.clone(), sess.borrow().unwrap()).await.unwrap();
            assert!(result.unwrap().1.is_none());
            if mid_prompt {
                assert_eq!(requests_seen[0], [SESSION_PROMPT]);
                let [interrupt] = &requests_seen[1][..] else {
                    panic!("the interrupt should land alone: {requests_seen:?}");
                };
                assert!(interrupt.starts_with(INTERRUPT_OPEN) && interrupt.contains(NUDGE));
            } else {
                assert_eq!(requests_seen, [[NUDGE, SESSION_PROMPT]]);
            }
        };
        timeout(SESSION_TIMEOUT, run)
            .await
            .expect("the prompt never finished");
        assert!(requests.is_empty());
        assert!(events.drain().any(|envelope| {
            matches!(&envelope.event, AgentEvent::QueueItemConsumed { text, .. } if text == NUDGE)
                && envelope
                    .subagent
                    .is_some_and(|info| info.parent_tool_use_id == PARENT_ID)
        }));
    }

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

    #[tokio::test]
    async fn ctx_less_session_needs_a_task_and_a_live_session() {
        let lua = Lua::new();
        let agent =
            create_agent_table(&lua, Arc::from(PLUGIN), PluginPermissions::trusted()).unwrap();
        lua.globals().set("agent", agent).unwrap();
        let session = MakiId::generate();
        lua.globals().set("sid", session.to_string()).unwrap();
        let open = || {
            lua.load("return select(2, agent.session({ session = sid }))")
                .eval_async::<Option<String>>()
        };
        let task = TaskScope::new(&lua, TaskCell::new(CancelToken::none(), None, None));

        assert_eq!(open().await.unwrap().as_deref(), Some(TASK_REQUIRED_ERR));
        assert_eq!(
            task.scope_future(open()).await.unwrap(),
            Some(not_live(session))
        );
    }

    /// A prompt cut short leaves its run's `Done` unread. The next prompt must
    /// read past it and keep waiting for its own, even when it gets dropped
    /// halfway and asked again.
    #[tokio::test]
    async fn next_prompt_waits_past_a_leftover_done() {
        let (tx, rx) = flume::unbounded();
        let mut dones = RunDones::new(rx);
        dones.expect_one();
        tx.send(()).unwrap();
        dones.expect_one();
        assert!(dones.caught_up().now_or_never().is_none());

        tx.send(()).unwrap();
        assert_eq!(dones.caught_up().await, Ok(()));
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
            parent_call: None,
        })
        .unwrap();
        info
    }

    fn run_end() -> [AgentEvent; 2] {
        [
            AgentEvent::Error {
                message: IGNORED_ERROR.into(),
                auth: false,
                totals: RunTotals::default(),
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
        ]
    }

    /// Two turns, an error and the run's `Done`, then the guard drops. The
    /// first turn plays one handed up by a nested subagent, so it has to be
    /// reported under that subagent's model, not the session's.
    fn relayed_run() -> SessionEvents {
        let (guard, sub_events) = event_stream();
        let sub_tx = guard.sender(RUN_ID);
        let [error, done] = run_end();
        let events = [
            turn(NESTED_MODEL, tokens(100, 20), 0.25),
            turn(SESSION_MODEL, tokens(50, 10), 0.5),
            error,
            done,
        ];
        for event in events {
            sub_tx.send(event).unwrap();
        }
        drop(guard);
        sub_events
    }

    fn live_usages(rx: &flume::Receiver<ToolLive>) -> Vec<String> {
        rx.drain()
            .map(|event| match event {
                ToolLive::Usage(usage) => usage,
                _ => panic!("relay must only publish usage"),
            })
            .collect()
    }

    fn event_names(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                serde_json::to_value(event).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    fn parent_upstream() -> (Upstream, flume::Receiver<Envelope>) {
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let upstream = Upstream::Parent {
            tx: EventSender::new(parent_raw_tx, RUN_ID),
            adoption: None,
        };
        (upstream, parent_rx)
    }

    fn chat_upstream(session: MakiId, ui_tx: Sender<UiAction>) -> Upstream {
        Upstream::Chat {
            session,
            plugin: Arc::from(PLUGIN),
            ui: Some(ui_tx),
            registry: BackgroundTasks::default(),
            id: PARENT_ID.into(),
        }
    }

    fn permission_ask() -> AgentEvent {
        AgentEvent::PermissionRequest {
            id: REQUEST_ID.into(),
            tool: ToolKey::native(ASKING_TOOL),
            scopes: Vec::new(),
            reason: None,
        }
    }

    fn unattended_permission() -> String {
        TaggedAnswer::new(REQUEST_ID, PermissionAnswer::Unattended).encode()
    }

    /// Headless, a moved session's chat has no reader, so an ask that parks
    /// its run would wait forever. The relay answers it in place of the user
    /// it lacks, and nobody hears the ask. A watched chat's ask reaches the
    /// tab instead, and its user answers it.
    #[test_case(permission_ask, unattended_permission(), false ; "unwatched_permission")]
    #[test_case(|| AgentEvent::AuthRequired, String::new(), false ; "unwatched_auth")]
    #[test_case(permission_ask, unattended_permission(), true ; "watched_permission")]
    #[test_case(|| AgentEvent::AuthRequired, String::new(), true ; "watched_auth")]
    #[tokio::test]
    async fn relay_answers_an_ask_only_when_nobody_watches(
        ask: fn() -> AgentEvent,
        unattended: String,
        watched: bool,
    ) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let info = Arc::new(OnceLock::new());
        info.set(SubagentInfo {
            answer_tx: Some(answer_tx),
            ..subagent_info().get().unwrap().clone()
        })
        .unwrap();
        let session = MakiId::generate();
        let (ui_tx, ui_rx) = flume::unbounded();
        let upstream = Upstream::Chat {
            session,
            plugin: Arc::from(PLUGIN),
            ui: watched.then_some(ui_tx),
            registry: BackgroundTasks::default(),
            id: PARENT_ID.into(),
        };
        let (guard, sub_events) = event_stream();
        guard.sender(RUN_ID).send(ask()).unwrap();
        drop(guard);

        relay_session_events(sub_events, upstream, info, flume::unbounded().0, None).await;

        let (answers, shown) = if watched {
            (Vec::new(), event_names(&[ask(), AgentEvent::StreamClosed]))
        } else {
            (vec![unattended], Vec::new())
        };
        assert_eq!(answer_rx.drain().collect::<Vec<_>>(), answers);
        assert_eq!(event_names(&chat_events(&ui_rx, session)), shown);
    }

    /// Dropping the guard is what ends the relay: a Lua tool context keeps a
    /// sender alive past the run, so disconnect never comes. The forwarded
    /// count also pins down that the close marker itself stays behind, since
    /// passing it on would end the *parent's* stream at the first subagent.
    /// The parent hears neither `Error` nor `Done`: both reach it through the
    /// tool result.
    #[tokio::test]
    async fn relay_to_parent_reports_each_turn_and_keeps_run_ends_behind() {
        let (upstream, parent_rx) = parent_upstream();
        let (done_tx, done_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();

        relay_session_events(
            relayed_run(),
            upstream,
            subagent_info(),
            done_tx,
            Some(live_tx),
        )
        .await;

        let expected_live = [
            tokens(100, 20).format_sum_cost(Some(0.25)),
            tokens(50, 10).format_sum_cost(Some(0.75)),
        ];
        assert_eq!(live_usages(&live_rx), expected_live);
        assert_eq!(done_rx.try_recv(), Ok(()));

        let forwarded = parent_rx.drain().collect::<Vec<_>>();
        assert_eq!(forwarded.len(), expected_live.len());
        assert!(forwarded.iter().all(|envelope| {
            matches!(envelope.event, AgentEvent::TurnComplete(_))
                && envelope
                    .subagent
                    .as_ref()
                    .is_some_and(|info| info.parent_tool_use_id == PARENT_ID)
        }));
    }

    /// The parent's stream drops any run but its own, so the transcript a
    /// close sends has to carry the parent's run like the relayed events do.
    #[test]
    fn history_sent_to_the_parent_carries_its_run() {
        let (upstream, parent_rx) = parent_upstream();
        upstream.send_history(PARENT_ID, Vec::new());
        let envelope = parent_rx.try_recv().unwrap();
        assert_eq!(envelope.run_id, RUN_ID);
        assert!(matches!(
            envelope.event,
            AgentEvent::SubagentHistory { tool_use_id, .. } if tool_use_id == PARENT_ID
        ));
    }

    /// What reached the tab of this chat, each event checked to name the
    /// owning plugin and the subagent.
    fn chat_events(ui_rx: &flume::Receiver<UiAction>, session: MakiId) -> Vec<AgentEvent> {
        ui_rx
            .drain()
            .map(|action| match action {
                UiAction::PluginTaskEvent {
                    session: owner,
                    plugin,
                    envelope,
                } if owner == session && &*plugin == PLUGIN => envelope,
                _ => panic!("relay must only send task events for this chat session"),
            })
            .map(|envelope| {
                assert!(
                    envelope
                        .subagent
                        .as_ref()
                        .is_some_and(|info| info.parent_tool_use_id == PARENT_ID)
                );
                envelope.event
            })
            .collect()
    }

    /// A chat tab hears everything a task needs to show itself: the turns,
    /// the error, the run's `Done` (its task goes idle on it) and, once the
    /// guard drops, `StreamClosed` (its task is done). Every event names the
    /// owning plugin and chat session.
    #[tokio::test]
    async fn relay_to_chat_forwards_run_ends_and_closes_the_task() {
        let session = MakiId::generate();
        let (done_tx, done_rx) = flume::unbounded();
        let (ui_tx, ui_rx) = flume::unbounded();

        relay_session_events(
            relayed_run(),
            chat_upstream(session, ui_tx),
            subagent_info(),
            done_tx,
            None,
        )
        .await;

        assert_eq!(done_rx.try_recv(), Ok(()));
        assert_eq!(
            event_names(&chat_events(&ui_rx, session)),
            [
                "turn_complete",
                "turn_complete",
                "error",
                "done",
                "stream_closed"
            ]
        );
    }

    fn cancellation(
        ui_id: &str,
        trigger: CancelTrigger,
        registry: BackgroundTasks,
        background: bool,
    ) -> SessionCancellation {
        let parent_cancels = Arc::new(CancelMap::new());
        let (slot_trigger, _cancel) = CancelToken::new();
        SessionCancellation {
            slot: parent_cancels.insert(ui_id.to_owned(), slot_trigger),
            parent_cancels,
            ui_id: ui_id.to_owned(),
            trigger,
            registry,
            background,
            mailbox: SessionMailbox::register(MakiId::generate()),
            adoption: None,
            stop: StopMark::default(),
        }
    }

    /// A running prompt holds the session lock until it unwinds, so `send`
    /// can only ask the cancellation whether Esc stopped the task.
    #[test]
    fn user_stop_refuses_without_the_session_lock() {
        let session = MakiId::generate();
        let _live = crate::background::tests::live(session);
        let registry = BackgroundTasks::default();
        let (mut entry, _token) = crate::background::tests::entry(session, None);
        let (trigger, _session_cancel) = CancelToken::new();
        let cancellation = cancellation(&entry.info.id, trigger, registry.clone(), true);
        entry.stop = cancellation.stop.clone();
        registry.register(entry).unwrap();
        assert_eq!(cancellation.refusal(), None);

        registry.stop(&cancellation.ui_id, Stop::User);

        assert_eq!(cancellation.refusal(), Some(STOPPED_BY_USER_ERR));
    }

    /// The parent message of a stopped call would otherwise queue for the
    /// prompt that is still unwinding and never reads it.
    #[test]
    fn a_stopped_call_refuses_while_its_prompt_unwinds() {
        let (call_trigger, call) = CancelToken::new();
        let (trigger, _session_cancel) = call.child();
        let cancellation = cancellation(PARENT_ID, trigger, BackgroundTasks::default(), false);
        assert_eq!(cancellation.refusal(), None);

        call_trigger.cancel();

        assert_eq!(cancellation.refusal(), Some(CANCELLED_MSG));
    }

    /// The first turn reaches the parent while the call is in the
    /// foreground. Everything after the move goes to the chat, in order, and
    /// the run's bill so far lands on the moved call's entry, whose id a
    /// top-level `task` shares.
    #[tokio::test]
    async fn relay_follows_a_moved_call_to_its_chat_in_order() {
        let (chat, _mailbox, _seed) = live_chat();
        let (_run_trigger, run) = CancelToken::new();
        let call = Arc::new(CallScope::new(PARENT_ID.into(), &run));
        let registry = BackgroundTasks::default();
        registry.register(call_entry(chat, PARENT_ID)).unwrap();
        let (parent_tx, parent_rx) = flume::unbounded();
        let (ui_tx, ui_rx) = flume::unbounded();
        let upstream = Upstream::Parent {
            tx: EventSender::new(parent_tx, RUN_ID),
            adoption: Some(Adoption {
                call: Arc::clone(&call),
                ui: Some(ui_tx),
                registry: registry.clone(),
                id: PARENT_ID.into(),
                plugin: Arc::from(PLUGIN),
            }),
        };
        let (guard, sub_events) = event_stream();
        let sub_tx = guard.sender(RUN_ID);
        let (done_tx, done_rx) = flume::unbounded();
        let moving = async {
            sub_tx
                .send(turn(SESSION_MODEL, tokens(100, 20), 0.25))
                .unwrap();
            let before = parent_rx.recv_async().await.unwrap();
            let trigger = call.move_to(home(chat, PARENT_ID)).unwrap();
            let [error, done] = run_end();
            for event in [turn(SESSION_MODEL, tokens(50, 10), 0.5), error, done] {
                sub_tx.send(event).unwrap();
            }
            drop(guard);
            (before, trigger)
        };

        let ((before, _trigger), ()) = futures::future::join(
            moving,
            relay_session_events(sub_events, upstream, subagent_info(), done_tx, None),
        )
        .await;

        assert!(matches!(before.event, AgentEvent::TurnComplete(_)));
        assert_eq!(before.run_id, RUN_ID);
        assert!(parent_rx.is_empty());
        assert_eq!(done_rx.try_recv(), Ok(()));
        assert_eq!(
            event_names(&chat_events(&ui_rx, chat)),
            ["turn_complete", "error", "done", "stream_closed"]
        );
        let costs = registry
            .list(chat)
            .into_iter()
            .map(|info| info.cost)
            .collect::<Vec<_>>();
        assert_eq!(costs, [Some(0.75)]);
    }

    /// A cancel that drops a background prompt mid-run drops the run too, so
    /// no `Done` comes. The tab must still hear the run ended, or its task
    /// stays working, and the relay must not count that as the run's `Done`.
    /// The error bills what the run spent before the cancel.
    #[tokio::test]
    async fn dropped_background_prompt_still_ends_the_run_in_its_tab() {
        let (guard, sub_events) = event_stream();
        let background = Background {
            registry: BackgroundTasks::default(),
            session: MakiId::generate(),
            thinking: None,
            fast: None,
        };
        let ledger = Arc::new(RunLedger::default());
        let settle = background.start_prompt(PARENT_ID, guard.sender(RUN_ID), Arc::clone(&ledger));
        ledger.add(DONE_USAGE, None, None);
        drop(settle);
        drop(guard);
        let (done_tx, done_rx) = flume::unbounded();
        let (ui_tx, ui_rx) = flume::unbounded();

        relay_session_events(
            sub_events,
            chat_upstream(MakiId::generate(), ui_tx),
            subagent_info(),
            done_tx,
            None,
        )
        .await;

        assert!(done_rx.is_empty());
        let events = ui_rx
            .drain()
            .map(|action| match action {
                UiAction::PluginTaskEvent { envelope, .. } => envelope.event,
                _ => panic!("relay must only send task events"),
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::Error { message, auth: false, totals }, AgentEvent::StreamClosed]
                    if message == CANCELLED_MSG && totals.usage == DONE_USAGE
            ),
            "{:?}",
            event_names(&events)
        );
    }

    #[test]
    fn task_model_selection_tier_override_beats_the_session_provider() {
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
        ctx.model = Arc::new(Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap());
        let ctx = AgentContext::from(&ctx);

        let same_provider_weak = resolve_model_from_ctx(&ctx, Some("weak")).unwrap().spec();
        assert!(same_provider_weak.starts_with("anthropic/"));

        let dir = tempfile::TempDir::new().unwrap();
        maki_providers::model_registry::set_and_persist(
            "zai/glm-5".into(),
            ModelTier::Weak,
            &maki_storage::StateDir::from_path(dir.path().to_path_buf()),
        );

        let model = resolve_model_from_ctx(&ctx, Some("weak")).unwrap();
        assert_eq!(model.spec(), "zai/glm-5");
        assert_ne!(model.spec(), same_provider_weak);
    }

    fn live_chat() -> (MakiId, SessionMailbox, Arc<AgentSeed>) {
        let chat = MakiId::generate();
        let mailbox = SessionMailbox::register(chat);
        let seed = mailbox.publish_seed(stub_seed(chat));
        (chat, mailbox, seed)
    }

    /// The call's plugin differs from the sessions' own, which name their
    /// entries and tabs.
    fn home(chat: MakiId, call_id: &str) -> TaskHome {
        TaskHome {
            chat,
            id: call_id.into(),
            plugin: Arc::from(CALL_PLUGIN),
        }
    }

    fn call_entry(chat: MakiId, call_id: &str) -> BackgroundTask {
        BackgroundTask {
            info: BackgroundInfo {
                id: call_id.into(),
                plugin: Arc::from(PLUGIN),
                name: String::new(),
                kind: Kind::Call {
                    tool: Arc::from(CALL_TOOL),
                },
                running: true,
                started_at: 0,
                cost: None,
            },
            session: chat,
            task: None,
            lifetime: Lifetime::Call,
            stop: StopMark::default(),
            cancel: Fire::Owned(CancelToken::new().0),
            mailbox: None,
        }
    }

    /// A model call's ctx as dispatch builds it: one scope under the run.
    fn call_ctx(run: &CancelToken, tool_use_id: Option<&str>) -> (ToolContext, Arc<CallScope>) {
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, tool_use_id);
        let call = Arc::new(CallScope::new(CALL_ID.into(), run));
        ctx.cancel = call.cancel().clone();
        ctx.call = Some(Arc::clone(&call));
        (ctx, call)
    }

    /// The runtime's move commit, minus the call's own entry.
    fn move_call(
        lua: &Lua,
        call: &CallScope,
        chat: MakiId,
        ui: Option<&Sender<UiAction>>,
    ) -> CancelTrigger {
        let home = home(chat, CALL_ID);
        let trigger = call.move_to(home.clone()).unwrap();
        let registry = BackgroundTasks::of(lua);
        for stub in call.take_sessions() {
            adopt_session(&registry, ui, &home, stub);
        }
        trigger
    }

    fn session_state<T>(sess: &AnyUserData, read: impl FnOnce(&SessionState) -> T) -> T {
        let session = sess.borrow::<LuaSession>().unwrap();
        let state = session.inner.try_lock().unwrap();
        read(&state)
    }

    fn session_cancelled(sess: &AnyUserData) -> bool {
        session_state(sess, |state| state.child_cancel.is_cancelled())
    }

    fn session_ui_id(sess: &AnyUserData) -> String {
        sess.borrow::<LuaSession>()
            .unwrap()
            .cancellation
            .ui_id
            .clone()
    }

    enum Opened {
        BeforeTheMove,
        AfterTheMove,
    }

    /// Hands back the moved call's trigger too: dropping it ends the call.
    async fn open_under_moved_call(
        lua: &Lua,
        ctx: &ToolContext,
        call: &CallScope,
        chat: MakiId,
        opened: Opened,
    ) -> (AnyUserData, CancelTrigger) {
        match opened {
            Opened::BeforeTheMove => {
                let sess = native_session(lua, ctx).await;
                (sess, move_call(lua, call, chat, None))
            }
            Opened::AfterTheMove => {
                let trigger = move_call(lua, call, chat, None);
                (native_session(lua, ctx).await, trigger)
            }
        }
    }

    /// Run end drains `subagent_cancels` and fires the run token. Both must
    /// leave a session alone once its call moved.
    #[test_case(false ; "an_unmoved_session_ends_with_the_run")]
    #[test_case(true ; "a_moved_session_outlives_the_run")]
    #[tokio::test]
    async fn run_end_reaches_a_session_only_until_its_call_moves(moved: bool) {
        let lua = Lua::new();
        let (run_trigger, run) = CancelToken::new();
        let (ctx, call) = call_ctx(&run, Some(CALL_ID));
        let sess = native_session(&lua, &ctx).await;
        let _call_trigger = moved.then(|| call.move_to(home(MakiId::generate(), CALL_ID)).unwrap());

        ctx.subagent_cancels.cancel_all();
        assert!(!session_cancelled(&sess));
        run_trigger.cancel();
        assert_eq!(session_cancelled(&sess), !moved);
    }

    /// Esc on a subagent's row cancels its id, before or after it opened.
    #[test_case(true ; "cancelled_before_it_opens")]
    #[test_case(false ; "cancelled_while_open")]
    #[tokio::test]
    async fn cancelling_its_id_stops_a_session(before_open: bool) {
        let lua = Lua::new();
        let ctx = stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
        if before_open {
            ctx.subagent_cancels.cancel(PARENT_ID.to_owned());
        }
        let sess = native_session(&lua, &ctx).await;
        if !before_open {
            ctx.subagent_cancels.cancel(PARENT_ID.to_owned());
        }
        assert!(session_cancelled(&sess));
    }

    #[test_case(false ; "a_watched_call")]
    #[test_case(true ; "work_already_in_the_background")]
    #[tokio::test]
    async fn a_session_honours_plan_mode_once_its_call_moves(inherited: bool) {
        let lua = Lua::new();
        let (_run_trigger, run) = CancelToken::new();
        let (mut ctx, call) = call_ctx(&run, Some(CALL_ID));
        let owner = MakiId::generate();
        if inherited {
            ctx.background_for = BackgroundFor::Chat(owner);
        }
        let sess = native_session(&lua, &ctx).await;
        let blocked_by = || session_state(&sess, |state| state.params.background_for.chat());
        assert_eq!(blocked_by(), inherited.then_some(owner));

        let chat = MakiId::generate();
        let _call_trigger = call.move_to(home(chat, CALL_ID)).unwrap();
        assert_eq!(blocked_by(), Some(if inherited { owner } else { chat }));
    }

    /// Esc in the adopted tab of a subagent a moved `batch` runs.
    #[test_case(Opened::BeforeTheMove ; "opened_before_the_move")]
    #[test_case(Opened::AfterTheMove ; "opened_after_the_move")]
    #[tokio::test]
    async fn a_nested_session_under_a_moved_call_stops_by_its_own_id(opened: Opened) {
        let lua = Lua::new();
        let (chat, _mailbox, _seed) = live_chat();
        let (_run_trigger, run) = CancelToken::new();
        let (ctx, call) = call_ctx(&run, None);
        let registry = BackgroundTasks::of(&lua);
        let (sess, _call_trigger) = open_under_moved_call(&lua, &ctx, &call, chat, opened).await;
        let id = session_ui_id(&sess);
        let kinds = registry
            .list(chat)
            .into_iter()
            .map(|info| (info.id, info.kind))
            .collect::<Vec<_>>();
        assert_eq!(kinds, [(id.clone(), Kind::Agent)]);

        registry.stop(&id, Stop::User);
        assert!(session_cancelled(&sess));
        assert!(!call.cancel().is_cancelled());
        assert!(registry.list(chat).is_empty());
    }

    /// A top-level `task` shares its id with the call, so the call's entry
    /// is its only one, and its close must not retire that entry.
    #[test_case(Some(CALL_ID), Opened::BeforeTheMove, 1 ; "top_level_opened_before_the_move")]
    #[test_case(Some(CALL_ID), Opened::AfterTheMove, 1 ; "top_level_opened_after_the_move")]
    #[test_case(None, Opened::BeforeTheMove, 2 ; "nested_opened_before_the_move")]
    #[test_case(None, Opened::AfterTheMove, 2 ; "nested_opened_after_the_move")]
    #[tokio::test]
    async fn closing_an_adopted_session_leaves_the_call_entry(
        tool_use_id: Option<&str>,
        opened: Opened,
        while_open: usize,
    ) {
        let lua = Lua::new();
        let (chat, _mailbox, _seed) = live_chat();
        let (_run_trigger, run) = CancelToken::new();
        let (ctx, call) = call_ctx(&run, tool_use_id);
        let registry = BackgroundTasks::of(&lua);
        registry.register(call_entry(chat, CALL_ID)).unwrap();
        let (sess, _call_trigger) = open_under_moved_call(&lua, &ctx, &call, chat, opened).await;
        assert_eq!(registry.list(chat).len(), while_open);
        close(lua.clone(), sess.borrow().unwrap()).await.unwrap();
        let kinds = registry
            .list(chat)
            .into_iter()
            .map(|info| info.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [Kind::Call {
                tool: Arc::from(CALL_TOOL)
            }]
        );
    }

    /// A session that already ran has a chat under the call's row, so the
    /// move hands it to the UI at once, under the session's own plugin. One
    /// never prompted has no chat yet and opens on its first event.
    #[test_case(true ; "one_that_already_ran")]
    #[test_case(false ; "one_not_prompted_yet")]
    #[tokio::test]
    async fn a_move_opens_the_tab_of_a_nested_session_that_ran(prompted: bool) {
        let lua = Lua::new();
        let (chat, _mailbox, _seed) = live_chat();
        let (_run_trigger, run) = CancelToken::new();
        let (ctx, call) = call_ctx(&run, None);
        let sess = native_session(&lua, &ctx).await;
        if prompted {
            let info = subagent_info().get().cloned().unwrap();
            session_state(&sess, |state| state.subagent_info.set(info).unwrap());
        }
        let (ui_tx, ui_rx) = flume::unbounded();

        let _call_trigger = move_call(&lua, &call, chat, Some(&ui_tx));

        let opened = ui_rx
            .drain()
            .filter_map(|action| match action {
                UiAction::PluginTaskOpened {
                    session,
                    plugin,
                    info,
                } => Some((session, plugin.to_string(), info.parent_tool_use_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected = prompted.then(|| (chat, PLUGIN.to_owned(), PARENT_ID.to_owned()));
        assert_eq!(opened, Vec::from_iter(expected));
    }

    /// What a request carried, down to the parts the prompt cache keys on.
    struct ScriptedRequest {
        system: String,
        tools: JsonValue,
        messages: Vec<Message>,
    }

    /// Answers each request with the next reply and hands over what it carried.
    struct ScriptedProvider {
        requests: Sender<ScriptedRequest>,
        replies: flume::Receiver<&'static str>,
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a JsonValue,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests
                    .send(ScriptedRequest {
                        system: system.to_owned(),
                        tools: tools.clone(),
                        messages: messages.to_vec(),
                    })
                    .unwrap();
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: self.replies.try_recv().unwrap().into(),
                        }],
                        ..Default::default()
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn scripted_ctx() -> (ToolContext, flume::Receiver<ScriptedRequest>) {
        let (requests_tx, requests) = flume::unbounded();
        let (replies_tx, replies) = flume::unbounded();
        for reply in [FIRST_REPLY, WOKEN_REPLY] {
            replies_tx.send(reply).unwrap();
        }
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
        ctx.provider = Arc::new(ScriptedProvider {
            requests: requests_tx,
            replies,
        });
        (ctx, requests)
    }

    fn session_mailbox(sess: &AnyUserData) -> SessionMailbox {
        sess.borrow::<LuaSession>()
            .unwrap()
            .cancellation
            .mailbox
            .clone()
    }

    async fn prompt_text(lua: &Lua, sess: &AnyUserData) -> (Option<String>, Option<String>) {
        let (tbl, error) = prompt(lua.clone(), sess.borrow().unwrap(), SESSION_PROMPT.into())
            .await
            .unwrap();
        (tbl.map(|tbl| tbl.get("text").unwrap()), error)
    }

    fn moved_call_notice(wake: bool) -> BackgroundNotice<'static> {
        BackgroundNotice {
            text: NOTICE_TEXT.into(),
            display: None,
            wake,
            end: TaskEnd::Finished,
        }
    }

    /// The agent loop's own append-only contract: `system` and `tools` stay
    /// put and earlier messages are never edited.
    #[track_caller]
    fn assert_append_only(prev: &ScriptedRequest, next: &ScriptedRequest) {
        assert_eq!(prev.system, next.system, "system changed");
        assert_eq!(prev.tools, next.tools, "tools changed");
        assert!(
            next.messages.starts_with(&prev.messages),
            "earlier messages were edited"
        );
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .filter_map(Message::first_text_content)
            .collect()
    }

    /// The call is settled only once the first request went out, so the run
    /// that made it can't have drained the result: only the wait reads it.
    /// A user stop reports quietly, but the caller still waits on an answer
    /// that knows the call ended.
    #[test_case(false, true ; "without_moved_calls_returns_after_its_run")]
    #[test_case(true, true ; "with_a_moved_call_returns_the_woken_turn")]
    #[test_case(true, false ; "with_a_stopped_call_returns_the_woken_turn")]
    #[tokio::test]
    async fn a_prompt_waits_for_the_calls_its_session_moved(moved: bool, wake: bool) {
        let lua = Lua::new();
        let (ctx, requests) = scripted_ctx();
        let sess = native_session(&lua, &ctx).await;
        let mailbox = session_mailbox(&sess);
        if moved {
            mailbox.track(MOVED_CALL_ID);
        }
        let ((text, error), first) = futures::future::join(prompt_text(&lua, &sess), async {
            let first = requests.recv_async().await.unwrap();
            if moved {
                mailbox.settle(MOVED_CALL_ID, Some(moved_call_notice(wake)));
            }
            first
        })
        .await;
        close(lua.clone(), sess.borrow().unwrap()).await.unwrap();

        assert!(error.is_none(), "{error:?}");
        let later = requests.drain().collect::<Vec<_>>();
        if !moved {
            assert_eq!(text.as_deref(), Some(FIRST_REPLY));
            assert!(later.is_empty());
            return;
        }
        assert_eq!(text.as_deref(), Some(WOKEN_REPLY));
        let [woken] = later.as_slice() else {
            panic!("expected one woken request, got {}", later.len());
        };
        assert_append_only(&first, woken);
        let appended = texts(&woken.messages[first.messages.len()..]);
        assert_eq!(appended.first(), Some(&FIRST_REPLY));
        assert!(
            appended
                .last()
                .is_some_and(|text| text.contains(NOTICE_TEXT))
        );
        assert!(mailbox.unsettled().is_empty());
    }

    /// A call moved to the background may never end, so a message sent while
    /// the prompt waits on it has to wake the prompt by itself.
    #[tokio::test]
    async fn a_message_sent_while_waiting_for_moved_calls_wakes_the_prompt() {
        let lua = Lua::new();
        let (ctx, requests) = scripted_ctx();
        let sess = native_session(&lua, &ctx).await;
        let mailbox = session_mailbox(&sess);
        mailbox.track(MOVED_CALL_ID);
        let ((text, error), woken) = futures::future::join(prompt_text(&lua, &sess), async {
            requests.recv_async().await.unwrap();
            let session = sess.borrow::<LuaSession>().unwrap();
            let (sent, _) = send(&lua, &session, SENT_MESSAGE.into()).unwrap();
            assert_eq!(sent, Some(true));
            let woken = requests.recv_async().await.unwrap();
            mailbox.settle(MOVED_CALL_ID, None);
            woken
        })
        .await;
        close(lua.clone(), sess.borrow().unwrap()).await.unwrap();

        assert!(error.is_none(), "{error:?}");
        assert_eq!(text.as_deref(), Some(WOKEN_REPLY));
        assert!(
            texts(&woken.messages)
                .iter()
                .any(|text| text.contains(SENT_MESSAGE))
        );
    }

    /// Its token fired with no prompt left to notice, so nothing would ever
    /// read what the call it moved reports.
    #[test_case(false ; "between_prompts")]
    #[test_case(true ; "after_its_prompt_was_dropped")]
    #[tokio::test]
    async fn a_fired_session_token_stops_its_moved_calls(prompted: bool) {
        let lua = Lua::new();
        let (ctx, requests) = scripted_ctx();
        let (chat, _chat_mailbox, _seed) = live_chat();
        let sess = native_session(&lua, &ctx).await;
        let (work_trigger, work) = CancelToken::new();
        BackgroundTasks::of(&lua)
            .register(BackgroundTask {
                cancel: Fire::Owned(work_trigger),
                mailbox: Some(session_mailbox(&sess)),
                ..call_entry(chat, MOVED_CALL_ID)
            })
            .unwrap();
        if prompted {
            let prompt = pin!(prompt_text(&lua, &sess));
            let first_request = pin!(requests.recv_async());
            let _ = select(prompt, first_request).await;
        }

        ctx.subagent_cancels.cancel(PARENT_ID.to_owned());
        work.cancelled().await;
        close(lua.clone(), sess.borrow().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_session_ends_its_prompt_waiting_for_moved_calls() {
        let lua = Lua::new();
        let (ctx, requests) = scripted_ctx();
        let sess = native_session(&lua, &ctx).await;
        session_mailbox(&sess).track(MOVED_CALL_ID);
        let ((_, error), ()) = futures::future::join(prompt_text(&lua, &sess), async {
            requests.recv_async().await.unwrap();
            ctx.subagent_cancels.cancel(PARENT_ID.to_owned());
        })
        .await;
        assert_eq!(error.as_deref(), Some(CANCELLED_MSG));
        assert!(requests.is_empty());
        close(lua.clone(), sess.borrow().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn a_caller_cancel_ends_the_wait_for_moved_calls() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(MOVED_CALL_ID);
        let session = CancelToken::none();
        let (caller_trigger, caller) = CancelToken::new();
        let inbox = SubagentInbox::default();
        let mut wake = pin!(next_wake(&mailbox, &inbox, &session, Some(&caller)));
        assert!((&mut wake).now_or_never().is_none());
        caller_trigger.cancel();
        assert!(wake.await.is_err());
    }
}
