use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::agent::LoadedInstructions;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::{Deadline, FileReadTracker, LocalTools, ToolContext, ToolLive};
use maki_config::{AgentConfig, ToolOutputLines};
use mlua::{Lua, LuaSerdeExt, MultiValue, Table, UserData, UserDataMethods, Value as LuaValue};

use crate::api::tool::ToolCallReply;
use crate::api::ui::buf::BufHandle;
use crate::runtime::{active_task, lock_cell};

const DEADLINE_ALREADY_SET_MSG: &str = "ctx:set_deadline() already called";

/// The restore ctx is a plain table, not a userdata, so a plugin that
/// drives another tool's restore (batch composes children this way) can
/// build the same shape itself.
pub(crate) fn restore_ctx(
    lua: &Lua,
    tool_output_lines: ToolOutputLines,
    state: Option<serde_json::Value>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "tool_output_lines",
        lua.create_function(move |lua, _: MultiValue| lua.to_value(&tool_output_lines))?,
    )?;
    t.set(
        "state",
        lua.create_function(move |lua, _: MultiValue| match &state {
            Some(v) => crate::api::util::convert::json_to_lua(lua, v),
            None => Ok(LuaValue::Nil),
        })?,
    )?;
    Ok(t)
}

/// The `start` hook runs before permission checks, so its ctx only lets a
/// tool publish a preview; dispatching tools from it is structurally
/// impossible.
pub(crate) struct StartCtx {
    pub(crate) tool_output_lines: ToolOutputLines,
}

impl UserData for StartCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("live_buf", |lua, _this, buf: mlua::AnyUserData| {
            send_live_buf(lua, &buf)
        });
        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });
    }
}

fn send_live_buf(lua: &mlua::Lua, buf: &mlua::AnyUserData) -> mlua::Result<()> {
    let shared = buf.borrow::<BufHandle>().map(|h| Arc::clone(&h.buf))?;
    let task = active_task(lua);
    let (live, sink) = {
        let mut cell = lock_cell(&task);
        cell.root_buf = Some(Arc::clone(&shared));
        (cell.live.clone(), cell.live_sink.clone())
    };
    if let Some(live) = live {
        let _ = live.event_tx.send(maki_agent::AgentEvent::LiveToolBuf {
            id: live.tool_use_id.clone(),
            body: Arc::clone(&shared),
        });
    }
    if let Some(sink) = sink {
        let _ = sink.send(ToolLive::Buf(shared));
    }
    Ok(())
}

/// Captured snapshot of the parent `ToolContext`. Per-call state (deadline,
/// instructions, output lines) is reset so child calls start clean.
#[derive(Clone)]
pub(crate) struct AgentContext(ToolContext);

impl From<&ToolContext> for AgentContext {
    fn from(ctx: &ToolContext) -> Self {
        let mut c = ctx.clone();
        c.loaded_instructions = LoadedInstructions::new();
        c.deadline = Deadline::None;
        c.tool_output_lines = ToolOutputLines::default();
        c.local_tools = LocalTools::default();
        Self(c)
    }
}

impl Deref for AgentContext {
    type Target = ToolContext;
    fn deref(&self) -> &ToolContext {
        &self.0
    }
}

impl AgentContext {
    /// Drops `tool_use_id` so an inner tool never emits UI events under the
    /// outer call's id, and `live_sink` so a grandchild never streams into
    /// a sink meant for its parent.
    pub(crate) fn to_tool_context(&self) -> ToolContext {
        let mut c = self.0.clone();
        c.tool_use_id = None;
        c.live_sink = None;
        c
    }
}

pub(crate) struct LuaCtx {
    pub(crate) cancel: CancelToken,
    pub(crate) config: AgentConfig,
    pub(crate) tool_output_lines: ToolOutputLines,
    pub(crate) finish_tx: Option<flume::Sender<ToolCallReply>>,
    pub(crate) file_tracker: Arc<FileReadTracker>,
    pub(crate) loaded_instructions: LoadedInstructions,
    pub(crate) agent: AgentContext,
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("workflow", |_, this, ()| Ok(this.agent.workflow));

        methods.add_method("audience", |_, this, ()| {
            Ok(this.agent.audience.name().unwrap_or("main"))
        });

        methods.add_method("live_buf", |lua, _this, buf: mlua::AnyUserData| {
            send_live_buf(lua, &buf)
        });

        methods.add_method("config", |lua, this, args: MultiValue| {
            let config_val = lua.to_value(&this.config)?;
            if args.is_empty() {
                return Ok(config_val);
            }
            let key: String = lua.from_value(args[0].clone())?;
            let default = args.get(1).cloned().unwrap_or(LuaValue::Nil);
            match config_val {
                LuaValue::Table(ref tbl) => {
                    let val = tbl.raw_get::<LuaValue>(key.as_str())?;
                    if matches!(val, LuaValue::Nil) {
                        Ok(default)
                    } else {
                        Ok(val)
                    }
                }
                _ => Ok(default),
            }
        });

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

        methods.add_method_mut("finish", |lua, this, val: LuaValue| {
            let tx = this
                .finish_tx
                .take()
                .ok_or_else(|| mlua::Error::runtime("ctx:finish() already called"))?;

            if let Some(buf) = crate::api::ui::buf::buf_from_reply(&val) {
                lock_cell(&active_task(lua)).root_buf = Some(buf);
            }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use maki_agent::AgentMode;
    use maki_agent::tools::LocalToolFn;
    use maki_agent::tools::test_support::stub_ctx_with;

    use super::*;

    const TOOL_USE_ID: &str = "tu-1";
    const INSTRUCTION_PATH: &str = "/tmp/nested/AGENTS.md";
    const LOCAL_TOOL_NAME: &str = "sess_tool";

    fn populated_ctx() -> ToolContext {
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(TOOL_USE_ID));
        ctx.deadline = Deadline::after(Duration::from_secs(60));
        ctx.tool_output_lines = ToolOutputLines {
            bash: 999,
            ..ToolOutputLines::default()
        };
        assert!(
            !ctx.loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH))
        );
        let mut tools: HashMap<String, LocalToolFn> = HashMap::new();
        tools.insert(
            LOCAL_TOOL_NAME.into(),
            Arc::new(|_: &serde_json::Value| Ok(String::new())) as LocalToolFn,
        );
        ctx.local_tools = Arc::new(tools);
        ctx.live_sink = Some(flume::unbounded().0);
        ctx
    }

    #[test]
    fn agent_context_keeps_tool_use_id_and_resets_per_call_state() {
        let agent = AgentContext::from(&populated_ctx());
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert!(matches!(agent.deadline, Deadline::None));
        assert_eq!(agent.tool_output_lines, ToolOutputLines::default());
        assert!(agent.local_tools.is_empty());
        assert!(
            !agent
                .loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH)),
            "loaded_instructions must be a fresh set, not a shared clone"
        );
    }

    #[test]
    fn agent_context_to_tool_context_drops_tool_use_id_and_sink() {
        let agent = AgentContext::from(&populated_ctx());
        assert!(
            agent.live_sink.is_some(),
            "the sink set by the caller must survive into AgentContext"
        );
        let inner = agent.to_tool_context();
        assert_eq!(inner.tool_use_id, None);
        assert!(inner.live_sink.is_none(), "sink must not be inherited");
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
    }
}
