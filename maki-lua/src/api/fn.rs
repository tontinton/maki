use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use maki_agent::SessionMailbox;
use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::MakiId;
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::fs::expand_tilde;
use crate::api::util::command::{UiAction, ui_roundtrip, ui_send};
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{active_task_id, job_task_id, with_jobs};

const DEFAULT_TAIL: usize = 20;
const MAX_TAIL_LINES: usize = 1024;
const MAX_COMPLETED_SESSION_JOBS: usize = 256;
const DEFAULT_WAIT_MS: u64 = 30_000;

const READER_BUF_SIZE: usize = 8 * 1024;

#[derive(Clone)]
pub(crate) enum JobEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum JobOwner {
    Task(u64),
    Plugin(Arc<str>),
    /// Lives until the session ends (or the host process exits). Survives
    /// plugin reload: the starting plugin can still inspect and stop it.
    Session {
        session: MakiId,
        plugin: Arc<str>,
    },
}

#[derive(Clone)]
struct JobNotify {
    session: MakiId,
    wake: bool,
    on_success: bool,
}

struct JobMeta {
    owner: JobOwner,
    command: String,
    pid: u32,
    started: Instant,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    event_rx: Option<flume::Receiver<JobEvent>>,
    stdout_tail: VecDeque<String>,
    stderr_tail: VecDeque<String>,
    tail_cap: usize,
    notify: Option<JobNotify>,
    exit_code: Option<i32>,
    /// Recorded at exit so elapsed time stops counting once the process is gone.
    elapsed_secs: Option<u64>,
}

impl JobMeta {
    fn session(&self) -> Option<MakiId> {
        match self.owner {
            JobOwner::Session { session, .. } => Some(session),
            _ => None,
        }
    }

    fn record_line(&mut self, stdout: bool, line: &str) {
        if self.tail_cap == 0 {
            return;
        }
        let tail = if stdout {
            &mut self.stdout_tail
        } else {
            &mut self.stderr_tail
        };
        if tail.len() >= self.tail_cap {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
    }
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
    completed_order: VecDeque<u32>,
}

struct CheckedOutReceiver {
    lua: Lua,
    job_id: u32,
    receiver: Option<flume::Receiver<JobEvent>>,
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
            completed_order: VecDeque::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        owner: JobOwner,
        cmd: &str,
        cwd: Option<String>,
        env: Option<HashMap<String, String>>,
        on_stdout: Option<RegistryKey>,
        on_stderr: Option<RegistryKey>,
        on_exit: Option<RegistryKey>,
    ) -> Result<u32, String> {
        let mut command = shell_command(cmd);
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
            unsafe {
                command.pre_exec(|| {
                    rustix::process::setsid()?;
                    Ok(())
                });
            }
        }

        if let Some(dir) = cwd.as_deref().map(expand_tilde) {
            if !dir.is_dir() {
                return Err(format!("cwd is not a directory: {}", dir.display()));
            }
            command.current_dir(dir);
        }
        if let Some(ref env_map) = env {
            for (k, v) in env_map {
                command.env(k, v);
            }
        }

        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let id = self.next_id;
        self.next_id += 1;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (event_tx, event_rx) = flume::unbounded();

        macro_rules! spawn_reader {
            ($stream:expr, $name:expr, $variant:ident) => {
                if let Some(stream) = $stream {
                    let tx = event_tx.clone();
                    Some(
                        thread::Builder::new()
                            .name($name.into())
                            .spawn(move || {
                                for line in BufReader::with_capacity(READER_BUF_SIZE, stream)
                                    .lines()
                                    .map_while(Result::ok)
                                {
                                    if tx.send(JobEvent::$variant(line)).is_err() {
                                        break;
                                    }
                                }
                            })
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                }
            };
        }
        let stdout_handle = spawn_reader!(stdout, "job-stdout", Stdout);
        let stderr_handle = spawn_reader!(stderr, "job-stderr", Stderr);

        thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                if let Some(h) = stdout_handle {
                    let _ = h.join();
                }
                if let Some(h) = stderr_handle {
                    let _ = h.join();
                }
                let _ = event_tx.send(JobEvent::Exit(code));
            })
            .map_err(|e| e.to_string())?;

        self.jobs.insert(
            id,
            JobMeta {
                owner,
                command: cmd.to_string(),
                pid,
                started: Instant::now(),
                on_stdout,
                on_stderr,
                on_exit,
                event_rx: Some(event_rx),
                stdout_tail: VecDeque::new(),
                stderr_tail: VecDeque::new(),
                tail_cap: DEFAULT_TAIL,
                notify: None,
                exit_code: None,
                elapsed_secs: None,
            },
        );

        Ok(id)
    }

    fn configure(&mut self, id: u32, notify: Option<JobNotify>, tail: Option<usize>) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if let Some(cap) = tail {
            job.tail_cap = cap.min(MAX_TAIL_LINES);
        }
        job.notify = notify;
    }

    pub fn is_empty(&self, owner: &JobOwner) -> bool {
        !self.jobs.values().any(|job| job.owner == *owner)
    }

    pub fn callback_key(&self, job_id: u32, event: &JobEvent) -> Option<&RegistryKey> {
        let meta = self.jobs.get(&job_id)?;
        match event {
            JobEvent::Stdout(_) => meta.on_stdout.as_ref(),
            JobEvent::Stderr(_) => meta.on_stderr.as_ref(),
            JobEvent::Exit(_) => meta.on_exit.as_ref(),
        }
    }

    pub fn take_receiver(
        &mut self,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Option<flume::Receiver<JobEvent>> {
        let job = self.jobs.get_mut(&job_id)?;
        job.can_access(task_id, plugin)
            .then(|| job.event_rx.take())?
    }

    pub fn restore_receiver(&mut self, job_id: u32, receiver: flume::Receiver<JobEvent>) {
        if let Some(job) = self.jobs.get_mut(&job_id)
            && job.event_rx.is_none()
        {
            job.event_rx = Some(receiver);
        }
    }

    pub fn drain_events(&self, owner: &JobOwner, buf: &mut Vec<(u32, JobEvent)>) {
        buf.clear();
        for (&id, job) in self.jobs.iter().filter(|(_, job)| job.owner == *owner) {
            if let Some(ref rx) = job.event_rx {
                while let Ok(event) = rx.try_recv() {
                    buf.push((id, event));
                }
            }
        }
    }

    pub fn drain_plugin_events(&self, buf: &mut Vec<(u32, JobEvent)>) {
        buf.clear();
        for (&id, job) in self
            .jobs
            .iter()
            .filter(|(_, job)| matches!(job.owner, JobOwner::Plugin(_) | JobOwner::Session { .. }))
        {
            if let Some(ref rx) = job.event_rx {
                while let Ok(event) = rx.try_recv() {
                    buf.push((id, event));
                }
            }
        }
    }

    pub fn record_event(&mut self, job_id: u32, event: &JobEvent) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        match event {
            JobEvent::Stdout(line) => job.record_line(true, line),
            JobEvent::Stderr(line) => job.record_line(false, line),
            JobEvent::Exit(code) => {
                job.exit_code = Some(*code);
                job.elapsed_secs = Some(job.started.elapsed().as_secs());
            }
        }
    }

    /// Session-owned jobs stay inspectable after exit. Task/plugin jobs are
    /// removed the way they always were.
    pub fn complete(&mut self, lua: &Lua, job_id: u32, code: i32) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        job.exit_code = Some(code);
        if let Some(notify) = job.notify.clone()
            && (code != 0 || notify.on_success)
        {
            let message = format!("[job {job_id}] \"{}\" exited with code {code}", job.command);
            if let Err(e) = SessionMailbox::notify(notify.session, message, notify.wake) {
                tracing::warn!(error = %e, job_id, "session job notify failed");
            }
        }
        if matches!(job.owner, JobOwner::Session { .. }) {
            drop_callbacks(lua, job);
            self.completed_order.push_back(job_id);
            while self.completed_order.len() > MAX_COMPLETED_SESSION_JOBS {
                let oldest = self.completed_order.pop_front().unwrap();
                if self
                    .jobs
                    .get(&oldest)
                    .is_some_and(|j| j.exit_code.is_some())
                {
                    self.remove(lua, oldest, false);
                }
            }
            return;
        }
        self.finish(lua, job_id);
    }

    pub fn kill_session(&mut self, lua: &Lua, session: MakiId) {
        let ids: Vec<u32> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.session() == Some(session))
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.remove(lua, id, true);
        }
        let remaining: HashSet<u32> = self.jobs.keys().copied().collect();
        self.completed_order.retain(|id| remaining.contains(id));
    }

    /// Drop Lua callbacks for session jobs started by {plugin} without
    /// killing the process. Used on plugin unload/reload.
    pub fn detach_plugin_callbacks(&mut self, lua: &Lua, plugin: &str) {
        for job in self.jobs.values_mut() {
            if let JobOwner::Session {
                plugin: owner_plugin,
                ..
            } = &job.owner
                && owner_plugin.as_ref() == plugin
            {
                drop_callbacks(lua, job);
            }
        }
    }

    pub fn snapshot(&self, job_id: u32, task_id: Option<u64>, plugin: &str) -> Option<JobSnapshot> {
        let job = self.jobs.get(&job_id)?;
        job.can_access(task_id, plugin).then(|| JobSnapshot {
            id: job_id,
            command: job.command.clone(),
            session: job.session(),
            pid: job.pid,
            elapsed_secs: job
                .elapsed_secs
                .unwrap_or_else(|| job.started.elapsed().as_secs()),
            exit_code: job.exit_code,
            stdout_lines: job.stdout_tail.iter().cloned().collect(),
            stderr_lines: job.stderr_tail.iter().cloned().collect(),
        })
    }

    /// List jobs this plugin can see. Task and plugin jobs leave the map on
    /// exit; session-owned jobs stay, so exited ones show up here with their
    /// final tail and exit code.
    pub fn list(
        &self,
        session: Option<MakiId>,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Vec<JobSnapshot> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.can_access(task_id, plugin))
            .filter(|(_, job)| session.is_none_or(|s| job.session() == Some(s)))
            .map(|(&id, job)| JobSnapshot {
                id,
                command: job.command.clone(),
                session: job.session(),
                pid: job.pid,
                elapsed_secs: job
                    .elapsed_secs
                    .unwrap_or_else(|| job.started.elapsed().as_secs()),
                exit_code: job.exit_code,
                stdout_lines: job.stdout_tail.iter().cloned().collect(),
                stderr_lines: job.stderr_tail.iter().cloned().collect(),
            })
            .collect()
    }

    pub fn kill(&mut self, job_id: u32, task_id: Option<u64>, plugin: &str) {
        if let Some(job) = self.jobs.get_mut(&job_id)
            && job.can_access(task_id, plugin)
        {
            kill_job(job);
        }
    }

    pub fn kill_owner(&mut self, lua: &Lua, owner: &JobOwner) {
        let ids = self
            .jobs
            .iter()
            .filter_map(|(&id, job)| (job.owner == *owner).then_some(id))
            .collect::<Vec<_>>();
        for id in ids {
            self.remove(lua, id, true);
        }
    }

    pub fn finish(&mut self, lua: &Lua, job_id: u32) {
        self.remove(lua, job_id, false);
    }

    fn remove(&mut self, lua: &Lua, job_id: u32, kill: bool) {
        if let Some(mut job) = self.jobs.remove(&job_id) {
            if kill {
                kill_job(&mut job);
            }
            for key in [job.on_stdout, job.on_stderr, job.on_exit]
                .into_iter()
                .flatten()
            {
                lua.remove_registry_value(key).ok();
            }
        }
    }

    fn kill_all(&mut self) {
        for job in self.jobs.values_mut() {
            kill_job(job);
        }
    }
}

impl Drop for JobStore {
    fn drop(&mut self) {
        self.kill_all();
    }
}

impl CheckedOutReceiver {
    fn new(lua: &Lua, job_id: u32, receiver: flume::Receiver<JobEvent>) -> Self {
        Self {
            lua: lua.clone(),
            job_id,
            receiver: Some(receiver),
        }
    }

    fn get(&self) -> &flume::Receiver<JobEvent> {
        self.receiver.as_ref().expect("receiver is checked out")
    }
}

impl Drop for CheckedOutReceiver {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            with_jobs(&self.lua, |store| {
                store.restore_receiver(self.job_id, receiver);
            });
        }
    }
}

pub(crate) struct JobSnapshot {
    pub id: u32,
    pub command: String,
    pub session: Option<MakiId>,
    pub pid: u32,
    pub elapsed_secs: u64,
    pub exit_code: Option<i32>,
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
}

impl JobMeta {
    fn can_access(&self, task_id: Option<u64>, plugin: &str) -> bool {
        match &self.owner {
            JobOwner::Task(owner_id) => task_id == Some(*owner_id),
            JobOwner::Plugin(owner_plugin) => owner_plugin.as_ref() == plugin,
            JobOwner::Session {
                plugin: owner_plugin,
                ..
            } => owner_plugin.as_ref() == plugin,
        }
    }
}

fn drop_callbacks(lua: &Lua, job: &mut JobMeta) {
    for key in [&mut job.on_stdout, &mut job.on_stderr, &mut job.on_exit]
        .into_iter()
        .filter_map(Option::take)
    {
        lua.remove_registry_value(key).ok();
    }
}

fn shell_command(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(cmd);
        c
    }
}

fn kill_job(meta: &mut JobMeta) {
    // The wait thread already reaped the process, so its pid may have been
    // recycled. Only signal jobs we know are still alive.
    if meta.exit_code.is_some() {
        return;
    }
    let pid = meta.pid;
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        let raw = match i32::try_from(pid) {
            Ok(raw) => raw,
            Err(_) => return,
        };
        if let Some(pid) = Pid::from_raw(raw) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Run a shell command in the background. The command runs through
/// `bash -c` on Unix or `cmd /C` on Windows. You get back a job id
/// that you can pass to `jobstop` or `jobwait` to control the process.
///
/// @param cmd string Shell command to run.
/// @param opts table? Optional settings:
///   `cwd` (string?) working directory (tilde is expanded).
///   `env` (table?) extra environment variables, `{ VAR = "value" }`.
///   `on_stdout` (function?) called with `(job_id, line)` for each stdout line.
///   `on_stderr` (function?) called with `(job_id, line)` for each stderr line.
///   `on_exit` (function?) called with `(job_id, code)` when the process finishes.
///   `owner` (string?) job lifetime. `"task"` (default) ends the job with
///     the current call. `"plugin"` keeps it alive until the plugin unloads
///     or reloads. `"session"` keeps it alive until that session ends, and
///     survives plugin reload; requires `session`.
///   `session` (string?) session id. Required when `owner = "session"`.
///   `notify` (boolean|table?) when the process exits, post a mailbox
///     observation to `session`. `true` uses `{ wake = true, on_success = true }`.
///     A table accepts `wake` (boolean, default true) and `on_success`
///     (boolean, default true). Only valid with `owner = "session"`.
///   `tail` (integer?) trailing lines per stream kept for `jobinfo`
///     (default 20, 0 disables, max 1024).
/// @return (integer) Job id.
/// @example
/// local id = maki.fn.jobstart("ls -la", {
///   cwd = "~/projects",
///   on_stdout = function(_, line) print(line) end,
///   on_exit = function(_, code) print("exit: " .. code) end,
/// })
#[lua_fn(guard = Run)]
fn jobstart(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    cmd: String,
    opts: Option<Table>,
) -> LuaResult<u32> {
    let owner_name: Option<String> = opts
        .as_ref()
        .map(|opts| opts.get("owner"))
        .transpose()?
        .flatten();
    let session = opts
        .as_ref()
        .and_then(|opts| opts.get::<Option<String>>("session").ok().flatten());
    let owner = match owner_name.as_deref() {
        None | Some("task") => job_task_id(lua).map(JobOwner::Task).ok_or_else(|| {
            mlua::Error::runtime("jobstart: no active task; use owner = \"plugin\" or \"session\"")
        })?,
        Some("plugin") => JobOwner::Plugin(Arc::clone(&plugin)),
        Some("session") => {
            let raw = session.as_deref().ok_or_else(|| {
                mlua::Error::runtime("jobstart: owner = \"session\" requires session")
            })?;
            let session: MakiId =
                raw.parse()
                    .map_err(|e: maki_storage::id::MakiIdParseError| {
                        mlua::Error::runtime(e.to_string())
                    })?;
            JobOwner::Session {
                session,
                plugin: Arc::clone(&plugin),
            }
        }
        Some(other) => {
            return Err(mlua::Error::runtime(format!(
                "jobstart: unknown owner {other:?}; expected \"task\", \"plugin\", or \"session\""
            )));
        }
    };

    let (cwd, env, on_stdout, on_stderr, on_exit, notify, tail) = match opts {
        Some(ref opts) => {
            let cwd: Option<String> = opts.get("cwd").ok();
            let env: Option<HashMap<String, String>> = opts
                .get::<Table>("env")
                .ok()
                .map(|t| t.pairs::<String, String>().filter_map(Result::ok).collect());
            let on_stdout = opts
                .get::<Function>("on_stdout")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let on_stderr = opts
                .get::<Function>("on_stderr")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let on_exit = opts
                .get::<Function>("on_exit")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let notify = parse_notify(opts, &owner)?;
            let tail: Option<usize> = opts.get("tail").ok();
            if let Some(n) = tail
                && n > MAX_TAIL_LINES
            {
                return Err(mlua::Error::runtime(format!(
                    "jobstart: tail must be in 0..={MAX_TAIL_LINES}"
                )));
            }
            (cwd, env, on_stdout, on_stderr, on_exit, notify, tail)
        }
        None => (None, None, None, None, None, None, None),
    };

    with_jobs(lua, |store| {
        let id = store.start(owner, &cmd, cwd, env, on_stdout, on_stderr, on_exit)?;
        store.configure(id, notify, tail);
        Ok::<u32, String>(id)
    })
    .map_err(mlua::Error::runtime)
}

fn parse_notify(opts: &Table, owner: &JobOwner) -> LuaResult<Option<JobNotify>> {
    let session = match owner {
        JobOwner::Session { session, .. } => *session,
        _ => {
            if opts.contains_key("notify")? {
                return Err(mlua::Error::runtime(
                    "jobstart: notify requires owner = \"session\"",
                ));
            }
            return Ok(None);
        }
    };
    match opts.get::<Value>("notify")? {
        Value::Nil => Ok(None),
        Value::Boolean(false) => Ok(None),
        Value::Boolean(true) => Ok(Some(JobNotify {
            session,
            wake: true,
            on_success: true,
        })),
        Value::Table(t) => Ok(Some(JobNotify {
            session,
            wake: t.get("wake").unwrap_or(true),
            on_success: t.get("on_success").unwrap_or(true),
        })),
        _ => Err(mlua::Error::runtime(
            "jobstart: notify must be a boolean or table",
        )),
    }
}

/// Snapshot a job this plugin can see. Live jobs report tails collected
/// so far; session-owned jobs still answer after they exit.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return (table|nil, string|nil) `{ id, command, pid, session, status,
///   exit_code, elapsed_secs, stdout_lines, stderr_lines }`, or nil and
///   an error. `status` is `"running"` or `"exited"`.
/// @example
/// local info = maki.fn.jobinfo(id)
#[lua_fn(guard = Run)]
fn jobinfo(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<Pair<Value>> {
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.snapshot(job_id, task_id, &plugin)) {
        Some(snap) => Ok((Some(Value::Table(snapshot_table(lua, &snap)?)), None)),
        None => Ok(err_pair("job: not found")),
    }
}

/// List jobs this plugin can see, including exited session-owned jobs (so an
/// id started before a reload stays findable). Pass a session id to restrict
/// session-owned jobs to that session.
///
/// @param session string? Session id filter.
/// @return (table) array of `{ id, command, pid, session, status, exit_code,
///   elapsed_secs, stdout_lines, stderr_lines }`.
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
        result.set(i + 1, snapshot_table(lua, snap)?)?;
    }
    Ok((Some(Value::Table(result)), None))
}

fn snapshot_table(lua: &Lua, snap: &JobSnapshot) -> LuaResult<Table> {
    let row = lua.create_table()?;
    row.set("id", snap.id)?;
    row.set("command", snap.command.as_str())?;
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

/// Wait for a job to finish and collect its output. Returns a result
/// table with `stdout`, `stderr`, `exit_code`, and `truncated`. `truncated`
/// is false when the collected lines are the full output; a job that already
/// exited answers from its captured tail, so `truncated` is true and the
/// output may be missing lines. Returns `nil` if the job does not finish
/// before the timeout.
///
/// While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
/// callbacks fire as events arrive (like Neovim), so you can stream
/// output into a buffer while parked here.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @param timeout_ms integer? Maximum wait in milliseconds (default 30000).
/// @return (table?) `{ stdout, stderr, exit_code, truncated }`, or nil on timeout.
/// @example
/// local id = maki.fn.jobstart("echo hello")
/// local result = maki.fn.jobwait(id, 5000)
/// if result then
///   print(result.stdout)
/// end
#[lua_fn(guard = Run)]
async fn jobwait(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    job_id: u32,
    timeout_ms: Option<u64>,
) -> LuaResult<Value> {
    let task_id = active_task_id(&lua);
    if let Some(snap) = with_jobs(&lua, |store| store.snapshot(job_id, task_id, &plugin))
        && let Some(code) = snap.exit_code
    {
        let result = lua.create_table()?;
        result.set("stdout", snap.stdout_lines.join("\n"))?;
        result.set("stderr", snap.stderr_lines.join("\n"))?;
        result.set("exit_code", code)?;
        result.set("truncated", true)?;
        return Ok(mlua::Value::Table(result));
    }
    let receiver = with_jobs(&lua, |store| store.take_receiver(job_id, task_id, &plugin))
        .ok_or_else(|| mlua::Error::runtime("unknown job id or already waited"))?;
    let receiver = CheckedOutReceiver::new(&lua, job_id, receiver);

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
    let deadline = smol::Timer::after(timeout);
    futures_lite::pin!(deadline);

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();

    let exit_code = loop {
        let event =
            futures_lite::future::or(async { receiver.get().recv_async().await.ok() }, async {
                (&mut deadline).await;
                None
            })
            .await;

        let Some(event) = event else {
            return Ok(mlua::Value::Nil);
        };
        deliver_job_event(&lua, job_id, &event)?;
        match event {
            JobEvent::Stdout(line) => stdout_lines.push(line),
            JobEvent::Stderr(line) => stderr_lines.push(line),
            JobEvent::Exit(code) => break code,
        }
    };

    let result = lua.create_table()?;
    result.set("stdout", stdout_lines.join("\n"))?;
    result.set("stderr", stderr_lines.join("\n"))?;
    result.set("exit_code", exit_code)?;
    result.set("truncated", false)?;
    Ok(mlua::Value::Table(result))
}

/// Fire the job's Lua callback for {event} (if any) and mark the job
/// dead on exit. Shared by `jobwait` and the async dispatch loop so
/// both deliver events identically.
pub(crate) fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let callback = with_jobs(lua, |store| {
        store.record_event(job_id, event);
        store
            .callback_key(job_id, event)
            .and_then(|key| lua.registry_value::<Function>(key).ok())
    });
    if let JobEvent::Exit(code) = event {
        with_jobs(lua, |store| store.complete(lua, job_id, *code));
    }
    if let Some(callback) = callback {
        let arg = match event {
            JobEvent::Stdout(line) | JobEvent::Stderr(line) => {
                Value::String(lua.create_string(line)?)
            }
            JobEvent::Exit(code) => Value::Integer(*code as i64),
        };
        callback.call::<()>((job_id, arg))?;
    }
    Ok(())
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
#[lua_fn(guard = Env)]
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
    #[ctx] tx: Option<flume::Sender<UiAction>>,
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
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    view: Table,
) -> LuaResult<Pair<bool>> {
    let topline = view.get::<Option<i64>>("topline")?.unwrap_or(1);
    let scroll_top = topline.saturating_sub(1).clamp(0, i64::from(u32::MAX)) as u32;
    try_pair!(ui_send(tx.as_ref(), UiAction::WinRestView { scroll_top }));
    Ok((Some(true), None))
}

lua_table! {
    /// Process and environment helpers, modeled after Neovim's `vim.fn` job
    /// control. Use these to run shell commands, wait for output, and check
    /// whether programs are installed.
    ///
    /// ```lua
    /// local id = maki.fn.jobstart("git status", {
    ///   on_exit = function(code) print("done: " .. code) end,
    /// })
    /// ```
    "maki.fn" => pub(crate) fn create_fn_table(
        plugin: Arc<str>,
        perms: &PluginPermissions,
        tx: Option<flume::Sender<UiAction>>,
    ), DOCS [
        jobstart(perms, plugin), jobstop(perms, plugin), jobwait(perms, plugin),
        jobinfo(perms, plugin), joblist(perms, plugin), executable(perms),
        winsaveview(tx), winrestview(tx),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, WinView};

    const TEST_PLUGIN: &str = "test-plugin";

    fn make_store() -> JobStore {
        JobStore::new()
    }

    fn task_owner(id: u64) -> JobOwner {
        JobOwner::Task(id)
    }

    fn plugin_owner() -> JobOwner {
        JobOwner::Plugin(Arc::from(TEST_PLUGIN))
    }

    fn stub_job(
        owner: JobOwner,
        on_stdout: Option<RegistryKey>,
        on_exit: Option<RegistryKey>,
    ) -> JobMeta {
        JobMeta {
            owner,
            command: String::new(),
            pid: 0,
            started: Instant::now(),
            on_stdout,
            on_stderr: None,
            on_exit,
            event_rx: None,
            stdout_tail: VecDeque::new(),
            stderr_tail: VecDeque::new(),
            tail_cap: DEFAULT_TAIL,
            notify: None,
            exit_code: None,
            elapsed_secs: None,
        }
    }

    fn start_echo(store: &mut JobStore) -> u32 {
        store
            .start(task_owner(1), "echo hello", None, None, None, None, None)
            .unwrap()
    }

    #[cfg(unix)]
    fn group_alive(pid: u32) -> bool {
        use rustix::process::{Pid, test_kill_process_group};
        i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .is_some_and(|pid| test_kill_process_group(pid).is_ok())
    }

    #[cfg(unix)]
    fn wait_for_group_exit(pid: u32) -> bool {
        (0..500).any(|_| {
            thread::sleep(Duration::from_millis(10));
            !group_alive(pid)
        })
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_store_kills_its_jobs() {
        let mut store = make_store();
        let id = store
            .start(task_owner(1), "sleep 30", None, None, None, None, None)
            .expect("job started");
        let pid = store.jobs[&id].pid;
        assert!(group_alive(pid), "job should be running before the drop");

        drop(store);

        assert!(
            wait_for_group_exit(pid),
            "dropping the store must not orphan the process group"
        );
    }

    #[test]
    fn start_invalid_cwd_returns_error() {
        let mut store = make_store();
        let result = store.start(
            task_owner(1),
            "echo hello",
            Some("/nonexistent_dir_abc_xyz_123".into()),
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn finishing_a_job_removes_it() {
        let lua = Lua::new();
        let mut store = make_store();
        let owner = task_owner(1);
        assert!(store.is_empty(&owner));

        let id = start_echo(&mut store);
        assert!(!store.is_empty(&owner));
        let receiver = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();
        while !matches!(
            receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
            JobEvent::Exit(_)
        ) {}

        store.finish(&lua, id);
        assert!(store.is_empty(&owner));
    }

    #[test]
    fn unknown_job_operations_are_noops() {
        let mut store = make_store();
        store.kill(999, Some(1), TEST_PLUGIN);
        assert!(store.take_receiver(999, Some(1), TEST_PLUGIN).is_none());
        assert!(store.callback_key(999, &JobEvent::Exit(0)).is_none());
    }

    #[test]
    fn take_receiver_lifecycle() {
        let mut store = make_store();
        assert!(store.take_receiver(999, Some(1), TEST_PLUGIN).is_none());

        let id = start_echo(&mut store);
        assert!(
            store.take_receiver(id, Some(2), TEST_PLUGIN).is_none(),
            "another task must not access the job"
        );
        assert!(store.take_receiver(id, Some(1), TEST_PLUGIN).is_some());
        assert!(
            store.take_receiver(id, Some(1), TEST_PLUGIN).is_none(),
            "second take should fail (receiver already moved)"
        );
    }

    #[test]
    fn plugin_owner_can_be_accessed_only_by_its_plugin() {
        let mut store = make_store();
        let id = store
            .start(plugin_owner(), "echo hello", None, None, None, None, None)
            .unwrap();

        assert!(store.take_receiver(id, Some(1), "other-plugin").is_none());
        assert!(store.take_receiver(id, None, TEST_PLUGIN).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn kill_requires_owner_access() {
        let lua = Lua::new();
        let mut store = make_store();
        let id = store
            .start(task_owner(1), "sleep 30", None, None, None, None, None)
            .unwrap();
        let pid = store.jobs[&id].pid;

        store.kill(id, Some(2), TEST_PLUGIN);
        assert!(group_alive(pid));

        store.kill(id, Some(1), TEST_PLUGIN);
        assert!(wait_for_group_exit(pid));
        store.finish(&lua, id);
    }

    #[cfg(unix)]
    #[test]
    fn owner_cleanup_is_isolated() {
        let lua = Lua::new();
        let mut store = make_store();
        let task = task_owner(1);
        let plugin = plugin_owner();
        let task_id = store
            .start(task.clone(), "sleep 30", None, None, None, None, None)
            .unwrap();
        let plugin_id = store
            .start(plugin.clone(), "sleep 30", None, None, None, None, None)
            .unwrap();
        let task_pid = store.jobs[&task_id].pid;
        let plugin_pid = store.jobs[&plugin_id].pid;

        store.kill_owner(&lua, &task);

        assert!(store.is_empty(&task));
        assert!(!store.is_empty(&plugin));
        assert!(wait_for_group_exit(task_pid));
        assert!(group_alive(plugin_pid));
        store.kill_owner(&lua, &plugin);
        assert!(wait_for_group_exit(plugin_pid));
    }

    #[test]
    fn callback_key_returns_none_without_callbacks() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        assert!(
            store
                .callback_key(id, &JobEvent::Stdout("x".into()))
                .is_none()
        );
        assert!(
            store
                .callback_key(id, &JobEvent::Stderr("x".into()))
                .is_none()
        );
        assert!(store.callback_key(id, &JobEvent::Exit(0)).is_none());
    }

    #[test]
    fn take_receiver_delivers_events() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let rx = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        let mut got_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(JobEvent::Exit(_)) => {
                    got_exit = true;
                    break;
                }
                Ok(_) => continue,
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(got_exit, "should receive exit event for completed job");
    }

    #[test]
    fn drain_events_filters_by_owner() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let plugin_id = store
            .start(plugin_owner(), "echo plugin", None, None, None, None, None)
            .unwrap();

        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_events(&task_owner(1), &mut buf);
            if buf
                .iter()
                .any(|(jid, e)| *jid == id && matches!(e, JobEvent::Exit(_)))
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("should receive exit event for completed job");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(buf.iter().all(|(job_id, _)| *job_id != plugin_id));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_plugin_events(&mut buf);
            if buf
                .iter()
                .any(|(job_id, event)| *job_id == plugin_id && matches!(event, JobEvent::Exit(_)))
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("should receive plugin job exit event");
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(buf.iter().all(|(job_id, _)| *job_id != id));
    }

    #[test]
    fn drain_events_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _rx = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        let mut buf = Vec::new();
        store.drain_events(&task_owner(1), &mut buf);
        assert!(
            buf.is_empty(),
            "drained receiver yields no events via drain_events"
        );
    }

    fn lua_with_view(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        winsaveview__register(&t, &lua, tx.clone()).unwrap();
        winrestview__register(&t, &lua, tx).unwrap();
        lua.globals().set("f", t).unwrap();
        lua
    }

    #[test_case::test_case("return f.winsaveview()" ; "winsaveview")]
    #[test_case::test_case("return f.winrestview({ topline = 3 })" ; "winrestview")]
    fn view_without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_view(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test]
    fn winsaveview_reports_the_viewport_one_based() {
        const SCROLL_TOP: u32 = 6;
        const LINE_COUNT: u32 = 100;
        const HEIGHT: u16 = 24;

        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::WinSaveView { reply_tx }) = rx.recv() else {
                panic!("expected winsaveview request");
            };
            reply_tx
                .send(WinView {
                    scroll_top: SCROLL_TOP,
                    line_count: LINE_COUNT,
                    height: HEIGHT,
                    auto_scroll: false,
                })
                .unwrap();
        });
        let (view, err): (Table, Option<String>) =
            smol::block_on(lua.load("return f.winsaveview()").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(view.get::<u32>("topline").unwrap(), SCROLL_TOP + 1);
        assert_eq!(view.get::<u32>("line_count").unwrap(), LINE_COUNT);
        assert_eq!(view.get::<u16>("height").unwrap(), HEIGHT);
        assert!(!view.get::<bool>("auto_scroll").unwrap());
    }

    #[test_case::test_case("{ topline = 12 }", 11 ; "explicit_topline")]
    #[test_case::test_case("{}", 0 ; "missing_topline_defaults_to_first_line")]
    #[test_case::test_case("{ topline = -5 }", 0 ; "below_range_clamps_to_first_line")]
    fn winrestview_forwards_zero_based_scroll_top(arg: &str, expected: u32) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        let (ok, err): (bool, Option<String>) = smol::block_on(
            lua.load(format!("return f.winrestview({arg})"))
                .eval_async(),
        )
        .unwrap();
        assert!(ok);
        assert_eq!(err, None);
        let Ok(UiAction::WinRestView { scroll_top }) = rx.recv() else {
            panic!("expected winrestview request");
        };
        assert_eq!(scroll_top, expected);
    }

    #[test]
    fn exit_cleanup_runs_before_a_failing_callback() {
        let lua = Lua::new();
        lua.set_app_data(JobStore::new());
        let callback = lua
            .create_function(|_, ()| Err::<(), _>(mlua::Error::runtime("callback failed")))
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        with_jobs(&lua, |store| {
            store
                .jobs
                .insert(1, stub_job(task_owner(1), None, Some(callback_key)));
        });

        assert!(deliver_job_event(&lua, 1, &JobEvent::Exit(0)).is_err());
        assert!(with_jobs(&lua, |store| store.is_empty(&task_owner(1))));
    }

    #[test]
    fn finish_releases_callback_registry_values() {
        let lua = Lua::new();
        let capture = Arc::new(());
        let callback_capture = Arc::clone(&capture);
        let callback = lua
            .create_function(move |_, ()| {
                let _ = &callback_capture;
                Ok(())
            })
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        let mut store = make_store();
        store
            .jobs
            .insert(1, stub_job(task_owner(1), Some(callback_key), None));
        assert_eq!(Arc::strong_count(&capture), 2);

        store.finish(&lua, 1);
        lua.gc_collect().unwrap();

        assert_eq!(Arc::strong_count(&capture), 1);
    }

    #[test]
    fn snapshot_reports_live_tails_and_hides_inaccessible_jobs() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        store.record_event(id, &JobEvent::Stdout("hello".into()));
        store.record_event(id, &JobEvent::Stderr("warn".into()));

        let snap = store.snapshot(id, Some(1), TEST_PLUGIN).unwrap();
        assert_eq!(snap.command, "echo hello");
        assert_eq!(snap.stdout_lines, ["hello"]);
        assert_eq!(snap.stderr_lines, ["warn"]);
        assert!(snap.exit_code.is_none());
        assert!(store.snapshot(id, Some(2), TEST_PLUGIN).is_none());
        assert!(store.snapshot(999, Some(1), TEST_PLUGIN).is_none());
    }

    #[test]
    fn tail_cap_drops_oldest_lines() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        store.configure(id, None, Some(2));
        store.record_event(id, &JobEvent::Stdout("a".into()));
        store.record_event(id, &JobEvent::Stdout("b".into()));
        store.record_event(id, &JobEvent::Stdout("c".into()));
        let snap = store.snapshot(id, Some(1), TEST_PLUGIN).unwrap();
        assert_eq!(snap.stdout_lines, ["b", "c"]);
    }

    #[test]
    fn list_is_live_and_access_scoped() {
        let lua = Lua::new();
        let mut store = make_store();
        let task = start_echo(&mut store);
        let plugin = store
            .start(plugin_owner(), "echo plugin", None, None, None, None, None)
            .unwrap();

        let from_task: Vec<u32> = store
            .list(None, Some(1), TEST_PLUGIN)
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(from_task.contains(&task));
        assert!(from_task.contains(&plugin));

        let from_other: Vec<u32> = store
            .list(None, Some(2), "other-plugin")
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(from_other.is_empty());

        store.record_event(task, &JobEvent::Exit(0));
        store.finish(&lua, task);
        let live: Vec<u32> = store
            .list(None, Some(1), TEST_PLUGIN)
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(live, [plugin]);
        assert!(store.snapshot(task, Some(1), TEST_PLUGIN).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn session_job_survives_plugin_detach_and_answers_after_exit() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mailbox = SessionMailbox::register(session);
        let owner = JobOwner::Session {
            session,
            plugin: Arc::from(TEST_PLUGIN),
        };
        let mut store = make_store();
        let id = store
            .start(
                owner.clone(),
                "echo hi; echo err >&2; exit 3",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store.configure(
            id,
            Some(JobNotify {
                session,
                wake: false,
                on_success: true,
            }),
            Some(5),
        );

        store.detach_plugin_callbacks(&lua, TEST_PLUGIN);
        assert!(store.jobs.contains_key(&id), "detach must keep the job");

        let mut buf = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_plugin_events(&mut buf);
            for (_, event) in &buf {
                store.record_event(id, event);
            }
            if buf
                .iter()
                .any(|(jid, e)| *jid == id && matches!(e, JobEvent::Exit(_)))
            {
                break;
            }
            assert!(Instant::now() < deadline, "session job never exited");
            thread::sleep(Duration::from_millis(20));
        }
        store.complete(&lua, id, 3);

        let snap = store
            .snapshot(id, None, TEST_PLUGIN)
            .expect("peek after exit");
        assert_eq!(snap.exit_code, Some(3));
        assert!(
            snap.stdout_lines.iter().any(|l| l.contains("hi")),
            "stdout tail: {:?}",
            snap.stdout_lines
        );
        assert!(
            snap.stderr_lines.iter().any(|l| l.contains("err")),
            "stderr tail: {:?}",
            snap.stderr_lines
        );
        let notes = mailbox.drain();
        assert!(
            notes.iter().any(|m| m
                .user_text()
                .is_some_and(|t| t.contains("exited with code 3"))),
            "expected exit notify"
        );

        store.kill_session(&lua, session);
        assert!(store.snapshot(id, None, TEST_PLUGIN).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn kill_session_does_not_touch_other_sessions() {
        let lua = Lua::new();
        let a = MakiId::generate();
        let b = MakiId::generate();
        let mut store = make_store();
        let first = store
            .start(
                JobOwner::Session {
                    session: a,
                    plugin: Arc::from(TEST_PLUGIN),
                },
                "sleep 30",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let second = store
            .start(
                JobOwner::Session {
                    session: b,
                    plugin: Arc::from(TEST_PLUGIN),
                },
                "sleep 30",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store.kill_session(&lua, a);
        assert!(store.snapshot(first, None, TEST_PLUGIN).is_none());
        assert!(store.snapshot(second, None, TEST_PLUGIN).is_some());
        store.kill_session(&lua, b);
    }

    #[cfg(unix)]
    #[test]
    fn exited_session_job_freezes_elapsed_at_exit() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        let id = store
            .start(
                JobOwner::Session {
                    session,
                    plugin: Arc::from(TEST_PLUGIN),
                },
                "sleep 0.1",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let mut buf = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_plugin_events(&mut buf);
            for (_, event) in &buf {
                store.record_event(id, event);
            }
            if buf
                .iter()
                .any(|(job_id, event)| *job_id == id && matches!(event, JobEvent::Exit(_)))
            {
                break;
            }
            assert!(Instant::now() < deadline, "session job never exited");
            thread::sleep(Duration::from_millis(10));
        }
        store.complete(&lua, id, 0);
        let at_exit = store.snapshot(id, None, TEST_PLUGIN).unwrap().elapsed_secs;
        thread::sleep(Duration::from_millis(200));
        let later = store.snapshot(id, None, TEST_PLUGIN).unwrap().elapsed_secs;
        assert_eq!(at_exit, later, "elapsed must freeze once the job exits");
        store.kill_session(&lua, session);
    }

    #[cfg(unix)]
    #[test]
    fn exited_session_job_stays_listed_and_kill_is_a_noop() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        let id = store
            .start(
                JobOwner::Session {
                    session,
                    plugin: Arc::from(TEST_PLUGIN),
                },
                "sleep 0.1",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let mut buf = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_plugin_events(&mut buf);
            for (_, event) in &buf {
                store.record_event(id, event);
            }
            if buf
                .iter()
                .any(|(job_id, event)| *job_id == id && matches!(event, JobEvent::Exit(_)))
            {
                break;
            }
            assert!(Instant::now() < deadline, "session job never exited");
            thread::sleep(Duration::from_millis(10));
        }
        store.complete(&lua, id, 0);
        store.kill(id, None, TEST_PLUGIN);
        let snap = store
            .snapshot(id, None, TEST_PLUGIN)
            .expect("exited session job must stay inspectable");
        assert_eq!(snap.exit_code, Some(0));
        assert!(
            store
                .list(Some(session), None, TEST_PLUGIN)
                .iter()
                .any(|listed| listed.id == id && listed.exit_code == Some(0)),
            "exited session job must stay listed"
        );
        store.kill_session(&lua, session);
    }
}
