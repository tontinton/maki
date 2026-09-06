use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::MakiId;
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};
use shell_words::join as shell_join;

use crate::api::fs::expand_tilde;
use crate::api::util::command::{UiAction, ui_roundtrip, ui_send};
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::plugin_permissions::{Permission, PluginPermissions, denied_error};
use crate::runtime::{active_task_id, job_task_id, strip_traceback, with_jobs};

const DEFAULT_TAIL: usize = 20;
const MAX_TAIL_LINES: usize = 1024;
const MAX_COMPLETED_SESSION_JOBS: usize = 256;
const DEFAULT_WAIT_MS: u64 = 30_000;

const READER_BUF_SIZE: usize = 8 * 1024;

const NO_TASK_SCOPE_ERR: &str =
    "jobstart: no active task; use scope = \"plugin\" or { session = ... }";
const TABLE_SCOPE_ERR: &str = "jobstart: table scope must be { session = <id> }";
const SCOPE_TYPE_ERR: &str = "jobstart: scope must be \"task\", \"plugin\", or { session = <id> }";
const JOB_NOT_FOUND_ERR: &str = "job: not found";
const BLANK_NAME_ERR: &str = "jobstart: name must be non-blank";
const EMPTY_ARGV_ERR: &str = "jobstart: argv table must not be empty";
const CMD_TYPE_ERR: &str = "jobstart: cmd must be a shell string or an argv table";

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

/// A shell line, or an argv the plugin built itself so there are no quoting
/// rules to get wrong.
pub(crate) enum JobCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl From<&str> for JobCommand {
    fn from(cmd: &str) -> Self {
        Self::Shell(cmd.to_string())
    }
}

impl JobCommand {
    fn build(&self) -> Command {
        match self {
            Self::Shell(cmd) => shell_command(cmd),
            Self::Argv(argv) => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
        }
    }

    /// Only for `jobinfo` / `joblist` rows: an argv job spawns from the vec,
    /// so nothing ever re-parses this.
    fn display(&self) -> String {
        match self {
            Self::Shell(cmd) => cmd.clone(),
            Self::Argv(argv) => shell_join(argv),
        }
    }
}

pub(crate) enum Redirect {
    /// Piped to a reader thread, so callbacks and tails see the lines.
    Capture,
    Discard,
    /// The child appends to the file on its own: no reader thread, no events,
    /// no tail.
    File(PathBuf),
}

impl Redirect {
    fn stdio(&self) -> Result<Stdio, String> {
        match self {
            Self::Capture => Ok(Stdio::piped()),
            Self::Discard => Ok(Stdio::null()),
            Self::File(path) => File::options()
                .create(true)
                .append(true)
                .open(path)
                .map(Stdio::from)
                .map_err(|e| format!("cannot open {}: {e}", path.display())),
        }
    }
}

pub(crate) struct JobSpec {
    pub owner: JobOwner,
    pub cmd: JobCommand,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub stdout: Redirect,
    pub stderr: Redirect,
    pub on_stdout: Option<RegistryKey>,
    pub on_stderr: Option<RegistryKey>,
    pub on_exit: Option<RegistryKey>,
}

impl JobSpec {
    pub(crate) fn new(owner: JobOwner, cmd: impl Into<JobCommand>) -> Self {
        Self {
            owner,
            cmd: cmd.into(),
            name: None,
            cwd: None,
            env: None,
            stdout: Redirect::Capture,
            stderr: Redirect::Capture,
            on_stdout: None,
            on_stderr: None,
            on_exit: None,
        }
    }
}

struct JobMeta {
    owner: JobOwner,
    command: String,
    /// A reloaded plugin looks its job up by this instead of matching on the
    /// command string. See [`JobStore::find_named`].
    name: Option<String>,
    pid: u32,
    started: Instant,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    event_rx: Option<flume::Receiver<JobEvent>>,
    stdout_tail: VecDeque<String>,
    stderr_tail: VecDeque<String>,
    tail_cap: usize,
    /// Whether the tails ever lost a line, which is what `truncated` answers.
    /// True from the start for a stream sent to a file or dropped, since
    /// nothing ever reaches the tail there and an empty tail is no evidence
    /// the job stayed quiet.
    dropped_output: bool,
    /// Set by the wait thread the moment the child is reaped, which is well
    /// before `exit_code`. Read by [`kill_job`].
    reaped: Arc<AtomicBool>,
    exit_code: Option<i32>,
    /// Recorded at exit so elapsed time stops counting once the process is gone.
    elapsed_secs: Option<u64>,
    /// Exit code owed to an `on_exit` attached after the process already died,
    /// served once by [`JobStore::next_matching`] as a synthetic event.
    replay_exit: Option<i32>,
}

impl JobMeta {
    fn session(&self) -> Option<MakiId> {
        match self.owner {
            JobOwner::Session { session, .. } => Some(session),
            _ => None,
        }
    }

    /// Owning plugin of a session job. Other owners drop their job on exit, so
    /// only these keep a history worth capping.
    fn session_plugin(&self) -> Option<&Arc<str>> {
        match &self.owner {
            JobOwner::Session { plugin, .. } => Some(plugin),
            _ => None,
        }
    }

    fn record_line(&mut self, stdout: bool, line: &str) {
        let cap = self.tail_cap;
        if cap == 0 {
            self.dropped_output = true;
            return;
        }
        let tail = if stdout {
            &mut self.stdout_tail
        } else {
            &mut self.stderr_tail
        };
        let evicting = tail.len() >= cap;
        if evicting {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
        self.dropped_output |= evicting;
    }

    fn has_pending(&self) -> bool {
        self.replay_exit.is_some() || self.event_rx.as_ref().is_some_and(|rx| !rx.is_empty())
    }
}

/// What a `jobattach` opts table says about one callback slot.
enum CallbackUpdate {
    Keep,
    Clear,
    Set(RegistryKey),
}

impl CallbackUpdate {
    fn apply(self, lua: &Lua, slot: &mut Option<RegistryKey>) {
        let replacement = match self {
            Self::Keep => return,
            Self::Clear => None,
            Self::Set(key) => Some(key),
        };
        if let Some(old) = mem::replace(slot, replacement) {
            lua.remove_registry_value(old).ok();
        }
    }
}

pub(crate) struct CallbackUpdates {
    on_stdout: CallbackUpdate,
    on_stderr: CallbackUpdate,
    on_exit: CallbackUpdate,
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
    /// Id served by the last [`JobStore::next_matching`], so the next scan
    /// starts past it.
    scan_cursor: u32,
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
            scan_cursor: 0,
        }
    }

    pub fn start(&mut self, spec: JobSpec) -> Result<u32, String> {
        let JobSpec {
            owner,
            cmd,
            name,
            cwd,
            env,
            stdout,
            stderr,
            on_stdout,
            on_stderr,
            on_exit,
        } = spec;
        let mut command = cmd.build();
        command
            .stdout(stdout.stdio()?)
            .stderr(stderr.stdio()?)
            .stdin(Stdio::null());

        // Keep std on the posix_spawn fast path so libmalloc's atfork child
        // handler never runs (it prints a MallocStackLogging warning when the
        // parent has MSL enabled; see tontinton/maki#909). pgid == pid, so
        // kill_process_group below is unaffected.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
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
        let stdout_handle = spawn_reader!(child.stdout.take(), "job-stdout", Stdout);
        let stderr_handle = spawn_reader!(child.stderr.take(), "job-stderr", Stderr);

        let reaped = Arc::new(AtomicBool::new(false));
        let wait_reaped = Arc::clone(&reaped);
        thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                // Reaping frees the pid, and that pid is the process group
                // `kill_job` signals. The readers only return once every
                // descendant dropped the pipes, so joining them first keeps a
                // kill target around for as long as the job is really alive.
                if let Some(h) = stdout_handle {
                    let _ = h.join();
                }
                if let Some(h) = stderr_handle {
                    let _ = h.join();
                }
                let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                wait_reaped.store(true, Ordering::Relaxed);
                let _ = event_tx.send(JobEvent::Exit(code));
            })
            .map_err(|e| e.to_string())?;

        self.jobs.insert(
            id,
            JobMeta {
                owner,
                command: cmd.display(),
                name,
                pid,
                started: Instant::now(),
                on_stdout,
                on_stderr,
                on_exit,
                event_rx: Some(event_rx),
                stdout_tail: VecDeque::new(),
                stderr_tail: VecDeque::new(),
                tail_cap: DEFAULT_TAIL,
                dropped_output: !matches!(
                    (&stdout, &stderr),
                    (Redirect::Capture, Redirect::Capture)
                ),
                reaped,
                exit_code: None,
                elapsed_secs: None,
                replay_exit: None,
            },
        );

        Ok(id)
    }

    fn set_tail(&mut self, id: u32, tail: Option<usize>) {
        if let Some(job) = self.jobs.get_mut(&id)
            && let Some(cap) = tail
        {
            job.tail_cap = cap.min(MAX_TAIL_LINES);
        }
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

    /// Pop one queued event. Deliberately one at a time: a batch pulled into
    /// a caller-owned buffer is lost if that caller is dropped mid-delivery,
    /// and with it the tail and exit code the event carries.
    pub fn next_event(&mut self, owner: &JobOwner) -> Option<(u32, JobEvent)> {
        self.next_matching(|job| job.owner == *owner)
    }

    /// [`Self::next_event`] for the jobs the host pumps: plugin-owned ones and
    /// session-owned ones, which outlive the task that started them.
    pub fn next_plugin_event(&mut self) -> Option<(u32, JobEvent)> {
        self.next_matching(|job| {
            matches!(job.owner, JobOwner::Plugin(_) | JobOwner::Session { .. })
        })
    }

    /// Round-robins over the jobs holding events: taking the first match every
    /// time would let a job printing faster than we deliver starve the rest.
    fn next_matching(&mut self, pred: impl Fn(&JobMeta) -> bool) -> Option<(u32, JobEvent)> {
        let mut past_cursor = None;
        let mut lowest = None;
        for (&id, job) in &self.jobs {
            if !pred(job) || !job.has_pending() {
                continue;
            }
            if id > self.scan_cursor {
                past_cursor = Some(past_cursor.map_or(id, |seen: u32| seen.min(id)));
            }
            lowest = Some(lowest.map_or(id, |seen: u32| seen.min(id)));
        }
        let id = past_cursor.or(lowest)?;
        self.scan_cursor = id;
        let job = self.jobs.get_mut(&id)?;
        if let Some(code) = job.replay_exit.take() {
            return Some((id, JobEvent::Exit(code)));
        }
        Some((id, job.event_rx.as_ref()?.try_recv().ok()?))
    }

    pub fn record_event(&mut self, job_id: u32, event: &JobEvent) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        match event {
            JobEvent::Stdout(line) => job.record_line(true, line),
            JobEvent::Stderr(line) => job.record_line(false, line),
            // Termination is [`Self::complete`]'s business: it owns
            // `exit_code` and runs once per job.
            JobEvent::Exit(_) => {}
        }
    }

    /// Session-owned jobs stay inspectable after exit. Task/plugin jobs are
    /// removed the way they always were.
    pub fn complete(&mut self, lua: &Lua, job_id: u32, code: i32) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        // A replayed exit reaches here a second time: the job is already
        // booked, only the callbacks the replay just used need releasing.
        if job.exit_code.is_some() {
            drop_callbacks(lua, job);
            return;
        }
        job.exit_code = Some(code);
        job.elapsed_secs = Some(job.started.elapsed().as_secs());
        let session_plugin = job.session_plugin().cloned();
        drop_callbacks(lua, job);
        match session_plugin {
            Some(plugin) => self.evict_completed(lua, &plugin),
            None => self.finish(lua, job_id),
        }
    }

    /// Cap the exited session jobs {plugin} keeps, dropping the oldest ids
    /// first, so a chatty plugin only ever evicts its own history. Scanning
    /// `jobs` beats keeping a second list in sync with it.
    fn evict_completed(&mut self, lua: &Lua, plugin: &str) {
        let mut exited: Vec<u32> = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.exit_code.is_some()
                    && job
                        .session_plugin()
                        .is_some_and(|owner| owner.as_ref() == plugin)
            })
            .map(|(&id, _)| id)
            .collect();
        if exited.len() <= MAX_COMPLETED_SESSION_JOBS {
            return;
        }
        exited.sort_unstable();
        for oldest in &exited[..exited.len() - MAX_COMPLETED_SESSION_JOBS] {
            self.remove(lua, *oldest, false);
        }
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

    /// Re-arm callbacks on an existing job, the way a reloaded plugin picks up
    /// a session job it started before. An `on_exit` attached to an already
    /// exited job is owed a replay, queued rather than fired inline so the
    /// attaching plugin finishes its own setup first.
    pub fn attach(
        &mut self,
        lua: &Lua,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
        updates: CallbackUpdates,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return false;
        };
        if !job.can_access(task_id, plugin) {
            return false;
        }
        if matches!(updates.on_exit, CallbackUpdate::Set(_)) {
            job.replay_exit = job.exit_code;
        }
        updates.on_stdout.apply(lua, &mut job.on_stdout);
        updates.on_stderr.apply(lua, &mut job.on_stderr);
        updates.on_exit.apply(lua, &mut job.on_exit);
        true
    }

    pub fn snapshot(&self, job_id: u32, task_id: Option<u64>, plugin: &str) -> Option<JobSnapshot> {
        let job = self.jobs.get(&job_id)?;
        job.can_access(task_id, plugin)
            .then(|| JobSnapshot::from_job(job_id, job, true))
    }

    /// Id of the live job this plugin can see holding `name`. An exited job
    /// keeps its name on the row but stops answering here, so a plugin that
    /// adopts by name restarts a job that died instead of taking over its id.
    pub fn find_named(&self, name: &str, task_id: Option<u64>, plugin: &str) -> Option<u32> {
        self.jobs
            .iter()
            .find(|(_, job)| {
                job.exit_code.is_none()
                    && job.name.as_deref() == Some(name)
                    && job.can_access(task_id, plugin)
            })
            .map(|(&id, _)| id)
    }

    /// List jobs this plugin can see. Task and plugin jobs leave the map on
    /// exit; session-owned jobs stay so exited ids stay findable. Tails live
    /// on `snapshot` / `jobinfo`.
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
            .map(|(&id, job)| JobSnapshot::from_job(id, job, false))
            .collect()
    }

    /// Drop an exited job this plugin can see. Running jobs are left alone.
    pub fn forget(&mut self, lua: &Lua, job_id: u32, task_id: Option<u64>, plugin: &str) {
        let Some(job) = self.jobs.get(&job_id) else {
            return;
        };
        if !job.can_access(task_id, plugin) || job.exit_code.is_none() {
            return;
        }
        self.remove(lua, job_id, false);
    }

    pub fn kill(&mut self, job_id: u32, task_id: Option<u64>, plugin: &str) {
        if let Some(job) = self.jobs.get(&job_id)
            && job.can_access(task_id, plugin)
        {
            kill_job(job);
        }
    }

    pub fn kill_owner(&mut self, lua: &Lua, owner: &JobOwner) -> Vec<u32> {
        let ids = self
            .jobs
            .iter()
            .filter_map(|(&id, job)| (job.owner == *owner).then_some(id))
            .collect::<Vec<_>>();
        for id in &ids {
            self.remove(lua, *id, true);
        }
        ids
    }

    pub fn finish(&mut self, lua: &Lua, job_id: u32) {
        self.remove(lua, job_id, false);
    }

    fn remove(&mut self, lua: &Lua, job_id: u32, kill: bool) {
        if let Some(job) = self.jobs.remove(&job_id) {
            if kill {
                kill_job(&job);
            }
            for key in [job.on_stdout, job.on_stderr, job.on_exit]
                .into_iter()
                .flatten()
            {
                lua.remove_registry_value(key).ok();
            }
        }
    }

    fn kill_all(&self) {
        for job in self.jobs.values() {
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
    pub name: Option<String>,
    pub session: Option<MakiId>,
    pub pid: u32,
    pub elapsed_secs: u64,
    pub exit_code: Option<i32>,
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
    /// Some output never reached the tails, so what they hold is a window
    /// onto a longer stream.
    pub dropped_output: bool,
}

impl JobSnapshot {
    fn from_job(id: u32, job: &JobMeta, tails: bool) -> Self {
        Self {
            id,
            command: job.command.clone(),
            name: job.name.clone(),
            session: job.session(),
            pid: job.pid,
            elapsed_secs: job
                .elapsed_secs
                .unwrap_or_else(|| job.started.elapsed().as_secs()),
            exit_code: job.exit_code,
            stdout_lines: if tails {
                job.stdout_tail.iter().cloned().collect()
            } else {
                Vec::new()
            },
            stderr_lines: if tails {
                job.stderr_tail.iter().cloned().collect()
            } else {
                Vec::new()
            },
            dropped_output: job.dropped_output,
        }
    }
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
        maki_agent::child_env::strip_inherited_malloc_stack_logging(&mut c);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(cmd);
        c
    }
}

/// Signalling a reaped pid would hit whoever the kernel handed it to next, so
/// skip the jobs the wait thread already reaped. Until then the child is a
/// zombie, and a zombie group leader keeps its pid and pgid off the free list,
/// so the group is still the right target. The flag carries no data of its
/// own, hence `Relaxed`.
fn kill_job(job: &JobMeta) {
    if job.reaped.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        if let Ok(raw) = i32::try_from(job.pid)
            && let Some(pid) = Pid::from_raw(raw)
        {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &job.pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Run a command in the background. A string runs through `bash -c` on Unix
/// or `cmd /C` on Windows; a table is spawned as argv, with no shell in
/// between (nothing in it can be read as a redirect, a pipe, or `$(...)`).
/// You get back a job id that you can pass to `jobstop` or `jobwait` to
/// control the process.
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
///   `on_stdout` (function?) called with `(job_id, line)` for each stdout line.
///   `on_stderr` (function?) called with `(job_id, line)` for each stderr line.
///   `on_exit` (function?) called with `(job_id, code)` when the process finishes.
///   `stdout` (string|false?) append stdout to this path, or `false` to
///     discard it.
///   `stderr` (string|false?) same for stderr; both may name one path.
///   `scope` (string|table?) job lifetime. `"task"` (default) ends the job
///     with the current call. `"plugin"` keeps it alive until the plugin
///     unloads or reloads. `{ session = "<id>" }` keeps it alive until that
///     session ends, and survives plugin reload.
///   `tail` (integer?) trailing lines per stream kept for `jobinfo`
///     (default 20, 0 disables, max 1024).
///   `name` (string?) handle for `jobfind`, unique among the live jobs this
///     plugin can see. Starting a second job under a live name is an error.
/// @return (integer) Job id.
/// @example
/// local id = maki.fn.jobstart({ "rg", "--json", pattern, dir }, {
///   on_stdout = function(_, line) print(line) end,
///   on_exit = function(_, code) print("exit: " .. code) end,
/// })
#[lua_fn(guard = Run)]
fn jobstart(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] fs_write: bool,
    cmd: Value,
    opts: Option<Table>,
) -> LuaResult<u32> {
    let scope = opts
        .as_ref()
        .map(|opts| opts.get::<Value>("scope"))
        .transpose()?
        .unwrap_or(Value::Nil);
    let mut spec = JobSpec::new(parse_scope(lua, &plugin, scope)?, parse_command(cmd)?);
    let mut tail = None;

    if let Some(ref opts) = opts {
        spec.cwd = opts.get("cwd").ok();
        spec.env = opts
            .get::<Table>("env")
            .ok()
            .map(|t| t.pairs::<String, String>().filter_map(Result::ok).collect());
        spec.name = job_name(opts)?;
        spec.on_stdout = callback_key(lua, opts, "on_stdout")?;
        spec.on_stderr = callback_key(lua, opts, "on_stderr")?;
        spec.on_exit = callback_key(lua, opts, "on_exit")?;
        spec.stdout = parse_redirect(opts, "stdout", spec.on_stdout.is_some(), fs_write)?;
        spec.stderr = parse_redirect(opts, "stderr", spec.on_stderr.is_some(), fs_write)?;
        tail = opts.get("tail").ok();
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
            return Err(format!(
                "jobstart: name {name:?} is already held by live job {held}"
            ));
        }
        let id = store.start(spec)?;
        store.set_tail(id, tail);
        Ok::<u32, String>(id)
    })
    .map_err(mlua::Error::runtime)
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
    opts: &Table,
    key: &str,
    has_callback: bool,
    fs_write: bool,
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
            if !fs_write {
                return Err(denied_error(Permission::FsWrite));
            }
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
    let name = opts.get::<Option<String>>("name")?;
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
/// table with `stdout`, `stderr`, `exit_code`, and `truncated`. A job that
/// already exited answers from its captured tail, so `truncated` says
/// whether that tail ever lost a line (`tail` too small or 0, or the stream
/// redirected away). Waiting on a live job collects every line and is never
/// truncated. Returns `nil` if the job does not finish before the timeout.
///
/// While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
/// callbacks fire as events arrive (like Neovim), so you can stream
/// output into a buffer while parked here. An already-exited
/// session-owned job answers from its snapshot and fires no callbacks.
/// Task and plugin jobs leave the store on exit, so waiting after that
/// is an error.
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
        return wait_result(
            &lua,
            &snap.stdout_lines,
            &snap.stderr_lines,
            code,
            snap.dropped_output,
        );
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
        // A failing callback must not abort the wait: the event is already
        // recorded and the exit still needs collecting. Same policy as the
        // detached dispatch pump.
        if let Err(e) = deliver_job_event(&lua, job_id, &event).await {
            tracing::warn!(job_id, error = %strip_traceback(&e), "jobwait callback failed");
        }
        match event {
            JobEvent::Stdout(line) => stdout_lines.push(line),
            JobEvent::Stderr(line) => stderr_lines.push(line),
            JobEvent::Exit(code) => break code,
        }
    };

    wait_result(&lua, &stdout_lines, &stderr_lines, exit_code, false)
}

/// The single shape `jobwait` answers with, so its two paths cannot drift
/// apart from the documented keys.
fn wait_result(
    lua: &Lua,
    stdout: &[String],
    stderr: &[String],
    exit_code: i32,
    truncated: bool,
) -> LuaResult<Value> {
    let result = lua.create_table()?;
    result.set("stdout", stdout.join("\n"))?;
    result.set("stderr", stderr.join("\n"))?;
    result.set("exit_code", exit_code)?;
    result.set("truncated", truncated)?;
    Ok(Value::Table(result))
}

/// Fire the job's Lua callback for {event} (if any) and mark the job
/// dead on exit. Shared by `jobwait` and the async dispatch loop so
/// both deliver events identically.
///
/// The callback runs in a fresh coroutine so it may suspend (the
/// `maki.fs.*` helpers park on `smol::unblock`); resumed inline from a
/// poll loop it would die with "attempt to yield across metamethod /
/// C-call boundary" on its first suspension.
pub(crate) async fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
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
        lua.create_thread(callback)?
            .into_async::<()>((job_id, arg))?
            .await?;
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
    ///   on_exit = function(_, code) print("done: " .. code) end,
    /// })
    /// ```
    "maki.fn" => pub(crate) fn create_fn_table(
        plugin: Arc<str>,
        perms: &PluginPermissions,
        fs_write: bool,
        tx: Option<flume::Sender<UiAction>>,
    ), DOCS [
        jobstart(perms, plugin, fs_write), jobstop(perms, plugin), jobforget(perms, plugin),
        jobwait(perms, plugin), jobinfo(perms, plugin), joblist(perms, plugin),
        jobattach(perms, plugin), jobfind(perms, plugin),
        executable(perms),
        winsaveview(tx), winrestview(tx),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, WinView};

    const TEST_PLUGIN: &str = "test-plugin";
    const JOB_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
    const JOB_POLL_INTERVAL: Duration = Duration::from_millis(10);
    #[cfg(unix)]
    const EXIT_WITHOUT_REAP: &str =
        "an exit event must mean the child was reaped, or a later kill can signal a recycled pid";
    const NEVER_EXITED: &str = "job never reported its exit";

    /// Pull events until {id} reports its exit, so assertions run against a
    /// job that is certainly done.
    fn collect_until_exit(
        id: u32,
        mut next: impl FnMut() -> Option<(u32, JobEvent)>,
    ) -> Vec<(u32, JobEvent)> {
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        let mut events = Vec::new();
        loop {
            while let Some(event) = next() {
                events.push(event);
            }
            if events
                .iter()
                .any(|(job_id, event)| *job_id == id && matches!(event, JobEvent::Exit(_)))
            {
                return events;
            }
            assert!(Instant::now() < deadline, "{NEVER_EXITED}");
            thread::sleep(JOB_POLL_INTERVAL);
        }
    }

    fn make_store() -> JobStore {
        JobStore::new()
    }

    fn task_owner(id: u64) -> JobOwner {
        JobOwner::Task(id)
    }

    fn plugin_owner() -> JobOwner {
        JobOwner::Plugin(Arc::from(TEST_PLUGIN))
    }

    fn session_owner(session: MakiId) -> JobOwner {
        JobOwner::Session {
            session,
            plugin: Arc::from(TEST_PLUGIN),
        }
    }

    fn stub_job(
        owner: JobOwner,
        on_stdout: Option<RegistryKey>,
        on_exit: Option<RegistryKey>,
    ) -> JobMeta {
        JobMeta {
            owner,
            command: String::new(),
            name: None,
            pid: 0,
            started: Instant::now(),
            on_stdout,
            on_stderr: None,
            on_exit,
            event_rx: None,
            stdout_tail: VecDeque::new(),
            stderr_tail: VecDeque::new(),
            tail_cap: DEFAULT_TAIL,
            dropped_output: false,
            reaped: Arc::new(AtomicBool::new(false)),
            exit_code: None,
            elapsed_secs: None,
            replay_exit: None,
        }
    }

    fn start_echo(store: &mut JobStore) -> u32 {
        store
            .start(JobSpec::new(task_owner(1), "echo hello"))
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
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        while Instant::now() < deadline {
            if !group_alive(pid) {
                return true;
            }
            thread::sleep(JOB_POLL_INTERVAL);
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_store_kills_its_jobs() {
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(task_owner(1), "sleep 30"))
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
        let result = store.start(JobSpec {
            cwd: Some("/nonexistent_dir_abc_xyz_123".into()),
            ..JobSpec::new(task_owner(1), "echo hello")
        });
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
            .start(JobSpec::new(plugin_owner(), "echo hello"))
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
            .start(JobSpec::new(task_owner(1), "sleep 30"))
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
        let task_id = store.start(JobSpec::new(task.clone(), "sleep 30")).unwrap();
        let plugin_id = store
            .start(JobSpec::new(plugin.clone(), "sleep 30"))
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
    fn next_event_filters_by_owner() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let plugin_id = store
            .start(JobSpec::new(plugin_owner(), "echo plugin"))
            .unwrap();

        let task_events = collect_until_exit(id, || store.next_event(&task_owner(1)));
        assert!(task_events.iter().all(|(job_id, _)| *job_id != plugin_id));

        let plugin_events = collect_until_exit(plugin_id, || store.next_plugin_event());
        assert!(plugin_events.iter().all(|(job_id, _)| *job_id != id));
    }

    #[test]
    fn next_event_is_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _rx = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        assert!(
            store.next_event(&task_owner(1)).is_none(),
            "a checked out receiver yields no events to the pump"
        );
    }

    #[test]
    fn next_event_round_robins_so_a_chatty_job_cannot_starve_its_sibling() {
        const QUEUED_PER_JOB: usize = 2;
        let mut store = make_store();
        for id in [1, 2] {
            let (tx, rx) = flume::unbounded();
            for _ in 0..QUEUED_PER_JOB {
                tx.send(JobEvent::Stdout("spam".into())).unwrap();
            }
            let mut job = stub_job(task_owner(1), None, None);
            job.event_rx = Some(rx);
            store.jobs.insert(id, job);
        }

        let served: Vec<u32> = std::iter::from_fn(|| store.next_event(&task_owner(1)))
            .map(|(id, _)| id)
            .collect();

        assert_eq!(served, [1, 2, 1, 2]);
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

        assert!(smol::block_on(deliver_job_event(&lua, 1, &JobEvent::Exit(0))).is_err());
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
        store.set_tail(id, Some(2));
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
            .start(JobSpec::new(plugin_owner(), "echo plugin"))
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
        let owner = JobOwner::Session {
            session,
            plugin: Arc::from(TEST_PLUGIN),
        };
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(owner.clone(), "echo hi; echo err >&2; exit 3"))
            .unwrap();
        store.set_tail(id, Some(5));

        store.detach_plugin_callbacks(&lua, TEST_PLUGIN);
        assert!(store.jobs.contains_key(&id), "detach must keep the job");

        for (_, event) in collect_until_exit(id, || store.next_plugin_event()) {
            store.record_event(id, &event);
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
            .start(JobSpec::new(session_owner(a), "sleep 30"))
            .unwrap();
        let second = store
            .start(JobSpec::new(session_owner(b), "sleep 30"))
            .unwrap();
        store.kill_session(&lua, a);
        assert!(store.snapshot(first, None, TEST_PLUGIN).is_none());
        assert!(store.snapshot(second, None, TEST_PLUGIN).is_some());
        store.kill_session(&lua, b);
    }

    #[test]
    fn snapshot_elapsed_freezes_at_recorded_exit() {
        const PAST_SECS: u64 = 45;
        let lua = Lua::new();
        let mut store = make_store();
        let mut job = stub_job(session_owner(MakiId::generate()), None, None);
        job.started = Instant::now()
            .checked_sub(Duration::from_secs(PAST_SECS))
            .expect("clock has enough history");
        store.jobs.insert(1, job);

        store.complete(&lua, 1, 0);
        let at_exit = store.snapshot(1, None, TEST_PLUGIN).unwrap().elapsed_secs;
        assert!(
            at_exit >= PAST_SECS,
            "elapsed at exit should reflect the backdated start, got {at_exit}"
        );

        store.jobs.get_mut(&1).unwrap().started = Instant::now();
        let later = store.snapshot(1, None, TEST_PLUGIN).unwrap().elapsed_secs;
        assert_eq!(later, at_exit, "elapsed must freeze once the job exits");
    }

    fn keep_all() -> CallbackUpdates {
        CallbackUpdates {
            on_stdout: CallbackUpdate::Keep,
            on_stderr: CallbackUpdate::Keep,
            on_exit: CallbackUpdate::Keep,
        }
    }

    fn exit_updates(lua: &Lua) -> CallbackUpdates {
        CallbackUpdates {
            on_exit: CallbackUpdate::Set(noop_key(lua)),
            ..keep_all()
        }
    }

    fn noop_key(lua: &Lua) -> RegistryKey {
        let noop = lua.create_function(|_, ()| Ok(())).unwrap();
        lua.create_registry_value(noop).unwrap()
    }

    /// Which callbacks the stub job (always id 1 here) still holds, as
    /// (stdout, stderr, exit).
    fn armed_slots(store: &JobStore) -> (bool, bool, bool) {
        let held = |event| store.callback_key(1, &event).is_some();
        (
            held(JobEvent::Stdout(String::new())),
            held(JobEvent::Stderr(String::new())),
            held(JobEvent::Exit(0)),
        )
    }

    fn exited_session_job(lua: &Lua, store: &mut JobStore, code: i32) {
        store
            .jobs
            .insert(1, stub_job(session_owner(MakiId::generate()), None, None));
        store.complete(lua, 1, code);
    }

    #[test]
    fn an_argv_row_reads_back_as_the_same_argv() {
        const ARGV: [&str; 2] = ["echo", "a; echo pwned"];
        let row = JobCommand::Argv(ARGV.map(String::from).to_vec()).display();
        assert_eq!(
            shell_words::split(&row).unwrap(),
            ARGV,
            "the row a user reads must quote what the shell would have eaten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn redirected_streams_append_to_the_file_and_keep_no_tail() {
        const EXISTING: &str = "older\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.log");
        std::fs::write(&path, EXISTING).unwrap();
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                stdout: Redirect::File(path.clone()),
                stderr: Redirect::Discard,
                ..JobSpec::new(task_owner(1), "echo hi; echo err >&2")
            })
            .unwrap();

        for (_, event) in collect_until_exit(id, || store.next_event(&task_owner(1))) {
            store.record_event(id, &event);
        }

        let snap = store.snapshot(id, Some(1), TEST_PLUGIN).unwrap();
        assert!(
            snap.stdout_lines.is_empty() && snap.stderr_lines.is_empty(),
            "a redirected stream must not be buffered here"
        );
        assert!(
            snap.dropped_output,
            "an empty tail on a redirected stream must not read as the whole output"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{EXISTING}hi\n")
        );
    }

    #[test]
    fn a_name_is_held_by_the_live_job_only() {
        const NAME: &str = "log-tail";
        let lua = Lua::new();
        let mut store = make_store();
        let mut job = stub_job(session_owner(MakiId::generate()), None, None);
        job.name = Some(NAME.to_string());
        store.jobs.insert(1, job);

        assert_eq!(store.find_named(NAME, None, TEST_PLUGIN), Some(1));
        assert_eq!(store.find_named(NAME, None, "other-plugin"), None);
        assert_eq!(store.find_named("absent", None, TEST_PLUGIN), None);

        store.complete(&lua, 1, 0);

        assert_eq!(
            store.find_named(NAME, None, TEST_PLUGIN),
            None,
            "an exited job must release its name so the next start is not blocked"
        );
        assert_eq!(
            store
                .snapshot(1, None, TEST_PLUGIN)
                .unwrap()
                .name
                .as_deref(),
            Some(NAME),
            "the name stays on the row that explains the exit"
        );
    }

    #[test]
    fn attaching_on_exit_after_the_exit_replays_it_once() {
        const CODE: i32 = 5;
        const REWIND: Duration = Duration::from_secs(60);
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, CODE);
        let at_exit = store.jobs[&1].elapsed_secs;

        assert!(store.attach(&lua, 1, None, TEST_PLUGIN, exit_updates(&lua)));

        let (id, event) = store.next_plugin_event().expect("replayed exit");
        assert_eq!(id, 1);
        assert!(matches!(event, JobEvent::Exit(CODE)));
        assert!(
            store.next_plugin_event().is_none(),
            "the replay must be served once"
        );

        // Rewind the start so a second round of bookkeeping would show up in
        // the elapsed time instead of hiding in the same second.
        store.jobs.get_mut(&1).unwrap().started = Instant::now() - REWIND;
        store.complete(&lua, 1, CODE);
        assert_eq!(
            store.jobs[&1].elapsed_secs, at_exit,
            "a replayed exit must not restate when the job died"
        );
        assert_eq!(
            armed_slots(&store),
            (false, false, false),
            "completing on the replay must release the callback it used"
        );
    }

    #[test]
    fn attach_is_refused_for_jobs_this_plugin_cannot_see() {
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, 0);

        assert!(!store.attach(&lua, 1, None, "other-plugin", exit_updates(&lua)));
        assert!(!store.attach(&lua, 999, None, TEST_PLUGIN, exit_updates(&lua)));
        assert!(
            store.next_plugin_event().is_none(),
            "a refused attach must not queue a replay"
        );
    }

    #[test]
    fn attach_keeps_absent_callbacks_and_clears_on_false() {
        let lua = Lua::new();
        let mut store = make_store();
        store
            .jobs
            .insert(1, stub_job(plugin_owner(), Some(noop_key(&lua)), None));

        store.attach(
            &lua,
            1,
            None,
            TEST_PLUGIN,
            CallbackUpdates {
                on_stderr: CallbackUpdate::Set(noop_key(&lua)),
                ..keep_all()
            },
        );
        assert_eq!(
            armed_slots(&store),
            (true, true, false),
            "an absent key must keep the callback it found"
        );

        store.attach(
            &lua,
            1,
            None,
            TEST_PLUGIN,
            CallbackUpdates {
                on_stdout: CallbackUpdate::Clear,
                ..keep_all()
            },
        );
        assert_eq!(
            armed_slots(&store),
            (false, true, false),
            "false must clear that callback and leave the rest"
        );
    }

    #[test_case::test_case(3, false ; "tail_holds_every_line")]
    #[test_case::test_case(2, true ; "tail_evicted_a_line")]
    #[test_case::test_case(0, true ; "tail_disabled")]
    fn dropped_says_whether_the_tail_is_the_whole_output(cap: usize, expected: bool) {
        let mut store = make_store();
        store.jobs.insert(1, stub_job(task_owner(1), None, None));
        store.set_tail(1, Some(cap));

        for line in ["a", "b", "c"] {
            store.record_event(1, &JobEvent::Stdout(line.into()));
        }

        assert_eq!(
            store
                .snapshot(1, Some(1), TEST_PLUGIN)
                .unwrap()
                .dropped_output,
            expected
        );
    }

    #[test]
    fn completed_history_is_evicted_per_plugin() {
        const QUIET_PLUGIN: &str = "quiet-plugin";
        const QUIET_JOB: u32 = 1;
        const OLDEST_CHATTY_JOB: u32 = 2;
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        let exit_job = |store: &mut JobStore, id: u32, plugin: &str| {
            let owner = JobOwner::Session {
                session,
                plugin: Arc::from(plugin),
            };
            store.jobs.insert(id, stub_job(owner, None, None));
            store.complete(&lua, id, 0);
        };

        exit_job(&mut store, QUIET_JOB, QUIET_PLUGIN);
        for id in OLDEST_CHATTY_JOB..=(MAX_COMPLETED_SESSION_JOBS as u32 + OLDEST_CHATTY_JOB) {
            exit_job(&mut store, id, TEST_PLUGIN);
        }

        assert!(
            store.snapshot(QUIET_JOB, None, QUIET_PLUGIN).is_some(),
            "a chatty plugin must not evict another plugin's history"
        );
        assert!(
            store
                .snapshot(OLDEST_CHATTY_JOB, None, TEST_PLUGIN)
                .is_none(),
            "the chatty plugin evicts its own oldest job first"
        );
        assert_eq!(
            store.list(None, None, TEST_PLUGIN).len(),
            MAX_COMPLETED_SESSION_JOBS,
            "the cap counts only this plugin's exited jobs"
        );
    }

    #[test]
    fn list_omits_tails() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        store.record_event(id, &JobEvent::Stdout("hello".into()));
        let listed = store.list(None, Some(1), TEST_PLUGIN);
        assert!(
            listed
                .iter()
                .any(|s| s.id == id && s.stdout_lines.is_empty() && s.stderr_lines.is_empty()),
            "joblist must not clone tails"
        );
        assert_eq!(
            store
                .snapshot(id, Some(1), TEST_PLUGIN)
                .unwrap()
                .stdout_lines,
            ["hello"]
        );
    }

    #[test]
    fn forget_drops_exited_session_jobs_only() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        store.jobs.insert(
            1,
            stub_job(
                JobOwner::Session {
                    session,
                    plugin: Arc::from(TEST_PLUGIN),
                },
                None,
                None,
            ),
        );

        store.forget(&lua, 1, None, TEST_PLUGIN);
        assert!(
            store.snapshot(1, None, TEST_PLUGIN).is_some(),
            "running job must stay"
        );

        store.complete(&lua, 1, 0);
        assert!(
            store
                .list(Some(session), None, TEST_PLUGIN)
                .iter()
                .any(|s| s.id == 1)
        );

        store.forget(&lua, 1, None, "other-plugin");
        assert!(
            store.snapshot(1, None, TEST_PLUGIN).is_some(),
            "other plugin cannot forget"
        );

        store.forget(&lua, 1, None, TEST_PLUGIN);
        assert!(store.snapshot(1, None, TEST_PLUGIN).is_none());
        assert!(store.list(Some(session), None, TEST_PLUGIN).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn exited_session_job_stays_listed_and_kill_is_a_noop() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(session_owner(session), "sleep 0.1"))
            .unwrap();
        for (_, event) in collect_until_exit(id, || store.next_plugin_event()) {
            store.record_event(id, &event);
        }
        assert!(
            store.jobs[&id].reaped.load(Ordering::Relaxed),
            "{EXIT_WITHOUT_REAP}"
        );
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
