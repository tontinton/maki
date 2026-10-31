//! `maki.task`. The host keeps the subagent transcripts, so a plugin only needs
//! to list the tasks and show one.

use maki_agent::SessionMailbox;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Either as LuaEither, Lua, LuaSerdeExt, Result as LuaResult, UserDataRef, Value};

use crate::api::util::command::{TaskRequest, UiAction, ui_json_roundtrip};
use crate::api::util::ctx::LuaCtx;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::background::BackgroundTasks;
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{CANCEL_ABANDON_AFTER, is_user_action};

const USER_ACTION_ERR: &str = "stopping a task requires a user command or keybind";
const TASK_ID_ERR: &str = "stop(ctx, id): id must be a string";
const STOP_OPTS_ERR: &str = "stop(id, opts): opts must be a table";

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
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
///   parent?, part?, started_at?, ended_at?, cost?}` where `status` is `"working"`,
///   `"idle"` (a plugin task between prompts), `"done"`, `"error"`, or
///   `"cancelled"` (stopped by the user or the model), and `session` is the
///   chat session that `stop()` asks for. `parent` is the id of the call the
///   task runs under, like a `batch` that started it. That call is listed
///   only once it moved. `part` is true for a piece listed with
///   `ctx:set_parts()`. Stopping a part stops its whole call, while a
///   subagent the call started stops alone. `started_at` and `ended_at` are
///   Unix seconds: when the task started, or moved to the background, and
///   when it finished. A task loaded from disk has neither. `cost` is what a
///   subagent billed so far, in dollars. The main chat has none of the
///   three. Or nil and an error.
/// @example
/// for _, t in ipairs(maki.task.list() or {}) do
///   print(t.name, t.status or "main")
/// end
#[lua_fn]
async fn list(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
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
    #[ctx] tx: Option<flume::Sender<UiAction>>,
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
    #[ctx] tx: Option<flume::Sender<UiAction>>,
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
/// It waits up to 5 seconds for the final reply and returns it, and the agent
/// gets no notice of the end. A late reply returns no `output`, and the
/// result reaches the agent later as a quiet notice. A task another agent
/// moved, a finished task or an unknown id returns an error that names the
/// agent's running task ids.
///
/// @param id string|LuaCtx Task id, as returned by `list()`. Or the handler's
///   ctx, for the model's stop.
/// @param opts table|string `session` (string), the chat session of the
///   `list()` row. If the user switched sessions since, this returns an error
///   rather than stopping a task with the same id in the other session. With a
///   ctx: the background task id.
/// @return (boolean|table|nil, string|nil) true on success, or nil and an
///   error. With a ctx, success is `{name, output?}`. `name` is the task's
///   name in `/tasks`, and `output` is the final reply, missing when it was
///   late.
/// @example
/// local _, err = maki.task.stop(task.id, { session = task.session })
/// -- in a tool handler
/// local stopped, err = maki.task.stop(ctx, input.id)
#[lua_fn]
async fn stop(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    id: LuaEither<UserDataRef<LuaCtx>, String>,
    opts: Value,
) -> LuaResult<Pair<Value>> {
    let id = match id {
        LuaEither::Left(ctx) => {
            let Value::String(task_id) = opts else {
                return Err(mlua::Error::runtime(TASK_ID_ERR));
            };
            let task_id = String::from(&*task_id.to_str()?);
            let Some(agent) = ctx.agent() else {
                return Ok(ctx.cap_err_pair("maki.task.stop"));
            };
            let asker = agent.mailbox.as_ref().map(SessionMailbox::session_id);
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

lua_table! {
    /// The subagents of the focused session and their transcripts. Tasks are
    /// spawned by the `task` tool and addressed by an id that survives a reload.
    /// Without an interactive UI every function returns
    /// `nil, "no interactive UI attached"`.
    "maki.task" => pub(crate) fn create_task_table(
        tx: Option<flume::Sender<UiAction>>,
        perms: &PluginPermissions,
    ),
    DOCS [list(tx), focus(tx), remove(perms, tx), stop(tx)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, UiReply};
    use mlua::Table;
    use serde_json::json;
    use std::thread::JoinHandle;
    use test_case::test_case;

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
    const JOB_ID: u32 = 7;
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

    fn lua_with_task(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_task_table(&lua, tx, &PluginPermissions::trusted()).unwrap();
        lua.globals().set("task", t).unwrap();
        lua
    }

    /// Answers the first request with {reply} and hands it back to assert on.
    /// Join only after dropping the `Lua` that holds the sender, or a missing
    /// request parks the thread forever.
    fn spawn_host(rx: flume::Receiver<UiAction>, reply: UiReply) -> JoinHandle<TaskRequest> {
        std::thread::spawn(move || {
            let Ok(UiAction::Task { req, reply_tx }) = rx.recv() else {
                panic!("{NO_REQUEST_ERR}");
            };
            reply_tx.send(reply).unwrap();
            req
        })
    }

    #[test_case(LIST_CALL ; "list")]
    #[test_case(FOCUS_CALL ; "focus")]
    #[test_case(REMOVE_CALL ; "remove")]
    fn without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_task(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// A stale id should be visible but not fatal: the host's `Err` comes back
    /// as the `(nil, err)` pair, word for word, without raising into the plugin.
    #[test_case(LIST_CALL ; "list")]
    #[test_case(FOCUS_CALL ; "focus")]
    #[test_case(REMOVE_CALL ; "remove")]
    fn host_error_reply_surfaces_as_error_pair(code: &str) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Err(UNKNOWN_TASK_ERR.to_owned()));

        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).expect("error reply must not throw");
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(UNKNOWN_TASK_ERR));

        drop(lua);
        host.join().unwrap();
    }

    /// `plugins/task/picker_rows.lua` spots the main chat with `if not task.status`,
    /// so a missing key has to stay missing on the Lua side.
    #[test]
    fn list_passes_host_array_through_unchanged() {
        let (tx, rx) = flume::unbounded::<UiAction>();
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
            smol::block_on(lua.load(LIST_SCRIPT).eval_async()).unwrap();
        assert_eq!(ids, format!("{MAIN_ID},{TASK_ID},{DONE_ID},{ERROR_ID}"));
        assert_eq!(
            statuses,
            format!("{NIL_STATUS},{WORKING_STATUS},{DONE_STATUS},{ERROR_STATUS}")
        );
        assert_eq!(main_name, MAIN_NAME);

        drop(lua);
        assert!(matches!(host.join().unwrap(), TaskRequest::List));
    }

    #[test]
    fn focus_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::Task {
                req: TaskRequest::Focus { id },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) = smol::block_on(
            lua.load(format!("return task.focus('{TASK_ID}')"))
                .eval_async(),
        )
        .unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), TASK_ID);
    }

    #[test]
    fn remove_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Ok(json!(true)));

        let (val, err): (bool, Option<String>) =
            smol::block_on(lua.load(REMOVE_CALL).eval_async()).unwrap();
        assert!(val);
        assert_eq!(err, None);

        drop(lua);
        assert!(matches!(host.join().unwrap(), TaskRequest::Remove { id } if id == TASK_ID));
    }

    #[test]
    fn stop_from_a_user_command_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Ok(json!(true)));

        let (val, err): (bool, Option<String>) = smol::block_on(run_command_scoped(
            &lua,
            0,
            lua.load(STOP_CALL).eval_async(),
        ))
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
    fn stop_without_user_authority_never_reaches_the_host(caller: Caller) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let stop = lua.load(STOP_CALL).eval_async();
        let (val, err): (Value, Option<String>) = smol::block_on(async {
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
        })
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
        let lua = picker_lua(stop_error, &["<Down>", "<C-s>", "<Esc>"]);
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

    /// A shared job is listed and stopped by the picker even though another
    /// plugin started it, and stopping a job never touches the task path.
    #[test]
    fn tasks_picker_stops_a_listed_job() {
        let lua = picker_lua(None, &["<Down>", "<Down>", "<C-s>", "<Esc>"]);
        lua.load("open_tasks()").exec().unwrap();

        let globals = lua.globals();
        assert_eq!(globals.get::<u32>("stopped_job").unwrap(), JOB_ID);
        assert!(
            globals
                .get::<Option<String>>("stopped_id")
                .unwrap()
                .is_none()
        );
        assert_eq!(globals.get::<u32>("list_calls").unwrap(), 2);
    }

    /// Runs `plugins/task/picker.lua` against a fake `maki` that lists the main
    /// chat, one task and one shared job, then presses {keys}.
    fn picker_lua(stop_error: Option<&str>, keys: &[&str]) -> Lua {
        let lua = Lua::new();
        let globals = lua.globals();
        globals.set("task_id", TASK_ID).unwrap();
        globals.set("chat_session", CHAT_SESSION).unwrap();
        globals.set("stop_error", stop_error).unwrap();
        globals.set("next_keys", keys).unwrap();
        lua.load(
            r#"
            local modules = {}
            function require(name) return assert(modules[name], name) end
            function install_module(name, source) modules[name] = assert(loadstring(source))() end
            local keys = next_keys
            local tasks = {
                {id = 'main', session = chat_session, name = 'chat', focused = true},
                {id = task_id, session = chat_session, name = 'work', status = 'working'},
            }
            local jobs = {{id = 7, command = 'cargo test', status = 'running'}}
            list_calls = 0
            local win = {width = 80, height = 20}
            function win:set_config(_) end
            function win:set_cursor(_) end
            function win:close() end
            function win:recv(_)
                return {type = 'key', key = assert(table.remove(keys, 1), 'picker did not close')}
            end
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
                session = {current = function() return chat_session end},
                fn = {
                    joblist = function() return jobs end,
                    jobstop = function(id) stopped_job = id end,
                },
                ui = {
                    buf = function() return {set_lines = function() end} end,
                    open_win = function() return win end,
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
        )
        .exec()
        .unwrap();
        let install: mlua::Function = globals.get("install_module").unwrap();
        for (name, source) in [
            (
                "maki.text_input",
                include_str!("../../../plugins/lib/maki/text_input.lua"),
            ),
            (
                "maki.color",
                include_str!("../../../plugins/lib/maki/color.lua"),
            ),
            (
                "maki.list_picker",
                include_str!("../../../plugins/lib/maki/list_picker.lua"),
            ),
            (
                "picker_rows",
                include_str!("../../../plugins/task/picker_rows.lua"),
            ),
        ] {
            install.call::<()>((name, source)).unwrap();
        }
        lua.load(include_str!("../../../plugins/task/picker.lua"))
            .exec()
            .unwrap();
        lua
    }
}
