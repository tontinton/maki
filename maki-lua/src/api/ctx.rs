use maki_agent::AgentEvent;
use maki_agent::BufferSnapshot;
use maki_agent::cancel::CancelToken;
use maki_config::{AgentConfig, ToolOutputLines};
use mlua::{LuaSerdeExt, UserData, UserDataMethods, Value as LuaValue};

use crate::api::buf::BufHandle;
use crate::api::tool::coerce_tool_result;
use crate::runtime::LiveCtx;

pub(crate) struct FinishPayload {
    pub llm_output: String,
    pub is_error: bool,
    pub body: Option<BufferSnapshot>,
}

pub(crate) struct LuaCtx {
    pub(crate) cancel: CancelToken,
    pub(crate) config: AgentConfig,
    pub(crate) tool_output_lines: ToolOutputLines,
    pub(crate) finish_tx: Option<flume::Sender<FinishPayload>>,
    pub(crate) live: Option<LiveCtx>,
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("config", |lua, this, ()| lua.to_value(&this.config));

        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });

        methods.add_method("emit_output", |_, this, content: String| {
            if let Some(live) = &this.live {
                live.event_tx.try_send(AgentEvent::ToolOutput {
                    id: live.tool_use_id.clone(),
                    content,
                });
            }
            Ok(())
        });

        methods.add_method_mut("finish", |_lua, this, val: LuaValue| {
            let tx = this
                .finish_tx
                .take()
                .ok_or_else(|| mlua::Error::runtime("ctx:finish() already called"))?;

            let (result, body) = match &val {
                LuaValue::Table(t) => {
                    let body_snap = t.get::<LuaValue>("body").ok().and_then(|v| {
                        let ud = v.as_userdata()?;
                        let h = ud.borrow::<BufHandle>().ok()?;
                        Some(h.buf.take())
                    });
                    (coerce_tool_result(&val), body_snap)
                }
                _ => (coerce_tool_result(&val), None),
            };

            let (llm_output, is_error) = match result {
                Ok(s) => (s, false),
                Err(s) => (s, true),
            };

            let _ = tx.send(FinishPayload {
                llm_output,
                is_error,
                body,
            });
            Ok(())
        });
    }
}
