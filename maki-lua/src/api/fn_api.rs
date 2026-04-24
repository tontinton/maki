use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;

use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table};

use crate::runtime::with_task_jobs;

const READER_BUF_SIZE: usize = 8 * 1024;

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
}

/// All jobs in a task share one event channel so the dispatch loop
/// can poll a single receiver for stdout/stderr/exit from any child.
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

    pub fn mark_dead(&mut self, job_id: u32) {
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            meta.alive = false;
        }
    }

    pub fn kill(&mut self, job_id: u32) {
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            if meta.alive {
                kill_process(meta.pid);
            }
        }
    }

    pub fn kill_all(&mut self) {
        for meta in self.jobs.values_mut() {
            if meta.alive {
                kill_process(meta.pid);
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

#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(windows)]
fn kill_process(pid: u32) {
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

    Ok(t)
}
