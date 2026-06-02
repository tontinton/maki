use std::time::{Duration, Instant};

use maki_agent::cancel::CancelToken;
use maki_config::{AgentConfig, ToolOutputLines};
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

pub(crate) struct LuaCtx {
    pub(crate) cancel: CancelToken,
    pub(crate) config: AgentConfig,
    pub(crate) tool_output_lines: ToolOutputLines,
    pub(crate) finish_tx: Option<flume::Sender<ToolCallReply>>,
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
