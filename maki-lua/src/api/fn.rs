use std::env;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::MakiId;
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;

use crate::api::fs::expand_tilde;
use crate::api::keymap::{InputMode, KeymapWriter};
use crate::api::util::command::{UiAction, ui_roundtrip, ui_send};
use crate::api::util::convert::TableExt;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::jobs::{
    CallbackUpdate, CallbackUpdates, JobCommand, JobEvent, JobOwner, JobSnapshot, JobSpec,
    MAX_TAIL_LINES, Redirect, deliver_job_event,
};
use crate::plugin_permissions::{Ceiling, Permission, PluginPermissions, require};
use crate::runtime::{active_task_id, job_task_id, strip_traceback, with_jobs};

const DEFAULT_WAIT_MS: u64 = 30_000;

const NO_TASK_SCOPE_ERR: &str =
    "jobstart: no active task; use scope = \"plugin\" or { session = ... }";
const TABLE_SCOPE_ERR: &str = "jobstart: table scope must be { session = <id> }";
const SCOPE_TYPE_ERR: &str = "jobstart: scope must be \"task\", \"plugin\", or { session = <id> }";
const JOB_NOT_FOUND_ERR: &str = "job: not found";
const JOB_UNWAITABLE_ERR: &str = "jobwait: unknown job id or already waited";
const JOB_WAIT_TIMEOUT_ERR: &str = "jobwait: timed out";
const BLANK_NAME_ERR: &str = "jobstart: name must be non-blank";
const EMPTY_ARGV_ERR: &str = "jobstart: argv table must not be empty";
const CMD_TYPE_ERR: &str = "jobstart: cmd must be a shell string or an argv table";

/// Run a command in the background. A string runs through `bash -c` on Unix
/// and through [`agent.shell`](/docs/configuration/#agent) on Windows. A table
/// is spawned as argv with no shell, so nothing in it is read as a redirect, a
/// pipe, or `$(...)`.
/// You get back a job id that you can pass to `jobstop` or `jobwait` to
/// control the process.
///
/// Output nobody has read yet waits in a queue of up to 16 MiB per job.
/// Past that the child blocks on write until `on_stdout` / `on_stderr` or
/// `jobwait` drain it, so a job started where nothing delivers its events,
/// like a tool handler before it returns, pauses there.
///
/// `stdout` and `stderr` route a stream to a file instead of into maki. A
/// path is opened for append and handed to the child, so nothing is buffered
/// here: no callback, no tail, no events for that stream, and it counts as
/// truncated everywhere a tail is reported. That makes the two mutually
/// exclusive with `on_stdout` / `on_stderr` for the same stream, and a path
/// additionally needs the `fs_write` permission. To both persist and react,
/// run one job writing the file and a second one tailing it.
///
/// @param cmd string|table Shell command, or an argv table like
///   `{ "tail", "-F", path }`.
/// @param opts table? Optional settings:
///   `cwd` (string?) working directory (tilde is expanded).
///   `env` (table?) extra environment variables, `{ VAR = "value" }`.
///   `on_stdout` (function?) called with `(job_id, line, "stdout", bytes)`
///     for each stdout line, the stream name third as in Neovim (which
///     passes a list of lines where maki passes one). `bytes` is the line's
///     length before `max_line_bytes` cut it, so it equals `#line` unless
///     the line was cut. Bytes that are not UTF-8 arrive as U+FFFD.
///   `on_stderr` (function?) called with `(job_id, line, "stderr", bytes)`
///     for each stderr line.
///   `on_exit` (function?) called with `(job_id, code)` when the process finishes.
///   `stdout` (string|false?) append stdout to this path, or `false` to
///     discard it.
///   `stderr` (string|false?) same for stderr; both may name one path.
///   `scope` (string|table?) job lifetime. `"task"` (default) ends the job
///     with the current call. `"plugin"` keeps it alive until the plugin
///     unloads or reloads. `{ session = "<id>" }` keeps it alive until that
///     session ends, and survives plugin reload.
///   `tail` (integer?) trailing lines per stream kept for `jobinfo` and
///     `jobwait` (default 20, 0 disables, max 1024).
///   `max_line_bytes` (integer?) longest line delivered, in bytes. A longer
///     line is cut at a UTF-8 boundary and ends in `...`, and its rest is
///     skipped without being read into memory (default, `0`, and max: 64 MiB,
///     the `maki.fs` read limit).
///   `name` (string?) handle for `jobfind`, unique among the live jobs this
///     plugin can see. Starting a second job under a live name is an error.
/// @return (integer?, string?) Job id, or nil plus an error message when the
///   process could not start (binary not found, bad `cwd`, redirect file not
///   writable).
/// @example
/// local id, err = maki.fn.jobstart({ "rg", "--json", pattern, dir }, {
///   on_stdout = function(_, line) print(line) end,
///   on_exit = function(_, code) print("exit: " .. code) end,
/// })
/// if not id then
///   maki.log.warn("rg failed to start: " .. err)
/// end
#[lua_fn(guard = Run)]
fn jobstart(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] grants: PluginPermissions,
    cmd: Value,
    opts: Option<Table>,
) -> LuaResult<Pair<u32>> {
    let scope = opts
        .as_ref()
        .map(|opts| opts.get::<Value>("scope"))
        .transpose()?
        .unwrap_or(Value::Nil);
    let mut spec = JobSpec::new(parse_scope(lua, &plugin, scope)?, parse_command(cmd)?);
    let mut tail = None;

    if let Some(ref opts) = opts {
        spec.cwd = opts.opt("cwd").ok().flatten();
        spec.env = opts
            .get::<Table>("env")
            .ok()
            .map(|t| t.pairs::<String, String>().filter_map(Result::ok).collect());
        spec.name = job_name(opts)?;
        spec.on_stdout = callback_key(lua, opts, "on_stdout")?;
        spec.on_stderr = callback_key(lua, opts, "on_stderr")?;
        spec.on_exit = callback_key(lua, opts, "on_exit")?;
        spec.ceiling = Ceiling::current_for(lua, &plugin);
        spec.stdout = parse_redirect(lua, opts, "stdout", spec.on_stdout.is_some(), &grants)?;
        spec.stderr = parse_redirect(lua, opts, "stderr", spec.on_stderr.is_some(), &grants)?;
        if let Some(max) = opts.opt_nonzero("max_line_bytes")? {
            spec.max_line_bytes = max;
        }
        tail = opts.opt("tail").ok().flatten();
        if let Some(n) = tail
            && n > MAX_TAIL_LINES
        {
            return Err(mlua::Error::runtime(format!(
                "jobstart: tail must be in 0..={MAX_TAIL_LINES}"
            )));
        }
    }

    let task_id = active_task_id(lua);
    with_jobs(lua, |store| {
        if let Some(ref name) = spec.name
            && let Some(held) = store.find_named(name, task_id, &plugin)
        {
            return Err(mlua::Error::runtime(format!(
                "jobstart: name {name:?} is already held by live job {held}"
            )));
        }
        let id = try_pair!(store.start(spec));
        store.set_tail(id, tail);
        Ok((Some(id), None))
    })
}

fn parse_command(cmd: Value) -> LuaResult<JobCommand> {
    match cmd {
        Value::String(cmd) => Ok(JobCommand::Shell(cmd.to_str()?.to_string())),
        Value::Table(argv) => {
            let argv: Vec<String> = argv.sequence_values().collect::<LuaResult<_>>()?;
            if argv.is_empty() {
                return Err(mlua::Error::runtime(EMPTY_ARGV_ERR));
            }
            Ok(JobCommand::Argv(argv))
        }
        _ => Err(mlua::Error::runtime(CMD_TYPE_ERR)),
    }
}

fn parse_redirect(
    lua: &Lua,
    opts: &Table,
    key: &str,
    has_callback: bool,
    grants: &PluginPermissions,
) -> LuaResult<Redirect> {
    let value = opts.get::<Value>(key)?;
    if value.is_nil() {
        return Ok(Redirect::Capture);
    }
    if has_callback {
        return Err(mlua::Error::runtime(format!(
            "jobstart: {key} and on_{key} are mutually exclusive; tail the file from a second job to get both"
        )));
    }
    match value {
        Value::Boolean(false) => Ok(Redirect::Discard),
        Value::String(path) => {
            require(lua, grants, Permission::FsWrite)?;
            Ok(Redirect::File(expand_tilde(&path.to_str()?)))
        }
        _ => Err(mlua::Error::runtime(format!(
            "jobstart: {key} must be a path string or false"
        ))),
    }
}

fn callback_key(lua: &Lua, opts: &Table, key: &str) -> LuaResult<Option<RegistryKey>> {
    opts.get::<Function>(key)
        .ok()
        .map(|f| lua.create_registry_value(f))
        .transpose()
}

fn job_name(opts: &Table) -> LuaResult<Option<String>> {
    let name = opts.opt::<String>("name")?;
    if name.as_deref().is_some_and(|name| name.trim().is_empty()) {
        return Err(mlua::Error::runtime(BLANK_NAME_ERR));
    }
    Ok(name)
}

/// Find the live job of this plugin that `jobstart` registered under {name}.
/// An exited job never answers, so `jobfind(...) or jobstart(...)` restarts a
/// job that died instead of adopting its id. The name stays on the `joblist`
/// row, which is where you go to see why it died.
///
/// @param name string Name passed to `jobstart`.
/// @return (integer|nil, string|nil) Job id, or nil and an error when no live
///   job holds the name.
/// @example
/// local id = maki.fn.jobfind("log-tail")
/// if not id then
///   id = maki.fn.jobstart("tail -F /tmp/log", { name = "log-tail", scope = "plugin" })
/// end
#[lua_fn(guard = Run)]
fn jobfind(lua: &Lua, #[ctx] plugin: Arc<str>, name: String) -> LuaResult<Pair<u32>> {
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.find_named(&name, task_id, &plugin)) {
        Some(id) => Ok((Some(id), None)),
        None => Ok(err_pair(JOB_NOT_FOUND_ERR)),
    }
}

fn parse_scope(lua: &Lua, plugin: &Arc<str>, scope: Value) -> LuaResult<JobOwner> {
    let task_scope = || {
        job_task_id(lua)
            .map(JobOwner::Task)
            .ok_or_else(|| mlua::Error::runtime(NO_TASK_SCOPE_ERR))
    };
    match scope {
        Value::Nil => task_scope(),
        Value::String(name) => match &*name.to_str()? {
            "task" => task_scope(),
            "plugin" => Ok(JobOwner::Plugin(Arc::clone(plugin))),
            other => Err(mlua::Error::runtime(format!(
                "jobstart: unknown scope {other:?}; expected \"task\", \"plugin\", or {{ session = ... }}"
            ))),
        },
        Value::Table(scope) => {
            let Value::String(raw) = scope.get::<Value>("session")? else {
                return Err(mlua::Error::runtime(TABLE_SCOPE_ERR));
            };
            let session: MakiId =
                raw.to_str()?
                    .parse()
                    .map_err(|e: maki_storage::id::MakiIdParseError| {
                        mlua::Error::runtime(e.to_string())
                    })?;
            Ok(JobOwner::Session {
                session,
                plugin: Arc::clone(plugin),
            })
        }
        _ => Err(mlua::Error::runtime(SCOPE_TYPE_ERR)),
    }
}

/// Snapshot a job this plugin can see. Live jobs report tails collected
/// so far; session-owned jobs still answer after they exit.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return (table|nil, string|nil) `{ id, command, name, pid, session, status,
///   exit_code, elapsed_secs, stdout_lines, stderr_lines }`, or nil and
///   an error. `status` is `"running"` or `"exited"`.
/// @example
/// local info = maki.fn.jobinfo(id)
#[lua_fn(guard = Run)]
fn jobinfo(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<Pair<Value>> {
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.snapshot(job_id, task_id, &plugin)) {
        Some(snap) => Ok((Some(Value::Table(snapshot_table(lua, &snap, true)?)), None)),
        None => Ok(err_pair(JOB_NOT_FOUND_ERR)),
    }
}

/// Attach (or replace) callbacks on a job this plugin can see. This is how a
/// plugin picks its jobs back up after a reload: unloading drops the Lua
/// callbacks of its session-owned jobs, but the processes keep running.
///
/// Keys absent from {opts} leave the current callback alone. Attaching
/// `on_exit` to a job that already exited still fires it once, with the
/// recorded exit code, so a reload racing the exit cannot lose it.
///
/// @param job_id integer Job id, e.g. from `joblist`.
/// @param opts table `on_stdout`, `on_stderr`, `on_exit`: a function, or `false` to clear.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// -- A monitor that survives /reload: adopt the live job or start one.
/// local sid = maki.session.current()
/// local id = maki.fn.jobfind("log-tail")
///   or maki.fn.jobstart({ "tail", "-F", path }, {
///     name = "log-tail",
///     scope = { session = sid },
///   })
/// maki.fn.jobattach(id, {
///   on_stdout = function(_, line) maki.session.notify(line, { session = sid }) end,
///   on_exit = function(_, code) maki.session.notify("tail died: " .. code, { session = sid }) end,
/// })
#[lua_fn(guard = Run)]
fn jobattach(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    job_id: u32,
    opts: Table,
) -> LuaResult<Pair<bool>> {
    let updates = CallbackUpdates {
        on_stdout: callback_update(lua, &opts, "on_stdout")?,
        on_stderr: callback_update(lua, &opts, "on_stderr")?,
        on_exit: callback_update(lua, &opts, "on_exit")?,
    };
    let task_id = active_task_id(lua);
    let attached = with_jobs(lua, |store| {
        store.attach(lua, job_id, task_id, &plugin, updates)
    });
    if attached {
        Ok((Some(true), None))
    } else {
        Ok(err_pair(JOB_NOT_FOUND_ERR))
    }
}

fn callback_update(lua: &Lua, opts: &Table, key: &str) -> LuaResult<CallbackUpdate> {
    match opts.get::<Value>(key)? {
        Value::Nil => Ok(CallbackUpdate::Keep),
        Value::Boolean(false) => Ok(CallbackUpdate::Clear),
        Value::Function(callback) => Ok(CallbackUpdate::Set(lua.create_registry_value(callback)?)),
        _ => Err(mlua::Error::runtime(format!(
            "jobattach: {key} must be a function or false"
        ))),
    }
}

/// List jobs this plugin can see, including exited session-owned jobs (so an
/// id started before a reload stays findable). Rows identify the job; call
/// `jobinfo` for tails. Pass a session id to list only that session's jobs.
/// Plugin and task jobs carry no session, so a filter never matches them.
///
/// @param session string? Session id filter.
/// @return (table) array of `{ id, command, name, pid, session, status,
///   exit_code, elapsed_secs }`.
/// @example
/// local jobs = maki.fn.joblist(maki.session.current())
#[lua_fn(guard = Run)]
fn joblist(lua: &Lua, #[ctx] plugin: Arc<str>, session: Option<String>) -> LuaResult<Pair<Value>> {
    let filter = match session {
        Some(raw) => match raw.parse::<MakiId>() {
            Ok(id) => Some(id),
            Err(e) => return Ok(err_pair(e.to_string())),
        },
        None => None,
    };
    let task_id = active_task_id(lua);
    let snaps = with_jobs(lua, |store| store.list(filter, task_id, &plugin));
    let result = lua.create_table()?;
    for (i, snap) in snaps.iter().enumerate() {
        result.set(i + 1, snapshot_table(lua, snap, false)?)?;
    }
    Ok((Some(Value::Table(result)), None))
}

fn snapshot_table(lua: &Lua, snap: &JobSnapshot, tails: bool) -> LuaResult<Table> {
    let row = lua.create_table()?;
    row.set("id", snap.id)?;
    row.set("command", snap.command.as_str())?;
    row.set("name", snap.name.as_deref())?;
    row.set("pid", snap.pid)?;
    row.set("session", snap.session.map(|s| s.to_string()))?;
    row.set("elapsed_secs", snap.elapsed_secs)?;
    row.set(
        "status",
        if snap.exit_code.is_some() {
            "exited"
        } else {
            "running"
        },
    )?;
    row.set("exit_code", snap.exit_code)?;
    if tails {
        let stdout = lua.create_table()?;
        for (i, line) in snap.stdout_lines.iter().enumerate() {
            stdout.set(i + 1, line.as_str())?;
        }
        row.set("stdout_lines", stdout)?;
        let stderr = lua.create_table()?;
        for (i, line) in snap.stderr_lines.iter().enumerate() {
            stderr.set(i + 1, line.as_str())?;
        }
        row.set("stderr_lines", stderr)?;
    }
    Ok(row)
}

/// Kill a running job immediately (SIGKILL on Unix). Safe to call on
/// jobs that already exited or on unknown ids.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return
/// @example
/// maki.fn.jobstop(id)
#[lua_fn(guard = Run)]
fn jobstop(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<()> {
    let task_id = active_task_id(lua);
    with_jobs(lua, |store| store.kill(job_id, task_id, &plugin));
    Ok(())
}

/// Drop an exited session-owned job from the store. Running jobs are left
/// alone; use `jobstop` to kill those. Unknown ids are a no-op.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return
/// @example
/// maki.fn.jobforget(id)
#[lua_fn(guard = Run)]
fn jobforget(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<()> {
    let task_id = active_task_id(lua);
    with_jobs(lua, |store| store.forget(lua, job_id, task_id, &plugin));
    Ok(())
}

/// Wait for a job to finish and collect its output. Returns a result
/// table with `stdout`, `stderr`, `exit_code`, and `truncated`. The output
/// starts from the job's captured tail, which holds what was delivered before
/// the wait, and a live job adds every line from then on. `truncated` says
/// whether that tail had already lost a line by then (`tail` too small or 0,
/// or the stream redirected away). It doesn't count lines cut by
/// `max_line_bytes`: those still arrive, ending in `...`.
///
/// While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
/// callbacks fire as events arrive (like Neovim), so you can stream
/// output into a buffer while parked here. An already-exited
/// session-owned job answers from its snapshot and fires no callbacks.
/// Task and plugin jobs leave the store on exit, so waiting after that
/// answers nil plus an error.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @param timeout_ms integer? Maximum wait in milliseconds (default 30000).
/// @return (table?, string?) `{ stdout, stderr, exit_code, truncated }`, or
///   nil plus an error on timeout or an unknown job.
/// @example
/// local id = maki.fn.jobstart("echo hello")
/// local result, err = maki.fn.jobwait(id, 5000)
/// if result then
///   print(result.stdout)
/// end
#[lua_fn(guard = Run)]
async fn jobwait(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    job_id: u32,
    timeout_ms: Option<u64>,
) -> LuaResult<Pair<Table>> {
    let task_id = active_task_id(&lua);
    let Some(snap) = with_jobs(&lua, |store| store.snapshot(job_id, task_id, &plugin)) else {
        return Ok(err_pair(JOB_UNWAITABLE_ERR));
    };
    if let Some(code) = snap.exit_code {
        return wait_result(
            &lua,
            &snap.stdout_lines,
            &snap.stderr_lines,
            code,
            snap.dropped_output,
        );
    }
    let Some(mut lease) = with_jobs(&lua, |store| store.lease_events(job_id, task_id, &plugin))
    else {
        return Ok(err_pair(JOB_UNWAITABLE_ERR));
    };

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
    let mut deadline = pin!(sleep(timeout));

    // A pump delivers until the wait takes its lease, and only the tail
    // remembers those lines.
    let mut stdout_lines = snap.stdout_lines;
    let mut stderr_lines = snap.stderr_lines;

    let exit_code = loop {
        let event = tokio::select! {
            biased;
            event = lease.recv() => event,
            () = &mut deadline => None,
        };

        let Some(event) = event else {
            return Ok(err_pair(JOB_WAIT_TIMEOUT_ERR));
        };
        // A failing callback must not abort the wait: the event is already
        // recorded and the exit still needs collecting. Same policy as the
        // detached dispatch pump.
        if let Err(e) = deliver_job_event(&lua, job_id, &event).await {
            tracing::warn!(job_id, error = %strip_traceback(&e), "jobwait callback failed");
        }
        match event {
            JobEvent::Stdout(line) => stdout_lines.push(line.text),
            JobEvent::Stderr(line) => stderr_lines.push(line.text),
            JobEvent::Exit(code) => break code,
        }
    };

    wait_result(
        &lua,
        &stdout_lines,
        &stderr_lines,
        exit_code,
        snap.dropped_output,
    )
}

/// The single shape `jobwait` answers with, so its two paths cannot drift
/// apart from the documented keys.
fn wait_result(
    lua: &Lua,
    stdout: &[String],
    stderr: &[String],
    exit_code: i32,
    truncated: bool,
) -> LuaResult<Pair<Table>> {
    let result = lua.create_table()?;
    result.set("stdout", stdout.join("\n"))?;
    result.set("stderr", stderr.join("\n"))?;
    result.set("exit_code", exit_code)?;
    result.set("truncated", truncated)?;
    Ok((Some(result), None))
}

/// Check whether {name} can be found on `$PATH` or is an absolute path
/// to a file. Returns 1 when found, 0 otherwise (matches Neovim's
/// `vim.fn.executable`).
///
/// @param name string Program name (e.g. `"git"`) or absolute path.
/// @return (integer) `1` if found, `0` otherwise.
/// @example
/// if maki.fn.executable("rg") == 1 then
///   -- use ripgrep
/// end
// A file probe over `$PATH`: it answers whether a file exists, never what the
// environment holds, so `fs_read` covers it.
#[lua_fn(guard = FsRead)]
fn executable(_lua: &Lua, name: String) -> LuaResult<i32> {
    let found = env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(&name).is_file()))
        .unwrap_or(false)
        || Path::new(&name).is_file();
    Ok(if found { 1 } else { 0 })
}

/// Read the viewport of the focused chat transcript, like Neovim's
/// `vim.fn.winsaveview()`. The transcript is the only scrollable window
/// maki has, so there is no window argument.
///
/// `topline` is the 1-based transcript line at the top of the viewport, so
/// the last visible one is `math.min(topline + height - 1, line_count)`.
/// `auto_scroll` has no Vim counterpart: it is true while the transcript
/// follows streaming output.
///
/// @return (table|nil, string|nil) `{topline, line_count, height, auto_scroll}`, or nil and an error.
/// @example
/// local view = maki.fn.winsaveview()
/// maki.fn.winrestview({ topline = view.topline + 1 })
#[lua_fn]
async fn winsaveview(
    lua: Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
) -> LuaResult<Pair<Table>> {
    let view =
        try_pair!(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::WinSaveView { reply_tx }).await);
    let t = lua.create_table()?;
    t.set("topline", i64::from(view.scroll_top) + 1)?;
    t.set("line_count", view.line_count)?;
    t.set("height", view.height)?;
    t.set("auto_scroll", view.auto_scroll)?;
    Ok((Some(t), None))
}

/// Scroll the focused chat transcript so that the `topline` field of
/// {view} becomes the top visible line, like Neovim's
/// `vim.fn.winrestview()`. Out of range values are clamped. Other keys are
/// ignored, so a table straight from `winsaveview()` round-trips.
///
/// Scrolling away from the bottom unpins the transcript; landing back at
/// the bottom re-pins it so streaming output keeps following.
///
/// @param view table View to restore. Only `topline` (1-based) is read.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// maki.fn.winrestview({ topline = 1 })
#[lua_fn]
fn winrestview(
    _lua: &Lua,
    #[ctx] tx: Option<UnboundedSender<UiAction>>,
    view: Table,
) -> LuaResult<Pair<bool>> {
    let topline = view.opt::<i64>("topline")?.unwrap_or(1);
    let scroll_top = topline.saturating_sub(1).clamp(0, i64::from(u32::MAX)) as u32;
    try_pair!(ui_send(tx.as_ref(), UiAction::WinRestView { scroll_top }));
    Ok((Some(true), None))
}

/// The current mode, like `vim.fn.mode()`: `"i"` while the chat input has
/// the keyboard and takes typed text, `"n"` otherwise. See
/// [Modes](/docs/lua-api/#modes). Always `"n"` without a UI.
///
/// @return (string) `"n"` or `"i"`.
/// @example
/// maki.keymap.set({ "n", "i" }, "<M-m>", function()
///   maki.ui.set_mode(maki.fn.mode() == "n" and "i" or "n")
/// end)
#[lua_fn]
fn mode(lua: &Lua) -> LuaResult<&'static str> {
    let mode = lua
        .app_data_ref::<KeymapWriter>()
        .map_or(InputMode::Normal, |writer| writer.mode.get());
    Ok(mode.letter())
}

lua_table! {
    /// Process and environment helpers, modeled after Neovim's `vim.fn` job
    /// control. Use these to run shell commands, wait for output, and check
    /// whether programs are installed.
    ///
    /// ```lua
    /// local id = maki.fn.jobstart("git status", {
    ///   on_exit = function(_, code) print("done: " .. code) end,
    /// })
    /// ```
    "maki.fn" => pub(crate) fn create_fn_table(
        plugin: Arc<str>,
        perms: &PluginPermissions,
        grants: PluginPermissions,
        tx: Option<UnboundedSender<UiAction>>,
    ), DOCS [
        jobstart(perms, plugin, grants), jobstop(perms, plugin), jobforget(perms, plugin),
        jobwait(perms, plugin), jobinfo(perms, plugin), joblist(perms, plugin),
        jobattach(perms, plugin), jobfind(perms, plugin),
        executable(perms),
        winsaveview(tx), winrestview(tx), mode,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, WinView};
    use futures::FutureExt;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::mpsc::unbounded_channel;

    const TEST_PLUGIN: &str = "test-plugin";
    /// Can't exit while a test runs, so only the code under test ends it.
    const SLEEPING_JOB: &str = "sleep 600";
    const LONG_WAIT_MS: u64 = 600_000;

    fn lua_with_view(tx: Option<UnboundedSender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        winsaveview__register(&t, &lua, tx.clone()).unwrap();
        winrestview__register(&t, &lua, tx).unwrap();
        lua.globals().set("f", t).unwrap();
        lua
    }

    #[test_case::test_case("return f.winsaveview()" ; "winsaveview")]
    #[test_case::test_case("return f.winrestview({ topline = 3 })" ; "winrestview")]
    #[tokio::test]
    async fn view_without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_view(None);
        let (val, err): (Value, Option<String>) = lua.load(code).eval_async().await.unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[tokio::test]
    async fn winsaveview_reports_the_viewport_one_based() {
        const SCROLL_TOP: u32 = 6;
        const LINE_COUNT: u32 = 100;
        const HEIGHT: u16 = 24;

        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_view(Some(tx));
        std::thread::spawn(move || {
            let Some(UiAction::WinSaveView { reply_tx }) = rx.blocking_recv() else {
                panic!("expected winsaveview request");
            };
            let sent = reply_tx.send(WinView {
                scroll_top: SCROLL_TOP,
                line_count: LINE_COUNT,
                height: HEIGHT,
                auto_scroll: false,
            });
            assert!(sent.is_ok());
        });
        let (view, err): (Table, Option<String>) = lua
            .load("return f.winsaveview()")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(view.get::<u32>("topline").unwrap(), SCROLL_TOP + 1);
        assert_eq!(view.get::<u32>("line_count").unwrap(), LINE_COUNT);
        assert_eq!(view.get::<u16>("height").unwrap(), HEIGHT);
        assert!(!view.get::<bool>("auto_scroll").unwrap());
    }

    #[test_case::test_case("{ topline = 12 }", 11 ; "explicit_topline")]
    #[test_case::test_case("{}", 0 ; "missing_topline_defaults_to_first_line")]
    #[test_case::test_case("{ topline = -5 }", 0 ; "below_range_clamps_to_first_line")]
    #[tokio::test]
    async fn winrestview_forwards_zero_based_scroll_top(arg: &str, expected: u32) {
        let (tx, mut rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_view(Some(tx));
        let (ok, err): (bool, Option<String>) = lua
            .load(format!("return f.winrestview({arg})"))
            .eval_async()
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(err, None);
        let Some(UiAction::WinRestView { scroll_top }) = rx.recv().await else {
            panic!("expected winrestview request");
        };
        assert_eq!(scroll_top, expected);
    }

    enum Abandoned {
        /// `call_async` ends the Lua call the moment its future drops.
        HostCall,
        /// The shape of #1345: a coroutine the plugin made, which only the
        /// collector ever frees.
        PluginCoroutine,
    }

    #[test_case::test_case(Abandoned::HostCall ; "host_call")]
    #[test_case::test_case(Abandoned::PluginCoroutine ; "plugin_coroutine")]
    #[tokio::test]
    async fn abandoned_jobwait_releases_the_job(abandoned: Abandoned) {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        jobwait__register(
            &t,
            &lua,
            &PluginPermissions::trusted(),
            Arc::from(TEST_PLUGIN),
        )
        .unwrap();
        lua.globals().set("f", t).unwrap();
        let owner = JobOwner::Plugin(Arc::from(TEST_PLUGIN));
        let id = with_jobs(&lua, |store| store.start(JobSpec::new(owner, SLEEPING_JOB))).unwrap();
        let wait_src = format!("function() f.jobwait({id}, {LONG_WAIT_MS}) end");

        match abandoned {
            Abandoned::HostCall => {
                let wait: Function = lua.load(format!("return {wait_src}")).eval().unwrap();
                assert!(
                    wait.call_async::<()>(()).now_or_never().is_none(),
                    "jobwait must park"
                );
            }
            Abandoned::PluginCoroutine => {
                let resume = format!("coroutine.resume(coroutine.create({wait_src}))");
                lua.load(resume).exec_async().await.unwrap();
                lua.gc_collect().unwrap();
                lua.gc_collect().unwrap();
            }
        }

        assert!(
            with_jobs(&lua, |store| store.lease_events(id, None, TEST_PLUGIN)).is_some(),
            "the abandoned wait must free the job"
        );
    }
}
