//! `maki.session`: host session primitives. Session management round-trips to
//! the UI event loop, which owns live runtimes and storage. `notify` posts
//! directly to the agent mailbox so synchronous callbacks can use it.
//!
//! Transcripts cross into Lua in the serde shape of [`Message`], the same
//! document `maki session export` prints. The one difference is an image
//! block's `source`, which is a `maki.image.Image` sharing the payload.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use maki_agent::agent::{SessionExport, live_header, live_history, live_snapshot};
use maki_agent::session::{StoredSession, store_new};
use maki_agent::{SessionMailbox, Transcript};
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::{ContentBlock, Message, Role};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_storage::paths::session_data_dir;
use maki_storage::sessions::ArchiveWindow;
use mlua::{IntoLua, Lua, LuaSerdeExt, Result as LuaResult, Table, Value};
use serde_json::{Map, Value as JsonValue};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{spawn_blocking, yield_now};

use crate::api::image::LuaImage;
use crate::api::util::command::{
    NO_UI_ERR, SessionRequest, UiAction, ui_json_roundtrip, ui_reachable, ui_roundtrip,
};
use crate::api::util::convert::{TableExt, json_to_lua, json_to_lua_exact, lua_to_json};
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::plugin_permissions::{Permission, PluginPermissions, require};
use crate::runtime::Lineage;
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
const CURRENT_WINDOW_ID: &str = "current";
const WINDOW_KEY: &str = "window";
const ID_KEY: &str = "id";
const KIND_KEY: &str = "kind";
const CREATED_AT_KEY: &str = "created_at";
const NR_KEY: &str = "nr";
const NAME_KEY: &str = "name";
const FALLBACK_SUBAGENT_NAME: &str = "subagent";
const CWD_KEY: &str = "cwd";
const PROMPT_KEY: &str = "prompt";
const FOCUS_KEY: &str = "focus";
/// What makes the chat input run a line on Enter rather than send it.
const RUN_PREFIXES: [char; 2] = ['/', '!'];
const EXIT_WORD: &str = "exit";
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

/// Rewind and fork put a user turn back in the chat input, and Enter runs it
/// as if a plugin that could send prompts wrote it. So a turn that runs
/// needs what sending it took, which an import alone does not prove.
fn runs_on_enter(message: &Message) -> bool {
    message.role == Role::User
        && !message.is_from_host()
        && !message.is_context_update()
        && message.user_text().is_some_and(|text| {
            let text = text.trim();
            text.starts_with(RUN_PREFIXES) || text == EXIT_WORD
        })
}

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
    spawn_blocking(move || answer(StoredSession::read_only(id, &dir).map_err(|e| e.to_string())?))
        .await
        .map_err(|e| e.to_string())
        .flatten()
}

async fn roundtrip(
    lua: Lua,
    tx: Option<UnboundedSender<UiAction>>,
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
async fn list(lua: Lua, #[ctx] tx: Option<UnboundedSender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::List).await
}

/// Lists the sessions currently running in this UI. Status is "working",
/// "needs_input", or "idle". Background work a turn moved out keeps the
/// session "working" until its result is in, with no "idle" in between.
///
/// @return (table|nil, string|nil) Array of `{id, title, status, updated_at, focused}`, or nil and an error.
/// @example
/// local live, err = maki.session.live()
#[lua_fn]
async fn live(lua: Lua, #[ctx] tx: Option<UnboundedSender<UiAction>>) -> LuaResult<Pair<Value>> {
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
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
    tx: Option<&UnboundedSender<UiAction>>,
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
    tx: Option<&UnboundedSender<UiAction>>,
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
async fn session_export(
    lua: &Lua,
    tx: Option<&UnboundedSender<UiAction>>,
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
    let envelope = message.with_content(Vec::new());
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
            yield_now().await;
        }
        out.raw_set(index + 1, message_table(lua, message)?)?;
    }
    out.set_metatable(Some(lua.array_metatable()))?;
    Ok(out)
}

/// The newest `last` of {messages}, or all of them.
pub(crate) async fn newest_messages_table(
    lua: &Lua,
    messages: &[Message],
    last: Option<usize>,
) -> LuaResult<Table> {
    let skip = last.map_or(0, |n| messages.len().saturating_sub(n));
    messages_table(lua, &messages[skip..]).await
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let id = match &opts {
        Some(opts) => opts.opt::<String>("session")?,
        None => None,
    };
    let session = try_pair!(named_or_focused(&lua, tx.as_ref(), id).await);
    let session_export = try_pair!(session_export(&lua, tx.as_ref(), session).await);
    Ok((Some(transcript_table(&lua, session_export).await?), None))
}

/// Reads a session's transcript, oldest first: everything the model has
/// been sent so far, tool calls and results included. Read only.
///
/// Each message has the same shape as in the session file:
///
/// ```text
/// { role = "user" | "assistant", kind, facts?, display_text?, receipts?, settles?, partial?, created_at?, content = { blocks } }
/// ```
///
/// `kind` is `"turn"` for something the user or the model said,
/// `"observation"` for a report sent to the model as a user message, like
/// `maki.session.notify`, and `"context_update"` for a change since the
/// system prompt was built (date, model, plan mode, ...), which carries the
/// changed `facts`. `display_text` is what the UI shows instead of the
/// content: nil shows the content, `""` hides the message (a nudge, a
/// compaction note). `receipts` lists the calls whose results in this
/// message moved to the background, and `settles` = `{ task, end }` marks the
/// notice that answered one, with `end` one of `"finished"`, `"failed"`,
/// `"cancelled"` or `"lost"`. `partial` has the same shape and marks a notice
/// that answered one piece early, like one call of a moved batch, and left
/// the receipt open. `created_at` is when the message was appended, in epoch
/// milliseconds, and nil for messages saved before the field existed.
/// `content` lists blocks:
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
/// from disk.
///
/// @param opts table? Options:
///   `session` (string?) Session id, live or stored. Defaults to the focused one.
///   `window` (string?) `"current"` or an archive id from `maki.session.windows`.
///     Defaults to the current window.
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let (id, last, window) = match &opts {
        Some(opts) => (
            opts.opt::<String>("session")?,
            opts.opt::<usize>("last")?,
            opts.opt::<String>(WINDOW_KEY)?,
        ),
        None => (None, None, None),
    };
    let session = try_pair!(named_or_focused(&lua, tx.as_ref(), id).await);
    if let Some(window) = window.filter(|window| window != CURRENT_WINDOW_ID) {
        let seq = try_pair!(parse_window_id(&window));
        let messages = try_pair!(archive_messages(&lua, session, seq).await);
        return Ok((
            Some(newest_messages_table(&lua, &messages, last).await?),
            None,
        ));
    }
    let history = match live_history(session) {
        Some(history) => history,
        None => try_pair!(session_export(&lua, tx.as_ref(), session).await).messages,
    };
    Ok((
        Some(newest_messages_table(&lua, &history, last).await?),
        None,
    ))
}

/// Returns the id of the focused session. Under `maki -p` and the sdk,
/// that is the one session they run.
///
/// @return (string|nil, string|nil) Session id, or nil and an error.
/// @example
/// local id = maki.session.current()
#[lua_fn]
async fn current(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
) -> LuaResult<Pair<String>> {
    Ok(pair(focused_session(&lua, tx.as_ref()).await))
}

/// Returns this plugin's data directory for a session,
/// `state/sessions/data/<session>/<plugin>/`. Deleting the session deletes
/// it. The directory may not exist yet, so create it with `maki.fs.mkdir`
/// before the first write. Writing there needs `fs_write` like any other path.
///
/// @param opts table? Options:
///   `session` (string?) Session id. Defaults to the session the calling
///     agent serves (so a tool in a background tab gets that tab's
///     directory), else the focused one.
/// @return (string|nil, string|nil) The directory path, or nil and an error.
/// @example
/// local dir, err = maki.session.data_dir()
/// if dir then
///   maki.fs.mkdir(dir, { parents = true })
///   maki.fs.write(maki.fs.joinpath(dir, "scratch.md"), text)
/// end
#[lua_fn]
async fn data_dir(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<String>> {
    let named = match &opts {
        Some(opts) => opts.opt::<String>("session")?,
        None => None,
    };
    let path: Result<String, String> = async {
        let named = named
            .map(|raw| raw.parse::<MakiId>().map_err(|e| e.to_string()))
            .transpose()?;
        let session = match named.or_else(|| Lineage::current(&lua).session) {
            Some(id) => id,
            None => focused_session(&lua, tx.as_ref())
                .await?
                .parse::<MakiId>()
                .map_err(|e| e.to_string())?,
        };
        let state = state_dir(&lua)?;
        Ok(session_data_dir(state.path(), session)
            .join(&*plugin)
            .display()
            .to_string())
    }
    .await;
    Ok(pair(path))
}

/// Lists a session's transcript windows, newest first. `[1]` is the current
/// window. The rest are archives, each sealed by the compaction or rewind
/// that replaced its turns. Listing reads no transcript, so it stays cheap.
///
/// Each window is:
/// ```text
/// {
///   id = "current" | "<seq>",  -- pass to `messages` or `subagents`
///   created_at,                -- when the window began, in Unix seconds,
///                              -- or nil if unknown
///   kind = "current" | "compaction" | "rewind" | "unknown",
/// }
/// ```
///
/// @param opts table? Options:
///   `session` (string?) Session id, live or stored. Defaults to the focused one.
/// @return (table|nil, string|nil) Array of windows, or nil and an error.
/// @example
/// for _, window in ipairs(assert(maki.session.windows())) do
///   print(window.id, window.kind, window.created_at)
/// end
#[lua_fn(guard = SessionRead)]
async fn windows(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let id = match &opts {
        Some(opts) => opts.opt::<String>("session")?,
        None => None,
    };
    let session = try_pair!(named_or_focused(&lua, tx.as_ref(), id).await);
    let dir = try_pair!(state_dir(&lua));
    let live = live_history(session).is_some();
    let read = spawn_blocking(move || {
        let archives = StoredSession::archive_windows(session, &dir);
        let exists = StoredSession::exists(session, &dir);
        let started_at = StoredSession::started_at(session, &dir);
        (archives, exists, started_at)
    })
    .await
    .map_err(mlua::Error::external)?;
    let (archives, exists, started_at) = read;
    if !exists && !live && archives.is_empty() {
        return Ok(err_pair(format!("session not found: {session}")));
    }
    Ok((Some(windows_table(&lua, &archives, started_at)?), None))
}

/// Reads the subagent transcripts of one window, in spawn order. A
/// transcript belongs to the window holding the `tool_use` that started it.
///
/// Each entry is:
/// ```text
/// {
///   id,       -- the `tool_use` id that spawned it, unique in the session
///   nr,       -- 1-based number in the stored subagent list. Can skip
///             -- numbers and repeat across windows, so key on `id`
///   name,     -- the task description that spawned it, or "subagent"
///   messages, -- same shape as `maki.session.messages`
/// }
/// ```
///
/// Only the TUI stores subagent transcripts. Sessions run by `maki -p`, the
/// SDK or ACP give an empty list. Transcripts come from disk, so a running
/// subagent shows only the turns saved so far.
///
/// @param opts table? Options:
///   `session` (string?) Session id, live or stored. Defaults to the focused one.
///   `window` (string?) `"current"` or an archive id from `maki.session.windows`.
///     Defaults to the current window.
/// @return (table|nil, string|nil) Array of subagents, or nil and an error.
/// @example
/// for _, sub in ipairs(assert(maki.session.subagents())) do
///   print(sub.id, sub.nr, sub.name, #sub.messages)
/// end
#[lua_fn(guard = SessionRead)]
async fn subagents(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let (id, window) = match &opts {
        Some(opts) => (
            opts.opt::<String>("session")?,
            opts.opt::<String>(WINDOW_KEY)?,
        ),
        None => (None, None),
    };
    let session = try_pair!(named_or_focused(&lua, tx.as_ref(), id).await);
    let live = live_history(session);
    let live_exists = live.is_some();
    let dir = try_pair!(state_dir(&lua));
    let read = spawn_blocking(move || -> Result<_, String> {
        let archives = StoredSession::archive_windows(session, &dir);
        let index = window_index(window.as_deref(), &archives)?;
        let start = index.saturating_sub(1);
        let end = (index + 1).min(archives.len());
        let mut windows = Vec::with_capacity(end + 1 - start);
        for position in start..=end {
            windows.push(load_window(
                session,
                &dir,
                &archives,
                position,
                live.as_ref(),
            )?);
        }
        Ok((index - start, windows))
    })
    .await
    .map_err(mlua::Error::external)?;
    let (asked, windows) = try_pair!(read);
    let window = windows[asked].session.as_ref();
    if window.is_none() && !live_exists {
        return Ok(err_pair(format!("session not found: {session}")));
    }
    let attach = subagent_attach(&windows, asked);
    let table = subagents_table(&lua, window, &attach).await?;
    Ok((Some(table), None))
}

fn parse_window_id(raw: &str) -> Result<u64, String> {
    raw.parse().map_err(|_| format!("window not found: {raw}"))
}

fn window_index(window: Option<&str>, archives: &[ArchiveWindow]) -> Result<usize, String> {
    match window {
        None | Some(CURRENT_WINDOW_ID) => Ok(archives.len()),
        Some(raw) => {
            let seq = parse_window_id(raw)?;
            archives
                .iter()
                .position(|archive| archive.seq == seq)
                .ok_or_else(|| format!("window not found: {raw}"))
        }
    }
}

/// One window's transcript, and the stored session that holds its subagent
/// records. Only the asked window and its two neighbors are ever loaded.
struct WindowData {
    messages: Arc<Vec<Message>>,
    session: Option<StoredSession>,
}

/// Loads the window at `index`, the current window last. The current window
/// reads the live transcript when there is one, since it holds turns the
/// stored copy does not.
fn load_window(
    id: MakiId,
    dir: &StateDir,
    archives: &[ArchiveWindow],
    index: usize,
    live: Option<&Arc<Vec<Message>>>,
) -> Result<WindowData, String> {
    if let Some(archive) = archives.get(index) {
        let session = StoredSession::archive(id, archive.seq, dir)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("window not found: {}", archive.seq))?;
        return Ok(WindowData {
            messages: session.shared_messages().clone(),
            session: Some(session),
        });
    }
    let session = match StoredSession::read_only(id, dir) {
        Ok(session) => Some(session),
        Err(e) if e.is_not_found() => None,
        Err(e) => return Err(e.to_string()),
    };
    let messages = match (live, session.as_ref()) {
        (Some(live), _) => live.clone(),
        (None, Some(session)) => session.shared_messages().clone(),
        (None, None) => Arc::default(),
    };
    Ok(WindowData { messages, session })
}

async fn archive_messages(lua: &Lua, id: MakiId, seq: u64) -> Result<Vec<Message>, String> {
    let dir = state_dir(lua)?;
    spawn_blocking(move || {
        StoredSession::archive(id, seq, &dir)
            .map_err(|e| e.to_string())?
            .map(StoredSession::take_messages)
            .ok_or_else(|| format!("window not found: {seq}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn windows_table(
    lua: &Lua,
    archives: &[ArchiveWindow],
    started_at: Option<u64>,
) -> LuaResult<Table> {
    let out = lua.create_table_with_capacity(archives.len() + 1, 0)?;
    let current_at = archives
        .last()
        .map_or(started_at, |archive| Some(archive.sealed_at));
    out.raw_set(
        1,
        window_table(lua, CURRENT_WINDOW_ID, current_at, CURRENT_WINDOW_ID)?,
    )?;
    let mut began = started_at;
    for (index, archive) in archives.iter().enumerate() {
        out.raw_set(
            archives.len() - index + 1,
            window_table(
                lua,
                &archive.seq.to_string(),
                began,
                archive.sealed_by.as_str(),
            )?,
        )?;
        began = Some(archive.sealed_at);
    }
    out.set_metatable(Some(lua.array_metatable()))?;
    Ok(out)
}

fn window_table(lua: &Lua, id: &str, created_at: Option<u64>, kind: &str) -> LuaResult<Table> {
    let table = lua.create_table_with_capacity(0, 3)?;
    table.set(ID_KEY, id)?;
    table.set(CREATED_AT_KEY, created_at)?;
    table.set(KIND_KEY, kind)?;
    Ok(table)
}

fn tool_use_ids(messages: &[Message]) -> impl Iterator<Item = &str> {
    messages
        .iter()
        .flat_map(|message| message.tool_uses().map(|(id, _, _)| id))
}

/// The subagent ids whose transcript attaches to the asked window. A
/// transcript belongs to the window holding its parent `tool_use`; one
/// without a parent, like a plugin run beside the chat, goes to the newest
/// loaded window that holds its records, since it keeps running across
/// rewrites. Both are decided by the asked window and its two neighbors: a
/// rewrite carries every record forward.
fn subagent_attach(windows: &[WindowData], asked: usize) -> Vec<String> {
    let mut parent_at: HashMap<&str, usize> = HashMap::new();
    let mut records_at: HashMap<&str, usize> = HashMap::new();
    for (index, window) in windows.iter().enumerate() {
        for id in tool_use_ids(&window.messages) {
            parent_at.insert(id, index);
        }
        if let Some(session) = &window.session {
            for id in session.subagent_messages().keys() {
                records_at.insert(id.as_str(), index);
            }
        }
    }
    records_at
        .into_iter()
        .filter(|(id, records_at)| parent_at.get(id).copied().unwrap_or(*records_at) == asked)
        .map(|(id, _)| id.to_owned())
        .collect()
}

async fn subagents_table(
    lua: &Lua,
    window: Option<&StoredSession>,
    attach: &[String],
) -> LuaResult<Table> {
    let out = lua.create_table_with_capacity(attach.len(), 0)?;
    let attached: HashSet<&str> = attach.iter().map(String::as_str).collect();
    let meta = window.map(StoredSession::subagents).unwrap_or_default();
    let mut index = 1;
    for (position, subagent) in meta.iter().enumerate() {
        if !attached.contains(subagent.tool_use_id.as_str()) {
            continue;
        }
        let Some(messages) = window.and_then(|s| s.subagent_messages().get(&subagent.tool_use_id))
        else {
            continue;
        };
        out.raw_set(
            index,
            subagent_table(
                lua,
                &subagent.tool_use_id,
                position + 1,
                &subagent.name,
                messages,
            )
            .await?,
        )?;
        index += 1;
    }
    let mut unlisted: Vec<&str> = attach
        .iter()
        .map(String::as_str)
        .filter(|id| !meta.iter().any(|subagent| subagent.tool_use_id == *id))
        .collect();
    unlisted.sort_unstable();
    for (position, id) in unlisted.into_iter().enumerate() {
        let Some(messages) = window.and_then(|s| s.subagent_messages().get(id)) else {
            continue;
        };
        out.raw_set(
            index,
            subagent_table(
                lua,
                id,
                meta.len() + position + 1,
                FALLBACK_SUBAGENT_NAME,
                messages,
            )
            .await?,
        )?;
        index += 1;
    }
    out.set_metatable(Some(lua.array_metatable()))?;
    Ok(out)
}

async fn subagent_table(
    lua: &Lua,
    id: &str,
    nr: usize,
    name: &str,
    messages: &[Message],
) -> LuaResult<Table> {
    let table = lua.create_table_with_capacity(0, 4)?;
    table.set(ID_KEY, id)?;
    table.set(NR_KEY, nr)?;
    table.set(NAME_KEY, name)?;
    table.set(MESSAGES_KEY, messages_table(lua, messages).await?)?;
    Ok(table)
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
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
            yield_now().await;
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
///   `messages` (table) what `export` or `messages` return. A user turn
///     that starts with `/` or `!`, or is `exit`, needs the `agent`
///     permission too, since a rewind puts it back in the input for Enter
///     to run.
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    #[ctx] grants: PluginPermissions,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let opts = match opts {
        Some(opts) => opts,
        None => lua.create_table()?,
    };
    let prompt: Option<String> = opts.opt(PROMPT_KEY)?;
    let focus = opts.opt_bool(FOCUS_KEY).unwrap_or(false);
    if prompt.is_some() {
        require(&lua, &grants, Permission::Agent)?;
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
    let parsed = spawn_blocking(move || serde_json::from_value::<Transcript>(doc))
        .await
        .map_err(mlua::Error::external)?;
    let transcript = try_pair!(parsed.map_err(|e| e.to_string()));
    if transcript.messages.iter().any(runs_on_enter) {
        require(&lua, &grants, Permission::Agent)?;
    }
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
        spawn_blocking(move || {
            let session = transcript.into_session(base).map_err(|e| e.to_string())?;
            store_new(session, &dir).map_err(|e| e.to_string())
        })
        .await
        .map_err(mlua::Error::external)?
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.opt("session")?,
        None => None,
    };
    roundtrip(lua, tx, SessionRequest::Prompt { id, text }).await
}

/// Reports {text} to a live session or one of its subagents without creating
/// a user turn. The observation waits for the target's next agent run.
///
/// @param text string What to report. Must not be blank.
/// @param opts table Options:
///   `session` (string) id of a live session.
///   `task` (string?) id of a live subagent of `session`, as `TurnComplete`
///     reports it; the report goes to that subagent instead of the session's
///     own mailbox.
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
    lua: &Lua,
    #[ctx] grants: PluginPermissions,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<bool>> {
    let wake = opts
        .as_ref()
        .and_then(|t| t.opt_bool("wake"))
        .unwrap_or(false);
    if wake {
        require(lua, &grants, Permission::Agent)?;
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
    let task = opts
        .as_ref()
        .map(|t| t.opt::<String>("task"))
        .transpose()?
        .flatten();
    let notified = match task {
        Some(task) => {
            SessionMailbox::notify_task(session_id, &task, text, display.as_deref(), wake)
        }
        None => SessionMailbox::notify(session_id, text, display.as_deref(), wake),
    };
    match notified {
        Ok(()) => Ok((Some(true), None)),
        Err(error) => Ok(err_pair(error)),
    }
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
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
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
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
    /// `current`, `read`, `export`, `messages`, `windows`, `subagents`,
    /// `notify` and `new` without `prompt` or `focus` still work. ACP has no
    /// focused session, so there `current` fails and the read functions need
    /// `session`. Hook and event payloads carry it as `session_id`.
    "maki.session" => pub(crate) fn create_session_table(
        tx: Option<UnboundedSender<UiAction>>,
        perms: &PluginPermissions,
        grants: PluginPermissions,
        plugin: Arc<str>,
    ),
    DOCS [
        list(tx), live(tx), current(tx), read(tx), export(perms, tx), messages(perms, tx),
        windows(perms, tx), subagents(perms, tx), focus(tx), delete(perms, tx),
        new(perms, tx, grants), prompt(perms, tx), notify(perms, grants), set_mode(perms, tx),
        set_title(perms, tx), data_dir(plugin, tx),
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
    use maki_storage::sessions::{SessionClaim, StoredMode, StoredSubagent, StoredThinking};
    use tempfile::TempDir;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::api::base64::create_base64_table;
    use crate::api::image::create_image_table;
    use crate::api::json::create_json_table;
    use crate::api::top::add_top_methods;
    use crate::api::util::fake_picker::{DOWN, ESC, PAGE_DOWN, UP, load_picker};
    use crate::plugin_permissions::denied_error;
    use crate::session_snapshot::{MODE_PLAN, STATUS_IDLE, STATUS_STORED};
    use mlua::Value;
    use serde_json::json;
    use test_case::test_case;

    const FOCUSED_TEXT: &str = "focused";
    const ENTER: &str = "<CR>";
    const CTRL_D: &str = "<C-d>";
    const CTRL_N: &str = "<C-n>";
    const NEWEST_SESSION: &str = "session-newest";
    const MIDDLE_SESSION: &str = "session-middle";
    const OLDEST_SESSION: &str = "session-oldest";
    const RECENCY_ORDER: [&str; 3] = [NEWEST_SESSION, MIDDLE_SESSION, OLDEST_SESSION];
    const SESSION_FOCUS: &str = "focus";
    const SESSION_NEW: &str = "new";
    const SESSION_DELETE: &str = "delete";
    const TOOL_ID: &str = "toolu_1";
    const TOOL_ID_2: &str = "toolu_2";
    const PLUGIN_ID: &str = "plugin-1";
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

    fn lua_with_session(tx: Option<UnboundedSender<UiAction>>) -> Lua {
        lua_granted(tx, PluginPermissions::trusted())
    }

    fn lua_granted(tx: Option<UnboundedSender<UiAction>>, grants: PluginPermissions) -> Lua {
        let lua = Lua::new();
        let t = create_session_table(&lua, tx, &grants, grants.clone(), PLUGIN.into()).unwrap();
        lua.globals().set("session", t).unwrap();
        lua
    }

    #[test_case(headless_focused_on ; "headless_snapshot_id")]
    #[test_case(ui_focused_on ; "ui_current_bare_id")]
    #[tokio::test]
    async fn data_dir_names_the_plugin_dir_under_the_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let tmp = TempDir::new().unwrap();
        let lua = focused_on(id);
        lua.set_app_data(StateDir::from_path(tmp.path().to_path_buf()));
        let (got, err): (Option<String>, Option<String>) = lua
            .load("return session.data_dir()")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        let want = session_data_dir(tmp.path(), id).join(PLUGIN);
        assert_eq!(got, Some(want.display().to_string()));
        assert!(!want.exists(), "asking for the dir must not create it");
    }

    #[tokio::test]
    async fn data_dir_prefers_the_named_then_the_lineage_session() {
        use crate::runtime::{TaskCell, TaskHandle};
        use maki_agent::cancel::CancelToken;
        use std::sync::Mutex;

        let (serving, focused) = (MakiId::generate(), MakiId::generate());
        let tmp = TempDir::new().unwrap();
        let lua = headless_focused_on(focused);
        lua.set_app_data(StateDir::from_path(tmp.path().to_path_buf()));
        let mut cell = TaskCell::new(CancelToken::none(), None, None);
        cell.lineage.session = Some(serving);
        lua.set_app_data::<TaskHandle>(Arc::new(Mutex::new(cell)));

        let (got, err): (Option<String>, Option<String>) = lua
            .load("return session.data_dir()")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        let want = session_data_dir(tmp.path(), serving).join(PLUGIN);
        assert_eq!(got, Some(want.display().to_string()));

        let (got, err): (Option<String>, Option<String>) = lua
            .load(format!(
                "return session.data_dir({{ session = '{focused}' }})"
            ))
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        let want = session_data_dir(tmp.path(), focused).join(PLUGIN);
        assert_eq!(got, Some(want.display().to_string()));
    }

    #[tokio::test]
    async fn live_without_ui_returns_error_pair() {
        let lua = lua_with_session(None);
        let (val, err): (Value, Option<String>) = lua
            .load("return session.live()")
            .eval_async()
            .await
            .unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[tokio::test]
    async fn focus_roundtrips_through_ui_channel() {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_session(Some(tx));
        std::thread::spawn(move || {
            let Some(UiAction::Session {
                req: SessionRequest::Focus { id },
                reply_tx,
            }) = rx.blocking_recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) = lua
            .load("return session.focus('abc')")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), "abc");
    }

    #[test_case("return session.prompt('hi', { session = 'abc' })", Some("abc") ; "explicit_session_id")]
    #[test_case("return session.prompt('hi')", None ; "defaults_to_focused")]
    #[tokio::test]
    async fn prompt_forwards_text_and_session_id(code: &str, expected_id: Option<&str>) {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected_id = expected_id.map(str::to_owned);
        let checker = std::thread::spawn(move || {
            let Some(UiAction::Session {
                req: SessionRequest::Prompt { id, text },
                reply_tx,
            }) = rx.blocking_recv()
            else {
                panic!("expected prompt request");
            };
            assert_eq!(id, expected_id);
            assert_eq!(text, "hi");
            reply_tx.send(Ok(json!("queued"))).unwrap();
        });
        let (val, err): (String, Option<String>) = lua.load(code).eval_async().await.unwrap();
        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(val, "queued");
    }

    /// Mode is per session like the plan it drives, so a row handler firing
    /// for a background tab has to be able to name it.
    #[test_case(r#"return session.set_mode('build', { session = 'abc' })"#, Some("abc"), "build" ; "explicit_session_id")]
    #[test_case("return session.set_mode('plan')", None, "plan" ; "defaults_to_focused")]
    #[tokio::test]
    async fn set_mode_forwards_the_mode_and_session_id(
        code: &str,
        expected_id: Option<&str>,
        expected_mode: &str,
    ) {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected_id = expected_id.map(str::to_owned);
        let expected_mode = expected_mode.to_owned();
        let checker = std::thread::spawn(move || {
            let Some(UiAction::Session {
                req: SessionRequest::SetMode { id, mode },
                reply_tx,
            }) = rx.blocking_recv()
            else {
                panic!("expected set_mode request");
            };
            assert_eq!(id, expected_id);
            assert_eq!(mode, expected_mode);
            reply_tx.send(Ok(json!(true))).unwrap();
        });
        let (val, err): (bool, Option<String>) = lua.load(code).eval_async().await.unwrap();
        checker.join().unwrap();
        assert_eq!(err, None);
        assert!(val);
    }

    #[test]
    fn notify_is_synchronous_and_queues_an_observation() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (tx, mut rx) = unbounded_channel::<UiAction>();
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

    #[test]
    fn notify_with_a_task_reaches_that_subagent() {
        let id = MakiId::generate();
        let task_id = MakiId::generate().to_string();
        let subagent = SessionMailbox::register(MakiId::generate());
        subagent.track_task(&task_id, id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();
        lua.globals().set("task_id", task_id.clone()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('built', { session = session_id, task = task_id })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = subagent.drain();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].first_text_content(), Some("built"));
        subagent.untrack_task(&task_id);
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

    #[tokio::test]
    async fn messages_reads_a_live_transcript() {
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
        let (msgs, err): (Table, Option<String>) = lua.load(&code).eval_async().await.unwrap();
        assert_eq!(err, None);
        assert_eq!(msgs.raw_len(), 1);
        let last: Table = msgs.get(1).unwrap();
        assert_eq!(last.get::<String>("role").unwrap(), "assistant");
        let block: Table = last.get::<Table>("content").unwrap().get(1).unwrap();
        assert_eq!(block.get::<String>("text").unwrap(), ANSWER);
    }

    #[test_case("messages" ; "messages")]
    #[test_case("export" ; "export")]
    #[tokio::test]
    async fn reading_a_session_that_does_not_exist_is_an_error_pair(call: &str) {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let code = format!("return session.{call}({{ session = '{id}' }})");
        let (value, err): (Value, Option<String>) = lua.load(&code).eval_async().await.unwrap();
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
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        std::thread::spawn(move || {
            let Some(UiAction::Session {
                req: SessionRequest::Current,
                reply_tx,
            }) = rx.blocking_recv()
            else {
                panic!("expected current request");
            };
            reply_tx.send(Ok(json!(id.to_string()))).unwrap();
        });
        lua_with_session(Some(tx))
    }

    #[test_case(headless_focused_on ; "headless_snapshot_id")]
    #[test_case(ui_focused_on ; "ui_current_bare_id")]
    #[tokio::test]
    async fn current_names_the_focused_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let lua = focused_on(id);
        let (got, err): (Option<String>, Option<String>) = lua
            .load("return session.current()")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(got, Some(id.to_string()));
    }

    #[test_case(headless_focused_on ; "headless_snapshot_id")]
    #[test_case(ui_focused_on ; "ui_current_bare_id")]
    #[tokio::test]
    async fn messages_defaults_to_the_focused_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let _history = live(id, vec![Message::user(FOCUSED_TEXT.into())]);
        let lua = focused_on(id);

        let (msgs, err): (Table, Option<String>) = lua
            .load("return session.messages()")
            .eval_async()
            .await
            .unwrap();
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

    #[tokio::test]
    async fn messages_without_a_focused_session_is_an_error_pair() {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_session(Some(tx));
        std::thread::spawn(move || {
            let Some(UiAction::Session { reply_tx, .. }) = rx.blocking_recv() else {
                panic!("expected session request");
            };
            reply_tx.send(Ok(serde_json::Value::Null)).unwrap();
        });
        let (_, err): (Value, Option<String>) = lua
            .load("return session.messages()")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err.as_deref(), Some(NO_FOCUSED_ERR));
    }

    /// `session` plus the `maki` pieces a plugin round trips with, and
    /// `one_message`, the smallest history `new` stores.
    fn plugin_lua(tx: Option<UnboundedSender<UiAction>>) -> Lua {
        with_plugin_globals(lua_with_session(tx))
    }

    fn with_plugin_globals(lua: Lua) -> Lua {
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
        headless_granted(PluginPermissions::trusted())
    }

    fn headless_granted(grants: PluginPermissions) -> (Lua, TempDir) {
        let dir = TempDir::new().unwrap();
        let lua = with_plugin_globals(lua_granted(None, grants));
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
                    reasoning_id: None,
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

    /// Saves a session that already has a file, the way a run that rewrites
    /// its log does.
    fn save_again(lua: &Lua, mut session: StoredSession) {
        let dir = storage(lua);
        let claim = SessionClaim::acquire(session.id, &dir).unwrap();
        session.save(&claim, &dir).unwrap();
    }

    /// Two archives: a compaction of the first window, then a rewind of the
    /// turn after it.
    fn stored_with_windows(lua: &Lua) -> MakiId {
        let dir = storage(lua);
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![
            Message::user("old question".into()),
            assistant(vec![ContentBlock::Text {
                text: "old answer".into(),
            }]),
        ]);
        let id = store_new(session, &dir).unwrap();

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("summary".into())]);
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.push_message(Message::user("next turn".into()));
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.truncate_messages(1);
        save_again(lua, session);
        id
    }

    fn subagent_meta(id: &str, name: &str) -> StoredSubagent {
        StoredSubagent {
            tool_use_id: id.into(),
            name: name.into(),
            model: None,
            thinking: None,
            fast: false,
            plugin: None,
            cost: None,
            started_at: None,
            ended_at: None,
        }
    }

    fn with_subagent(mut session: StoredSession, id: &str, name: &str) -> StoredSession {
        session.set_subagent_messages(
            id.into(),
            vec![Message::user(format!("{name} task"))],
            |_| Vec::new(),
        );
        session.set_subagents(vec![subagent_meta(id, name)]);
        session
    }

    /// What a compaction or rewind leaves of the state whose turns it dropped.
    fn prune(session: &mut StoredSession) {
        session.prune_orphans(|message| {
            message
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        });
    }

    fn with_plugin_subagent(mut session: StoredSession, said: &str) -> StoredSession {
        let mut messages = session
            .subagent_messages()
            .get(PLUGIN_ID)
            .map(|messages| messages.as_ref().clone())
            .unwrap_or_default();
        messages.push(Message::user(said.into()));
        session.set_subagent_messages(PLUGIN_ID.into(), messages, |_| Vec::new());
        session.set_subagents(vec![StoredSubagent {
            plugin: Some(PLUGIN.into()),
            ..subagent_meta(PLUGIN_ID, "beside")
        }]);
        session
    }

    fn stored_with_subagent(lua: &Lua) -> MakiId {
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![assistant(vec![ContentBlock::tool_use(
            TOOL_ID,
            TOOL_NAME,
            json!({}),
        )])]);
        store_new(with_subagent(session, TOOL_ID, "worker"), &storage(lua)).unwrap()
    }

    /// The subagent's parent `tool_use` is in the first window, which a
    /// compaction then archives.
    fn stored_with_archived_subagent(lua: &Lua) -> MakiId {
        let dir = storage(lua);
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![
            Message::user("start".into()),
            assistant(vec![ContentBlock::tool_use(TOOL_ID, TOOL_NAME, json!({}))]),
        ]);
        let id = store_new(with_subagent(session, TOOL_ID, "worker"), &dir).unwrap();

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("summary".into())]);
        prune(&mut session);
        save_again(lua, session);
        id
    }

    /// Three archives, with the subagent spawned in the middle window.
    fn stored_with_middle_subagent(lua: &Lua) -> MakiId {
        let dir = storage(lua);
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![
            Message::user("start".into()),
            assistant(vec![ContentBlock::Text {
                text: "answer".into(),
            }]),
        ]);
        let id = store_new(session, &dir).unwrap();

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("summary".into())]);
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.push_message(Message::user("second".into()));
        session.push_message(assistant(vec![ContentBlock::tool_use(
            TOOL_ID,
            TOOL_NAME,
            json!({}),
        )]));
        save_again(lua, with_subagent(session, TOOL_ID, "worker"));

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("summary again".into())]);
        prune(&mut session);
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.push_message(Message::user("third".into()));
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("last summary".into())]);
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.push_message(Message::user("fourth".into()));
        save_again(lua, session);
        id
    }

    /// A rewind keeps the subagent's parent `tool_use` in the next window.
    fn stored_with_carried_parent(lua: &Lua) -> MakiId {
        let dir = storage(lua);
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![
            Message::user("start".into()),
            assistant(vec![ContentBlock::tool_use(TOOL_ID, TOOL_NAME, json!({}))]),
            Message::user("next".into()),
        ]);
        let id = store_new(with_subagent(session, TOOL_ID, "worker"), &dir).unwrap();

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.truncate_messages(2);
        save_again(lua, session);
        id
    }

    /// Two windows whose subagents both take `nr = 1`.
    fn stored_with_two_subagent_windows(lua: &Lua) -> MakiId {
        let dir = storage(lua);
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![
            Message::user("start".into()),
            assistant(vec![ContentBlock::tool_use(TOOL_ID, TOOL_NAME, json!({}))]),
        ]);
        let id = store_new(with_subagent(session, TOOL_ID, "worker"), &dir).unwrap();

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("summary".into())]);
        prune(&mut session);
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.push_message(Message::user("second".into()));
        session.push_message(assistant(vec![ContentBlock::tool_use(
            TOOL_ID_2,
            TOOL_NAME,
            json!({}),
        )]));
        save_again(lua, with_subagent(session, TOOL_ID_2, "helper"));
        id
    }

    /// A plugin run beside the chat has no parent `tool_use` and keeps its
    /// records across a compaction.
    fn stored_with_running_plugin_subagent(lua: &Lua) -> MakiId {
        let dir = storage(lua);
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(vec![
            Message::user("start".into()),
            assistant(vec![ContentBlock::Text {
                text: "answer".into(),
            }]),
        ]);
        let id = store_new(with_plugin_subagent(session, "started"), &dir).unwrap();

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.replace_messages(vec![Message::user("summary".into())]);
        save_again(lua, session);

        let mut session = StoredSession::load(id, &dir).unwrap();
        session.push_message(Message::user("next".into()));
        save_again(lua, with_plugin_subagent(session, "finished"));
        id
    }

    async fn subagents_of(lua: &Lua, id: MakiId, window: Option<&str>) -> Vec<(String, usize)> {
        let window = window.map_or_else(String::new, |window| format!(", window = '{window}'"));
        let table: Table = eval(
            lua,
            &format!("return session.subagents({{ session = '{id}'{window} }})"),
        )
        .await;
        (1..=table.raw_len())
            .map(|index| {
                let row: Table = table.get(index).unwrap();
                (row.get("id").unwrap(), row.get("nr").unwrap())
            })
            .collect()
    }

    async fn eval<T: mlua::FromLuaMulti>(lua: &Lua, code: &str) -> T {
        lua.load(code).eval_async().await.unwrap()
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

    #[tokio::test]
    async fn messages_are_the_serde_shape_with_nulls_kept() {
        let id = MakiId::generate();
        let history = rich_history();
        let _live = live(id, history.clone());
        let (lua, _dir) = headless();

        let msgs: Table = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}' }})"),
        )
        .await;

        assert_eq!(
            json_of(&lua, Value::Table(msgs)),
            serde_json::to_value(&history).unwrap()
        );
    }

    #[tokio::test]
    async fn an_image_source_is_a_handle_sharing_the_payload() {
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
        )
        .await;

        let shared = handle.borrow::<LuaImage>().unwrap().to_source();
        assert!(Arc::ptr_eq(&shared.data, &payload));
    }

    /// The document goes out and comes back as one table, through
    /// `maki.deepcopy` like a redactor would copy it, and only what `new`
    /// drops on purpose is gone.
    #[tokio::test]
    async fn export_new_export_round_trips_minus_thinking() {
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
        )
        .await;

        let after = json_of(&lua, after);
        assert_eq!(after, without_thinking(json_of(&lua, before)));
        assert_eq!(after["facts"]["cwd"], FACTS_CWD);
    }

    #[tokio::test]
    async fn messages_reads_the_tail_of_a_stored_transcript() {
        let (lua, _dir) = headless();
        let history = rich_history();
        let id = stored(&lua, history.clone());

        let msgs: Value = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}', last = 2 }})"),
        )
        .await;

        assert_eq!(
            json_of(&lua, msgs),
            serde_json::to_value(&history[history.len() - 2..]).unwrap()
        );
    }

    #[tokio::test]
    async fn windows_lists_current_then_archives_newest_first() {
        let (lua, _dir) = headless();
        let id = stored_with_windows(&lua);

        let windows: Table = eval(
            &lua,
            &format!("return session.windows({{ session = '{id}' }})"),
        )
        .await;
        let rows: Vec<(String, String, u64)> = (1..=windows.raw_len())
            .map(|index| {
                let row: Table = windows.get(index).unwrap();
                (
                    row.get("id").unwrap(),
                    row.get("kind").unwrap(),
                    row.get("created_at").unwrap(),
                )
            })
            .collect();

        let labels: Vec<(&str, &str)> = rows
            .iter()
            .map(|(id, kind, _)| (id.as_str(), kind.as_str()))
            .collect();
        assert_eq!(
            labels,
            [("current", "current"), ("2", "rewind"), ("1", "compaction"),]
        );
        assert!(rows[2].2 <= rows[1].2 && rows[1].2 <= rows[0].2);
    }

    #[tokio::test]
    async fn windows_leave_created_at_nil_without_a_header() {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let _live = live_unsaved(id, vec![Message::user(TEXT.into())]);
        lua.globals().set("id", id.to_string()).unwrap();

        let windows: Table = eval(&lua, "return assert(session.windows({ session = id }))").await;

        assert_eq!(windows.raw_len(), 1);
        let window: Table = windows.get(1).unwrap();
        assert_eq!(window.get::<String>("id").unwrap(), CURRENT_WINDOW_ID);
        assert!(window.get::<Option<u64>>(CREATED_AT_KEY).unwrap().is_none());
    }

    #[tokio::test]
    async fn messages_reads_one_archived_window() {
        let (lua, _dir) = headless();
        let id = stored_with_windows(&lua);

        let archived: Value = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}', window = '1' }})"),
        )
        .await;
        assert_eq!(
            json_of(&lua, archived),
            serde_json::to_value([
                Message::user("old question".into()),
                assistant(vec![ContentBlock::Text {
                    text: "old answer".into(),
                }]),
            ])
            .unwrap()
        );

        let current: Value = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}' }})"),
        )
        .await;
        assert_eq!(
            json_of(&lua, current),
            serde_json::to_value([Message::user("summary".into())]).unwrap()
        );
    }

    #[tokio::test]
    async fn messages_rejects_an_unknown_window() {
        let (lua, _dir) = headless();
        let id = stored(&lua, vec![Message::user(TEXT.into())]);

        let (messages, error): (Value, Option<String>) = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}', window = '9' }})"),
        )
        .await;

        assert!(messages.is_nil());
        assert_eq!(error.as_deref(), Some("window not found: 9"));
    }

    #[tokio::test]
    async fn subagents_reads_the_transcripts_of_one_window() {
        let (lua, _dir) = headless();
        let id = stored_with_subagent(&lua);

        let subagents: Table = eval(
            &lua,
            &format!("return session.subagents({{ session = '{id}' }})"),
        )
        .await;

        assert_eq!(subagents.raw_len(), 1);
        let subagent: Table = subagents.get(1).unwrap();
        assert_eq!(subagent.get::<String>("id").unwrap(), TOOL_ID);
        assert_eq!(subagent.get::<usize>("nr").unwrap(), 1);
        assert_eq!(subagent.get::<String>("name").unwrap(), "worker");
        let messages: Value = subagent.get(MESSAGES_KEY).unwrap();
        assert_eq!(
            json_of(&lua, messages),
            serde_json::to_value([Message::user("worker task".into())]).unwrap()
        );
    }

    #[tokio::test]
    async fn subagents_attach_to_the_window_that_spawned_them() {
        let (lua, _dir) = headless();
        let id = stored_with_archived_subagent(&lua);

        assert_eq!(
            subagents_of(&lua, id, Some("1")).await,
            [(TOOL_ID.to_owned(), 1)]
        );
        assert!(subagents_of(&lua, id, None).await.is_empty());
    }

    #[tokio::test]
    async fn subagents_of_a_middle_window_come_from_its_neighbors() {
        let (lua, _dir) = headless();
        let id = stored_with_middle_subagent(&lua);

        assert_eq!(
            subagents_of(&lua, id, Some("2")).await,
            [(TOOL_ID.to_owned(), 1)]
        );
        assert!(subagents_of(&lua, id, Some("1")).await.is_empty());
        assert!(subagents_of(&lua, id, Some("3")).await.is_empty());
        assert!(subagents_of(&lua, id, None).await.is_empty());
    }

    #[tokio::test]
    async fn subagents_follow_a_parent_carried_into_the_next_window() {
        let (lua, _dir) = headless();
        let id = stored_with_carried_parent(&lua);

        assert!(subagents_of(&lua, id, Some("1")).await.is_empty());
        assert_eq!(
            subagents_of(&lua, id, None).await,
            [(TOOL_ID.to_owned(), 1)]
        );
    }

    #[tokio::test]
    async fn subagents_in_different_windows_can_share_a_number() {
        let (lua, _dir) = headless();
        let id = stored_with_two_subagent_windows(&lua);

        assert_eq!(
            subagents_of(&lua, id, Some("1")).await,
            [(TOOL_ID.to_owned(), 1)]
        );
        assert_eq!(
            subagents_of(&lua, id, None).await,
            [(TOOL_ID_2.to_owned(), 1)]
        );
    }

    #[tokio::test]
    async fn a_plugin_subagent_attaches_to_the_newest_window() {
        let (lua, _dir) = headless();
        let id = stored_with_running_plugin_subagent(&lua);

        assert!(subagents_of(&lua, id, Some("1")).await.is_empty());
        assert_eq!(
            subagents_of(&lua, id, None).await,
            [(PLUGIN_ID.to_owned(), 1)]
        );

        let subagents: Table = eval(
            &lua,
            &format!("return session.subagents({{ session = '{id}' }})"),
        )
        .await;
        let subagent: Table = subagents.get(1).unwrap();
        let messages: Value = subagent.get(MESSAGES_KEY).unwrap();
        assert_eq!(
            json_of(&lua, messages),
            serde_json::to_value([
                Message::user("started".into()),
                Message::user("finished".into()),
            ])
            .unwrap()
        );
    }

    #[tokio::test]
    async fn read_of_a_stored_session_reports_its_settings() {
        let (lua, _dir) = headless();
        let id = stored(&lua, vec![Message::user(TEXT.into())]);

        let snapshot: Table = eval(
            &lua,
            &format!("return session.read({{ session = '{id}' }})"),
        )
        .await;

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
    #[tokio::test]
    async fn new_refuses(opts: &str, expected: String) {
        let (lua, _dir) = headless();
        let (id, err): (Value, Option<String>) =
            eval(&lua, &format!("return session.new({opts})")).await;
        assert!(id.is_nil());
        assert_eq!(err, Some(expected));
        assert!(StoredSession::list_all(&storage(&lua)).unwrap().is_empty());
    }

    /// Rewind brings a turn that runs back as written by a plugin that
    /// could send prompts, so importing one takes `agent` as well.
    #[test_case("hi" => true ; "a_plain_turn")]
    #[test_case("/compact" => false ; "a_command")]
    #[test_case("!ls" => false ; "a_shell_line")]
    #[test_case(" exit " => false ; "the_exit_word")]
    #[tokio::test]
    async fn new_with_session_write_alone_imports(text: &str) -> bool {
        let (lua, _dir) = headless_granted(PluginPermissions::from_approved(["session_write"]));
        let code = format!(
            "return session.new({{ messages = {{ {{ role = 'user', content = {{ {{ type = 'text', text = '{text}' }} }} }} }} }})"
        );
        match lua.load(code).eval_async::<(Value, Option<String>)>().await {
            Ok((id, err)) => {
                assert_eq!(err, None);
                !id.is_nil()
            }
            Err(e) => {
                let expected = denied_error(Permission::Agent).to_string();
                assert!(e.to_string().contains(&expected), "{e}");
                assert!(StoredSession::list_all(&storage(&lua)).unwrap().is_empty());
                false
            }
        }
    }

    fn image_history(source: &str) -> String {
        format!(
            "{{ {{ role = 'user', content = {{ {{ type = 'image', source = {source} }} }} }} }}"
        )
    }

    #[test_case("{ type = 'base64', media_type = 'image/png', data = image_data }" ; "base64_table")]
    #[test_case("maki.image.decode(maki.base64.decode(image_data))" ; "handle")]
    #[tokio::test]
    async fn headless_new_stores_an_image_given_either_way(source: &str) {
        let (lua, _dir) = headless();
        lua.globals().set("image_data", tiny_png()).unwrap();
        let code = format!(
            "return assert(session.new({{ messages = {} }}))",
            image_history(source)
        );
        let id: String = eval(&lua, &code).await;

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
    #[tokio::test]
    async fn new_with_a_ui_hands_it_the_parsed_transcript() {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = plugin_lua(Some(tx));
        let id = MakiId::generate();
        let checker = std::thread::spawn(move || {
            let Some(UiAction::Session {
                req:
                    SessionRequest::New {
                        transcript,
                        prompt,
                        focus,
                    },
                reply_tx,
            }) = rx.blocking_recv()
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
        let (got, err): (String, Option<String>) = eval(&lua, &code).await;
        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(got, id.to_string());
    }

    #[tokio::test]
    async fn export_with_a_ui_is_answered_by_it() {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let id = MakiId::generate();
        std::thread::spawn(move || {
            let Some(UiAction::ExportSession {
                id: asked,
                reply_tx,
            }) = rx.blocking_recv()
            else {
                panic!("expected export request");
            };
            assert_eq!(asked, id);
            let mut session = StoredSession::new(MODEL, FACTS_CWD);
            session.set_title(TITLE.into());
            session.replace_messages(vec![Message::user(TEXT.into())]);
            let sent = reply_tx.send(Ok(SessionExport::stored(session)));
            assert!(sent.is_ok());
        });

        let doc: Value = eval(
            &lua,
            &format!("return session.export({{ session = '{id}' }})"),
        )
        .await;

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

    #[tokio::test]
    async fn headless_reads_of_a_live_unsaved_session_need_no_file() {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let history = vec![Message::user(TEXT.into())];
        let _live = live_unsaved(id, history.clone());
        lua.globals().set("id", id.to_string()).unwrap();

        let (doc, snapshot): (Value, Table) = eval(
            &lua,
            "return assert(session.export({ session = id })), assert(session.read({ session = id }))",
        ).await;

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
    #[tokio::test]
    async fn headless_new_lands_in_the_driver_session_cwd() {
        const DRIVER_CWD: &str = "/driver/project";
        let (lua, _dir) = headless();
        let provider: SessionSnapshotFn =
            Box::new(|_| Ok(json!({ "id": MakiId::generate().to_string(), "cwd": DRIVER_CWD })));
        lua.set_app_data(SessionSnapshotSlot(provider));

        let id: String = eval(
            &lua,
            "return assert(session.new({ messages = one_message }))",
        )
        .await;

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        assert_eq!(session.cwd, DRIVER_CWD);
    }

    /// ACP installs no slot. The one session it happens to have open must not
    /// decide where an import lands, or a second open session would move it.
    #[tokio::test]
    async fn headless_new_without_a_driver_session_lands_where_maki_runs() {
        let (lua, _dir) = headless();
        let _live = live_unsaved(MakiId::generate(), Vec::new());

        let id: String = eval(
            &lua,
            "return assert(session.new({ messages = one_message }))",
        )
        .await;

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        let here = std::env::current_dir().unwrap();
        assert_eq!(session.cwd, here.to_string_lossy());
    }

    #[tokio::test]
    async fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> = lua
            .load("return session.set_title('oops')")
            .eval_async()
            .await;
        assert!(result.unwrap_err().to_string().contains("table"));
    }

    /// Rows are newest first, and the fake window is far taller than three
    /// rows, so a page lands on an end.
    #[test_case(&[ENTER], SESSION_FOCUS, Some(NEWEST_SESSION) ; "enter_opens_the_first_row")]
    #[test_case(&[DOWN, ENTER], SESSION_FOCUS, Some(MIDDLE_SESSION) ; "down_moves_one_row")]
    #[test_case(&[UP, ENTER], SESSION_FOCUS, Some(OLDEST_SESSION) ; "up_from_the_first_row_wraps_to_the_last")]
    #[test_case(&[PAGE_DOWN, ENTER], SESSION_FOCUS, Some(OLDEST_SESSION) ; "page_down_clamps_to_the_last_row")]
    #[test_case(&[DOWN, CTRL_D, CTRL_D, ESC], SESSION_DELETE, Some(MIDDLE_SESSION) ; "ctrl_d_twice_deletes_the_selected_row")]
    #[test_case(&[CTRL_N], SESSION_NEW, None ; "ctrl_n_starts_a_new_session")]
    fn sessions_picker_keys_act_on_the_selected_row(
        keys: &[&str],
        call: &str,
        target: Option<&str>,
    ) {
        let lua = sessions_lua(keys);
        lua.load("open_sessions()").exec().unwrap();

        let globals = lua.globals();
        assert_eq!(globals.get::<String>("called").unwrap(), call);
        assert_eq!(
            globals
                .get::<Option<String>>("called_id")
                .unwrap()
                .as_deref(),
            target
        );
    }

    /// Runs `plugins/sessions/init.lua` against a fake `maki` whose live
    /// sessions are `RECENCY_ORDER`, and presses {keys}. The fake records the
    /// last `maki.session` focus, new or delete as `called` and the session it
    /// named as `called_id`, and raises on any flash.
    fn sessions_lua(keys: &[&str]) -> Lua {
        let lua = Lua::new();
        lua.globals()
            .set(
                "session_ids",
                lua.create_sequence_from(RECENCY_ORDER).unwrap(),
            )
            .unwrap();
        load_picker(
            &lua,
            keys,
            r#"
            local sessions = {}
            for i, id in ipairs(session_ids) do
                sessions[i] = {id = id, title = id, updated_at = #session_ids - i}
            end
            local session = {
                live = function() return sessions end,
                list = function() return {} end,
            }
            for _, name in ipairs({'focus', 'new', 'delete'}) do
                session[name] = function(arg)
                    called, called_id = name, type(arg) == 'string' and arg or nil
                    return true
                end
            end
            maki = {
                session = session,
                async = {run = function(fn) fn() end},
                ui = {
                    buf = function() return {set_lines = function() end} end,
                    open_win = function() return fake_win(picker_keys) end,
                    flash = error,
                },
                api = {
                    create_autocmd = function() end,
                    register_command = function(opts)
                        if opts.name == '/sessions' then open_sessions = opts.handler end
                    end,
                },
                keymap = {set = function() end},
            }
        "#,
            &[],
            include_str!("../../../plugins/sessions/init.lua"),
        );
        lua
    }
}
