use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use maki_agent::cancel::CancelMap;
use maki_agent::tools::registry::ToolRegistry;
use maki_agent::tools::{DescriptionContext, FileReadTracker, ToolAudience, ToolFilter};
use maki_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope, EventSender,
    History, SubagentInfo,
};
use maki_providers::model::ModelTier;
use maki_providers::provider;
use maki_providers::{ContentBlock, Model, ModelError, Role, ThinkingConfig};
use mlua::{Lua, Result as LuaResult, Table, Value as LuaValue};
use serde_json::Value as JsonValue;
use tracing::info;
use uuid::Uuid;

use crate::api::util::convert::{json_to_lua, lua_to_json};
use crate::api::util::ctx::AgentContext;

pub(crate) fn register(lua: &Lua, maki: &Table) -> LuaResult<()> {
    let agent = lua.create_table()?;

    agent.set("resolve_model", lua.create_async_function(resolve_model)?)?;
    agent.set("system_prompt", lua.create_async_function(system_prompt)?)?;
    agent.set("tools", lua.create_async_function(tools)?)?;
    agent.set("run", lua.create_async_function(run)?)?;

    maki.set("agent", agent)?;
    Ok(())
}

fn resolve_model_from_ctx(ctx: &AgentContext, tier: Option<&str>) -> Result<Model, mlua::Error> {
    let Some(tier_str) = tier else {
        return Ok(Model::clone(&ctx.model));
    };
    let requested: ModelTier = tier_str
        .parse()
        .map_err(|e: ModelError| mlua::Error::runtime(e))?;
    let effective = requested.min(ctx.model.tier);
    if effective == ctx.model.tier {
        return Ok(Model::clone(&ctx.model));
    }
    let map = maki_providers::model_registry::model_registry()
        .read()
        .unwrap();
    map.spec_for_tier(ctx.model.provider, effective)
        .or_else(|| map.spec_for_tier_any(effective))
        .and_then(|s| Model::from_spec(&s).ok())
        .map(Ok)
        .unwrap_or_else(|| {
            Model::from_tier_dynamic(
                ctx.model.provider,
                effective,
                ctx.model.dynamic_slug.as_deref(),
            )
            .map_err(mlua::Error::runtime)
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

async fn resolve_model(
    lua: Lua,
    (agent_ctx, opts): (mlua::UserDataRef<AgentContext>, Option<Table>),
) -> LuaResult<Table> {
    let tier_str = opts
        .as_ref()
        .and_then(|t| t.get::<Option<String>>("tier").ok().flatten());
    let spec_str = opts
        .as_ref()
        .and_then(|t| t.get::<Option<String>>("spec").ok().flatten());

    let model = if let Some(ref spec) = spec_str {
        Model::from_spec(spec).map_err(mlua::Error::runtime)?
    } else {
        resolve_model_from_ctx(&agent_ctx, tier_str.as_deref())?
    };

    model_to_lua_table(&lua, &model)
}

async fn system_prompt(
    _lua: Lua,
    (agent_ctx, opts): (mlua::UserDataRef<AgentContext>, Table),
) -> LuaResult<String> {
    let prompt_id_str: String = opts.get("prompt_id")?;
    let prompt_id = match prompt_id_str.as_str() {
        "research" => maki_agent::prompt::PromptId::Research,
        "general" => maki_agent::prompt::PromptId::General,
        "system" => maki_agent::prompt::PromptId::System,
        other => return Err(mlua::Error::runtime(format!("unknown prompt_id: {other}"))),
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

    let assembled = maki_agent::prompt::assemble(prompt_id, &agent_ctx.prompt_slots, &instructions);
    Ok(vars.apply(&assembled).into_owned())
}

async fn tools(
    lua: Lua,
    (agent_ctx, opts): (mlua::UserDataRef<AgentContext>, Table),
) -> LuaResult<LuaValue> {
    let audience_str: String = opts.get("audience")?;
    let audience = match audience_str.as_str() {
        "main" => ToolAudience::MAIN,
        "research_sub" => ToolAudience::RESEARCH_SUB,
        "general_sub" => ToolAudience::GENERAL_SUB,
        other => return Err(mlua::Error::runtime(format!("unknown audience: {other}"))),
    };

    let only: Option<Vec<String>> = opts.get("only")?;
    let except: Option<Vec<String>> = opts.get("except")?;
    let include_mcp: bool = opts.get::<Option<bool>>("include_mcp")?.unwrap_or(true);
    let spec_str: Option<String> = opts.get("spec")?;

    let supports_examples = if let Some(ref spec) = spec_str {
        Model::from_spec(spec)
            .map(|m| m.supports_tool_examples())
            .unwrap_or(false)
    } else {
        agent_ctx.model.supports_tool_examples()
    };

    let snapshot = ToolRegistry::native().iter();
    let allowed: Vec<String> = snapshot
        .iter()
        .filter(|e| {
            e.tool.audience().contains(audience)
                && maki_agent::tools::is_tool_enabled(&agent_ctx.config, e.name())
        })
        .map(|e| e.name().to_owned())
        .collect();

    let filter = match (only, except) {
        (Some(o), _) => {
            let intersected: Vec<String> = o.into_iter().filter(|n| allowed.contains(n)).collect();
            ToolFilter::Only(intersected)
        }
        (_, Some(e)) => {
            let filtered: Vec<String> = allowed.into_iter().filter(|n| !e.contains(n)).collect();
            ToolFilter::Only(filtered)
        }
        _ => ToolFilter::Only(allowed),
    };

    let vars = maki_agent::template::env_vars();
    let ctx_desc = DescriptionContext { filter: &filter };
    let mut defs = ToolRegistry::native().definitions(&vars, &ctx_desc, supports_examples);

    if include_mcp && let Some(ref mcp) = agent_ctx.mcp {
        mcp.extend_tools(&mut defs);
    }

    json_to_lua(&lua, &defs)
}

async fn run(lua: Lua, (agent_ctx_ud, opts): (mlua::AnyUserData, Table)) -> LuaResult<Table> {
    let agent_ctx: AgentContext = agent_ctx_ud.take()?;
    let prompt: String = opts.get("prompt")?;
    let model_spec: Option<String> = opts.get("model_spec")?;
    let system: Option<String> = opts.get("system")?;
    let tools_val: Option<LuaValue> = opts.get("tools")?;
    let name: Option<String> = opts.get("name")?;
    let thinking_val: Option<LuaValue> = opts.get("thinking")?;
    let fast: bool = opts
        .get::<Option<bool>>("fast")?
        .unwrap_or(agent_ctx.opts.fast);

    let (model, provider): (Model, Arc<dyn provider::Provider>) = if let Some(ref spec) = model_spec
    {
        let mut m = Model::from_spec(spec).map_err(mlua::Error::runtime)?;
        let p = provider::from_model_async(&mut m, agent_ctx.timeouts)
            .await
            .map_err(mlua::Error::runtime)?;
        (m, Arc::from(p))
    } else {
        (
            Model::clone(&agent_ctx.model),
            Arc::clone(&agent_ctx.provider),
        )
    };

    let tools_json: JsonValue = if let Some(val) = tools_val {
        lua_to_json(&val)?
    } else {
        JsonValue::Array(vec![])
    };

    let system_prompt = system.unwrap_or_default();

    let thinking = match thinking_val {
        Some(LuaValue::String(s)) => match s.to_str()?.as_ref() {
            "off" => ThinkingConfig::Off,
            "adaptive" => ThinkingConfig::Adaptive,
            other => return Err(mlua::Error::runtime(format!("invalid thinking: {other}"))),
        },
        Some(LuaValue::Integer(n)) => ThinkingConfig::Budget(n as u32),
        Some(LuaValue::Number(n)) => ThinkingConfig::Budget(n as u32),
        Some(_) => return Err(mlua::Error::runtime("thinking must be string or number")),
        None => agent_ctx.opts.thinking,
    };

    let description = name.as_deref().unwrap_or_default();

    let session_id = Uuid::new_v4().to_string();
    let (sub_tx, sub_rx) = flume::unbounded::<Envelope>();
    let sub_event_tx = EventSender::new(sub_tx, agent_ctx.event_tx.run_id());
    let parent_tx = agent_ctx.event_tx.clone();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let answer_rx = Arc::new(async_lock::Mutex::new(answer_rx));

    let subagent_info = agent_ctx.tool_use_id.as_ref().map(|id| SubagentInfo {
        parent_tool_use_id: id.clone(),
        name: description.to_owned(),
        prompt: Some(prompt.clone()),
        model: Some(model.spec()),
        answer_tx: Some(answer_tx),
    });

    let total_input = Arc::new(AtomicU32::new(0));
    let total_output = Arc::new(AtomicU32::new(0));
    let ti = Arc::clone(&total_input);
    let to = Arc::clone(&total_output);

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
            envelope.subagent = subagent_info.clone();
            let _ = parent_tx.send_envelope(envelope);
        }
    })
    .detach();

    let (child_trigger, child_cancel) = agent_ctx.cancel.child();
    if let Some(ref id) = agent_ctx.tool_use_id {
        agent_ctx.subagent_cancels.insert(id.clone(), child_trigger);
    } else {
        drop(child_trigger);
    }

    let input = AgentInput {
        message: prompt.clone(),
        mode: AgentMode::Build,
        thinking,
        fast,
        ..Default::default()
    };

    info!(
        description = %description,
        model = %model.id,
        "subagent spawning",
    );

    let mut history = History::new(Vec::new());
    let mut agent = Agent::new(
        AgentParams {
            provider,
            model,
            config: agent_ctx.config.clone(),
            tool_output_lines: maki_config::ToolOutputLines::default(),
            permissions: Arc::clone(&agent_ctx.permissions),
            session_id: Some(session_id),
            timeouts: agent_ctx.timeouts,
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::clone(&agent_ctx.prompt_slots),
            subagent_cancels: Arc::new(CancelMap::new()),
            registry: Arc::clone(maki_agent::tools::ToolRegistry::native_arc()),
        },
        AgentRunParams {
            history: &mut history,
            system: system_prompt,
            event_tx: sub_event_tx,
            tools: tools_json,
        },
    )
    .with_user_response_rx(answer_rx)
    .with_cancel(child_cancel)
    .with_mcp(agent_ctx.mcp.clone());

    let start = Instant::now();
    let result = agent.run(input).await;
    let duration_ms = start.elapsed().as_millis() as u64;
    drop(agent);

    if let Some(ref id) = agent_ctx.tool_use_id {
        agent_ctx.subagent_cancels.remove(id);
    }

    let success = result.is_ok();
    info!(description = %description, duration_ms, success, "subagent completed");
    result.map_err(|e| mlua::Error::runtime(format!("sub-agent error: {e}")))?;

    let messages = history.into_vec();
    let text = messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role, Role::Assistant))
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("(no response)")
        .to_string();

    if let Some(tool_use_id) = agent_ctx.tool_use_id.clone() {
        let _ = agent_ctx.event_tx.send(AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
        });
    }

    let tbl = lua.create_table()?;
    tbl.set("text", text)?;
    tbl.set("duration_ms", duration_ms)?;
    tbl.set("input_tokens", total_input.load(Ordering::Relaxed))?;
    tbl.set("output_tokens", total_output.load(Ordering::Relaxed))?;
    Ok(tbl)
}
