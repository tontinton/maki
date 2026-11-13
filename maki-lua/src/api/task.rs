//! `maki.task`. The host keeps the subagent transcripts, so a plugin only needs
//! to list the tasks and show one.

use std::sync::Arc;

use maki_agent::SessionMailbox;
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::RequestOptions;
use maki_storage::id::MakiId;
use mlua::{Either as LuaEither, Lua, LuaSerdeExt, Result as LuaResult, Table, UserDataRef, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::agent::{delivery, plugin_input};
use crate::api::session::newest_messages_table;
use crate::api::util::command::{TaskRequest, UiAction, ui_json_roundtrip};
use crate::api::util::convert::TableExt;
use crate::api::util::ctx::LuaCtx;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::background::BackgroundTasks;
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{CANCEL_ABANDON_AFTER, is_user_action};

const USER_ACTION_ERR: &str = "stopping a task requires a user command or keybind";
const TASK_ID_ERR: &str = "stop(ctx, id): id must be a string";
const STOP_OPTS_ERR: &str = "stop(id, opts): opts must be a table";
const LAST_KEY: &str = "last";
const NAME_KEY: &str = "name";
const NAMES_SEP: &str = ", ";

async fn roundtrip(
    lua: Lua,
    tx: Option<UnboundedSender<UiAction>>,
    req: TaskRequest,
) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Task {
        req,
        reply_tx,
    })
    .await
}

/// Lists the focused session's chats in chat order. Entry 1 is always the main
/// chat, with id `"main"` and no `status`: its work is the session's own, and
/// `maki.session.live()` already reports that. The rest are subagents: one a
/// tool spawned has its tool call's id, one a plugin opened with
/// [maki.agent.session](#maki-agent-session) has its session id and a
/// `plugin` field.
///
/// @return (table|nil, string|nil) Array of `{id, session, name, focused, status?, plugin?,
///   tool?, parent?, part?, wrapper?, started_at?, ended_at?, cost?}`, or nil and an error.
///   `status` is `"working"`, `"idle"` (a plugin task between prompts), `"done"`,
///   `"error"` or `"cancelled"`. `session` is the chat session that `stop()` asks for.
///   `tool` is the tool whose call the task runs, like `"task"` or `"bash"`, and is
///   absent on a session a plugin opened on its own. `parent` is the id of the call
///   the task runs under, like a `batch`, which is listed only once it moved. `part`
///   is true for a piece listed with `ctx:set_parts()`: stopping it stops its whole
///   call, while a subagent the call started stops alone. `wrapper` is true for a
///   call that `/tasks` replaces with the tasks under it, so it gets no row and does
///   not count as running. `started_at` and `ended_at` are Unix seconds: when the
///   task started or moved, and when it finished. Tasks saved before they were
///   recorded lack them. `cost` is what a subagent billed so far, in dollars. The
///   main chat has none of the three.
/// @example
/// for _, t in ipairs(maki.task.list() or {}) do
///   print(t.name, t.status or "main")
/// end
#[lua_fn]
async fn list(lua: Lua, #[ctx] tx: Option<UnboundedSender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, TaskRequest::List).await
}

/// Shows a task's transcript, the way the chat cycling keys do. An id from
/// another session returns an error instead of landing on the wrong task.
///
/// @param id string Task id, as returned by `list()`. `"main"` is the main chat.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.task.focus("main")
#[lua_fn]
async fn focus(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    id: String,
) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, TaskRequest::Focus { id }).await
}

/// Deletes a finished task and its saved transcript. Refuses the main chat and
/// running tasks. Deleting the focused task moves focus to the chat before it.
///
/// @param id string Task id, as returned by `list()`.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.task.remove("toolu_01")
#[lua_fn(guard = SessionWrite)]
async fn remove(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    id: String,
) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, TaskRequest::Remove { id }).await
}

/// Stops a task in the focused session, the same way Esc in its tab does. A
/// plugin task can be stopped while idle, even before its first prompt. The
/// main chat, a finished task, or an unknown id returns an error.
///
/// Only the user may stop a task, so this works in a command the user typed
/// or a key they pressed. Tools, hooks, autocmds, `maki.async` tasks, and
/// commands run through `maki.api.run_command` get an error instead, or any
/// plugin could stop work that is not its own.
///
/// From a tool handler, `stop(ctx, id)` is the model's stop. It stops a
/// background task (a command or subagent) that the calling agent moved,
/// which is allowed because the tool call belongs to a turn the user started.
/// A subagent stopped this way ends its pending `:prompt()` with
/// `"stopped by parent agent"`.
///
/// It waits up to 5 seconds for the final reply and returns it, and the agent
/// gets no notice of the end. A later reply reaches the agent as a quiet
/// notice instead. A subagent nested in a moved call, like a `task` in a
/// `batch`, stops alone and returns at once, because that call's result
/// reports what it did. A task another agent moved, a finished task or an
/// unknown id returns an error that lists the agent's running tasks.
///
/// @param id string|LuaCtx Task id, as returned by `list()`. Or the handler's
///   ctx, for the model's stop.
/// @param opts table|string `session` (string), the chat session of the
///   `list()` row. If the user switched sessions since, this returns an error
///   rather than stopping a task with the same id in the other session. With a
///   ctx: the background task id.
/// @return (boolean|table|nil, string|nil) true on success, or nil and an
///   error. With a ctx, success is `{name, output?, parent?}`: the task's
///   name in `/tasks`, the final reply unless it was late, and the id of the
///   call a nested subagent ran in.
/// @example
/// local _, err = maki.task.stop(task.id, { session = task.session })
/// -- in a tool handler
/// local stopped, err = maki.task.stop(ctx, input.id)
#[lua_fn]
async fn stop(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    id: LuaEither<UserDataRef<LuaCtx>, String>,
    opts: Value,
) -> LuaResult<Pair<Value>> {
    let id = match id {
        LuaEither::Left(ctx) => {
            let Value::String(task_id) = opts else {
                return Err(mlua::Error::runtime(TASK_ID_ERR));
            };
            let task_id = String::from(&*task_id.to_str()?);
            let asker = try_pair!(asker(&ctx, "maki.task.stop"));
            drop(ctx);
            let stopped = try_pair!(
                BackgroundTasks::of(&lua)
                    .stop_by_model(asker, &task_id, CANCEL_ABANDON_AFTER)
                    .await
            );
            return Ok((Some(lua.to_value(&stopped)?), None));
        }
        LuaEither::Right(id) => id,
    };
    if !is_user_action(&lua) {
        return Ok(err_pair(USER_ACTION_ERR));
    }
    let Value::Table(opts) = opts else {
        return Err(mlua::Error::runtime(STOP_OPTS_ERR));
    };
    let session = opts.get::<String>("session")?;
    roundtrip(lua, tx, TaskRequest::Stop { id, session }).await
}

/// Whose tasks `stop`, `send` and `messages` may touch from a tool handler:
/// the mailbox its agent reads. `None` outside a run.
fn asker(ctx: &LuaCtx, method: &str) -> Result<Option<MakiId>, String> {
    let agent = ctx.agent().ok_or_else(|| ctx.cap_err(method))?;
    Ok(agent.mailbox.as_ref().map(SessionMailbox::session_id))
}

/// Messages a background subagent that the calling agent moved, the way the
/// `task_message` tool does. The subagent reads it as an interrupt from its
/// parent once its current turn ends, and one still waiting for a slot reads
/// it with its first prompt.
///
/// With `interrupt` it does not wait for the turn to end, nor for the calls
/// the subagent moved. The reply stops and running tools are cancelled (bash
/// returns what it printed so far), then the subagent reads the message and
/// carries on with its history intact. With no turn running the message
/// waits like any other.
///
/// {id} can also name a subagent nested in a call the agent moved, like a
/// `task` in a `batch`. The id of the call itself messages every subagent
/// still running in it.
///
/// @param ctx LuaCtx The tool handler's ctx.
/// @param id string Task id, from its receipt.
/// @param text string The message.
/// @param opts table? Optional fields:
///   `interrupt` (boolean?) - cut its current turn short. Default: `false`.
/// @return (table|nil, string|nil) `{name}`, the task's name in `/tasks`
///   (comma-separated when it reached several), or nil and
///   `"no running background subagent with id <id>; running: ..."`.
/// @example
/// local sent, err = maki.task.send(ctx, input.id, input.message)
/// if not sent then return { llm_output = err, is_error = true } end
/// return "Message queued for " .. sent.name .. "."
#[lua_fn]
async fn send(
    lua: Lua,
    ctx: UserDataRef<LuaCtx>,
    id: String,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let delivery = delivery(opts.as_ref());
    let asker = try_pair!(asker(&ctx, "maki.task.send"));
    // A sent message never picks the run's settings, so these are inert.
    let input = || plugin_input(text.clone(), RequestOptions::default());
    let names = try_pair!(BackgroundTasks::of(&lua).send(asker, &id, input, delivery));
    let sent = lua.create_table()?;
    sent.set(NAME_KEY, names.join(NAMES_SEP))?;
    Ok((Some(sent), None))
}

/// Reads the transcript of a background subagent that the calling agent
/// moved, oldest first, while it works. The messages have the shape
/// `maki.session.messages` returns. It takes the ids `send(ctx, id)` takes,
/// except the id of a call, since it reads one subagent. Works without an
/// interactive UI.
///
/// @param ctx LuaCtx The tool handler's ctx.
/// @param id string Task id, from its receipt.
/// @param opts table? `last` (integer?) only the newest `last` messages.
/// @return (table|nil, string|nil) Array of messages, or nil and
///   `"no running background subagent with id <id>; running: ..."`.
/// @example
/// local msgs = maki.task.messages(ctx, input.id, { last = 1 })
/// local last = msgs and msgs[1]
/// if last and last.content[1].type == "text" then
///   print(last.content[1].text)
/// end
#[lua_fn(guard = SessionRead)]
async fn messages(
    lua: Lua,
    ctx: UserDataRef<LuaCtx>,
    id: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let last = opts
        .map(|opts| opts.opt::<usize>(LAST_KEY))
        .transpose()?
        .flatten();
    let asker = try_pair!(asker(&ctx, "maki.task.messages"));
    drop(ctx);
    let control = try_pair!(BackgroundTasks::of(&lua).subagent(asker, &id));
    let history = Arc::clone(&control.transcript().load().messages);
    Ok((
        Some(newest_messages_table(&lua, &history, last).await?),
        None,
    ))
}

lua_table! {
    /// The subagents of the focused session and their transcripts. Tasks are
    /// spawned by the `task` tool and addressed by an id that survives a reload.
    /// Without an interactive UI every function returns
    /// `nil, "no interactive UI attached"`, except the ones that take a tool
    /// handler's ctx: they reach the background tasks its agent moved.
    "maki.task" => pub(crate) fn create_task_table(
        tx: Option<UnboundedSender<UiAction>>,
        perms: &PluginPermissions,
    ),
    DOCS [list(tx), focus(tx), remove(perms, tx), stop(tx), send, messages(perms)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, UiReply};
    use crate::api::util::fake_picker::{DOWN, ESC, PAGE_DOWN, UP, load_picker};
    use mlua::Table;
    use serde_json::json;
    use std::thread::JoinHandle;
    use test_case::test_case;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::mpsc::unbounded_channel;

    use crate::runtime::{TaskCell, TaskScope, run_command_scoped};
    use maki_agent::cancel::CancelToken;

    const TASK_ID: &str = "toolu_01";
    const DONE_ID: &str = "toolu_02";
    const ERROR_ID: &str = "toolu_03";
    const MAIN_ID: &str = "main";
    const MAIN_NAME: &str = "chat";
    const SUBAGENT_NAME: &str = "explore repo";
    const WORKING_STATUS: &str = "working";
    const DONE_STATUS: &str = "done";
    const ERROR_STATUS: &str = "error";
    const NIL_STATUS: &str = "nil";
    const CHAT_SESSION: &str = "chat-session";
    const UNKNOWN_TASK_ERR: &str = "unknown task: toolu_01";
    const NO_REQUEST_ERR: &str = "expected a task request";
    const LIST_CALL: &str = "return task.list()";
    const FOCUS_CALL: &str = "return task.focus('toolu_01')";
    const REMOVE_CALL: &str = "return task.remove('toolu_01')";
    const STOP_CALL: &str = "return task.stop('toolu_01', {session='chat-session'})";
    const LIST_SCRIPT: &str = "
        local tasks = task.list()
        local ids, statuses = {}, {}
        for i, t in ipairs(tasks) do
            ids[i] = t.id
            statuses[i] = tostring(t.status)
        end
        return table.concat(ids, ','), table.concat(statuses, ','), tasks[1].name
    ";
    const CTRL_S: &str = "<C-s>";
    /// In `TASK_NAME` but not in the main chat's name.
    const VIM_UP: &str = "k";
    const TASK_NAME: &str = "work";
    const OLDEST_ID: &str = "toolu_04";
    const MIDDLE_ID: &str = "toolu_05";
    const NEWEST_ID: &str = "toolu_06";
    const SPAWN_ORDER: [&str; 3] = [OLDEST_ID, MIDDLE_ID, NEWEST_ID];

    fn lua_with_task(tx: Option<UnboundedSender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_task_table(&lua, tx, &PluginPermissions::trusted()).unwrap();
        lua.globals().set("task", t).unwrap();
        lua
    }

    /// Answers the first request with {reply} and hands it back to assert on.
    /// Join only after dropping the `Lua` that holds the sender, or a missing
    /// request parks the thread forever.
    fn spawn_host(mut rx: UnboundedReceiver<UiAction>, reply: UiReply) -> JoinHandle<TaskRequest> {
        std::thread::spawn(move || {
            let Some(UiAction::Task { req, reply_tx }) = rx.blocking_recv() else {
                panic!("{NO_REQUEST_ERR}");
            };
            reply_tx.send(reply).unwrap();
            req
        })
    }

    #[test_case(LIST_CALL ; "list")]
    #[test_case(FOCUS_CALL ; "focus")]
    #[test_case(REMOVE_CALL ; "remove")]
    #[tokio::test]
    async fn without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_task(None);
        let (val, err): (Value, Option<String>) = lua.load(code).eval_async().await.unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// A stale id should be visible but not fatal: the host's `Err` comes back
    /// as the `(nil, err)` pair, word for word, without raising into the plugin.
    #[test_case(LIST_CALL ; "list")]
    #[test_case(FOCUS_CALL ; "focus")]
    #[test_case(REMOVE_CALL ; "remove")]
    #[tokio::test]
    async fn host_error_reply_surfaces_as_error_pair(code: &str) {
        let (tx, rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Err(UNKNOWN_TASK_ERR.to_owned()));

        let (val, err): (Value, Option<String>) = lua
            .load(code)
            .eval_async()
            .await
            .expect("error reply must not throw");
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(UNKNOWN_TASK_ERR));

        drop(lua);
        host.join().unwrap();
    }

    /// `plugins/task/picker_rows.lua` spots the main chat with `if not task.status`,
    /// so a missing key has to stay missing on the Lua side.
    #[tokio::test]
    async fn list_passes_host_array_through_unchanged() {
        let (tx, rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(
            rx,
            Ok(json!([
                { "id": MAIN_ID, "name": MAIN_NAME, "focused": true },
                { "id": TASK_ID, "name": SUBAGENT_NAME, "status": WORKING_STATUS, "focused": false },
                { "id": DONE_ID, "name": SUBAGENT_NAME, "status": DONE_STATUS, "focused": false },
                { "id": ERROR_ID, "name": SUBAGENT_NAME, "status": ERROR_STATUS, "focused": false },
            ])),
        );

        let (ids, statuses, main_name): (String, String, String) =
            lua.load(LIST_SCRIPT).eval_async().await.unwrap();
        assert_eq!(ids, format!("{MAIN_ID},{TASK_ID},{DONE_ID},{ERROR_ID}"));
        assert_eq!(
            statuses,
            format!("{NIL_STATUS},{WORKING_STATUS},{DONE_STATUS},{ERROR_STATUS}")
        );
        assert_eq!(main_name, MAIN_NAME);

        drop(lua);
        assert!(matches!(host.join().unwrap(), TaskRequest::List));
    }

    #[tokio::test]
    async fn focus_roundtrips_through_ui_channel() {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_task(Some(tx));
        std::thread::spawn(move || {
            let Some(UiAction::Task {
                req: TaskRequest::Focus { id },
                reply_tx,
            }) = rx.blocking_recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) = lua
            .load(format!("return task.focus('{TASK_ID}')"))
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), TASK_ID);
    }

    #[tokio::test]
    async fn remove_roundtrips_through_ui_channel() {
        let (tx, rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Ok(json!(true)));

        let (val, err): (bool, Option<String>) = lua.load(REMOVE_CALL).eval_async().await.unwrap();
        assert!(val);
        assert_eq!(err, None);

        drop(lua);
        assert!(matches!(host.join().unwrap(), TaskRequest::Remove { id } if id == TASK_ID));
    }

    #[tokio::test]
    async fn stop_from_a_user_command_roundtrips_through_ui_channel() {
        let (tx, rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Ok(json!(true)));

        let (val, err): (bool, Option<String>) =
            run_command_scoped(&lua, 0, lua.load(STOP_CALL).eval_async())
                .await
                .unwrap();
        assert!(val);
        assert_eq!(err, None);

        drop(lua);
        assert!(matches!(
            host.join().unwrap(),
            TaskRequest::Stop { id, session } if id == TASK_ID && session == CHAT_SESSION
        ));
    }

    enum Caller {
        Hook,
        CommandAlias,
        ChildOfUserCommand,
    }

    #[test_case(Caller::Hook ; "hook_outside_any_command")]
    #[test_case(Caller::CommandAlias ; "command_run_by_run_command")]
    #[test_case(Caller::ChildOfUserCommand ; "task_nested_in_a_user_command")]
    #[tokio::test]
    async fn stop_without_user_authority_never_reaches_the_host(caller: Caller) {
        let (tx, rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let stop = lua.load(STOP_CALL).eval_async();
        let (val, err): (Value, Option<String>) = async {
            match caller {
                Caller::Hook => stop.await,
                Caller::CommandAlias => run_command_scoped(&lua, 1, stop).await,
                Caller::ChildOfUserCommand => {
                    run_command_scoped(&lua, 0, async {
                        let child =
                            TaskScope::new(&lua, TaskCell::new(CancelToken::none(), None, None));
                        child.scope_future(stop).await
                    })
                    .await
                }
            }
        }
        .await
        .unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(USER_ACTION_ERR));
        assert!(rx.is_empty());
    }

    /// A refusal means the rows on screen are stale, so the picker flashes it
    /// and lists again either way.
    #[test_case(None ; "stopped")]
    #[test_case(Some(UNKNOWN_TASK_ERR) ; "refused")]
    fn tasks_picker_stops_the_selected_row_and_refreshes(stop_error: Option<&str>) {
        let lua = picker_lua(stop_error, &[DOWN, CTRL_S, ESC], &[TASK_ID]);
        lua.load("open_tasks()").exec().unwrap();

        let globals = lua.globals();
        assert_eq!(globals.get::<String>("stopped_id").unwrap(), TASK_ID);
        assert_eq!(
            globals.get::<String>("stopped_session").unwrap(),
            CHAT_SESSION
        );
        assert_eq!(globals.get::<u32>("list_calls").unwrap(), 2);
        assert_eq!(
            globals.get::<Option<String>>("flashed").unwrap().as_deref(),
            stop_error
        );
    }

    /// Rows are main, then the running tasks newest first, and Ctrl+S stops
    /// whatever the keys left selected. `k` would move up with vim keys, but
    /// here the filter takes it. It hides main, so the newest task is left
    /// selected.
    #[test_case(&[DOWN], NEWEST_ID ; "down_from_main_selects_the_next_row")]
    #[test_case(&[UP], OLDEST_ID ; "up_from_the_first_row_wraps_to_the_last")]
    #[test_case(&[PAGE_DOWN], OLDEST_ID ; "page_down_clamps_to_the_last_row")]
    #[test_case(&[VIM_UP], NEWEST_ID ; "a_letter_filters_instead_of_moving")]
    fn tasks_picker_keys_move_the_selection(moves: &[&str], selected: &str) {
        let keys: Vec<&str> = moves.iter().copied().chain([CTRL_S, ESC, ESC]).collect();
        let lua = picker_lua(None, &keys, &SPAWN_ORDER);
        lua.load("open_tasks()").exec().unwrap();

        assert_eq!(lua.globals().get::<String>("stopped_id").unwrap(), selected);
    }

    /// Runs `plugins/task/picker.lua` against a fake `maki` that lists the main
    /// chat and then {task_ids} in spawn order, all working, and presses {keys}.
    fn picker_lua(stop_error: Option<&str>, keys: &[&str], task_ids: &[&str]) -> Lua {
        let lua = Lua::new();
        let globals = lua.globals();
        globals
            .set(
                "task_ids",
                lua.create_sequence_from(task_ids.iter().copied()).unwrap(),
            )
            .unwrap();
        globals.set("task_name", TASK_NAME).unwrap();
        globals.set("chat_session", CHAT_SESSION).unwrap();
        globals.set("stop_error", stop_error).unwrap();
        load_picker(
            &lua,
            keys,
            r#"
            local tasks = {{id = 'main', session = chat_session, name = 'chat', focused = true}}
            for _, id in ipairs(task_ids) do
                tasks[#tasks + 1] = {id = id, session = chat_session, name = task_name, status = 'working'}
            end
            list_calls = 0
            maki = {
                task = {
                    list = function() list_calls = list_calls + 1; return tasks end,
                    focus = function() return true end,
                    stop = function(id, opts)
                        stopped_id, stopped_session = id, opts.session
                        if stop_error then return nil, stop_error end
                        return true
                    end,
                },
                ui = {
                    buf = function() return {set_lines = function() end} end,
                    open_win = function() return fake_win(picker_keys) end,
                    display_width = function(s) return #s end,
                    flash = function(s) flashed = s end,
                },
                api = {
                    create_autocmd = function() end,
                    register_command = function(opts) open_tasks = opts.handler end,
                },
                keymap = {set = function() end},
            }
        "#,
            &[(
                "picker_rows",
                include_str!("../../../plugins/task/picker_rows.lua"),
            )],
            include_str!("../../../plugins/task/picker.lua"),
        );
        lua
    }
}
