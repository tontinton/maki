use maki_agent::cancel::CancelToken;
use maki_config::AgentConfig;
use mlua::{LuaSerdeExt, UserData, UserDataMethods};

pub(crate) struct LuaCtx {
    pub(crate) cancel: CancelToken,
    pub(crate) config: AgentConfig,
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("config", |lua, this, ()| lua.to_value(&this.config));
    }
}
