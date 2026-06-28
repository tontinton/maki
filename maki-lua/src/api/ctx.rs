use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::EventSender;
use maki_agent::agent::LoadedInstructions;
use maki_agent::cancel::{CancelMap, CancelToken};
use maki_agent::mcp::McpHandle;
use maki_agent::permissions::PermissionManager;
use maki_agent::prompt::ResolvedSlots;
use maki_agent::tools::FileReadTracker;
use maki_config::{AgentConfig, ToolOutputLines};
use maki_providers::provider::Provider;
use maki_providers::{Model, RequestOptions, Timeouts};
use mlua::{LuaSerdeExt, UserData, UserDataMethods, Value as LuaValue};

use crate::api::tool::ToolCallReply;
use crate::runtime::active_task;

const DEADLINE_ALREADY_SET_MSG: &str = "ctx:set_deadline() already called";

pub(crate) struct RestoreCtx {
    pub(crate) tool_output_lines: ToolOutputLines,
}

impl UserData for RestoreCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });
    }
}

pub(crate) struct AgentContext {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: Arc<Model>,
    pub(crate) event_tx: EventSender,
    pub(crate) tool_use_id: Option<String>,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) timeouts: Timeouts,
    pub(crate) prompt_slots: Arc<ResolvedSlots>,
    pub(crate) opts: RequestOptions,
    pub(crate) subagent_cancels: Arc<CancelMap<String>>,
    pub(crate) cancel: CancelToken,
    pub(crate) mcp: Option<McpHandle>,
    pub(crate) config: AgentConfig,
}

impl From<&maki_agent::tools::ToolContext> for AgentContext {
    fn from(ctx: &maki_agent::tools::ToolContext) -> Self {
        Self {
            provider: Arc::clone(&ctx.provider),
            model: Arc::clone(&ctx.model),
            event_tx: ctx.event_tx.clone(),
            tool_use_id: ctx.tool_use_id.clone(),
            permissions: Arc::clone(&ctx.permissions),
            timeouts: ctx.timeouts,
            prompt_slots: Arc::clone(&ctx.prompt_slots),
            opts: ctx.opts,
            subagent_cancels: Arc::clone(&ctx.subagent_cancels),
            cancel: ctx.cancel.clone(),
            mcp: ctx.mcp.clone(),
            config: ctx.config.clone(),
        }
    }
}

impl UserData for AgentContext {}

pub(crate) struct LuaCtx {
    pub(crate) cancel: CancelToken,
    pub(crate) config: AgentConfig,
    pub(crate) tool_output_lines: ToolOutputLines,
    pub(crate) finish_tx: Option<flume::Sender<ToolCallReply>>,
    pub(crate) file_tracker: Arc<FileReadTracker>,
    pub(crate) loaded_instructions: LoadedInstructions,
    pub(crate) agent: Option<AgentContext>,
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("config", |lua, this, ()| lua.to_value(&this.config));

        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });

        methods.add_method("set_deadline", |lua, _this, secs: u64| {
            let handle = active_task(lua);
            let cell = handle.lock().unwrap_or_else(|e| e.into_inner());
            if cell.deadline_secs.get().is_some() {
                return Err(mlua::Error::runtime(DEADLINE_ALREADY_SET_MSG));
            }
            cell.deadline_secs.set(Some(secs));
            cell.deadline
                .set(Some(Instant::now() + Duration::from_secs(secs)));
            Ok(())
        });

        methods.add_method("record_read", |_, this, path: String| {
            this.file_tracker.record_read(Path::new(&path));
            Ok(())
        });

        methods.add_method("check_before_edit", |_, this, path: String| {
            match this.file_tracker.check_before_edit(Path::new(&path)) {
                Ok(()) => Ok((true, Option::<String>::None)),
                Err(msg) => Ok((false, Some(msg))),
            }
        });

        methods.add_async_method(
            "find_instructions",
            |lua, this, dir_path: String| async move {
                let loaded = this.loaded_instructions.clone();
                let results = smol::unblock(move || {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let abs = resolve_abs_with_cwd(dir_path, &cwd);
                    maki_agent::find_subdirectory_instructions(&abs, &cwd, &loaded)
                })
                .await;
                let tbl = lua.create_table()?;
                for (i, (path, content)) in results.into_iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("path", path)?;
                    entry.set("content", content)?;
                    tbl.set(i + 1, entry)?;
                }
                Ok(tbl)
            },
        );

        methods.add_method("is_instruction_file", |_, _, name: String| {
            Ok(maki_agent::is_instruction_file(&name))
        });

        methods.add_method_mut("agent_context", |_, this, ()| {
            this.agent
                .take()
                .ok_or_else(|| mlua::Error::runtime("agent context not available"))
        });

        methods.add_method_mut("finish", |_lua, this, val: LuaValue| {
            let tx = this
                .finish_tx
                .take()
                .ok_or_else(|| mlua::Error::runtime("ctx:finish() already called"))?;

            let _ = tx.send(ToolCallReply::from_lua_value(&val));
            Ok(())
        });
    }
}

fn resolve_abs_with_cwd(path: String, cwd: &Path) -> PathBuf {
    if Path::new(&path).is_absolute() {
        path.into()
    } else {
        cwd.join(&path)
    }
}
