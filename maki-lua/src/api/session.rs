//! `maki.session`: host session primitives. Session management round-trips to
//! the UI event loop, which owns live runtimes and storage. `notify` posts
//! directly to the agent mailbox so synchronous callbacks can use it.

use maki_agent::SessionMailbox;
use maki_agent::agent::live_history;
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::{ContentBlock, Message, Role};
use maki_storage::id::MakiId;
use mlua::{Lua, Result as LuaResult, Table, Value};
use serde_json::json;

use crate::api::util::command::{SessionRequest, UiAction, ui_json_roundtrip, ui_roundtrip};
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, err_pair, try_pair};

/// Answers `maki.session.read` for a driver that has no UI to ask. Takes the
/// optional session id from Lua and returns a serialized
/// [`crate::SessionSnapshot`].
pub type SessionSnapshotFn =
    Box<dyn Fn(Option<&str>) -> Result<serde_json::Value, String> + Send + Sync + 'static>;

pub struct SessionSnapshotSlot(pub SessionSnapshotFn);

const BLANK_NOTIFY_ERR: &str = "text must not be blank";
const SESSION_REQUIRED_ERR: &str = "session is required";
const NOT_LIVE_ERR: &str = "session not live";
const NO_FOCUSED_ERR: &str = "no focused session";

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: SessionRequest,
) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Session {
        req,
        reply_tx,
    })
    .await
}

/// Lists sessions stored for the current project. Answered from a
/// background scan, so a slow disk never blocks the UI.
///
/// @return (table|nil, string|nil) Array of `{id, title, updated_at}`, or nil and an error.
/// @example
/// local stored, err = maki.session.list()
#[lua_fn]
async fn list(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::List).await
}

/// Lists the sessions currently running in this UI. Status is "working",
/// "needs_input", or "idle". A mailbox follow-up stays "working" without an
/// intermediate "idle" status.
///
/// @return (table|nil, string|nil) Array of `{id, title, status, updated_at, focused}`, or nil and an error.
/// @example
/// local live, err = maki.session.live()
#[lua_fn]
async fn live(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Live).await
}

/// One-call snapshot of a session: queue, usage, context, cost, mode, and
/// status. Reads the focused session, or the one you name in `session` when
/// you act on a background tab.
///
/// The returned table:
/// ```text
/// {
///   id, cwd, model, mode = "build" | "plan",
///   status = "idle" | "working" | "needs_input",
///   focused, updated_at,
///   usage = { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens },
///   context_size, context_window,
///   cost,
///   queue = { count }, -- nil under headless drivers
///   title,             -- nil under headless drivers
/// }
/// ```
///
/// `usage` and `cost` include subagent spend. `context_size` is the main
/// session's own, since a subagent runs its own window. There is no
/// `list_cost` here: the un-subsidised total for a run arrives on the
/// `TurnEnd` autocmd, and `maki.model.info` carries the rates behind it.
///
/// @param opts table? `session` (string?) Session id; defaults to focused.
/// @return (table|nil, string|nil) Snapshot table, or nil and an error.
/// @example
/// local s = maki.session.read()
/// if s.context_size > s.context_window * 0.8 then
///   maki.ui.notify("context is nearly full")
/// end
#[lua_fn]
async fn read(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(t) => t.get::<Option<String>>("session")?,
        None => None,
    };
    // Headless drivers install a provider, the UI leaves the slot empty and
    // answers from its event loop, which owns the live session runtimes.
    if let Some(slot) = lua.app_data_ref::<SessionSnapshotSlot>() {
        return match (slot.0)(id.as_deref()) {
            Ok(value) => Ok((Some(json_to_lua(&lua, &value)?), None)),
            Err(msg) => Ok(err_pair(msg)),
        };
    }
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Session {
        req: SessionRequest::Read { id },
        reply_tx,
    })
    .await
}

/// The focused tab, or under a headless driver the one session it runs.
async fn focused_session(
    lua: &Lua,
    tx: Option<&flume::Sender<UiAction>>,
) -> Result<String, String> {
    // Asked in its own statement, so the borrow of the app data ends before
    // the roundtrip below parks the task.
    let headless = lua
        .app_data_ref::<SessionSnapshotSlot>()
        .map(|slot| (slot.0)(None));
    let snapshot = match headless {
        Some(snapshot) => snapshot?,
        None => {
            ui_roundtrip(tx, |reply_tx| UiAction::Session {
                req: SessionRequest::Current,
                reply_tx,
            })
            .await??
        }
    };
    // The UI answers with the bare id, a headless driver with its snapshot.
    snapshot
        .get("id")
        .unwrap_or(&snapshot)
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| NO_FOCUSED_ERR.to_owned())
}

fn block_json(block: &ContentBlock) -> Option<serde_json::Value> {
    Some(match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { thinking, .. } => json!({ "type": "thinking", "text": thinking }),
        ContentBlock::RedactedThinking { .. } => return None,
        ContentBlock::ToolUse {
            id, name, input, ..
        } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::Image { source } => {
            json!({ "type": "image", "media_type": source.media_type })
        }
    })
}

/// An image keeps only its media type. A plugin reading history wants to know
/// one was there, and the base64 payload would cost megabytes per call.
fn message_json(message: &Message) -> serde_json::Value {
    let content: Vec<_> = message.content.iter().filter_map(block_json).collect();
    json!({
        "role": match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        "kind": message.kind,
        "hidden": message.display_text.as_deref() == Some(""),
        "content": content,
    })
}

/// Reads a live session's transcript, oldest first: everything the model has
/// been sent so far, tool calls and results included. Read only.
///
/// Each message is `{ role, kind, hidden, content }`. `role` is `"user"` or
/// `"assistant"`. `kind` is `"turn"` for something the user or the model said
/// and `"observation"` for a report sent to the model as a user message, like
/// `maki.session.notify`. `hidden` marks a message only the model sees, such
/// as a nudge or a compaction note. `content` lists blocks:
///
/// ```text
/// { type = "text", text }
/// { type = "thinking", text }
/// { type = "tool_use", id, name, input }
/// { type = "tool_result", tool_use_id, content, is_error }
/// { type = "image", media_type }
/// ```
///
/// Works in the TUI, `maki -p`, sdk mode, and ACP. ACP has no focused
/// session, so pass `session` there. Hook and event payloads carry the
/// `session_id` to pass.
///
/// @param opts table? Options:
///   `session` (string?) id of a live session, defaults to the focused one.
///   `last` (integer?) only the newest `last` messages.
/// @return (table|nil, string|nil) Array of messages, or nil and an error.
/// @example
/// local msgs = maki.session.messages({ last = 1 })
/// local last = msgs and msgs[1]
/// if last and last.role == "assistant" then
///   print(last.content[1].text)
/// end
#[lua_fn]
async fn messages(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let (id, last) = match &opts {
        Some(opts) => (
            opts.get::<Option<String>>("session")?,
            opts.get::<Option<usize>>("last")?,
        ),
        None => (None, None),
    };
    let raw = match id {
        Some(id) => id,
        None => try_pair!(focused_session(&lua, tx.as_ref()).await),
    };
    let session: MakiId = try_pair!(raw.parse());
    let Some(history) = live_history(session) else {
        return Ok(err_pair(format!("{NOT_LIVE_ERR}: {session}")));
    };
    let skip = last.map_or(0, |n| history.len().saturating_sub(n));
    let out = lua.create_table()?;
    for message in &history[skip..] {
        out.push(json_to_lua(&lua, &message_json(message))?)?;
    }
    Ok((Some(out), None))
}

/// Returns the id of the currently focused session.
///
/// @return (string|nil, string|nil) Session id, or nil and an error.
/// @example
/// local id = maki.session.current()
#[lua_fn]
async fn current(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Current).await
}

/// Switches the UI to the session with {id}.
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
) -> LuaResult<Pair<Value>> {
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
) -> LuaResult<Pair<Value>> {
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
) -> LuaResult<Pair<Value>> {
    let (prompt, focus) = match opts {
        Some(opts) => (opts.get("prompt")?, opts.get("focus").unwrap_or(false)),
        None => (None, false),
    };
    roundtrip(lua, tx, SessionRequest::New { prompt, focus }).await
}

/// Sends {text} as a regular user prompt to a live session. The text is
/// never interpreted: slash commands, `exit`, and `!` shell prefixes are
/// all sent to the model verbatim. If the session is currently streaming,
/// the prompt is queued and picked up when the agent reaches it.
///
/// @param text string The prompt to send. Must not be blank.
/// @param opts table? Optional fields: session (string) id of a live
///   session; defaults to the focused one.
/// @return (string|nil, string|nil) "started" or "queued", or nil and an error.
/// @example
/// local state, err = maki.session.prompt("run the tests", { session = id })
#[lua_fn]
async fn prompt(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.get("session")?,
        None => None,
    };
    roundtrip(lua, tx, SessionRequest::Prompt { id, text }).await
}

/// Reports {text} to a live session without creating a user turn. The
/// observation waits for the session's next agent run.
///
/// @param text string What to report. Must not be blank.
/// @param opts table Options:
///   `session` (string) id of a live session.
///   `wake` (boolean) start a TUI turn when it next becomes idle (default false).
/// @return (boolean|nil, string|nil) true, or nil and an error.
/// @example
/// maki.session.notify("[monitor] deploy failed", { session = id, wake = true })
#[lua_fn]
fn notify(_lua: &Lua, text: String, opts: Option<Table>) -> LuaResult<Pair<bool>> {
    if text.trim().is_empty() {
        return Ok(err_pair(BLANK_NOTIFY_ERR));
    }
    let Some(opts) = opts else {
        return Ok(err_pair(SESSION_REQUIRED_ERR));
    };
    let Some(raw_id) = opts.get::<Option<String>>("session")? else {
        return Ok(err_pair(SESSION_REQUIRED_ERR));
    };
    let session_id: MakiId = match raw_id.parse() {
        Ok(id) => id,
        Err(error) => return Ok(err_pair(error)),
    };
    let wake = opts.get("wake").unwrap_or(false);
    if let Err(error) = SessionMailbox::notify(session_id, text, wake) {
        return Ok(err_pair(error));
    }
    Ok((Some(true), None))
}

/// Switches a live session between plan and build mode. Entering plan mode
/// allocates the session's plan file if it has none.
///
/// A session that is mid-plan answers the next prompt with another draft of
/// the plan. Set `"build"` first and that prompt implements it.
///
/// @param mode string "build" or "plan".
/// @param opts table? Options:
///   session (string) id of a live session, defaults to the focused one.
/// @return (boolean|nil, string|nil) true, or nil and an error.
/// @example
/// maki.session.set_mode("build", { session = opts.session })
/// maki.session.prompt("Implement the plan at `" .. opts.path .. "`.", { session = opts.session })
#[lua_fn]
async fn set_mode(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    mode: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.get("session")?,
        None => None,
    };
    roundtrip(lua, tx, SessionRequest::SetMode { id, mode }).await
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
) -> LuaResult<Pair<Value>> {
    let req = SessionRequest::SetTitle {
        id: opts.get("id")?,
        title: opts.get("title")?,
    };
    roundtrip(lua, tx, req).await
}

lua_table! {
    /// Host session primitives. The interactive UI can run several sessions
    /// at once; these functions let plugins list, create, focus, rename, and
    /// delete them. Session management returns `nil, "no interactive UI
    /// attached"` without a UI. `notify` instead targets a live agent mailbox
    /// directly, so it also works under ACP and SDK frontends.
    "maki.session" => pub(crate) fn create_session_table(tx: Option<flume::Sender<UiAction>>),
    DOCS [list(tx), live(tx), current(tx), read(tx), messages(tx), focus(tx), delete(tx), new(tx), prompt(tx), notify(), set_mode(tx), set_title(tx)]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use maki_agent::agent::{History, HistorySnapshot, SharedMessages, publish_live_history};
    use maki_providers::{ImageMediaType, ImageSource, MessageKind};

    use super::*;
    use crate::api::util::command::NO_UI_ERR;
    use mlua::Value;
    use serde_json::json;
    use test_case::test_case;

    const FOCUSED_TEXT: &str = "focused";
    const TOOL_ID: &str = "toolu_1";
    const TOOL_NAME: &str = "read";
    const TOOL_PATH: &str = "src/main.rs";
    const TOOL_OUTPUT: &str = "no such file";
    const SIGNATURE: &str = "sig";
    const THOUGHT: &str = "let me check";
    const IMAGE_DATA: &str = "iVBORw0KGgo=";
    const TEXT: &str = "hello";

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

    #[test_case("return session.prompt('hi', { session = 'abc' })", Some("abc") ; "explicit_session_id")]
    #[test_case("return session.prompt('hi')", None ; "defaults_to_focused")]
    fn prompt_forwards_text_and_session_id(code: &str, expected_id: Option<&str>) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected_id = expected_id.map(str::to_owned);
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Prompt { id, text },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected prompt request");
            };
            assert_eq!(id, expected_id);
            assert_eq!(text, "hi");
            reply_tx.send(Ok(json!("queued"))).unwrap();
        });
        let (val, err): (String, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(val, "queued");
    }

    /// Mode is per session like the plan it drives, so a row handler firing
    /// for a background tab has to be able to name it.
    #[test_case(r#"return session.set_mode('build', { session = 'abc' })"#, Some("abc"), "build" ; "explicit_session_id")]
    #[test_case("return session.set_mode('plan')", None, "plan" ; "defaults_to_focused")]
    fn set_mode_forwards_the_mode_and_session_id(
        code: &str,
        expected_id: Option<&str>,
        expected_mode: &str,
    ) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected_id = expected_id.map(str::to_owned);
        let expected_mode = expected_mode.to_owned();
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::SetMode { id, mode },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected set_mode request");
            };
            assert_eq!(id, expected_id);
            assert_eq!(mode, expected_mode);
            reply_tx.send(Ok(json!(true))).unwrap();
        });
        let (val, err): (bool, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        checker.join().unwrap();
        assert_eq!(err, None);
        assert!(val);
    }

    #[test]
    fn notify_is_synchronous_and_queues_an_observation() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = mailbox.drain();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_observation());
        assert_eq!(messages[0].user_text(), Some("built"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn waking_notify_sets_the_mailbox_wake_flag() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('failed', { session = session_id, wake = true })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[test]
    fn notify_rejects_missing_and_non_live_sessions() {
        let lua = lua_with_session(None);
        let (_, missing): (Value, Option<String>) =
            lua.load("return session.notify('built')").eval().unwrap();
        assert_eq!(missing.as_deref(), Some(SESSION_REQUIRED_ERR));

        let id = MakiId::generate();
        lua.globals().set("session_id", id.to_string()).unwrap();
        let (_, not_live): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval()
            .unwrap();
        assert_eq!(not_live, Some(format!("session not live: {id}")));
    }

    #[test]
    fn notify_rejects_blank_text_and_invalid_session_ids() {
        let lua = lua_with_session(None);
        let (_, blank): (Value, Option<String>) = lua
            .load("return session.notify(' ', { session = 'invalid' })")
            .eval()
            .unwrap();
        assert_eq!(blank.as_deref(), Some(BLANK_NOTIFY_ERR));

        let (_, invalid): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = 'invalid' })")
            .eval()
            .unwrap();
        assert!(invalid.is_some_and(|error| error.contains("invalid base58")));
    }

    /// Keep the returned history alive for as long as the session should be.
    fn live(id: MakiId, messages: Vec<Message>) -> History {
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_history(id, &mirror);
        History::new(messages).with_mirror(mirror)
    }

    #[test]
    fn messages_reads_a_live_transcript() {
        const FIRST: &str = "first";
        const ANSWER: &str = "done";
        let id = MakiId::generate();
        let _history = live(
            id,
            vec![
                Message::user(FIRST.into()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: ANSWER.into(),
                    }],
                    ..Default::default()
                },
            ],
        );
        let lua = lua_with_session(None);

        let code = format!("return session.messages({{ session = '{id}', last = 1 }})");
        let (msgs, err): (Table, Option<String>) =
            smol::block_on(lua.load(&code).eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(msgs.raw_len(), 1);
        let last: Table = msgs.get(1).unwrap();
        assert_eq!(last.get::<String>("role").unwrap(), "assistant");
        let block: Table = last.get::<Table>("content").unwrap().get(1).unwrap();
        assert_eq!(block.get::<String>("text").unwrap(), ANSWER);
    }

    #[test]
    fn messages_of_a_session_that_is_not_live_is_an_error_pair() {
        let id = MakiId::generate();
        let lua = lua_with_session(None);
        let code = format!("return session.messages({{ session = '{id}' }})");
        let (_, err): (Value, Option<String>) =
            smol::block_on(lua.load(&code).eval_async()).unwrap();
        assert_eq!(err, Some(format!("{NOT_LIVE_ERR}: {id}")));
    }

    fn headless_focused_on(id: MakiId) -> Lua {
        let lua = lua_with_session(None);
        let snapshot = json!({ "id": id.to_string(), "title": FOCUSED_TEXT });
        let provider: SessionSnapshotFn = Box::new(move |requested| {
            assert_eq!(requested, None);
            Ok(snapshot.clone())
        });
        lua.set_app_data(SessionSnapshotSlot(provider));
        lua
    }

    /// The UI answers `Current` with the bare id, not a snapshot.
    fn ui_focused_on(id: MakiId) -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Current,
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected current request");
            };
            reply_tx.send(Ok(json!(id.to_string()))).unwrap();
        });
        lua_with_session(Some(tx))
    }

    #[test_case(headless_focused_on ; "headless_snapshot_id")]
    #[test_case(ui_focused_on ; "ui_current_bare_id")]
    fn messages_defaults_to_the_focused_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let _history = live(id, vec![Message::user(FOCUSED_TEXT.into())]);
        let lua = focused_on(id);

        let (msgs, err): (Table, Option<String>) =
            smol::block_on(lua.load("return session.messages()").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(msgs.raw_len(), 1);
        let block: Table = msgs
            .get::<Table>(1)
            .unwrap()
            .get::<Table>("content")
            .unwrap()
            .get(1)
            .unwrap();
        assert_eq!(block.get::<String>("text").unwrap(), FOCUSED_TEXT);
    }

    #[test]
    fn messages_without_a_focused_session_is_an_error_pair() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::Session { reply_tx, .. }) = rx.recv() else {
                panic!("expected session request");
            };
            reply_tx.send(Ok(serde_json::Value::Null)).unwrap();
        });
        let (_, err): (Value, Option<String>) =
            smol::block_on(lua.load("return session.messages()").eval_async()).unwrap();
        assert_eq!(err.as_deref(), Some(NO_FOCUSED_ERR));
    }

    #[test_case(
        ContentBlock::ToolUse {
            id: TOOL_ID.into(),
            name: TOOL_NAME.into(),
            input: json!({ "path": TOOL_PATH }),
            thought_signature: Some(SIGNATURE.into()),
        },
        Some(json!({ "type": "tool_use", "id": TOOL_ID, "name": TOOL_NAME, "input": { "path": TOOL_PATH } }))
        ; "tool_use_keeps_id_name_and_input"
    )]
    #[test_case(
        ContentBlock::ToolResult { tool_use_id: TOOL_ID.into(), content: TOOL_OUTPUT.into(), is_error: true },
        Some(json!({ "type": "tool_result", "tool_use_id": TOOL_ID, "content": TOOL_OUTPUT, "is_error": true }))
        ; "tool_result_keeps_id_content_and_error_flag"
    )]
    #[test_case(
        ContentBlock::Image { source: ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()) },
        Some(json!({ "type": "image", "media_type": ImageMediaType::Png.mime() }))
        ; "image_keeps_only_the_media_type"
    )]
    #[test_case(
        ContentBlock::Thinking { thinking: THOUGHT.into(), signature: Some(SIGNATURE.into()) },
        Some(json!({ "type": "thinking", "text": THOUGHT }))
        ; "thinking_becomes_text"
    )]
    #[test_case(
        ContentBlock::RedactedThinking { data: SIGNATURE.into() },
        None
        ; "redacted_thinking_is_omitted"
    )]
    fn block_json_shape(block: ContentBlock, expected: Option<serde_json::Value>) {
        assert_eq!(block_json(&block), expected);
    }

    #[test_case(Role::User, MessageKind::Turn, None, "user", "turn", false ; "user_turn_is_visible")]
    #[test_case(Role::User, MessageKind::Observation, Some(""), "user", "observation", true ; "blank_display_text_is_hidden")]
    #[test_case(Role::Assistant, MessageKind::Turn, Some(TEXT), "assistant", "turn", false ; "display_text_with_content_is_visible")]
    fn message_json_reports_role_kind_and_hidden(
        role: Role,
        kind: MessageKind,
        display_text: Option<&str>,
        expected_role: &str,
        expected_kind: &str,
        hidden: bool,
    ) {
        let message = Message {
            role,
            kind,
            display_text: display_text.map(str::to_owned),
            content: vec![
                ContentBlock::Text { text: TEXT.into() },
                ContentBlock::RedactedThinking {
                    data: SIGNATURE.into(),
                },
            ],
        };
        assert_eq!(
            message_json(&message),
            json!({
                "role": expected_role,
                "kind": expected_kind,
                "hidden": hidden,
                "content": [{ "type": "text", "text": TEXT }],
            })
        );
    }

    #[test]
    fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> =
            smol::block_on(lua.load("return session.set_title('oops')").eval_async());
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
