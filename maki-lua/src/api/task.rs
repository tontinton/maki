//! `maki.task`. The host keeps the subagent transcripts, so a plugin only needs
//! to list the tasks and show one.

use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::SessionRef;
use mlua::{Either as LuaEither, Lua, Result as LuaResult, UserDataRef, Value};

use crate::api::util::command::{TaskRequest, UiAction, ui_json_roundtrip};
use crate::api::util::ctx::LuaCtx;
use crate::api::util::pair::{Pair, try_pair};
use crate::background::BackgroundTasks;
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{CANCEL_ABANDON_AFTER, is_user_action};

const USER_ACTION_ERR: &str = "stopping a task requires a user command or keybind";
const TASK_ID_ERR: &str = "stop(ctx, id): id must be a string";
const NO_CHAT_ERR: &str = "maki.task.stop: the ctx has no chat session";
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
/// @return (table|nil, string|nil) Array of `{id, session, name, focused, status?, plugin?}`
///   where `status` is `"working"`, `"idle"` (a plugin task between prompts),
///   `"done"`, or `"error"`, and `session` is the chat session that `stop()`
///   asks for. Or nil and an error.
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
/// Called as `stop(ctx, id)` from a tool handler, it is the model's stop
/// instead: it stops a background task of the calling chat (a moved command
/// or subagent), since a tool call is a turn the user started. It waits up to
/// 5 seconds for the task's final reply and returns it, and then the chat gets
/// no notice of its end. If the reply is late it returns `"stop requested"`,
/// and the result still reaches the chat later as a quiet notice. An id of
/// another chat, a finished task or an unknown id returns an error naming the
/// chat's running task ids.
///
/// @param id string|LuaCtx Task id, as returned by `list()`. Or the handler's
///   ctx, for the model's stop.
/// @param opts table|string `session` (string), the chat session of the
///   `list()` row. If the user switched sessions since, this returns an error
///   rather than stopping a task with the same id in the other session. With a
///   ctx: the background task id.
/// @return (boolean|string|nil, string|nil) true on success (with a ctx: the
///   final reply or `"stop requested"`), or nil and an error.
/// @example
/// local _, err = maki.task.stop(task.id, { session = task.session })
/// -- in a tool handler
/// local output, err = maki.task.stop(ctx, input.id)
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
            let session = try_pair!(
                ctx.agent()
                    .ok_or_else(|| ctx.cap_err("maki.task.stop"))
                    .and_then(|agent| {
                        agent
                            .session_id
                            .as_ref()
                            .ok_or_else(|| NO_CHAT_ERR.to_owned())
                    })
                    .map(SessionRef::id)
            );
            drop(ctx);
            let output = try_pair!(
                BackgroundTasks::of(&lua)
                    .stop_by_model(session, &task_id, CANCEL_ABANDON_AFTER)
                    .await
            );
            return Ok((Some(Value::String(lua.create_string(output)?)), None));
        }
        LuaEither::Right(id) => id,
    };
    if !is_user_action(&lua) {
        return Ok((None, Some(USER_ACTION_ERR.to_owned())));
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
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use test_case::test_case;

    use crate::background::{BackgroundInfo, BackgroundTask, Fire, Kind, Lifetime, Stop, StopMark};
    use crate::runtime::{TaskCell, TaskScope, run_command_scoped};
    use maki_agent::cancel::CancelToken;
    use maki_agent::tools::test_support::{stub_ctx_with, stub_seed};
    use maki_agent::{AgentMode, SessionMailbox, ToolDoneEvent, ToolOutput};
    use maki_storage::id::MakiId;

    const TASK_ID: &str = "toolu_01";
    const PLUGIN: &str = "bash";
    const OUTPUT: &str = "compiled";
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
        let lua = picker_lua(stop_error);
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

    fn moved_call_lua(
        owner: MakiId,
        caller: MakiId,
    ) -> (Lua, BackgroundTasks, StopMark, CancelToken) {
        let lua = lua_with_task(None);
        let registry = BackgroundTasks::of(&lua);
        let (trigger, token) = CancelToken::new();
        let stop = StopMark::default();
        registry
            .register(BackgroundTask {
                info: BackgroundInfo {
                    id: TASK_ID.to_owned(),
                    plugin: Arc::from(PLUGIN),
                    name: SUBAGENT_NAME.to_owned(),
                    kind: Kind::Call {
                        tool: Arc::from(PLUGIN),
                    },
                    running: true,
                    started_at: 0,
                    cost: None,
                },
                session: owner,
                task: None,
                lifetime: Lifetime::Call,
                stop: stop.clone(),
                cancel: Fire::Owned(trigger),
            })
            .unwrap();
        let mut tool_ctx = stub_ctx_with(&AgentMode::Build, None, None);
        tool_ctx.session_id = Some(SessionRef::from_id(caller));
        let ctx = lua.create_userdata(LuaCtx::handler(&tool_ctx)).unwrap();
        lua.globals().set("ctx", ctx).unwrap();
        (lua, registry, stop, token)
    }

    fn live_chat(chat: MakiId) -> (SessionMailbox, Arc<maki_agent::AgentSeed>) {
        let mailbox = SessionMailbox::register(chat);
        let seed = mailbox.publish_seed(stub_seed(chat));
        (mailbox, seed)
    }

    #[test_case(true, "missing", "running: toolu_01" ; "unknown_id")]
    #[test_case(false, TASK_ID, "none running" ; "another_chats_id")]
    fn ctx_stop_refuses_what_the_calling_chat_does_not_run(
        caller_owns_it: bool,
        target: &str,
        listing: &str,
    ) {
        let (owner, stranger) = (MakiId::generate(), MakiId::generate());
        let _live = (live_chat(owner), live_chat(stranger));
        let caller = if caller_owns_it { owner } else { stranger };
        let (lua, _registry, stop, token) = moved_call_lua(owner, caller);

        let (val, err): (Value, Option<String>) = smol::block_on(
            lua.load(format!("return task.stop(ctx, '{target}')"))
                .eval_async(),
        )
        .unwrap();

        assert!(val.is_nil());
        assert_eq!(
            err,
            Some(format!("no running background task {target}; {listing}"))
        );
        assert_eq!(stop.get(), Stop::None);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn ctx_stop_returns_the_final_reply_of_the_task() {
        let chat = MakiId::generate();
        let (mailbox, _seed) = live_chat(chat);
        let (lua, registry, stop, token) = moved_call_lua(chat, chat);
        let stop_call = lua
            .load(format!("return task.stop(ctx, '{TASK_ID}')"))
            .eval_async::<(Option<String>, Option<String>)>();
        let deliver = async {
            while stop.get() != Stop::Model {
                futures_lite::future::yield_now().await;
            }
            registry.deliver(
                TASK_ID,
                ToolDoneEvent {
                    id: TASK_ID.to_owned(),
                    tool: Arc::from(PLUGIN),
                    output: Arc::new(ToolOutput::Plain(OUTPUT.into())),
                    is_error: false,
                    annotation: None,
                    written_path: None,
                    call: None,
                    backgrounded: false,
                },
                None,
            );
        };

        let (stopped, ()) = smol::block_on(futures_lite::future::zip(stop_call, deliver));

        assert_eq!(stopped.unwrap(), (Some(OUTPUT.to_owned()), None));
        assert!(token.is_cancelled());
        assert!(mailbox.drain().is_empty());
    }

    /// Runs `plugins/task/picker.lua` against a fake `maki` that lists the main
    /// chat and one task, then presses Down, Ctrl+S and Esc.
    fn picker_lua(stop_error: Option<&str>) -> Lua {
        let lua = Lua::new();
        let globals = lua.globals();
        globals.set("task_id", TASK_ID).unwrap();
        globals.set("chat_session", CHAT_SESSION).unwrap();
        globals.set("stop_error", stop_error).unwrap();
        lua.load(
            r#"
            local modules = {}
            function require(name) return assert(modules[name], name) end
            function install_module(name, source) modules[name] = assert(loadstring(source))() end
            local keys = {'<Down>', '<C-s>', '<Esc>'}
            local tasks = {
                {id = 'main', session = chat_session, name = 'chat', focused = true},
                {id = task_id, session = chat_session, name = 'work', status = 'working'},
            }
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
