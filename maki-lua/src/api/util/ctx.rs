use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::agent::LoadedInstructions;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::{Deadline, FileKey, MAIN_TASK_ID, ToolAudience, ToolContext, ToolLive};
use maki_config::{AgentConfig, ToolOutputLines};
use maki_storage::id::SessionRef;
use mlua::{LuaSerdeExt, MultiValue, UserData, UserDataMethods, Value as LuaValue};

use crate::api::tool::ToolCallReply;
use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::Pair;
use crate::runtime::{RestoreReason, active_task, lock_cell};

const DEADLINE_ALREADY_SET_MSG: &str = "ctx:set_deadline() already called";

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
///
/// Routing state is kept verbatim, `local_tools` included: a name a Lua tool
/// dispatches must land where the same name lands when the model calls it, or
/// shadowing quietly inverts for nested calls. A child session does not inherit
/// them anyway, `session()` always sets its own.
#[derive(Clone)]
pub(crate) struct AgentContext(ToolContext);

impl From<&ToolContext> for AgentContext {
    fn from(ctx: &ToolContext) -> Self {
        let mut c = ctx.clone();
        c.loaded_instructions = LoadedInstructions::new();
        c.deadline = Deadline::None;
        c.tool_output_lines = ToolOutputLines::default();
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

/// One ctx type for handler, `start`, and restore invocations. Each kind's
/// capabilities live in its `Caps` variant, so a capability exists exactly
/// when its data does. Methods a kind lacks return `(nil, err)` instead of
/// not existing, so callers can probe without pcall.
pub(crate) struct LuaCtx {
    caps: Caps,
    pub(crate) cancel: CancelToken,
    tool_output_lines: ToolOutputLines,
    /// Which chat the run serves, see [`ToolContext::session_id`] and
    /// [`ToolContext::task_id`]. Restore takes both from the chat being
    /// re-rendered.
    session_id: Option<SessionRef>,
    task_id: Option<Arc<str>>,
    pub(crate) finish_tx: Option<flume::Sender<ToolCallReply>>,
}

#[allow(clippy::large_enum_variant)]
enum Caps {
    Handler {
        agent: Box<AgentContext>,
        /// Kept apart from `agent`, which resets its copy so child calls
        /// start with a clean instruction set.
        loaded_instructions: LoadedInstructions,
    },
    /// `start` runs before permission checks: it reads config and publishes
    /// previews, but dispatching tools is structurally impossible.
    Start {
        config: AgentConfig,
        workflow: bool,
        audience: ToolAudience,
    },
    Restore {
        state: Option<serde_json::Value>,
        reason: RestoreReason,
    },
}

/// What a restore run knows about the call it re-renders: the host fills it
/// from a `RestoreItem`, batch from the plain table it hands its children.
#[derive(Default)]
pub(crate) struct RestoreCtx {
    pub(crate) tool_output_lines: ToolOutputLines,
    pub(crate) state: Option<serde_json::Value>,
    pub(crate) session_id: Option<SessionRef>,
    pub(crate) task_id: Option<Arc<str>>,
    pub(crate) reason: RestoreReason,
}

impl LuaCtx {
    fn new(ctx: &ToolContext, caps: Caps) -> Self {
        Self {
            caps,
            cancel: ctx.cancel.clone(),
            tool_output_lines: ctx.tool_output_lines,
            session_id: ctx.session_id.clone(),
            task_id: ctx.task_id.clone(),
            finish_tx: None,
        }
    }

    pub(crate) fn handler(ctx: &ToolContext) -> Self {
        Self::new(
            ctx,
            Caps::Handler {
                agent: Box::new(AgentContext::from(ctx)),
                loaded_instructions: ctx.loaded_instructions.clone(),
            },
        )
    }

    pub(crate) fn start(ctx: &ToolContext) -> Self {
        Self::new(
            ctx,
            Caps::Start {
                config: ctx.config.clone(),
                workflow: ctx.workflow,
                audience: ctx.audience,
            },
        )
    }

    pub(crate) fn restore(ctx: RestoreCtx) -> Self {
        Self {
            caps: Caps::Restore {
                state: ctx.state,
                reason: ctx.reason,
            },
            cancel: CancelToken::none(),
            tool_output_lines: ctx.tool_output_lines,
            session_id: ctx.session_id,
            task_id: ctx.task_id,
            finish_tx: None,
        }
    }

    /// Sandbox is active only for tool handlers whose context carries the
    /// live agent config; start and restore caps have none.
    pub(crate) fn sandbox_enabled(&self) -> bool {
        matches!(
            &self.caps,
            Caps::Handler { agent, .. } if agent.config.sandbox_enabled
        )
    }

    /// Dispatch capability: only handler ctxs can call `maki.agent.*`.
    pub(crate) fn agent(&self) -> Option<&AgentContext> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(agent),
            _ => None,
        }
    }

    fn config(&self) -> Option<&AgentConfig> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(&agent.config),
            Caps::Start { config, .. } => Some(config),
            Caps::Restore { .. } => None,
        }
    }

    fn workflow(&self) -> Option<bool> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(agent.workflow),
            Caps::Start { workflow, .. } => Some(*workflow),
            Caps::Restore { .. } => None,
        }
    }

    fn audience(&self) -> Option<ToolAudience> {
        match &self.caps {
            Caps::Handler { agent, .. } => Some(agent.audience),
            Caps::Start { audience, .. } => Some(*audience),
            Caps::Restore { .. } => None,
        }
    }

    fn restore_reason(&self) -> Option<RestoreReason> {
        match &self.caps {
            Caps::Restore { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    fn task_id(&self) -> &str {
        self.task_id.as_deref().unwrap_or(MAIN_TASK_ID)
    }

    fn loaded_instructions(&self) -> Option<&LoadedInstructions> {
        match &self.caps {
            Caps::Handler {
                loaded_instructions,
                ..
            } => Some(loaded_instructions),
            _ => None,
        }
    }

    fn state(&self) -> Option<&serde_json::Value> {
        match &self.caps {
            Caps::Restore { state, .. } => state.as_ref(),
            _ => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self.caps {
            Caps::Handler { .. } => "handler",
            Caps::Start { .. } => "start",
            Caps::Restore { .. } => "restore",
        }
    }

    pub(crate) fn cap_err(&self, method: &str) -> String {
        format!("{method} not available in {} ctx", self.kind())
    }

    fn cap_err_pair<T>(&self, method: &str) -> Pair<T> {
        (None, Some(self.cap_err(method)))
    }
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("workflow", |_, this, ()| {
            let Some(workflow) = this.workflow() else {
                return Ok(this.cap_err_pair("workflow"));
            };
            Ok((Some(workflow), None))
        });

        methods.add_method("audience", |_, this, ()| {
            let Some(audience) = this.audience() else {
                return Ok(this.cap_err_pair("audience"));
            };
            Ok((Some(audience.name().unwrap_or("main").to_string()), None))
        });

        // The session that called this tool, or whose transcript a restore
        // re-renders, which under concurrent sessions is not always the
        // focused one `maki.session.current()` reports. Nil when the run has
        // no session, as in the `maki index` one-shot.
        methods.add_method("session_id", |_, this, ()| {
            Ok(this.session_id.as_ref().map(|id| id.id().to_string()))
        });

        methods.add_method("task_id", |_, this, ()| Ok(this.task_id().to_owned()));

        methods.add_method("restore_reason", |_, this, ()| {
            let Some(reason) = this.restore_reason() else {
                return Ok(this.cap_err_pair("restore_reason"));
            };
            Ok((Some(<&str>::from(reason)), None))
        });

        methods.add_method("live_buf", |lua, this, buf: mlua::AnyUserData| {
            if matches!(this.caps, Caps::Restore { .. }) {
                return Ok(this.cap_err_pair("live_buf"));
            }
            send_live_buf(lua, &buf)?;
            Ok((Some(true), None))
        });

        methods.add_method("config", |lua, this, args: MultiValue| {
            let Some(config) = this.config() else {
                return Ok(this.cap_err_pair("config"));
            };
            let config_val = lua.to_value(config)?;
            if args.is_empty() {
                return Ok((Some(config_val), None));
            }
            let key: String = lua.from_value(args[0].clone())?;
            let default = args.get(1).cloned().unwrap_or(LuaValue::Nil);
            let val = match config_val {
                LuaValue::Table(ref tbl) => {
                    let val = tbl.raw_get::<LuaValue>(key.as_str())?;
                    if matches!(val, LuaValue::Nil) {
                        default
                    } else {
                        val
                    }
                }
                _ => default,
            };
            Ok((Some(val), None))
        });

        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });

        methods.add_method("state", |lua, this, ()| match this.state() {
            Some(v) => json_to_lua(lua, v),
            None => Ok(LuaValue::Nil),
        });

        methods.add_method("set_deadline", |lua, this, secs: u64| {
            if !matches!(this.caps, Caps::Handler { .. }) {
                return Ok(this.cap_err_pair("set_deadline"));
            }
            let handle = active_task(lua);
            let cell = handle.lock().unwrap_or_else(|e| e.into_inner());
            if cell.deadline_secs.get().is_some() {
                return Err(mlua::Error::runtime(DEADLINE_ALREADY_SET_MSG));
            }
            cell.deadline_secs.set(Some(secs));
            cell.deadline
                .set(Some(Instant::now() + Duration::from_secs(secs)));
            cell.deadline_changed.notify(usize::MAX);
            Ok((Some(true), None))
        });

        // The matching check before a write is not exposed: the dispatcher
        // runs it under the file lock for every tool declaring `mutable_path`,
        // and a handler-side copy would race its own sibling.
        methods.add_method("record_read", |_, this, path: String| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("record_read"));
            };
            agent
                .file_access
                .record_read(&FileKey::new(Path::new(&path)));
            Ok((Some(true), None))
        });

        methods.add_async_method(
            "find_instructions",
            |lua, this, dir_path: String| async move {
                let Some(loaded) = this.loaded_instructions().cloned() else {
                    return Ok(this.cap_err_pair("find_instructions"));
                };
                // Nothing may hold the ctx borrow across the wait: a cancel
                // hook firing meanwhile needs `ctx:finish`, which takes it
                // mutably.
                drop(this);
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
                Ok((Some(tbl), None))
            },
        );

        methods.add_method("is_instruction_file", |_, _, name: String| {
            Ok(maki_agent::is_instruction_file(&name))
        });

        methods.add_method_mut("finish", |lua, this, val: LuaValue| {
            if !matches!(this.caps, Caps::Handler { .. }) {
                return Ok(this.cap_err_pair("finish"));
            }
            let tx = this
                .finish_tx
                .take()
                .ok_or_else(|| mlua::Error::runtime("ctx:finish() already called"))?;

            if let Some(buf) = crate::api::ui::buf::buf_from_reply(&val) {
                lock_cell(&active_task(lua)).root_buf = Some(buf);
            }
            let _ = tx.send(ToolCallReply::from_lua_value(lua, &val));
            Ok((Some(true), None))
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
    use maki_agent::tools::test_support::stub_ctx_with;
    use maki_agent::tools::{LocalTool, ToolAudience};
    use test_case::test_case;

    use super::*;

    const TOOL_USE_ID: &str = "tu-1";
    const INSTRUCTION_PATH: &str = "/tmp/nested/AGENTS.md";
    const LOCAL_TOOL_NAME: &str = "sess_tool";
    /// Arbitrary ids are rejected: `SessionRef` parses base58 or a uuid.
    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const SUBAGENT_TASK_ID: &str = "toolu_task";

    fn session_ref() -> SessionRef {
        SESSION_ID.parse().expect("valid session id")
    }

    fn populated_ctx() -> ToolContext {
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(TOOL_USE_ID));
        ctx.session_id = Some(session_ref());
        ctx.deadline = Deadline::after(Duration::from_secs(60));
        ctx.tool_output_lines = ToolOutputLines {
            bash: 999,
            ..ToolOutputLines::default()
        };
        assert!(
            !ctx.loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH))
        );
        let mut tools: HashMap<String, LocalTool> = HashMap::new();
        tools.insert(
            LOCAL_TOOL_NAME.into(),
            maki_agent::tools::local_tool(ToolAudience::MODEL, |_, _| {
                Box::pin(async { Ok(String::new()) })
            }),
        );
        ctx.local_tools = Arc::new(tools);
        ctx.live_sink = Some(flume::unbounded().0);
        ctx
    }

    #[test]
    fn agent_context_keeps_tool_use_id_and_resets_per_call_state() {
        let agent = AgentContext::from(&populated_ctx());
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(
            agent.session_id,
            Some(session_ref()),
            "the session owns the whole run, so it is not per-call state"
        );
        assert!(matches!(agent.deadline, Deadline::None));
        assert_eq!(agent.tool_output_lines, ToolOutputLines::default());
        assert!(
            agent.local_tools.contains_key(LOCAL_TOOL_NAME),
            "a nested call must route names exactly like the model's own call"
        );
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
        assert_eq!(
            inner.session_id,
            Some(session_ref()),
            "a dispatched child runs in the same session, unlike tool_use_id"
        );
    }

    fn restore_ctx(ctx: &ToolContext) -> RestoreCtx {
        RestoreCtx {
            session_id: ctx.session_id.clone(),
            task_id: ctx.task_id.clone(),
            ..RestoreCtx::default()
        }
    }

    /// Restore has no `ToolContext`, so its session comes stamped on the item.
    #[test]
    fn session_id_reaches_every_ctx_kind() {
        let ctx = populated_ctx();
        for lua_ctx in [
            LuaCtx::handler(&ctx),
            LuaCtx::start(&ctx),
            LuaCtx::restore(restore_ctx(&ctx)),
        ] {
            assert_eq!(
                lua_ctx.session_id,
                Some(session_ref()),
                "{}",
                lua_ctx.kind()
            );
        }
    }

    #[test_case(None, MAIN_TASK_ID ; "session_owner")]
    #[test_case(Some(SUBAGENT_TASK_ID), SUBAGENT_TASK_ID ; "subagent")]
    fn task_id_reaches_every_ctx_kind(task_id: Option<&str>, expected: &str) {
        let mut ctx = populated_ctx();
        ctx.task_id = task_id.map(Arc::from);
        assert_eq!(LuaCtx::handler(&ctx).task_id(), expected);
        assert_eq!(LuaCtx::start(&ctx).task_id(), expected);
        assert_eq!(LuaCtx::restore(restore_ctx(&ctx)).task_id(), expected);
    }

    /// A plain-table ctx that names no reason must read as a rerender: side
    /// effects on load are opt-in, never the fallback.
    #[test]
    fn restore_reason_defaults_to_rerender_and_is_restore_only() {
        let ctx = populated_ctx();
        assert_eq!(
            LuaCtx::restore(RestoreCtx::default()).restore_reason(),
            Some(RestoreReason::Rerender)
        );
        assert_eq!(LuaCtx::handler(&ctx).restore_reason(), None);
        assert_eq!(LuaCtx::start(&ctx).restore_reason(), None);
    }

    #[test]
    fn handler_ctx_keeps_parent_instruction_set() {
        let ctx = LuaCtx::handler(&populated_ctx());
        assert!(
            ctx.loaded_instructions()
                .expect("handler has instructions")
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH)),
            "handler must share the parent's set; AgentContext resets its own copy"
        );
    }
}
