use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::BufReader;
use std::mem;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::thread;
use std::time::Instant;

use arc_swap::ArcSwap;
use maki_config::ShellPreference;
use maki_providers::{shell_command, strip_provider_keys};
use maki_storage::id::MakiId;
use mlua::{Function, Lua, RegistryKey, Result as LuaResult};
use shell_words::join as shell_join;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, watch};

use crate::api::fs::{Line, MAX_READ_BYTES, expand_tilde, next_line};
use crate::plugin_permissions::Ceiling;
use crate::runtime::{Narrowed, with_jobs};

const DEFAULT_TAIL: usize = 20;
pub(crate) const MAX_TAIL_LINES: usize = 1024;
const MAX_COMPLETED_SESSION_JOBS: usize = 256;

const READER_BUF_SIZE: usize = 8 * 1024;
/// Unread output a job may queue while Lua is busy. Past this its readers
/// wait, so the pipe fills and the child blocks on write until callbacks
/// catch up. Far above what a job normally has unread, so only a runaway
/// command ever meets it.
const JOB_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// The run of bytes a reader parked on a full queue waits for, then spends
/// line by line. Waking it for every freed line cost a cross-thread wake per
/// line, about a third of a saturated run's time.
const JOB_QUEUE_RESUME: u32 = JOB_QUEUE_BYTES as u32 / 2;
const STDOUT_STREAM: &str = "stdout";
const STDERR_STREAM: &str = "stderr";

/// `agent.shell`, set once from config at startup. Jobs get built far away
/// from any config, on whatever Lua thread runs the plugin, so they read it here.
static SHELL: LazyLock<ArcSwap<ShellPreference>> = LazyLock::new(ArcSwap::default);

pub fn set_shell_preference(shell: ShellPreference) {
    SHELL.store(Arc::new(shell));
}

#[derive(Clone)]
pub(crate) enum JobEvent {
    Stdout(Line),
    Stderr(Line),
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
            Self::Shell(cmd) => shell_command(cmd, &SHELL.load()),
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
    /// The [`Ceiling`] the job was started under, which its callbacks run
    /// under too.
    pub ceiling: Option<Ceiling>,
    /// Longest line a reader hands on. The rest of a longer line is skipped
    /// without being read into memory.
    pub max_line_bytes: u64,
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
            ceiling: None,
            max_line_bytes: MAX_READ_BYTES,
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
    ceiling: Option<Ceiling>,
    events: JobEvents,
    /// Set while a `jobwait` holds the events. Pumps leave the job alone
    /// until its [`WaitLease`] drops.
    waited: Arc<AtomicBool>,
    _queue: QueueOwner,
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
        self.replay_exit.is_some() || self.pumpable().is_some_and(|events| !events.is_empty())
    }

    fn pumpable(&self) -> Option<&JobEvents> {
        (!self.waited.load(Ordering::Acquire)).then_some(&self.events)
    }
}

/// What a `jobattach` opts table says about one callback slot.
pub(crate) enum CallbackUpdate {
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
    pub on_stdout: CallbackUpdate,
    pub on_stderr: CallbackUpdate,
    pub on_exit: CallbackUpdate,
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
    /// Id served by the last [`JobStore::next_matching`], so the next scan
    /// starts past it.
    scan_cursor: u32,
    /// Bumped by anything a pump could act on, so pumps park instead of
    /// polling. A `watch`, because each pump keeps its own place in it: a bump
    /// landing while a pump is busy delivering still wakes it afterwards. The
    /// store outlives every pump, each one holds the `Lua` that owns it.
    changed: watch::Sender<()>,
    /// Tasks a [`JobReaper`] reported, reaped on the next access.
    ended: Arc<Mutex<Vec<u64>>>,
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
            scan_cursor: 0,
            changed: watch::Sender::new(()),
            ended: Arc::default(),
        }
    }

    pub fn reaper(&self) -> JobReaper {
        JobReaper {
            ended: Arc::clone(&self.ended),
            changed: self.changed.clone(),
        }
    }

    /// Kills and forgets the jobs of every task a [`JobReaper`] reported.
    /// Coming late is fine: only the dead scope's own pump delivered its
    /// `Task` jobs, so none of their callbacks can run in between.
    pub fn reap_ended(&mut self, lua: &Lua) {
        let ended = mem::take(&mut *self.ended.lock().unwrap_or_else(PoisonError::into_inner));
        for task in ended {
            let killed = self.kill_owner(lua, &JobOwner::Task(task));
            if !killed.is_empty() {
                tracing::warn!(
                    task,
                    jobs = ?killed,
                    "scope finished with live jobs; they were killed"
                );
            }
        }
    }

    /// Subscribe before the first scan: a change after this point wakes the
    /// next `changed()`, even if it landed before the scan finished.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
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
            ceiling,
            max_line_bytes,
        } = spec;
        let mut command = cmd.build();
        strip_provider_keys(&mut command)
            .stdout(stdout.stdio()?)
            .stderr(stderr.stdio()?)
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

        // The owner exists before any thread does, so a failed spawn below
        // still closes the queue on a reader that is already waiting on it.
        let (mut sender, events, queue) = job_queue(self.changed.clone());

        macro_rules! spawn_reader {
            ($stream:expr, $name:expr, $variant:ident) => {
                if let Some(stream) = $stream {
                    let mut sender = sender.clone();
                    Some(
                        thread::Builder::new()
                            .name($name.into())
                            .spawn(move || {
                                let mut reader = BufReader::with_capacity(READER_BUF_SIZE, stream);
                                while let Ok(Some(line)) =
                                    next_line(&mut reader, Some(max_line_bytes), true)
                                {
                                    if !sender.send(JobEvent::$variant(line)) {
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
                sender.send(JobEvent::Exit(code));
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
                ceiling,
                events,
                waited: Arc::default(),
                _queue: queue,
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

    pub fn set_tail(&mut self, id: u32, tail: Option<usize>) {
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

    pub fn ceiling(&self, job_id: u32) -> Option<Ceiling> {
        self.jobs.get(&job_id)?.ceiling.clone()
    }

    /// None for a job this caller can't see, or one another wait holds.
    pub fn lease_events(
        &self,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Option<WaitLease> {
        let job = self.jobs.get(&job_id)?;
        if !job.can_access(task_id, plugin) || job.waited.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(WaitLease {
            events: job.events.clone(),
            waited: Arc::clone(&job.waited),
            changed: self.changed.clone(),
        })
    }

    /// Pop one queued event. Deliberately one at a time: a batch pulled into
    /// a caller-owned buffer is lost if that caller is dropped mid-delivery,
    /// and with it the tail and exit code the event carries.
    pub fn next_event(&mut self, owner: &JobOwner) -> Option<(u32, JobEvent)> {
        self.next_matching(|job| job.owner == *owner)
    }

    /// Jobs the host pumps that hold events: plugin-owned ones and
    /// session-owned ones, which outlive the task that started them. The
    /// host delivers each on a task of its own.
    pub fn plugin_jobs_with_events(&self) -> Vec<u32> {
        self.jobs
            .iter()
            .filter(|(_, job)| {
                matches!(job.owner, JobOwner::Plugin(_) | JobOwner::Session { .. })
                    && job.has_pending()
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Pop one queued event of {job_id}, a replayed exit first.
    pub fn next_event_of(&mut self, job_id: u32) -> Option<JobEvent> {
        let job = self.jobs.get_mut(&job_id)?;
        if let Some(code) = job.replay_exit.take() {
            return Some(JobEvent::Exit(code));
        }
        job.pumpable()?.try_recv()
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
        Some((id, self.next_event_of(id)?))
    }

    pub fn record_event(&mut self, job_id: u32, event: &JobEvent) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        match event {
            JobEvent::Stdout(line) => job.record_line(true, &line.text),
            JobEvent::Stderr(line) => job.record_line(false, &line.text),
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
            self.changed.send_replace(());
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
            self.changed.send_replace(());
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

/// A `jobwait`'s exclusive hold on one job's events. The store keeps its own
/// receiver, so there is nothing to give back: dropping the lease only lets
/// the pumps in again.
pub(crate) struct WaitLease {
    events: JobEvents,
    waited: Arc<AtomicBool>,
    changed: watch::Sender<()>,
}

impl WaitLease {
    pub fn events(&self) -> &JobEvents {
        &self.events
    }
}

/// The GC runs this when it frees a coroutine parked in `jobwait`, so it
/// must stay Lua-free. The bump wakes a parked pump for the events the wait
/// left queued.
impl Drop for WaitLease {
    fn drop(&mut self) {
        self.waited.store(false, Ordering::Release);
        self.changed.send_replace(());
    }
}

/// Hands a finished task's jobs to the store. A scope can end inside a GC
/// sweep, where neither the store nor the VM is in reach, so it leaves the
/// task here and [`JobStore::reap_ended`] does the rest.
pub(crate) struct JobReaper {
    ended: Arc<Mutex<Vec<u64>>>,
    changed: watch::Sender<()>,
}

impl JobReaper {
    pub fn task_ended(&self, task_id: u64) {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(task_id);
        self.changed.send_replace(());
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

/// The bytes a job may have queued, as permits. Readers take a line's bytes
/// before sending it and park once [`JOB_QUEUE_BYTES`] are out, until
/// consumers have given [`JOB_QUEUE_RESUME`] back. Dropping the job closes
/// the semaphore, which frees them even while a `jobwait` lease keeps the
/// pump off a job whose waiter is not draining.
struct JobQueue {
    bytes: Arc<Semaphore>,
}

impl Default for JobQueue {
    fn default() -> Self {
        Self {
            bytes: Arc::new(Semaphore::new(JOB_QUEUE_BYTES)),
        }
    }
}

impl JobQueue {
    fn popped(&self, event: &JobEvent) {
        self.bytes.add_permits(permits(event) as usize);
    }
}

/// Capped at one run: a parked reader keeps the run it waited for, so a line
/// costing more than the half left to the other reader could never fit.
fn permits(event: &JobEvent) -> u32 {
    (queued_bytes(event) as u32).min(JOB_QUEUE_RESUME)
}

/// What an event holds in the queue. The fixed part keeps a flood of empty
/// lines from queueing without bound, as `read_line_page` charges per line.
fn queued_bytes(event: &JobEvent) -> usize {
    size_of::<JobEvent>()
        + match event {
            JobEvent::Stdout(line) | JobEvent::Stderr(line) => line.text.len(),
            JobEvent::Exit(_) => 0,
        }
}

/// A job's receiving end. Taking an event through it settles that event's
/// bytes with the [`JobQueue`], so no consumer can forget to and leave the
/// readers waiting on a queue that is long drained.
#[derive(Clone)]
pub(crate) struct JobEvents {
    rx: flume::Receiver<JobEvent>,
    queue: Arc<JobQueue>,
}

impl JobEvents {
    fn try_recv(&self) -> Option<JobEvent> {
        let event = self.rx.try_recv().ok()?;
        self.queue.popped(&event);
        Some(event)
    }

    pub async fn recv_async(&self) -> Option<JobEvent> {
        let event = self.rx.recv_async().await.ok()?;
        self.queue.popped(&event);
        Some(event)
    }

    fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

struct QueueOwner(Arc<JobQueue>);

impl Drop for QueueOwner {
    fn drop(&mut self) {
        self.0.bytes.close();
    }
}

/// The sending, receiving and owning ends of one job's queue.
fn job_queue(changed: watch::Sender<()>) -> (JobEventSender, JobEvents, QueueOwner) {
    let (tx, rx) = flume::unbounded();
    let queue = Arc::new(JobQueue::default());
    let sender = JobEventSender {
        tx,
        queue: Arc::clone(&queue),
        changed,
        reserve: None,
    };
    let events = JobEvents {
        rx,
        queue: Arc::clone(&queue),
    };
    (sender, events, QueueOwner(queue))
}

struct JobEventSender {
    tx: flume::Sender<JobEvent>,
    queue: Arc<JobQueue>,
    changed: watch::Sender<()>,
    /// The run a parked reader waited for, spent line by line. Given back
    /// when the reader ends.
    reserve: Option<OwnedSemaphorePermit>,
}

/// Each reader thread starts without a reserve: permits are per reader.
impl Clone for JobEventSender {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            queue: Arc::clone(&self.queue),
            changed: self.changed.clone(),
            reserve: None,
        }
    }
}

impl JobEventSender {
    /// Returns false once the job is gone, so the reader can stop.
    fn send(&mut self, event: JobEvent) -> bool {
        let cost = permits(&event);
        loop {
            if let Some(spent) = self
                .reserve
                .as_mut()
                .and_then(|run| run.split(cost as usize))
            {
                spent.forget();
                break;
            }
            match self.queue.bytes.try_acquire_many(cost) {
                Ok(taken) => {
                    taken.forget();
                    break;
                }
                // The job is gone, so nothing drains the queue and parking
                // would never end. What is left runs to EOF unbounded; a
                // dropped receiver fails the send below.
                Err(TryAcquireError::Closed) => break,
                Err(TryAcquireError::NoPermits) => {}
            }
            // Parks until a whole run is free and keeps it: one wake per
            // drained run instead of one per line, and nothing handed back
            // for the other stream's reader to race for. The leftover goes
            // back first: kept through the park, it plus the other reader's
            // idle run could leave the pool one line short of a run for good.
            self.reserve = None;
            if let Ok(run) =
                Arc::clone(&self.queue.bytes).blocking_acquire_many_owned(JOB_QUEUE_RESUME)
            {
                self.reserve = Some(run);
            }
        }
        self.tx
            .send(event)
            .inspect(|()| self.changed.send_replace(()))
            .is_ok()
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

/// Fire the job's Lua callback for {event} (if any) and mark the job
/// dead on exit. Shared by `jobwait` and the async dispatch loop so
/// both deliver events identically.
///
/// The callback runs through `call_async`, in a coroutine of its own, so it
/// may suspend (the `maki.fs.*` helpers park on `spawn_blocking`). Resumed
/// inline from a poll loop, it would die with "attempt to yield across
/// metamethod / C-call boundary" on its first suspension.
pub(crate) async fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let (callback, ceiling) = with_jobs(lua, |store| {
        store.record_event(job_id, event);
        let callback = store
            .callback_key(job_id, event)
            .and_then(|key| lua.registry_value::<Function>(key).ok());
        (callback, store.ceiling(job_id))
    });
    if let JobEvent::Exit(code) = event {
        with_jobs(lua, |store| store.complete(lua, job_id, *code));
    }
    if let Some(callback) = callback {
        let call = match event {
            JobEvent::Stdout(line) | JobEvent::Stderr(line) => {
                // The stream name sits third, where Neovim passes it.
                let stream = if matches!(event, JobEvent::Stdout(_)) {
                    STDOUT_STREAM
                } else {
                    STDERR_STREAM
                };
                let text = lua.create_string(&line.text)?;
                callback.call_async::<()>((job_id, text, stream, line.bytes))
            }
            JobEvent::Exit(code) => callback.call_async::<()>((job_id, *code)),
        };
        Narrowed::under(lua, ceiling, call).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::api::fs::LINE_CUT_MARKER;
    use futures::FutureExt;
    use std::time::Duration;

    const TEST_PLUGIN: &str = "test-plugin";
    const JOB_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
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

    fn next_plugin_event(store: &mut JobStore) -> Option<(u32, JobEvent)> {
        let id = *store.plugin_jobs_with_events().iter().min()?;
        store.next_event_of(id).map(|event| (id, event))
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
        let (_, events, queue) = job_queue(watch::Sender::new(()));
        JobMeta {
            owner,
            command: String::new(),
            name: None,
            pid: 0,
            started: Instant::now(),
            on_stdout,
            on_stderr: None,
            on_exit,
            ceiling: None,
            events,
            waited: Arc::default(),
            _queue: queue,
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

    /// Without a cap of its own a job still stops at the `maki.fs` read
    /// limit, so one endless line cannot fill the shared heap.
    #[cfg(unix)]
    #[test]
    fn reader_cuts_at_the_read_limit_by_default() {
        let over = MAX_READ_BYTES + 1;
        let mut store = make_store();
        let cmd = format!("head -c {over} /dev/zero | tr '\\0' x");
        let id = store
            .start(JobSpec::new(task_owner(1), cmd.as_str()))
            .unwrap();

        let events = collect_until_exit(id, || store.next_event(&task_owner(1)));

        let [(_, JobEvent::Stdout(line)), (_, JobEvent::Exit(0))] = events.as_slice() else {
            panic!("expected one line and the exit");
        };
        assert_eq!(
            line.text.len() as u64,
            MAX_READ_BYTES + LINE_CUT_MARKER.len() as u64
        );
        assert_eq!(line.bytes, over);
    }

    #[test]
    fn finishing_a_job_removes_it() {
        let lua = Lua::new();
        let mut store = make_store();
        let owner = task_owner(1);
        assert!(store.is_empty(&owner));

        let id = start_echo(&mut store);
        assert!(!store.is_empty(&owner));
        let lease = store.lease_events(id, Some(1), TEST_PLUGIN).unwrap();
        while !matches!(
            lease
                .events
                .rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            JobEvent::Exit(_)
        ) {}

        store.finish(&lua, id);
        assert!(store.is_empty(&owner));
    }

    #[test]
    fn unknown_job_operations_are_noops() {
        let mut store = make_store();
        store.kill(999, Some(1), TEST_PLUGIN);
        assert!(store.lease_events(999, Some(1), TEST_PLUGIN).is_none());
        assert!(store.callback_key(999, &JobEvent::Exit(0)).is_none());
    }

    #[test]
    fn lease_lifecycle() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        assert!(
            store.lease_events(id, Some(2), TEST_PLUGIN).is_none(),
            "another task must not access the job"
        );
        let lease = store.lease_events(id, Some(1), TEST_PLUGIN);
        assert!(lease.is_some());
        assert!(
            store.lease_events(id, Some(1), TEST_PLUGIN).is_none(),
            "a second wait must not share the lease"
        );
        drop(lease);
        assert!(
            store.lease_events(id, Some(1), TEST_PLUGIN).is_some(),
            "dropping the lease frees the job"
        );
    }

    #[test]
    fn plugin_owner_can_be_accessed_only_by_its_plugin() {
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(plugin_owner(), "echo hello"))
            .unwrap();

        assert!(store.lease_events(id, Some(1), "other-plugin").is_none());
        assert!(store.lease_events(id, None, TEST_PLUGIN).is_some());
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
    fn lease_delivers_events() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let lease = store.lease_events(id, Some(1), TEST_PLUGIN).unwrap();

        let mut got_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match lease.events.rx.recv_timeout(Duration::from_millis(200)) {
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

        let plugin_events = collect_until_exit(plugin_id, || next_plugin_event(&mut store));
        assert!(plugin_events.iter().all(|(job_id, _)| *job_id != id));
    }

    #[test]
    fn next_event_is_empty_while_leased() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _lease = store.lease_events(id, Some(1), TEST_PLUGIN).unwrap();

        assert!(
            store.next_event(&task_owner(1)).is_none(),
            "a leased job yields no events to the pump"
        );
    }

    #[test]
    fn next_event_round_robins_so_a_chatty_job_cannot_starve_its_sibling() {
        const QUEUED_PER_JOB: usize = 2;
        let mut store = make_store();
        for id in [1, 2] {
            let (mut sender, events, queue) = job_queue(store.changed.clone());
            for _ in 0..QUEUED_PER_JOB {
                sender.send(JobEvent::Stdout("spam".into()));
            }
            let mut job = stub_job(task_owner(1), None, None);
            job.events = events;
            job._queue = queue;
            store.jobs.insert(id, job);
        }

        let served: Vec<u32> = std::iter::from_fn(|| store.next_event(&task_owner(1)))
            .map(|(id, _)| id)
            .collect();

        assert_eq!(served, [1, 2, 1, 2]);
    }

    #[tokio::test]
    async fn exit_cleanup_runs_before_a_failing_callback() {
        let lua = Lua::new();
        let callback = lua
            .create_function(|_, ()| Err::<(), _>(mlua::Error::runtime("callback failed")))
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        with_jobs(&lua, |store| {
            store
                .jobs
                .insert(1, stub_job(task_owner(1), None, Some(callback_key)));
        });

        assert!(
            deliver_job_event(&lua, 1, &JobEvent::Exit(0))
                .await
                .is_err()
        );
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

        for (_, event) in collect_until_exit(id, || next_plugin_event(&mut store)) {
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
            held(JobEvent::Stdout("".into())),
            held(JobEvent::Stderr("".into())),
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

        let (id, event) = next_plugin_event(&mut store).expect("replayed exit");
        assert_eq!(id, 1);
        assert!(matches!(event, JobEvent::Exit(CODE)));
        assert!(
            next_plugin_event(&mut store).is_none(),
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
    fn every_change_a_pump_can_act_on_wakes_it() {
        let lua = Lua::new();
        let mut store = make_store();
        let (mut sender, events, queue) = job_queue(store.changed.clone());
        let mut job = stub_job(plugin_owner(), None, None);
        job.events = events;
        job._queue = queue;
        job.exit_code = Some(0);
        store.jobs.insert(1, job);
        let mut changed = store.subscribe();
        let mut woke = || changed.changed().now_or_never().is_some();
        assert!(!woke(), "subscribing is not a change");

        sender.send(JobEvent::Stdout("line".into()));
        assert!(woke(), "a queued event");

        let lease = store.lease_events(1, None, TEST_PLUGIN).unwrap();
        assert!(!woke(), "taking the lease is not a change");
        assert!(
            store.plugin_jobs_with_events().is_empty(),
            "a leased job hides its queued event from the pump"
        );
        drop(lease);
        assert!(woke(), "a released lease may leave events queued");
        assert_eq!(
            store.plugin_jobs_with_events(),
            [1],
            "the queued event shows again once the lease drops"
        );

        assert!(store.attach(&lua, 1, None, TEST_PLUGIN, exit_updates(&lua)));
        assert!(woke(), "an exit replayed to a new on_exit");

        store.finish(&lua, 1);
        assert!(woke(), "a job leaving is what ends a dispatch");
    }

    #[test]
    fn attach_is_refused_for_jobs_this_plugin_cannot_see() {
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, 0);

        assert!(!store.attach(&lua, 1, None, "other-plugin", exit_updates(&lua)));
        assert!(!store.attach(&lua, 999, None, TEST_PLUGIN, exit_updates(&lua)));
        assert!(
            next_plugin_event(&mut store).is_none(),
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
        for (_, event) in collect_until_exit(id, || next_plugin_event(&mut store)) {
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

    const PARKED_LINE_BYTES: usize = 4096;

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            thread::sleep(JOB_POLL_INTERVAL);
        }
    }

    fn next_within(events: &JobEvents) -> Option<JobEvent> {
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        loop {
            if let Some(event) = events.try_recv() {
                return Some(event);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(JOB_POLL_INTERVAL);
        }
    }

    /// A parked reader keeps the run it waited for, so a line may never cost
    /// more than the run left to the other reader. Capped, a line longer than
    /// the whole queue still gets through past a reader holding a run idle.
    #[test]
    fn a_long_line_fits_past_an_idle_reserve() {
        let store = make_store();
        let (mut small, events, _queue) = job_queue(store.changed.clone());
        let line = || JobEvent::Stdout("0".repeat(PARKED_LINE_BYTES).as_str().into());
        let lines = JOB_QUEUE_BYTES / permits(&line()) as usize;
        for _ in 0..lines {
            assert!(small.send(line()));
        }
        let mut long = small.clone();
        let small = thread::spawn(move || (small.send(line()), small));
        // A parked reader's owned acquire holds a clone of the semaphore.
        wait_until("the small reader parked", || {
            Arc::strong_count(&events.queue.bytes) == 2
        });
        for _ in 0..=lines {
            assert!(matches!(next_within(&events), Some(JobEvent::Stdout(_))));
        }
        let (sent, _idle_reserve) = small.join().unwrap();
        assert!(sent);

        let long = thread::spawn(move || {
            long.send(JobEvent::Stderr(
                "0".repeat(JOB_QUEUE_BYTES).as_str().into(),
            ))
        });
        assert!(matches!(next_within(&events), Some(JobEvent::Stderr(_))));
        assert!(long.join().unwrap());
    }

    /// The other reader holds a run idle after one short line. A flooding
    /// reader that parks again must not count its leftover against the run
    /// it waits for, or the pool stays a few bytes short of one for good.
    #[test]
    fn a_parked_reader_drops_its_leftover() {
        let store = make_store();
        let (mut fill, events, _queue) = job_queue(store.changed.clone());
        let line = || JobEvent::Stdout("0".repeat(PARKED_LINE_BYTES).as_str().into());
        let lines = JOB_QUEUE_BYTES / permits(&line()) as usize;
        for _ in 0..lines {
            assert!(fill.send(line()));
        }
        // A line sized to the spare, so even a short one parks.
        let spare = events.queue.bytes.available_permits();
        let overhead = permits(&JobEvent::Stdout("".into())) as usize;
        assert!(fill.send(JobEvent::Stdout(
            "0".repeat(spare - overhead).as_str().into()
        )));
        assert_eq!(events.queue.bytes.available_permits(), 0);
        let mut short = fill.clone();
        let short = thread::spawn(move || (short.send(JobEvent::Stderr("!".into())), short));
        wait_until("the short line's reader parked", || {
            Arc::strong_count(&events.queue.bytes) == 2
        });
        for _ in 0..=lines {
            assert!(matches!(next_within(&events), Some(JobEvent::Stdout(_))));
        }
        assert!(matches!(next_within(&events), Some(JobEvent::Stderr(_))));
        let (sent, _idle_run) = short.join().unwrap();
        assert!(sent);

        let flood = 3 * lines;
        let flooder = thread::spawn(move || (0..flood).all(|_| fill.send(line())));
        for _ in 0..flood {
            assert!(matches!(next_within(&events), Some(JobEvent::Stdout(_))));
        }
        assert!(flooder.join().unwrap());
    }

    #[cfg(unix)]
    mod backpressure {
        use super::*;

        /// Prints 4 KiB lines forever: a few thousand of them fill the queue,
        /// fast enough on a slow CI runner.
        const FLOOD_CMD: &str = "yes \"$(printf '%04096d' 0)\"";
        /// Long lines, so filling the queue takes fewer of them.
        const PADDED_LINE_BYTES: usize = 4096;
        const NEVER_FILLED: &str = "the job's queue never filled";
        const NEVER_RELEASED: &str = "a reader blocked on a full queue was never released";

        /// A reader parks once the next padded line no longer fits.
        fn is_full(queue: &JobQueue) -> bool {
            let line = JobEvent::Stdout("0".repeat(PADDED_LINE_BYTES).as_str().into());
            queue.bytes.available_permits() < permits(&line) as usize
        }

        /// Starts {cmd} and waits for its queue to fill, so its readers are
        /// parked on a send.
        fn start_blocked(store: &mut JobStore, owner: JobOwner, cmd: &str) -> u32 {
            let id = store.start(JobSpec::new(owner, cmd)).unwrap();
            let queue = Arc::clone(&store.jobs[&id].events.queue);
            wait_until(NEVER_FILLED, || is_full(&queue));
            id
        }

        #[test]
        fn blocked_reader_delivers_every_line_in_order() {
            let lines = 2 * JOB_QUEUE_BYTES / PADDED_LINE_BYTES;
            let mut store = make_store();
            let cmd = format!("seq -f %0{PADDED_LINE_BYTES}g 1 {lines}");
            let id = start_blocked(&mut store, task_owner(1), &cmd);
            assert!(
                !store.jobs[&id].reaped.load(Ordering::Relaxed),
                "a parked reader must hold off the reap"
            );

            let events = collect_until_exit(id, || store.next_event(&task_owner(1)));

            let stdout: Vec<String> = events
                .iter()
                .filter_map(|(_, event)| match event {
                    JobEvent::Stdout(line) => Some(line.text.clone()),
                    _ => None,
                })
                .collect();
            let expected: Vec<String> = (1..=lines)
                .map(|n| format!("{n:0PADDED_LINE_BYTES$}"))
                .collect();
            assert_eq!(stdout, expected);
            assert!(matches!(events.last(), Some((_, JobEvent::Exit(0)))));
        }

        enum Teardown {
            KillTask,
            KillPlugin,
            KillSession,
            DropStore,
            RemoveWhileLeased,
        }

        /// The leased case is `jobwait` holding the events inside a
        /// callback: nothing drains them, so only closing the job's queue can
        /// release the readers and the wait thread.
        #[test_case::test_case(Teardown::KillTask ; "kill_task")]
        #[test_case::test_case(Teardown::KillPlugin ; "kill_plugin")]
        #[test_case::test_case(Teardown::KillSession ; "kill_session")]
        #[test_case::test_case(Teardown::DropStore ; "drop_store")]
        #[test_case::test_case(Teardown::RemoveWhileLeased ; "remove_while_leased")]
        fn teardown_releases_blocked_readers(teardown: Teardown) {
            let lua = Lua::new();
            let session = MakiId::generate();
            let owner = match teardown {
                Teardown::KillPlugin => plugin_owner(),
                Teardown::KillSession => session_owner(session),
                _ => task_owner(1),
            };
            let mut store = make_store();
            let id = start_blocked(&mut store, owner.clone(), FLOOD_CMD);
            let reaped = Arc::clone(&store.jobs[&id].reaped);
            let held = matches!(teardown, Teardown::RemoveWhileLeased)
                .then(|| store.lease_events(id, Some(1), TEST_PLUGIN).unwrap());

            match teardown {
                Teardown::KillSession => store.kill_session(&lua, session),
                Teardown::DropStore => drop(store),
                _ => {
                    store.kill_owner(&lua, &owner);
                }
            }

            wait_until(NEVER_RELEASED, || reaped.load(Ordering::Relaxed));
            if let Some(lease) = held {
                wait_until(NEVER_RELEASED, || lease.events.rx.is_disconnected());
            }
        }

        /// Both readers park at once. Each keeps the run it waited for, so
        /// released permits never bounce between them.
        #[test]
        fn two_blocked_streams_drain_to_exit() {
            let mut store = make_store();
            let cmd = format!("{FLOOD_CMD} & {FLOOD_CMD} >&2");
            let id = start_blocked(&mut store, task_owner(1), &cmd);

            store.kill(id, Some(1), TEST_PLUGIN);
            let events = collect_until_exit(id, || store.next_event(&task_owner(1)));

            let stdout = events
                .iter()
                .filter(|(_, event)| matches!(event, JobEvent::Stdout(_)))
                .count();
            let stderr = events
                .iter()
                .filter(|(_, event)| matches!(event, JobEvent::Stderr(_)))
                .count();
            assert!(stdout > 0 && stderr > 0, "both streams were read");
            let delivered: usize = events.iter().map(|(_, event)| queued_bytes(event)).sum();
            assert!(delivered > JOB_QUEUE_BYTES, "queued lines were lost");
            assert!(matches!(events.last(), Some((_, JobEvent::Exit(_)))));
        }

        #[test]
        fn jobstop_on_a_blocked_job_drains_to_exit() {
            let mut store = make_store();
            let id = start_blocked(&mut store, task_owner(1), FLOOD_CMD);

            store.kill(id, Some(1), TEST_PLUGIN);
            let events = collect_until_exit(id, || store.next_event(&task_owner(1)));

            let delivered: usize = events.iter().map(|(_, event)| queued_bytes(event)).sum();
            assert!(delivered > JOB_QUEUE_BYTES, "queued lines were lost");
            assert!(matches!(events.last(), Some((_, JobEvent::Exit(_)))));
        }

        #[test]
        fn forget_keeps_a_blocked_job_until_it_exits() {
            let lua = Lua::new();
            let mut store = make_store();
            let id = start_blocked(&mut store, session_owner(MakiId::generate()), FLOOD_CMD);

            store.forget(&lua, id, None, TEST_PLUGIN);
            assert!(
                store.snapshot(id, None, TEST_PLUGIN).is_some(),
                "a job that has not exited must survive forget"
            );

            store.kill(id, None, TEST_PLUGIN);
            let events = collect_until_exit(id, || next_plugin_event(&mut store));
            let Some((_, JobEvent::Exit(code))) = events.last() else {
                panic!("{NEVER_EXITED}");
            };
            store.complete(&lua, id, *code);
            store.forget(&lua, id, None, TEST_PLUGIN);
            assert!(store.snapshot(id, None, TEST_PLUGIN).is_none());
        }
    }
}
