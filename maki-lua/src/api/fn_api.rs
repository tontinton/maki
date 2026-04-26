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

enum JobKind {
    Process { pid: u32 },
    Timer,
}

struct JobMeta {
    kind: JobKind,
    alive: bool,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    event_rx: Option<flume::Receiver<JobEvent>>,
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
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
                kind: JobKind::Process { pid },
                alive: true,
                on_stdout,
                on_stderr,
                on_exit,
                event_rx: Some(event_rx),
            },
        );

        Ok(id)
    }

    pub fn start_timer(
        &mut self,
        timeout_ms: u64,
        on_exit: Option<RegistryKey>,
    ) -> Result<u32, String> {
        let id = self.next_id;
        self.next_id += 1;

        let (event_tx, event_rx) = flume::unbounded();

        thread::Builder::new()
            .name("timer".into())
            .spawn(move || {
                thread::sleep(Duration::from_millis(timeout_ms));
                let _ = event_tx.send(JobEvent::Exit(0));
            })
            .map_err(|e| e.to_string())?;

        self.jobs.insert(
            id,
            JobMeta {
                kind: JobKind::Timer,
                alive: true,
                on_stdout: None,
                on_stderr: None,
                on_exit,
                event_rx: Some(event_rx),
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
        match event {
            JobEvent::Stdout(_) => meta.on_stdout.as_ref(),
            JobEvent::Stderr(_) => meta.on_stderr.as_ref(),
            JobEvent::Exit(_) => meta.on_exit.as_ref(),
        }
    }

    pub fn take_receiver(&mut self, job_id: u32) -> Option<flume::Receiver<JobEvent>> {
        let meta = self.jobs.get_mut(&job_id)?;
        meta.event_rx.take()
    }

    pub fn drain_events(&self, buf: &mut Vec<(u32, JobEvent)>) {
        buf.clear();
        for (&id, meta) in &self.jobs {
            if let Some(ref rx) = meta.event_rx {
                while let Ok(event) = rx.try_recv() {
                    buf.push((id, event));
                }
            }
        }
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

fn kill_job(meta: &mut JobMeta) {
    match meta.kind {
        JobKind::Timer => {
            meta.alive = false;
        }
        JobKind::Process { pid } => {
            #[cfg(unix)]
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
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
                    let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if !handle.is_null() {
                        TerminateProcess(handle, 1);
                        CloseHandle(handle);
                    }
                }
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
            let rx = with_task_jobs(&lua, |store| store.take_receiver(job_id))
                .ok_or_else(|| mlua::Error::runtime("job store not initialized"))?
                .ok_or_else(|| mlua::Error::runtime("unknown job id or already waited"))?;

            let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
            let deadline = smol::Timer::after(timeout);
            futures_lite::pin!(deadline);

            let mut stdout_lines = Vec::new();
            let mut stderr_lines = Vec::new();

            let exit_code = loop {
                let event = futures_lite::future::or(async { rx.recv_async().await.ok() }, async {
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
    }

    #[test]
    fn take_receiver_lifecycle() {
        let mut store = make_store();
        assert!(store.take_receiver(999).is_none());

        let id = start_echo(&mut store);
        assert!(store.take_receiver(id).is_some());
        assert!(
            store.take_receiver(id).is_none(),
            "second take should fail (receiver already moved)"
        );
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
        let rx = store.take_receiver(id).unwrap();

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
    fn drain_events_collects_from_all_jobs() {
        let mut store = make_store();
        let id = start_echo(&mut store);

        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_events(&mut buf);
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
    }

    #[test]
    fn drain_events_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _rx = store.take_receiver(id).unwrap();

        let mut buf = Vec::new();
        store.drain_events(&mut buf);
        assert!(
            buf.is_empty(),
            "drained receiver yields no events via drain_events"
        );
    }

    #[test]
    fn start_timer_fires_exit_event() {
        let mut store = make_store();
        let id = store.start_timer(50, None).unwrap();
        assert!(store.has_alive_jobs());

        let rx = store.take_receiver(id).unwrap();
        let event = rx.recv_timeout(Duration::from_secs(2));
        assert!(matches!(event, Ok(JobEvent::Exit(0))));
    }

    #[test]
    fn kill_timer_marks_dead() {
        let mut store = make_store();
        let id = store.start_timer(10_000, None).unwrap();
        assert!(store.has_alive_jobs());

        store.kill(id);
        assert!(!store.has_alive_jobs());
    }
}
