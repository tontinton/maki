//! `maki.agent` exposes subagent primitives to Lua plugins. Policy (retries,
//! validation, concurrency) lives in the task plugin, not here.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex as AsyncMutex;
use futures::future::{Either, select};
use maki_agent::agent::tool_dispatch::{self, Emit};
use maki_agent::cancel::CancelMap;
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::registry::ToolRegistry;
use maki_agent::tools::{
    Deadline, DescriptionContext, FileReadTracker, LocalToolFn, LocalTools, ToolAudience,
    ToolContext, ToolFilter, ToolLive,
};
use maki_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope, EventSender,
    History, SubagentInfo, ToolDoneEvent,
};
use maki_providers::model::ModelTier;
use maki_providers::provider;
use maki_providers::{ContentBlock, Model, ModelError, Role, ThinkingConfig};
use maki_storage::id::MakiId;
use mlua::{
    Function, IntoLuaMulti, Lua, Result as LuaResult, Table, UserData, UserDataMethods,
    Value as LuaValue,
};
use serde_json::Value as JsonValue;
use tracing::info;

use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::{json_to_lua, lua_to_json, lua_tool_result};
use crate::api::util::ctx::{AgentContext, LuaCtx};

const SESSION_CLOSED_ERR: &str = "session closed";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;

pub(crate) fn register(lua: &Lua, maki: &Table) -> LuaResult<()> {
    let agent = lua.create_table()?;

    agent.set("resolve_model", lua.create_async_function(resolve_model)?)?;
    agent.set("system_prompt", lua.create_async_function(system_prompt)?)?;
    agent.set("tools", lua.create_async_function(tools)?)?;
    agent.set("call_tool", lua.create_async_function(call_tool)?)?;
    agent.set("session", lua.create_async_function(session)?)?;

    maki.set("agent", agent)?;
    Ok(())
}

fn resolve_model_from_ctx(ctx: &AgentContext, tier: Option<&str>) -> Result<Model, String> {
    let Some(tier_str) = tier else {
        return Ok(Model::clone(&ctx.model));
    };
    let requested: ModelTier = tier_str.parse().map_err(|e: ModelError| e.to_string())?;
    let effective = requested.min(ctx.model.tier);
    if effective == ctx.model.tier {
        return Ok(Model::clone(&ctx.model));
    }
    let map = maki_providers::model_registry::model_registry()
        .read()
        .unwrap();
    ctx.model
        .dynamic_slug
        .is_none()
        .then(|| map.spec_for_tier(ctx.model.provider, effective))
        .flatten()
        .or_else(|| map.spec_for_tier_any(effective))
        .and_then(|s| Model::from_spec(&s).ok())
        .map(Ok)
        .unwrap_or_else(|| {
            Model::from_tier_dynamic(
                ctx.model.provider,
                effective,
                ctx.model.dynamic_slug.as_deref(),
            )
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

type Pair<T> = (Option<T>, Option<String>);

fn err_pair<T>(err: impl ToString) -> Pair<T> {
    (None, Some(err.to_string()))
}

/// `maki.agent.*` convention: wrong argument types throw; every value or
/// runtime failure (including a ctx without dispatch capability) returns
/// `(nil, err)`.
macro_rules! try_pair {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return Ok(err_pair(e)),
        }
    };
}

async fn resolve_model(
    lua: Lua,
    (ctx, opts): (mlua::UserDataRef<LuaCtx>, Option<Table>),
) -> LuaResult<Pair<Table>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "resolve_model"));
    let tier_str = opts
        .as_ref()
        .and_then(|t| t.get::<Option<String>>("tier").ok().flatten());
    let spec_str = opts
        .as_ref()
        .and_then(|t| t.get::<Option<String>>("spec").ok().flatten());

    let model = match spec_str {
        Some(ref spec) => try_pair!(Model::from_spec(spec)),
        None => try_pair!(resolve_model_from_ctx(agent, tier_str.as_deref())),
    };
    Ok((Some(model_to_lua_table(&lua, &model)?), None))
}

async fn system_prompt(
    _lua: Lua,
    (ctx, opts): (mlua::UserDataRef<LuaCtx>, Table),
) -> LuaResult<Pair<String>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "system_prompt"));
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

    let assembled = maki_agent::prompt::assemble(prompt_id, &agent.prompt_slots, &instructions);
    Ok((Some(vars.apply(&assembled).into_owned()), None))
}

async fn tools(
    lua: Lua,
    (ctx, opts): (mlua::UserDataRef<LuaCtx>, Table),
) -> LuaResult<Pair<LuaValue>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "tools"));
    let audience_str: String = opts.get("audience")?;
    let audience = try_pair!(
        ToolAudience::parse_name(&audience_str)
            .ok_or_else(|| format!("unknown audience: {audience_str}"))
    );

    let only: Option<Vec<String>> = opts.get("only")?;
    let except: Option<Vec<String>> = opts.get("except")?;
    let include_mcp: bool = opts.get::<Option<bool>>("include_mcp")?.unwrap_or(true);
    let workflow: bool = opts.get::<Option<bool>>("workflow")?.unwrap_or(false);
    let spec_str: Option<String> = opts.get("spec")?;

    let parsed = spec_str
        .as_deref()
        .and_then(|spec| Model::from_spec(spec).ok());
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
    };
    let mut defs =
        ToolRegistry::global().definitions(&vars, &ctx_desc, model.supports_tool_examples());

    if include_mcp && let Some(ref mcp) = agent.mcp {
        mcp.extend_tools(&mut defs);
    }

    Ok((Some(json_to_lua(&lua, &defs)?), None))
}

/// Returns `(text, err)`. While the child runs, `opts.on_live_buf`
/// receives each buf it publishes (as a foreign `BufHandle`) and
/// `opts.on_annotation` every annotation, live ones and the completion
/// annotation alike. Both run on the Lua thread and may yield.
async fn call_tool(
    lua: Lua,
    (ctx, name, input, opts): (mlua::UserDataRef<LuaCtx>, String, LuaValue, Option<Table>),
) -> LuaResult<Pair<String>> {
    let input_json = lua_to_json(&lua, &input)?;
    let agent = try_pair!(dispatch_ctx(&ctx, "call_tool"));
    let mut tctx = agent.to_tool_context();
    let (mut on_buf, mut on_ann, mut rx) = (None, None, None);
    if let Some(o) = opts {
        if let Some(secs) = o.get::<Option<u64>>("timeout")? {
            tctx.deadline = Deadline::after(Duration::from_secs(secs));
        }
        on_buf = o.get::<Option<Function>>("on_live_buf")?;
        on_ann = o.get::<Option<Function>>("on_annotation")?;
        if on_buf.is_some() || on_ann.is_some() {
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

/// Must use `call_async`, not `call`: callbacks that yield (highlight,
/// markdown) hit the C-call boundary otherwise.
struct LiveCallbacks<'a> {
    tool: &'a str,
    on_buf: Option<Function>,
    on_ann: Option<Function>,
}

impl LiveCallbacks<'_> {
    async fn deliver(&self, ev: ToolLive) {
        let res = match ev {
            ToolLive::Buf(buf) => call_opt(&self.on_buf, BufHandle::foreign(buf)).await,
            ToolLive::Annotation(ann) => call_opt(&self.on_ann, ann).await,
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
    let run = tool_dispatch::run(
        &tctx.registry,
        tctx.mcp.as_ref(),
        String::new(),
        name,
        input,
        tctx,
        Emit::Silent,
    );
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
    tools: JsonValue,
    thinking: ThinkingConfig,
    fast: bool,
    mcp: Option<maki_agent::mcp::McpHandle>,
    history: History,
    sub_event_tx: EventSender,
    child_cancel: maki_agent::cancel::CancelToken,
    answer_rx: Arc<AsyncMutex<flume::Receiver<String>>>,
    answer_tx: Option<flume::Sender<String>>,
    parent_cancels: Arc<CancelMap<String>>,
    /// Stable identity for UI, cancel, and history. Falls back to a synthetic
    /// id for workflow-mode sessions (no model-issued tool call exists).
    ui_id: String,
    parent_event_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    local_tools: LocalTools,
    name: String,
    total_input: Arc<AtomicU32>,
    total_output: Arc<AtomicU32>,
    start: Instant,
    closed: bool,
}

impl SessionState {
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.parent_cancels.remove(&self.ui_id);
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let _ = self.parent_event_tx.send(AgentEvent::SubagentHistory {
            tool_use_id: self.ui_id.clone(),
            messages,
        });
        info!(
            name = %self.name,
            duration_ms = self.start.elapsed().as_millis() as u64,
            input_tokens = self.total_input.load(Ordering::Relaxed),
            output_tokens = self.total_output.load(Ordering::Relaxed),
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
            Some(mut s) => s.close(),
            // Prompt still in flight: close asynchronously so history
            // and cancel entry are never silently leaked.
            None => {
                let inner = Arc::clone(&self.inner);
                smol::spawn(async move { inner.lock().await.close() }).detach();
            }
        }
    }
}

impl UserData for LuaSession {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("prompt", |lua, this, message: String| async move {
            let inner = Arc::clone(&this.inner);
            drop(this);
            let mut guard = inner.lock().await;
            let s = &mut *guard;
            if s.closed {
                return Ok((LuaValue::Nil, Some(SESSION_CLOSED_ERR.to_owned())));
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
            if let Err(e) = result {
                return Ok((LuaValue::Nil, Some(e.to_string())));
            }

            let text = s
                .history
                .as_slice()
                .iter()
                .rev()
                .filter(|m| matches!(m.role, Role::Assistant))
                .flat_map(|m| m.content.iter())
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("(no response)")
                .to_owned();

            let tbl = lua.create_table()?;
            tbl.set("text", text)?;
            tbl.set("duration_ms", s.start.elapsed().as_millis() as u64)?;
            tbl.set("input_tokens", s.total_input.load(Ordering::Relaxed))?;
            tbl.set("output_tokens", s.total_output.load(Ordering::Relaxed))?;
            Ok((LuaValue::Table(tbl), None))
        });

        methods.add_async_method("close", |_lua, this, ()| async move {
            let inner = Arc::clone(&this.inner);
            drop(this);
            let mut s = inner.lock().await;
            s.close();
            Ok(())
        });
    }
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

/// `opts.local_tools` both advertises definitions to the model and dispatches
/// to the Lua handler, so they cannot drift apart.
async fn session(
    lua: Lua,
    (ctx, opts): (mlua::UserDataRef<LuaCtx>, Table),
) -> LuaResult<Pair<mlua::AnyUserData>> {
    let agent_ctx = try_pair!(dispatch_ctx(&ctx, "session")).clone();
    drop(ctx);
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

    let (model, provider): (Model, Arc<dyn provider::Provider>) = if let Some(ref spec) = model_spec
    {
        let mut m = try_pair!(Model::from_spec(spec));
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

    let mut local_map: HashMap<String, LocalToolFn> = HashMap::new();
    if let Some(tbl) = local_tools_tbl {
        let defs = tools_json.as_array_mut().expect("checked above");
        for pair in tbl.pairs::<String, Table>() {
            let (name, spec) = pair?;
            let description = try_pair!(
                spec.get::<String>("description")
                    .map_err(|_| format!("local_tools.{name}: 'description' is required"))
            );
            let input_schema = lua_to_json(&lua, &spec.get::<LuaValue>("input_schema")?)?;
            let handler = try_pair!(
                spec.get::<Function>("handler")
                    .map_err(|_| format!("local_tools.{name}: 'handler' is required"))
            );
            defs.push(serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": input_schema,
            }));
            let weak = lua.weak();
            local_map.insert(
                name,
                Arc::new(move |input: &JsonValue| call_local_tool(&weak, &handler, input))
                    as LocalToolFn,
            );
        }
    }

    let thinking = match thinking_val {
        Some(LuaValue::String(s)) => match s.to_str()?.as_ref() {
            "off" => ThinkingConfig::Off,
            "adaptive" => ThinkingConfig::Adaptive,
            other => return Ok(err_pair(format!("invalid thinking: {other}"))),
        },
        Some(LuaValue::Integer(n)) => ThinkingConfig::Budget(n as u32),
        Some(LuaValue::Number(n)) => ThinkingConfig::Budget(n as u32),
        Some(_) => return Err(mlua::Error::runtime("thinking must be string or number")),
        None => agent_ctx.opts.thinking,
    };

    let session_id = MakiId::generate();
    let (sub_tx, sub_rx) = flume::unbounded::<Envelope>();
    let sub_event_tx = EventSender::new(sub_tx, agent_ctx.event_tx.run_id());
    let parent_tx = agent_ctx.event_tx.clone();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();

    let subagent_info: Arc<OnceLock<SubagentInfo>> = Arc::new(OnceLock::new());
    let total_input = Arc::new(AtomicU32::new(0));
    let total_output = Arc::new(AtomicU32::new(0));

    {
        let info = Arc::clone(&subagent_info);
        let ti = Arc::clone(&total_input);
        let to = Arc::clone(&total_output);
        let parent_tx = parent_tx.clone();
        smol::spawn(async move {
            while let Ok(mut envelope) = sub_rx.recv_async().await {
                match &envelope.event {
                    AgentEvent::Done { usage, .. } => {
                        ti.fetch_add(usage.total_input(), Ordering::Relaxed);
                        to.fetch_add(usage.output, Ordering::Relaxed);
                        continue;
                    }
                    AgentEvent::Error { .. }
                    | AgentEvent::ToolOutput { .. }
                    | AgentEvent::ToolPending { .. }
                    | AgentEvent::SubagentHistory { .. } => continue,
                    _ => {}
                }
                envelope.subagent = info.get().cloned();
                let _ = parent_tx.send_envelope(envelope);
            }
        })
        .detach();
    }

    // Register a cancel trigger so the child token does not fire on drop
    // and kill the subagent at birth.
    let ui_id = agent_ctx
        .tool_use_id
        .clone()
        .unwrap_or_else(|| format!("session-{session_id}"));
    let (child_trigger, child_cancel) = agent_ctx.cancel.child();
    agent_ctx
        .subagent_cancels
        .insert(ui_id.clone(), child_trigger);

    let name = name.unwrap_or_default();
    info!(name = %name, model = %model.id, "subagent session opened");

    let state = SessionState {
        params: AgentParams {
            provider,
            model,
            config: agent_ctx.config.clone(),
            tool_output_lines: maki_config::ToolOutputLines::default(),
            permissions: Arc::clone(&agent_ctx.permissions),
            session_id: Some(session_id.into()),
            timeouts: agent_ctx.timeouts,
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::clone(&agent_ctx.prompt_slots),
            subagent_cancels: Arc::new(CancelMap::new()),
            registry: Arc::clone(maki_agent::tools::ToolRegistry::global_arc()),
            audience,
        },
        system: system.unwrap_or_default(),
        tools: tools_json,
        thinking,
        fast,
        mcp: agent_ctx.mcp.clone(),
        history: History::new(Vec::new()),
        sub_event_tx,
        child_cancel,
        answer_rx: Arc::new(AsyncMutex::new(answer_rx)),
        answer_tx: Some(answer_tx),
        parent_cancels: Arc::clone(&agent_ctx.subagent_cancels),
        ui_id,
        parent_event_tx: parent_tx,
        subagent_info,
        local_tools: Arc::new(local_map),
        name,
        total_input,
        total_output,
        start: Instant::now(),
        closed: false,
    };

    let sess = lua.create_userdata(LuaSession {
        inner: Arc::new(AsyncMutex::new(state)),
    })?;
    Ok((Some(sess), None))
}

#[cfg(test)]
mod tests {
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
}
