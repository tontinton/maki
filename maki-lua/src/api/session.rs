//! `maki.session` exposes host-side session control to Lua plugins:
//! triggering compaction and inspecting conversation history length.

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::util::command::UiAction;

pub(crate) fn register(
    lua: &Lua,
    maki: &Table,
    ui_action_tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<()> {
    let session = lua.create_table()?;

    session.set(
        "compact",
        lua.create_function(move |_, ()| {
            match &ui_action_tx {
                Some(tx) => {
                    if let Err(e) = tx.try_send(UiAction::Compact) {
                        tracing::warn!(error = %e, "compact: ui action channel send failed");
                    }
                }
                None => tracing::warn!("compact: no ui action channel available"),
            }
            Ok(())
        })?,
    )?;

    session.set(
        "history_len",
        lua.create_function(|lua, ()| {
            let len = lua
                .app_data_ref::<maki_agent::SharedMessages>()
                .map(|h| h.load().len());
            Ok(len)
        })?,
    )?;

    maki.set("session", session)?;
    Ok(())
}
