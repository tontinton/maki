use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table};

use crate::runtime::with_task_jobs;

const READER_BUF_SIZE: usize = 8 * 1024;

#[derive(Clone)]
pub(crate) enum JobEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

pub(crate) struct TaggedJobEvent {
    pub job_id: u32,
    pub event: JobEvent,
}

struct JobMeta {
    pid: u32,
    alive: bool,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    wait_tx: Option<flume::Sender<JobEvent>>,
}

/// Single channel per task so the dispatch loop can poll one receiver for all children.
pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    pub(crate) event_rx: flume::Receiver<TaggedJobEvent>,
    event_tx: flume::Sender<TaggedJobEvent>,
    next_id: u32,
}

impl JobStore {
    pub fn new() -> Self {
        let (event_tx, event_rx) = flume::unbounded();
        Self {
            jobs: HashMap::new(),
            event_rx,
            event_tx,
            next_id: 1,
        }
    }

    pub fn start(
        &mut self,
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
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        if let Some(ref dir) = cwd {
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
        let tx = self.event_tx.clone();

        macro_rules! spawn_reader {
            ($stream:expr, $name:expr, $variant:ident) => {
                if let Some(stream) = $stream {
                    let tx = tx.clone();
                    thread::Builder::new()
                        .name($name.into())
                        .spawn(move || {
                            for line in BufReader::with_capacity(READER_BUF_SIZE, stream)
                                .lines()
                                .map_while(Result::ok)
                            {
                                if tx
                                    .send(TaggedJobEvent {
                                        job_id: id,
                                        event: JobEvent::$variant(line),
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .map_err(|e| e.to_string())?;
                }
            };
        }
        spawn_reader!(stdout, "job-stdout", Stdout);
        spawn_reader!(stderr, "job-stderr", Stderr);

        thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                let _ = tx.send(TaggedJobEvent {
                    job_id: id,
                    event: JobEvent::Exit(code),
                });
            })
            .map_err(|e| e.to_string())?;

        self.jobs.insert(
            id,
            JobMeta {
                pid,
                alive: true,
                on_stdout,
                on_stderr,
                on_exit,
                wait_tx: None,
            },
        );

        Ok(id)
    }

    pub fn has_alive_jobs(&self) -> bool {
        self.jobs.values().any(|j| j.alive)
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn callback_key(&self, job_id: u32, event: &JobEvent) -> Option<&RegistryKey> {
        let meta = self.jobs.get(&job_id)?;
        if meta.wait_tx.is_some() {
            return None;
        }
        match event {
            JobEvent::Stdout(_) => meta.on_stdout.as_ref(),
            JobEvent::Stderr(_) => meta.on_stderr.as_ref(),
            JobEvent::Exit(_) => meta.on_exit.as_ref(),
        }
    }

    pub fn forward_to_waiter(&self, job_id: u32, event: &JobEvent) -> bool {
        self.jobs
            .get(&job_id)
            .and_then(|m| m.wait_tx.as_ref())
            .is_some_and(|tx| tx.send(event.clone()).is_ok())
    }

    pub fn subscribe_wait(&mut self, job_id: u32) -> Option<flume::Receiver<JobEvent>> {
        let meta = self.jobs.get_mut(&job_id)?;
        if meta.wait_tx.is_some() {
            return None;
        }
        let (tx, rx) = flume::unbounded();
        meta.wait_tx = Some(tx);
        Some(rx)
    }

    pub fn mark_dead(&mut self, job_id: u32) {
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            meta.alive = false;
        }
    }

    pub fn kill(&mut self, job_id: u32) {
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            if meta.alive {
                kill_job(meta);
            }
        }
    }

    pub fn kill_all(&mut self) {
        for meta in self.jobs.values_mut() {
            if meta.alive {
                kill_job(meta);
            }
        }
    }

    pub fn clear(&mut self, lua: &Lua) {
        for (_, meta) in self.jobs.drain() {
            for key in [meta.on_stdout, meta.on_stderr, meta.on_exit]
                .into_iter()
                .flatten()
            {
                lua.remove_registry_value(key).ok();
            }
        }
    }
}

fn shell_command(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("sh");
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

fn kill_job(meta: &JobMeta) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(meta.pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        const PROCESS_TERMINATE: u32 = 0x0001;
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn TerminateProcess(handle: *mut std::ffi::c_void, exit_code: u32) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, meta.pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}

pub(crate) fn create_fn_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "jobstart",
        lua.create_function(|lua, (cmd, opts): (String, Option<Table>)| {
            let (cwd, env, on_stdout, on_stderr, on_exit) = match opts {
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
                    (cwd, env, on_stdout, on_stderr, on_exit)
                }
                None => (None, None, None, None, None),
            };

            with_task_jobs(lua, |store| {
                store.start(&cmd, cwd, env, on_stdout, on_stderr, on_exit)
            })
            .ok_or_else(|| mlua::Error::runtime("job store not initialized"))?
            .map_err(mlua::Error::runtime)
        })?,
    )?;

    t.set(
        "jobstop",
        lua.create_function(|lua, job_id: u32| {
            with_task_jobs(lua, |store| store.kill(job_id))
                .ok_or_else(|| mlua::Error::runtime("job store not initialized"))?;
            Ok(())
        })?,
    )?;

    t.set(
        "jobwait",
        lua.create_async_function(|lua, (job_id, timeout_ms): (u32, Option<u64>)| async move {
            let wait_rx = with_task_jobs(&lua, |store| store.subscribe_wait(job_id))
                .ok_or_else(|| mlua::Error::runtime("job store not initialized"))?
                .ok_or_else(|| mlua::Error::runtime("unknown job id or already waited"))?;

            let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
            let deadline = smol::Timer::after(timeout);
            futures_lite::pin!(deadline);

            let mut stdout_lines = Vec::new();
            let mut stderr_lines = Vec::new();

            let exit_code = loop {
                let event =
                    futures_lite::future::or(async { wait_rx.recv_async().await.ok() }, async {
                        (&mut deadline).await;
                        None
                    })
                    .await;

                match event {
                    None => return Ok(mlua::Value::Nil),
                    Some(JobEvent::Stdout(line)) => stdout_lines.push(line),
                    Some(JobEvent::Stderr(line)) => stderr_lines.push(line),
                    Some(JobEvent::Exit(code)) => {
                        break code;
                    }
                }
            };

            let result = lua.create_table()?;
            result.set("stdout", stdout_lines.join("\n"))?;
            result.set("stderr", stderr_lines.join("\n"))?;
            result.set("exit_code", exit_code)?;
            Ok(mlua::Value::Table(result))
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> JobStore {
        JobStore::new()
    }

    fn start_echo(store: &mut JobStore) -> u32 {
        store
            .start("echo hello", None, None, None, None, None)
            .unwrap()
    }

    #[test]
    fn start_invalid_cwd_returns_error() {
        let mut store = make_store();
        let result = store.start(
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
    fn has_alive_jobs_tracks_state() {
        let mut store = make_store();
        assert!(!store.has_alive_jobs());

        let id = start_echo(&mut store);
        assert!(store.has_alive_jobs());

        store.mark_dead(id);
        assert!(!store.has_alive_jobs());
    }

    #[test]
    fn noop_on_nonexistent_or_dead_jobs() {
        let mut store = make_store();
        store.mark_dead(999);
        store.kill(999);

        let id = start_echo(&mut store);
        store.mark_dead(id);
        store.kill(id);

        assert!(store.callback_key(999, &JobEvent::Exit(0)).is_none());
        assert!(!store.forward_to_waiter(999, &JobEvent::Exit(0)));
    }

    #[test]
    fn subscribe_wait_lifecycle() {
        let mut store = make_store();
        assert!(store.subscribe_wait(999).is_none());

        let id = start_echo(&mut store);
        assert!(store.subscribe_wait(id).is_some());
        assert!(
            store.subscribe_wait(id).is_none(),
            "double subscribe should fail"
        );
    }

    #[test]
    fn callbacks_suppressed_while_waiting() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _rx = store.subscribe_wait(id).unwrap();

        assert!(
            store
                .callback_key(id, &JobEvent::Stdout("x".into()))
                .is_none()
        );
        assert!(store.callback_key(id, &JobEvent::Exit(0)).is_none());
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
    fn forward_to_waiter_delivers_events() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        assert!(!store.forward_to_waiter(id, &JobEvent::Stdout("x".into())));

        let rx = store.subscribe_wait(id).unwrap();
        assert!(store.forward_to_waiter(id, &JobEvent::Stdout("line1".into())));
        assert!(store.forward_to_waiter(id, &JobEvent::Exit(0)));

        let ev1 = rx.recv().unwrap();
        assert!(matches!(ev1, JobEvent::Stdout(s) if s == "line1"));
        let ev2 = rx.recv().unwrap();
        assert!(matches!(ev2, JobEvent::Exit(0)));
    }

    #[test]
    fn forward_to_waiter_fails_after_rx_dropped() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let rx = store.subscribe_wait(id).unwrap();
        drop(rx);

        assert!(!store.forward_to_waiter(id, &JobEvent::Stdout("orphan".into())));
    }

    #[test]
    fn event_channel_receives_exit() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let rx = store.event_rx.clone();

        let mut got_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(tagged) if tagged.job_id == id && matches!(tagged.event, JobEvent::Exit(_)) => {
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
}
