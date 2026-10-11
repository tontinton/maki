//! `maki.session`: host session primitives. Session management round-trips to
//! the UI event loop, which owns live runtimes and storage. `notify` posts
//! directly to the agent mailbox so synchronous callbacks can use it.
//!
//! Transcripts cross into Lua in the serde shape of [`Message`], the same
//! document `maki session export` prints. The one difference is an image
//! block's `source`, which is a `maki.image.Image` sharing the payload.

use std::sync::Arc;

use maki_agent::agent::{SessionExport, live_header, live_history, live_snapshot};
use maki_agent::session::{StoredSession, store_new};
use maki_agent::{SessionMailbox, Transcript};
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::{ContentBlock, Message};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use mlua::{IntoLua, Lua, LuaSerdeExt, Result as LuaResult, Table, Value};
use serde_json::{Map, Value as JsonValue};

use crate::api::image::LuaImage;
use crate::api::util::command::{
    NO_UI_ERR, SessionRequest, UiAction, ui_json_roundtrip, ui_reachable, ui_roundtrip,
};
use crate::api::util::convert::{TableExt, json_to_lua, json_to_lua_exact, lua_to_json};
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::plugin_permissions::{Permission, PluginPermissions, denied_error};
use crate::session_snapshot::SessionSnapshot;

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
const EMPTY_ARRAY_ERR: &str =
    "expected an array, got an empty table (copy with maki.deepcopy to keep it a list)";
/// Tables built between two yields, so a long history never holds the
/// shared Lua thread near its budget.
const MESSAGES_PER_YIELD: usize = 256;
const MESSAGES_KEY: &str = "messages";
const CONTENT_KEY: &str = "content";
const SOURCE_KEY: &str = "source";
const TYPE_KEY: &str = "type";
const BLOCK_IMAGE: &str = "image";
const CWD_KEY: &str = "cwd";
const PROMPT_KEY: &str = "prompt";
const FOCUS_KEY: &str = "focus";
/// What `new` takes from the export document. Anything else is ignored, like
/// an importer ignores unknown envelope fields, so a newer export still goes
/// back in.
const DOCUMENT_KEYS: [&str; 8] = [
    "format",
    "model",
    "mode",
    "thinking",
    "fast",
    "title",
    "facts",
    MESSAGES_KEY,
];

pub(crate) fn not_live(session: MakiId) -> String {
    format!("{NOT_LIVE_ERR}: {session}")
}

/// Some calls must name their session. Falling back to the focused tab would
/// make them follow the user from tab to tab.
pub(crate) fn required_session(opts: Option<&Table>) -> LuaResult<Result<MakiId, String>> {
    let Some(raw) = opts
        .map(|t| t.opt::<String>("session"))
        .transpose()?
        .flatten()
    else {
        return Ok(Err(SESSION_REQUIRED_ERR.to_owned()));
    };
    Ok(raw.parse::<MakiId>().map_err(|e| e.to_string()))
}

/// The UI answers stored reads from its own state dir. Without one, an
/// embedder may pin it as app data, else it resolves like the CLI's.
fn state_dir(lua: &Lua) -> Result<StateDir, String> {
    match lua.app_data_ref::<StateDir>() {
        Some(dir) => Ok(dir.clone()),
        None => StateDir::resolve().map_err(|e| e.to_string()),
    }
}

/// Reads a stored session off the Lua thread without claiming it, so one
/// another maki has open still answers. `answer` runs off the thread too, so
/// a long transcript is never dropped on it.
async fn from_disk<T: Send + 'static>(
    lua: &Lua,
    id: MakiId,
    answer: impl FnOnce(StoredSession) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let dir = state_dir(lua)?;
    smol::unblock(move || answer(StoredSession::read_only(id, &dir).map_err(|e| e.to_string())?))
        .await
}

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

/// A session no host here answers for: one a headless driver runs from what
/// it publishes, any other from disk without claiming it.
async fn unhosted_snapshot(lua: &Lua, raw: &str) -> Result<JsonValue, String> {
    let id = raw.parse::<MakiId>().map_err(|e| e.to_string())?;
    let snapshot = match live_header(id) {
        Some(header) => SessionSnapshot::live_headless(id, Arc::unwrap_or_clone(header)),
        None => from_disk(lua, id, |session| Ok(SessionSnapshot::stored(&session))).await?,
    };
    serde_json::to_value(snapshot).map_err(|e| e.to_string())
}

/// One-call snapshot of a session: queue, usage, context, cost, mode, and
/// status. Reads the focused session, or the one you name in `session`: a
/// background tab, or a stored session nothing has open.
///
/// Metadata only, so it needs no permission. Not to be confused with the
/// `session_read` permission, which guards transcript content
/// (`maki.session.export`, `maki.session.messages`).
///
/// The returned table:
/// ```text
/// {
///   id, cwd, model, mode = "build" | "plan",
///   thinking, fast,    -- nil under headless drivers
///   status = "idle" | "working" | "needs_input" | "stored",
///   focused, updated_at,
///   usage = { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens },
///   context_size,
///   context_window,    -- nil for a stored session
///   cost,              -- nil for a stored session
///   queue = { count }, -- nil under headless drivers and for a stored session
///   title,             -- nil under headless drivers
/// }
/// ```
///
/// `status` is `"stored"` for a session nothing here runs. Its `model` is the
/// one it was saved with, resolved only when it opens.
///
/// `usage` and `cost` include subagent spend. `context_size` is the main
/// session's own, since a subagent runs its own window. There is no
/// `list_cost` here: the un-subsidised total for a run arrives on the
/// `TurnEnd` autocmd, and `maki.model.info` carries the rates behind it.
///
/// @param opts table? `session` (string?) Session id, live or stored. Defaults to the focused one.
/// @return (table|nil, string|nil) Snapshot table, or nil and an error.
/// @example
/// local s = maki.session.read()
/// if s.context_size > s.context_window * 0.8 then
///   maki.notify("context is nearly full")
/// end
#[lua_fn]
async fn read(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(t) => t.opt::<String>("session")?,
        None => None,
    };
    // A headless driver installs a provider for the one session it runs. The
    // UI leaves the slot empty and answers from its event loop, which owns
    // the live runtimes. The slot is asked in its own statement, so its
    // borrow ends before anything below parks the task.
    let headless = lua
        .app_data_ref::<SessionSnapshotSlot>()
        .map(|slot| (slot.0)(id.as_deref()));
    // An id no host answers for may still be another driver's, or stored.
    let reply = match (headless, id) {
        (Some(Err(_)), Some(id)) => unhosted_snapshot(&lua, &id).await,
        (Some(reply), _) => reply,
        (None, id) => {
            let asked = ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::Session {
                req: SessionRequest::Read { id: id.clone() },
                reply_tx,
            })
            .await;
            match (asked, id) {
                (Ok(reply), _) => reply,
                (Err(NO_UI_ERR), Some(id)) => unhosted_snapshot(&lua, &id).await,
                (Err(e), _) => Err(e.to_owned()),
            }
        }
    };
    let snapshot = try_pair!(reply);
    Ok((Some(json_to_lua(&lua, &snapshot)?), None))
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

async fn named_or_focused(
    lua: &Lua,
    tx: Option<&flume::Sender<UiAction>>,
    id: Option<String>,
) -> Result<MakiId, String> {
    let raw = match id {
        Some(id) => id,
        None => focused_session(lua, tx).await?,
    };
    raw.parse::<MakiId>().map_err(|e| e.to_string())
}

/// A live session from memory, unsaved messages included, any other from
/// disk. The UI answers for its tabs. A headless driver publishes its
/// session's settings next to the transcript, so a session it has not saved
/// yet reads too.
async fn transcript(
    lua: &Lua,
    tx: Option<&flume::Sender<UiAction>>,
    id: MakiId,
) -> Result<SessionExport, String> {
    match ui_roundtrip(tx, |reply_tx| UiAction::ExportSession { id, reply_tx }).await {
        Ok(reply) => return reply,
        Err(e) if e != NO_UI_ERR => return Err(e.to_owned()),
        Err(_) => {}
    }
    if let Some(snapshot) = live_snapshot(id)
        && let Some(header) = live_header(id)
    {
        return Ok(SessionExport::live(Arc::unwrap_or_clone(header), &snapshot));
    }
    from_disk(lua, id, |stored| Ok(SessionExport::stored(stored))).await
}

/// The serde shape, nulls as `maki.NIL`. An image block is built around a
/// handle on its payload, which is never copied, not even into JSON.
fn message_table(lua: &Lua, message: &Message) -> LuaResult<Value> {
    let envelope = Message {
        role: message.role.clone(),
        content: Vec::new(),
        display_text: message.display_text.clone(),
        kind: message.kind.clone(),
    };
    let json = serde_json::to_value(&envelope).map_err(mlua::Error::external)?;
    let table = lua.unpack::<Table>(json_to_lua_exact(lua, &json)?)?;
    let content = lua.create_table_with_capacity(message.content.len(), 0)?;
    for (index, block) in message.content.iter().enumerate() {
        content.raw_set(index + 1, block_table(lua, block)?)?;
    }
    content.set_metatable(Some(lua.array_metatable()))?;
    table.set(CONTENT_KEY, content)?;
    Ok(Value::Table(table))
}

fn block_table(lua: &Lua, block: &ContentBlock) -> LuaResult<Value> {
    let ContentBlock::Image { source } = block else {
        let json = serde_json::to_value(block).map_err(mlua::Error::external)?;
        return json_to_lua_exact(lua, &json);
    };
    let table = lua.create_table_with_capacity(0, 2)?;
    table.set(TYPE_KEY, BLOCK_IMAGE)?;
    table.set(
        SOURCE_KEY,
        lua.create_userdata(LuaImage::from_source(source.clone()))?,
    )?;
    Ok(Value::Table(table))
}

/// Tagged as an array, so an empty history still encodes as `[]`.
async fn messages_table(lua: &Lua, messages: &[Message]) -> LuaResult<Table> {
    let out = lua.create_table_with_capacity(messages.len(), 0)?;
    for (index, message) in messages.iter().enumerate() {
        if index > 0 && index % MESSAGES_PER_YIELD == 0 {
            smol::future::yield_now().await;
        }
        out.raw_set(index + 1, message_table(lua, message)?)?;
    }
    out.set_metatable(Some(lua.array_metatable()))?;
    Ok(out)
}

async fn transcript_table(lua: &Lua, export: SessionExport) -> LuaResult<Table> {
    let envelope = serde_json::to_value(export.envelope()).map_err(mlua::Error::external)?;
    let table = lua.unpack::<Table>(json_to_lua_exact(lua, &envelope)?)?;
    table.set(MESSAGES_KEY, messages_table(lua, &export.messages).await?)?;
    Ok(table)
}

/// Reads a session as the document `maki session export` prints, the same
/// table `maki.session.new` takes back:
///
/// ```text
/// { format, model, mode, thinking, fast, title, facts, messages }
/// ```
///
/// `messages` is what `maki.session.messages` returns. `facts` is the date,
/// working directory, model, instructions and hints the model was last told,
/// nil when the session never ran. A live session is read from memory,
/// unsaved messages included, any other from disk without claiming it, so a
/// session another maki has open can be read too.
///
/// JSON null comes back as `maki.NIL` and arrays are tagged, so
/// `maki.session.export(...)` encodes to the same document as the CLI file.
/// Copy it with `maki.deepcopy` before editing, which keeps both intact.
///
/// @param opts table? `session` (string?) Session id, live or stored. Defaults to the focused one.
/// @return (table|nil, string|nil) The document, or nil and an error.
/// @example
/// local doc = assert(maki.session.export({ session = maki.session.current() }))
/// maki.fs.write("/tmp/session.json", maki.json.encode(doc))
#[lua_fn(guard = SessionRead)]
async fn export(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let id = match &opts {
        Some(opts) => opts.opt::<String>("session")?,
        None => None,
    };
    let session = try_pair!(named_or_focused(&lua, tx.as_ref(), id).await);
    let transcript = try_pair!(transcript(&lua, tx.as_ref(), session).await);
    Ok((Some(transcript_table(&lua, transcript).await?), None))
}

/// Reads a session's transcript, oldest first: everything the model has
/// been sent so far, tool calls and results included. Read only.
///
/// Each message has the same shape as in the session file:
///
/// ```text
/// { role = "user" | "assistant", kind, facts?, display_text?, content = { blocks } }
/// ```
///
/// `kind` is `"turn"` for something the user or the model said,
/// `"observation"` for a report sent to the model as a user message, like
/// `maki.session.notify`, and `"context_update"` for a change since the
/// system prompt was built (date, model, plan mode, ...), which carries the
/// changed `facts`. `display_text` is what the UI shows instead of the
/// content: nil shows the content, `""` hides the message (a nudge, a
/// compaction note). `content` lists blocks:
///
/// ```text
/// { type = "text", text }
/// { type = "thinking", thinking, signature? }
/// { type = "redacted_thinking", data }
/// { type = "tool_use", id, name, input, thought_signature? }
/// { type = "tool_result", tool_use_id, content, is_error?, loaded_tools? }
/// { type = "image", source = maki.image.Image }
/// ```
///
/// `signature`, `thought_signature`, `data` and `loaded_tools` are opaque:
/// pass them through untouched. A block type this build does not know comes
/// as its raw table. JSON null is `maki.NIL`.
///
/// A live session is read from memory, unsaved messages included, any other
/// from disk. Works in the TUI, `maki -p`, sdk mode, and ACP. ACP has no
/// focused session, so pass `session` there. Hook and event payloads carry
/// the `session_id` to pass.
///
/// @param opts table? Options:
///   `session` (string?) Session id, live or stored. Defaults to the focused one.
///   `last` (integer?) only the newest `last` messages.
/// @return (table|nil, string|nil) Array of messages, or nil and an error.
/// @example
/// local msgs = maki.session.messages({ last = 1 })
/// local last = msgs and msgs[1]
/// if last and last.role == "assistant" and last.content[1].type == "text" then
///   print(last.content[1].text)
/// end
#[lua_fn(guard = SessionRead)]
async fn messages(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let (id, last) = match &opts {
        Some(opts) => (opts.opt::<String>("session")?, opts.opt::<usize>("last")?),
        None => (None, None),
    };
    let session = try_pair!(named_or_focused(&lua, tx.as_ref(), id).await);
    let history = match live_history(session) {
        Some(history) => history,
        None => try_pair!(transcript(&lua, tx.as_ref(), session).await).messages,
    };
    let skip = last.map_or(0, |n| history.len().saturating_sub(n));
    Ok((Some(messages_table(&lua, &history[skip..]).await?), None))
}

/// Returns the id of the focused session. Under `maki -p` and the sdk,
/// that is the one session they run.
///
/// @return (string|nil, string|nil) Session id, or nil and an error.
/// @example
/// local id = maki.session.current()
#[lua_fn]
async fn current(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<String>> {
    Ok(pair(focused_session(&lua, tx.as_ref()).await))
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
#[lua_fn(guard = SessionWrite)]
async fn delete(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    id: String,
) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Delete { id }).await
}

fn is_untagged_empty(lua: &Lua, table: &Table) -> bool {
    table.is_empty() && table.metatable().as_ref() != Some(&lua.array_metatable())
}

/// An untagged `{}` cannot say whether it was a list, and guessing one
/// wrong reaches the model, so it is refused with its path.
async fn messages_json(lua: &Lua, messages: &Table) -> LuaResult<Result<JsonValue, String>> {
    if is_untagged_empty(lua, messages) {
        return Ok(Err(format!("{MESSAGES_KEY}: {EMPTY_ARRAY_ERR}")));
    }
    let len = messages.raw_len();
    // Not a list. The conversion keeps every key, and the transcript parser
    // names what is wrong with it.
    if messages.pairs::<Value, Value>().count() != len {
        return Ok(Ok(lua_to_json(lua, &Value::Table(messages.clone()))?));
    }
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        if index > 0 && index % MESSAGES_PER_YIELD == 0 {
            smol::future::yield_now().await;
        }
        let message: Value = messages.raw_get(index + 1)?;
        if let Value::Table(fields) = &message
            && let Value::Table(content) = fields.get::<Value>(CONTENT_KEY)?
            && is_untagged_empty(lua, &content)
        {
            return Ok(Err(format!(
                "{MESSAGES_KEY}[{index}].{CONTENT_KEY}: {EMPTY_ARRAY_ERR}"
            )));
        }
        out.push(lua_to_json(lua, &message)?);
    }
    Ok(Ok(JsonValue::Array(out)))
}

/// Exactly the export document's fields. The run options stay behind.
async fn document_json(lua: &Lua, opts: &Table) -> LuaResult<Result<JsonValue, String>> {
    let mut doc = Map::new();
    for key in DOCUMENT_KEYS {
        let value: Value = opts.get(key)?;
        let json = match &value {
            Value::Nil => continue,
            Value::Table(messages) if key == MESSAGES_KEY => {
                match messages_json(lua, messages).await? {
                    Ok(json) => json,
                    Err(e) => return Ok(Err(e)),
                }
            }
            _ => lua_to_json(lua, &value)?,
        };
        doc.insert(key.to_owned(), json);
    }
    Ok(Ok(JsonValue::Object(doc)))
}

/// Where a session lands without a UI: the project of the one session a
/// single-session driver (`maki -p`, sdk) runs, so the project list and
/// `maki -r` find it there, else where maki runs. ACP holds many sessions, so
/// it gets the process's directory too, never one of its sessions picked by
/// how many happen to be open.
fn headless_cwd(lua: &Lua) -> Result<String, String> {
    let driver = lua
        .app_data_ref::<SessionSnapshotSlot>()
        .and_then(|slot| (slot.0)(None).ok())
        .and_then(|snapshot| snapshot.get(CWD_KEY)?.as_str().map(str::to_owned));
    if let Some(cwd) = driver {
        return Ok(cwd);
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    Ok(cwd.to_string_lossy().into_owned())
}

/// Creates a session from the document `maki.session.export` returns, or
/// from nothing.
///
/// Whatever the document leaves out is what a new tab would get: in the TUI
/// the focused tab's project, model, mode, thinking, fast and workflow, under
/// `maki -p` and sdk mode the project of the session they run, under ACP the
/// directory maki runs in. Outside the TUI no model is stored, so the session
/// opens on the default model. A document with messages gets no permissions
/// (`yolo`, session rules) from either side. Without messages it is a new tab
/// like `Ctrl-N`, which keeps the focused tab's `yolo`.
///
/// Without `prompt` or `focus` the session is only stored, and
/// `maki.session.focus(id)` opens it later. With either it opens in a tab.
/// A session is only stored once it has messages, so storing a document
/// with none left after repair fails. Pass `focus` to open a blank one
/// instead.
/// Headless drivers (`maki -p`, ACP) have nothing to open it in, so there
/// `prompt` and `focus` answer `nil, "no interactive UI attached"` and store
/// nothing.
///
/// History is repaired the way a resume repairs it (orphan tool results go,
/// a dangling final tool call is closed) and anything else wrong is refused
/// with its location, like `messages[12].content[2]: ...` (0-based, as in
/// the JSON document). A block of a known type that does not parse, or one
/// without a `type`, is refused too, while one of a type this build does not
/// know is kept. Thinking blocks and `thought_signature` are dropped: they
/// are bound to a system prompt the new session will not have. `model` is
/// stored as given and resolved when the session opens, falling back with a
/// warning like any resume.
///
/// Image sources may be a `maki.image.Image` or the
/// `{ type = "base64", media_type, data }` table `maki.json` gives. An empty
/// table where a list belongs (`messages`, a message's `content`) is refused,
/// since nothing tells it from an empty object: copy with `maki.deepcopy`,
/// which keeps the tag. Fields `new` does not know are ignored.
///
/// @param opts table? Optional fields:
///   `messages` (table) what `export` or `messages` return.
///   `facts` (table) what `export` returns: the date, working directory,
///     model, instructions and hints the model was last told.
///   `model` (string) model spec. Defaults to the focused tab's in the TUI,
///     else the default model when the session opens.
///   `mode` (string) "build" or "plan".
///   `thinking` (string) spelled like the thinking setting.
///   `fast` (boolean) fast output mode, where the model supports it.
///   `title` (string) else generated from the history.
///   `prompt` (string) first user message to submit right away. Needs the
///     `agent` permission.
///   `focus` (boolean) switch the UI to the new session.
/// @return (string|nil, string|nil) New session id, or nil and an error.
/// @example
/// -- copy a session, keeping it in the background
/// local doc = assert(maki.session.export({ session = id }))
/// local copy, err = maki.session.new(doc)
///
/// -- start a fresh one right away
/// local id, err = maki.session.new({ prompt = "fix the tests", focus = true })
#[lua_fn(guard = SessionWrite)]
async fn new(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] may_run_agents: bool,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let opts = match opts {
        Some(opts) => opts,
        None => lua.create_table()?,
    };
    let prompt: Option<String> = opts.opt(PROMPT_KEY)?;
    let focus = opts.opt_bool(FOCUS_KEY).unwrap_or(false);
    if prompt.is_some() && !may_run_agents {
        return Err(denied_error(Permission::Agent));
    }
    let opens = prompt.is_some() || focus;
    // Asked once: a UI that detaches after this fails the roundtrip below
    // instead of having an opening call quietly stored.
    let has_ui = ui_reachable(tx.as_ref());
    if opens && !has_ui {
        return Ok(err_pair(NO_UI_ERR));
    }
    let doc = try_pair!(document_json(&lua, &opts).await?);
    // Off the Lua thread, since parsing a long history is real work.
    let parsed = smol::unblock(move || serde_json::from_value::<Transcript>(doc)).await;
    let transcript = try_pair!(parsed.map_err(|e| e.to_string()));
    if has_ui {
        let req = SessionRequest::New {
            transcript: Box::new(transcript),
            prompt,
            focus,
        };
        return roundtrip(lua, tx, req).await;
    }
    let base = StoredSession::new("", &try_pair!(headless_cwd(&lua)));
    let dir = try_pair!(state_dir(&lua));
    let id = try_pair!(
        smol::unblock(move || {
            let session = transcript.into_session(base).map_err(|e| e.to_string())?;
            store_new(session, &dir).map_err(|e| e.to_string())
        })
        .await
    );
    Ok((Some(id.to_string().into_lua(&lua)?), None))
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
#[lua_fn(guard = [Agent, SessionWrite])]
async fn prompt(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.opt("session")?,
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
///   `wake` (boolean) start a TUI turn when it next becomes idle (default
///     false). Needs the `agent` permission.
///   `display` (string?) one-line note for the transcript, shown and saved
///     when the session's agent run accepts the report. Whitespace collapses
///     to single spaces and the note is cut to 256 bytes. Defaults to
///     nothing; the model reads only {text}.
/// @return (boolean|nil, string|nil) true, or nil and an error.
/// @example
/// maki.session.notify("[monitor] deploy failed", { session = id, wake = true })
#[lua_fn(guard = SessionWrite)]
fn notify(
    _lua: &Lua,
    #[ctx] may_run_agents: bool,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<bool>> {
    let wake = opts
        .as_ref()
        .and_then(|t| t.opt_bool("wake"))
        .unwrap_or(false);
    if wake && !may_run_agents {
        return Err(denied_error(Permission::Agent));
    }
    if text.trim().is_empty() {
        return Ok(err_pair(BLANK_NOTIFY_ERR));
    }
    let session_id = try_pair!(required_session(opts.as_ref())?);
    let display = opts
        .as_ref()
        .map(|t| t.opt::<String>("display"))
        .transpose()?
        .flatten();
    if let Err(error) = SessionMailbox::notify(session_id, text, display.as_deref(), wake) {
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
#[lua_fn(guard = SessionSettings)]
async fn set_mode(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    mode: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.opt("session")?,
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
#[lua_fn(guard = SessionSettings)]
async fn set_title(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Table,
) -> LuaResult<Pair<Value>> {
    let req = SessionRequest::SetTitle {
        id: opts.get::<String>("id")?,
        title: opts.get::<String>("title")?,
    };
    roundtrip(lua, tx, req).await
}

lua_table! {
    /// Host session primitives. The interactive UI can run several sessions
    /// at once. These functions list, create, focus, rename, and delete them.
    ///
    /// Without a UI, most functions return `nil, "no interactive UI attached"`.
    /// `current`, `read`, `export`, `messages`, `notify` and `new` without
    /// `prompt` or `focus` still work. ACP has no focused session, so there
    /// `current` fails and `read`, `export` and `messages` need `session`.
    "maki.session" => pub(crate) fn create_session_table(
        tx: Option<flume::Sender<UiAction>>,
        perms: &PluginPermissions,
        may_run_agents: bool,
    ),
    DOCS [
        list(tx), live(tx), current(tx), read(tx), export(perms, tx), messages(perms, tx), focus(tx),
        delete(perms, tx), new(perms, tx, may_run_agents), prompt(perms, tx),
        notify(perms, may_run_agents), set_mode(perms, tx), set_title(perms, tx),
    ]
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use maki_agent::agent::{
        History, HistorySnapshot, SharedHeader, SharedMessages, publish_live_history,
        publish_live_session,
    };
    use maki_agent::session::{NOTHING_TO_STORE, SessionHeader};
    use maki_providers::{ImageMediaType, ImageSource, MessageKind, Role};
    use maki_storage::frame::{FactsUpdate, PromptFacts, StoredFrame};
    use maki_storage::sessions::{StoredMode, StoredThinking};
    use tempfile::TempDir;

    use super::*;
    use crate::api::base64::create_base64_table;
    use crate::api::image::create_image_table;
    use crate::api::json::create_json_table;
    use crate::api::top::add_top_methods;
    use crate::session_snapshot::{MODE_PLAN, STATUS_IDLE, STATUS_STORED};
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
    const TITLE: &str = "ported";
    const MODEL: &str = "anthropic/claude-sonnet-4-5";
    const THINKING: &str = "high";
    const PLUGIN: &str = "test";
    const FACTS_CWD: &str = "/origin";
    const ONE_MESSAGE: &str = "{ { role = 'user', content = { { type = 'text', text = 'hi' } } } }";

    fn lua_with_session(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_session_table(&lua, tx, &PluginPermissions::trusted(), true).unwrap();
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
        assert_eq!(messages[0].first_text_content(), Some("built"));
        assert_eq!(messages[0].display_text, None);
        assert!(rx.try_recv().is_err());
    }

    #[test_case(" agent-1 finished ", Some("agent-1 finished"); "trimmed")]
    #[test_case("agent-1\n finished", Some("agent-1 finished"); "single_line")]
    #[test_case("   ", None; "blank")]
    fn notify_normalizes_display_notes_without_changing_model_text(
        display: &str,
        expected: Option<&str>,
    ) {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();
        lua.globals().set("notice_text", TEXT).unwrap();
        lua.globals().set("display_note", display).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load(concat!(
                "return session.notify(notice_text, { session = session_id, ",
                "wake = true, display = display_note })"
            ))
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = mailbox.claim_wake();
        assert_eq!(messages[0].display_text.as_deref(), expected);
        assert_eq!(messages[0].first_text_content(), Some(TEXT));
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

    #[test_case("messages" ; "messages")]
    #[test_case("export" ; "export")]
    fn reading_a_session_that_does_not_exist_is_an_error_pair(call: &str) {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let code = format!("return session.{call}({{ session = '{id}' }})");
        let (value, err): (Value, Option<String>) =
            smol::block_on(lua.load(&code).eval_async()).unwrap();
        assert!(value.is_nil());
        assert!(err.is_some_and(|err| err.contains(&id.to_string())));
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
    fn current_names_the_focused_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let lua = focused_on(id);
        let (got, err): (Option<String>, Option<String>) =
            smol::block_on(lua.load("return session.current()").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(got, Some(id.to_string()));
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

    /// `session` plus the `maki` pieces a plugin round trips with, and
    /// `one_message`, the smallest history `new` stores.
    fn plugin_lua(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = lua_with_session(tx);
        let maki = lua.create_table().unwrap();
        maki.set("json", create_json_table(&lua).unwrap()).unwrap();
        maki.set("image", create_image_table(&lua).unwrap())
            .unwrap();
        maki.set("base64", create_base64_table(&lua).unwrap())
            .unwrap();
        maki.set("NIL", Value::NULL).unwrap();
        add_top_methods(&maki, &lua, PLUGIN.into()).unwrap();
        lua.globals().set("maki", maki).unwrap();
        lua.load(format!("one_message = {ONE_MESSAGE}"))
            .exec()
            .unwrap();
        lua
    }

    /// No UI, and stored sessions in a temp dir.
    fn headless() -> (Lua, TempDir) {
        let dir = TempDir::new().unwrap();
        let lua = plugin_lua(None);
        lua.set_app_data(StateDir::from_path(dir.path().to_path_buf()));
        (lua, dir)
    }

    fn storage(lua: &Lua) -> StateDir {
        lua.app_data_ref::<StateDir>().unwrap().clone()
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn user(content: Vec<ContentBlock>) -> Message {
        Message {
            content,
            ..Default::default()
        }
    }

    fn image() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()),
        }
    }

    /// Something of every shape a stored transcript holds, the opaque fields
    /// and a null inside a tool input included.
    fn rich_history() -> Vec<Message> {
        vec![
            Message::user(TEXT.into()),
            assistant(vec![
                ContentBlock::Thinking {
                    thinking: THOUGHT.into(),
                    signature: Some(SIGNATURE.into()),
                },
                ContentBlock::RedactedThinking {
                    data: SIGNATURE.into(),
                },
                ContentBlock::ToolUse {
                    id: TOOL_ID.into(),
                    name: TOOL_NAME.into(),
                    input: json!({ "path": TOOL_PATH, "offset": null, "files": [] }),
                    thought_signature: Some(SIGNATURE.into()),
                },
            ]),
            user(vec![ContentBlock::ToolResult {
                tool_use_id: TOOL_ID.into(),
                content: TOOL_OUTPUT.into(),
                is_error: true,
                loaded_tools: vec![TOOL_NAME.into()],
            }]),
            Message {
                kind: MessageKind::ContextUpdate(Arc::new(FactsUpdate {
                    cwd: Some(FACTS_CWD.into()),
                    ..Default::default()
                })),
                content: vec![ContentBlock::Text { text: TEXT.into() }],
                ..Default::default()
            },
            Message {
                kind: MessageKind::Observation,
                display_text: Some(String::new()),
                content: vec![
                    image(),
                    ContentBlock::Unknown(json!({ "type": "audio", "id": 1 })),
                ],
                ..Default::default()
            },
            assistant(vec![ContentBlock::Text { text: TEXT.into() }]),
        ]
    }

    fn stored(lua: &Lua, messages: Vec<Message>) -> MakiId {
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(messages);
        session.set_title(TITLE.into());
        session.meta.mode = Some(StoredMode::Plan);
        session.meta.thinking = Some(StoredThinking::parse_setting(THINKING).unwrap());
        session.meta.fast = true;
        session.set_frame(Some(Arc::new(StoredFrame::facts_only(PromptFacts {
            cwd: FACTS_CWD.into(),
            ..Default::default()
        }))));
        store_new(session, &storage(lua)).unwrap()
    }

    fn eval<T: mlua::FromLuaMulti>(lua: &Lua, code: &str) -> T {
        smol::block_on(lua.load(code).eval_async()).unwrap()
    }

    fn json_of(lua: &Lua, value: Value) -> JsonValue {
        lua_to_json(lua, &value).unwrap()
    }

    /// What `new` drops on purpose: thinking is bound to the origin's prefix.
    fn without_thinking(mut doc: JsonValue) -> JsonValue {
        for message in doc[MESSAGES_KEY].as_array_mut().unwrap() {
            let blocks = message[CONTENT_KEY].as_array_mut().unwrap();
            blocks.retain(|block| {
                !matches!(
                    block["type"].as_str(),
                    Some("thinking" | "redacted_thinking")
                )
            });
            for block in blocks {
                block.as_object_mut().unwrap().remove("thought_signature");
            }
        }
        doc
    }

    #[test]
    fn messages_are_the_serde_shape_with_nulls_kept() {
        let id = MakiId::generate();
        let history = rich_history();
        let _live = live(id, history.clone());
        let (lua, _dir) = headless();

        let msgs: Table = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}' }})"),
        );

        assert_eq!(
            json_of(&lua, Value::Table(msgs)),
            serde_json::to_value(&history).unwrap()
        );
    }

    #[test]
    fn an_image_source_is_a_handle_sharing_the_payload() {
        let id = MakiId::generate();
        let block = image();
        let ContentBlock::Image { source } = &block else {
            unreachable!()
        };
        let payload = Arc::clone(&source.data);
        let _live = live(id, vec![user(vec![block])]);
        let (lua, _dir) = headless();

        let handle: mlua::AnyUserData = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}' }})[1].content[1].source"),
        );

        let shared = handle.borrow::<LuaImage>().unwrap().to_source();
        assert!(Arc::ptr_eq(&shared.data, &payload));
    }

    /// The document goes out and comes back as one table, through
    /// `maki.deepcopy` like a redactor would copy it, and only what `new`
    /// drops on purpose is gone.
    #[test]
    fn export_new_export_round_trips_minus_thinking() {
        let (lua, _dir) = headless();
        let id = stored(&lua, rich_history());
        lua.globals().set("origin", id.to_string()).unwrap();

        let (before, after): (Value, Value) = eval(
            &lua,
            r#"
            local doc = assert(session.export({ session = origin }))
            local copy = assert(session.new(maki.deepcopy(doc)))
            return doc, assert(session.export({ session = copy }))
            "#,
        );

        let after = json_of(&lua, after);
        assert_eq!(after, without_thinking(json_of(&lua, before)));
        assert_eq!(after["facts"]["cwd"], FACTS_CWD);
    }

    #[test]
    fn messages_reads_the_tail_of_a_stored_transcript() {
        let (lua, _dir) = headless();
        let history = rich_history();
        let id = stored(&lua, history.clone());

        let msgs: Value = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}', last = 2 }})"),
        );

        assert_eq!(
            json_of(&lua, msgs),
            serde_json::to_value(&history[history.len() - 2..]).unwrap()
        );
    }

    #[test]
    fn read_of_a_stored_session_reports_its_settings() {
        let (lua, _dir) = headless();
        let id = stored(&lua, vec![Message::user(TEXT.into())]);

        let snapshot: Table = eval(
            &lua,
            &format!("return session.read({{ session = '{id}' }})"),
        );

        assert_eq!(snapshot.get::<String>("status").unwrap(), STATUS_STORED);
        assert_eq!(snapshot.get::<String>("thinking").unwrap(), THINKING);
        assert!(snapshot.get::<bool>("fast").unwrap());
        assert_eq!(snapshot.get::<String>("mode").unwrap(), MODE_PLAN);
        assert_eq!(snapshot.get::<String>("model").unwrap(), MODEL);
        assert_eq!(snapshot.get::<String>("title").unwrap(), TITLE);
    }

    /// A failed call leaves nothing behind. A headless driver has no tab to
    /// open, so `prompt` and `focus` fail there before anything is stored.
    #[test_case("{ messages = {} }", format!("{MESSAGES_KEY}: {EMPTY_ARRAY_ERR}") ; "untagged_messages")]
    #[test_case(
        "{ messages = { one_message[1], { role = 'user', content = {} } } }",
        format!("{MESSAGES_KEY}[1].{CONTENT_KEY}: {EMPTY_ARRAY_ERR}")
        ; "untagged_content_is_located"
    )]
    #[test_case("{}", NOTHING_TO_STORE.to_owned() ; "nothing_to_store")]
    #[test_case("{ title = 'x', messages = maki.json.decode('[]') }", NOTHING_TO_STORE.to_owned() ; "tagged_empty_history")]
    #[test_case(r#"{ messages = maki.json.decode('[{"role":"user","content":[]}]') }"#, NOTHING_TO_STORE.to_owned() ; "empty_after_repair")]
    #[test_case("{ prompt = 'go', messages = one_message }", NO_UI_ERR.to_owned() ; "headless_prompt")]
    #[test_case("{ focus = true, messages = one_message }", NO_UI_ERR.to_owned() ; "headless_focus")]
    fn new_refuses(opts: &str, expected: String) {
        let (lua, _dir) = headless();
        let (id, err): (Value, Option<String>) = eval(&lua, &format!("return session.new({opts})"));
        assert!(id.is_nil());
        assert_eq!(err, Some(expected));
        assert!(StoredSession::list_all(&storage(&lua)).unwrap().is_empty());
    }

    fn image_history(source: &str) -> String {
        format!(
            "{{ {{ role = 'user', content = {{ {{ type = 'image', source = {source} }} }} }} }}"
        )
    }

    #[test_case("{ type = 'base64', media_type = 'image/png', data = image_data }" ; "base64_table")]
    #[test_case("maki.image.decode(maki.base64.decode(image_data))" ; "handle")]
    fn headless_new_stores_an_image_given_either_way(source: &str) {
        let (lua, _dir) = headless();
        lua.globals().set("image_data", tiny_png()).unwrap();
        let code = format!(
            "return assert(session.new({{ messages = {} }}))",
            image_history(source)
        );
        let id: String = eval(&lua, &code);

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        let [Message { content, .. }] = session.messages() else {
            panic!("expected one message");
        };
        let [ContentBlock::Image { source }] = content.as_slice() else {
            panic!("expected one image block");
        };
        assert_eq!(source.media_type, ImageMediaType::Png);
        assert_eq!(&*source.data, tiny_png());
    }

    fn tiny_png() -> String {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        STANDARD.encode(bytes.into_inner())
    }

    /// Validation is the UI's: it lays the transcript over the focused tab's
    /// blank session, so the transcript arrives as parsed.
    #[test]
    fn new_with_a_ui_hands_it_the_parsed_transcript() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = plugin_lua(Some(tx));
        let id = MakiId::generate();
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req:
                    SessionRequest::New {
                        transcript,
                        prompt,
                        focus,
                    },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected new request");
            };
            assert_eq!(transcript.title.as_deref(), Some(TITLE));
            assert_eq!(transcript.messages.len(), 1);
            assert_eq!(transcript.fast, None);
            assert_eq!(prompt, None);
            assert!(focus);
            reply_tx.send(Ok(json!(id))).unwrap();
        });
        let code = format!(
            "return session.new({{ title = '{TITLE}', focus = true, messages = one_message }})"
        );
        let (got, err): (String, Option<String>) = eval(&lua, &code);
        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(got, id.to_string());
    }

    #[test]
    fn export_with_a_ui_is_answered_by_it() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let id = MakiId::generate();
        std::thread::spawn(move || {
            let Ok(UiAction::ExportSession {
                id: asked,
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected export request");
            };
            assert_eq!(asked, id);
            let mut session = StoredSession::new(MODEL, FACTS_CWD);
            session.set_title(TITLE.into());
            session.replace_messages(vec![Message::user(TEXT.into())]);
            reply_tx.send(Ok(SessionExport::stored(session))).unwrap();
        });

        let doc: Value = eval(
            &lua,
            &format!("return session.export({{ session = '{id}' }})"),
        );

        let doc = json_of(&lua, doc);
        assert_eq!(doc["title"], TITLE);
        assert_eq!(doc[MESSAGES_KEY][0]["content"][0]["text"], TEXT);
    }

    /// A headless driver's session before its first save: `maki -p`'s
    /// `TurnEnd` hook, ACP's first prompt. Keep the returned pair alive for
    /// as long as the session should be.
    fn live_unsaved(id: MakiId, messages: Vec<Message>) -> (History, SharedHeader) {
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.set_title(TITLE.into());
        session.meta.fast = true;
        let header: SharedHeader = Arc::new(ArcSwap::from_pointee(SessionHeader::of(&session)));
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_session(id, &mirror, &header);
        (History::new(messages).with_mirror(mirror), header)
    }

    #[test]
    fn headless_reads_of_a_live_unsaved_session_need_no_file() {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let history = vec![Message::user(TEXT.into())];
        let _live = live_unsaved(id, history.clone());
        lua.globals().set("id", id.to_string()).unwrap();

        let (doc, snapshot): (Value, Table) = eval(
            &lua,
            "return assert(session.export({ session = id })), assert(session.read({ session = id }))",
        );

        let doc = json_of(&lua, doc);
        assert_eq!(doc["title"], TITLE);
        assert_eq!(doc["model"], MODEL);
        assert_eq!(doc["fast"], true);
        assert_eq!(doc[MESSAGES_KEY], serde_json::to_value(&history).unwrap());
        assert_eq!(snapshot.get::<String>("status").unwrap(), STATUS_IDLE);
        assert_eq!(snapshot.get::<String>("cwd").unwrap(), FACTS_CWD);
    }

    /// A stored-only session lands in the project of the session the driver
    /// runs, not wherever the process started.
    #[test]
    fn headless_new_lands_in_the_driver_session_cwd() {
        const DRIVER_CWD: &str = "/driver/project";
        let (lua, _dir) = headless();
        let provider: SessionSnapshotFn =
            Box::new(|_| Ok(json!({ "id": MakiId::generate().to_string(), "cwd": DRIVER_CWD })));
        lua.set_app_data(SessionSnapshotSlot(provider));

        let id: String = eval(
            &lua,
            "return assert(session.new({ messages = one_message }))",
        );

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        assert_eq!(session.cwd, DRIVER_CWD);
    }

    /// ACP installs no slot. The one session it happens to have open must not
    /// decide where an import lands, or a second open session would move it.
    #[test]
    fn headless_new_without_a_driver_session_lands_where_maki_runs() {
        let (lua, _dir) = headless();
        let _live = live_unsaved(MakiId::generate(), Vec::new());

        let id: String = eval(
            &lua,
            "return assert(session.new({ messages = one_message }))",
        );

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        let here = std::env::current_dir().unwrap();
        assert_eq!(session.cwd, here.to_string_lossy());
    }

    #[test]
    fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> =
            smol::block_on(lua.load("return session.set_title('oops')").eval_async());
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
