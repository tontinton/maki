//! `maki.session`: host session primitives. Every call round-trips to the UI
//! event loop, which owns the live session runtimes and the session store;
//! the loop answers `list` from a background task so slow scans never block.

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::util::command::{SessionReply, SessionRequest, UiAction};
use crate::api::util::convert::json_to_lua;

const NO_UI_ERR: &str = "no interactive UI attached";

type Pair = (Value, Option<String>);

fn err_pair(err: impl ToString) -> Pair {
    (Value::Nil, Some(err.to_string()))
}

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: SessionRequest,
) -> LuaResult<Pair> {
    let Some(tx) = tx else {
        return Ok(err_pair(NO_UI_ERR));
    };
    let (reply_tx, reply_rx) = flume::bounded::<SessionReply>(1);
    if tx.try_send(UiAction::Session { req, reply_tx }).is_err() {
        return Ok(err_pair(NO_UI_ERR));
    }
    match reply_rx.recv_async().await {
        Ok(Ok(value)) => Ok((json_to_lua(&lua, &value)?, None)),
        Ok(Err(e)) => Ok(err_pair(e)),
        Err(_) => Ok(err_pair("ui event loop dropped the request")),
    }
}

/// Lists sessions stored for the current project. Answered from a
/// background scan, so a slow disk never blocks the UI.
///
/// @return (table|nil, string|nil) Array of `{id, title, updated_at}`, or nil and an error.
/// @example
/// local stored, err = maki.session.list()
#[lua_fn]
async fn list(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair> {
    roundtrip(lua, tx, SessionRequest::List).await
}

/// Lists the sessions currently running in this UI. Status is "working",
/// "needs_input", or "idle".
///
/// @return (table|nil, string|nil) Array of `{id, title, status, updated_at, focused}`, or nil and an error.
/// @example
/// local live, err = maki.session.live()
#[lua_fn]
async fn live(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair> {
    roundtrip(lua, tx, SessionRequest::Live).await
}

/// Returns the id of the currently focused session.
///
/// @return (string|nil, string|nil) Session id, or nil and an error.
/// @example
/// local id = maki.session.current()
#[lua_fn]
async fn current(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair> {
    roundtrip(lua, tx, SessionRequest::Current).await
}

/// Switches the UI to the session with {id}. The session must be live.
///
/// @param id string Session id, as returned by `list()` or `live()`.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.focus(id)
#[lua_fn]
async fn focus(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    id: String,
) -> LuaResult<Pair> {
    roundtrip(lua, tx, SessionRequest::Focus { id }).await
}

/// Deletes a session and its stored history, cancelling it first if it
/// is running. The focused session cannot be deleted.
///
/// @param id string Session id to delete.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.delete(id)
#[lua_fn]
async fn delete(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    id: String,
) -> LuaResult<Pair> {
    roundtrip(lua, tx, SessionRequest::Delete { id }).await
}

/// Starts a new session in the current project.
///
/// @param opts table? Optional fields: prompt (string) first user message
///   to submit right away; focus (boolean) switch the UI to the new session.
/// @return (string|nil, string|nil) New session id, or nil and an error.
/// @example
/// local id, err = maki.session.new({ prompt = "fix the tests", focus = true })
#[lua_fn]
async fn new(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair> {
    let (prompt, focus) = match opts {
        Some(opts) => (opts.get("prompt")?, opts.get("focus").unwrap_or(false)),
        None => (None, false),
    };
    roundtrip(lua, tx, SessionRequest::New { prompt, focus }).await
}

/// Renames a session, live or stored.
///
/// @param opts table Required fields: id (string) session to rename;
///   title (string) the new title.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.set_title({ id = id, title = "refactor" })
#[lua_fn]
async fn set_title(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Table,
) -> LuaResult<Pair> {
    let req = SessionRequest::SetTitle {
        id: opts.get("id")?,
        title: opts.get("title")?,
    };
    roundtrip(lua, tx, req).await
}

lua_table! {
    /// Host session primitives. The interactive UI can run several sessions
    /// at once; these functions let plugins list, create, focus, rename, and
    /// delete them. Every call round-trips to the UI event loop and returns
    /// the pair `(value, err)`. Without an interactive UI attached, every
    /// call returns `nil, "no interactive UI attached"`.
    "maki.session" => pub(crate) fn create_session_table(tx: Option<flume::Sender<UiAction>>),
    DOCS [list(tx), live(tx), current(tx), focus(tx), delete(tx), new(tx), set_title(tx)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lua_with_session(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_session_table(&lua, tx).unwrap();
        lua.globals().set("session", t).unwrap();
        lua
    }

    #[test]
    fn live_without_ui_returns_error_pair() {
        let lua = lua_with_session(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load("return session.live()").eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test]
    fn focus_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Focus { id },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) =
            smol::block_on(lua.load("return session.focus('abc')").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), "abc");
    }

    #[test]
    fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> =
            smol::block_on(lua.load("return session.set_title('oops')").eval_async());
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
