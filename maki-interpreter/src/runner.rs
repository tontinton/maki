//! Drives the monty interpreter through its execution states.
//! Sync tool calls resolve immediately; async (`await`) calls are batched via `ResolveFutures`
//! and dispatched concurrently through [`AsyncResolver`], with results fed back one by one.
//! The sandbox never touches the OS itself. Every `OsCall` lands in [`OsHost`],
//! which hands file content and new directories to the host's [`FileFn`], and
//! reads metadata and listings from disk only for paths the host cleared.

use std::borrow::Cow;
use std::collections::HashMap;
use std::env::{current_dir, var};
use std::mem;
use std::path::{Component, Path, PathBuf, absolute};
use std::thread;
use std::time::{Duration, Instant};

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MkdirCallArgs, MontyException, MontyFileHandle,
    MontyObject, NameLookupResult, OpenCallArgs, OsFunctionCall, PathStringDataArgs, PrintWriter,
    PrintWriterCallback, ResourceLimits, ResourceTracker, normalize_virtual_path,
};
use serde_json::Value;
use tracing::{debug, warn};

use crate::alloc::SandboxScope;
use crate::convert::{json_to_monty, monty_to_json};
use crate::disk::{self, Cleared};
use crate::error::InterpreterError;

const DEFAULT_MAX_RECURSION: usize = 100;
const SCRIPT_NAME: &str = "agent.py";
const OS_CALLS_DENIED: &str = "OS calls are not permitted";
const BINARY_FILES_UNSUPPORTED: &str = "binary files are not supported, open in text mode";
const REMOVAL_UNSUPPORTED: &str =
    "removing or renaming files is not supported in the sandbox, use the bash tool";
const HOST_TIME_EXCEEDED: &str =
    "time limit exceeded: sleeps and filesystem queries may add up to the script timeout at most";
const MAX_URANDOM_BYTES: usize = 1024 * 1024;
const VERBATIM_PREFIX: &str = r"\\?\";
/// Environment variables often hold secrets, and nobody approves what a script
/// reads, so it only sees these. The rest read as unset.
const ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "LANG",
    "TERM",
    "TMPDIR",
    "USERPROFILE",
    "USERNAME",
    "TEMP",
    "TMP",
];

pub type ToolFn = Box<dyn Fn(&str, Vec<Value>, Vec<(String, Value)>) -> Result<Value, String>>;

pub struct FileWrite {
    pub path: String,
    pub content: String,
    pub append: bool,
}

pub struct DirCreate {
    pub path: String,
    pub parents: bool,
}

pub enum FileOp {
    Read(String),
    Write(FileWrite),
    /// May the script read metadata at this absolute path and list what is
    /// under it?
    Inspect(String),
    /// Always an absolute path that just passed `Inspect`.
    Mkdir(DirCreate),
}

/// `Ok` holds the content for a read and is ignored otherwise.
pub type FileFn = Box<dyn Fn(FileOp) -> Result<String, String>>;

pub struct PendingCall {
    pub call_id: u32,
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: Vec<(String, Value)>,
}

pub type AsyncResolver =
    Box<dyn Fn(Vec<PendingCall>) -> Result<Vec<(u32, Result<Value, String>)>, InterpreterError>>;

#[derive(Debug)]
pub struct InterpreterResult {
    pub output: Option<Value>,
    pub stdout: String,
}

struct StreamingWriter<'a> {
    buffer: String,
    flushed_pos: usize,
    on_line: &'a mut dyn FnMut(&str),
}

impl PrintWriterCallback for StreamingWriter<'_> {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.buffer.push_str(&output);
        Ok(())
    }

    fn stdout_push(&mut self, ch: char) -> Result<(), MontyException> {
        self.buffer.push(ch);
        if ch == '\n' {
            (self.on_line)(&self.buffer[self.flushed_pos..]);
            self.flushed_pos = self.buffer.len();
        }
        Ok(())
    }
}

/// `preamble` holds imports and helpers; it is compiled ahead of `code` as one
/// script, and tracebacks come back rebased so line 1 is `code` line 1.
pub fn run(
    code: &str,
    preamble: &str,
    tools: &HashMap<String, ToolFn>,
    resolver: Option<&AsyncResolver>,
    files: Option<&FileFn>,
    limits: ResourceLimits,
    on_output: &mut dyn FnMut(&str),
) -> Result<InterpreterResult, InterpreterError> {
    let mut writer = StreamingWriter {
        buffer: String::new(),
        flushed_pos: 0,
        on_line: on_output,
    };
    let preamble = preamble.trim_end_matches('\n');
    let script = if preamble.is_empty() {
        code.to_owned()
    } else {
        format!("{preamble}\n{code}")
    };
    let output = execute(
        script,
        tools,
        resolver,
        files,
        limits,
        &mut PrintWriter::Callback(&mut writer),
    )
    .map_err(|e| {
        let offset = preamble.lines().count();
        match e {
            InterpreterError::Parse(msg) => InterpreterError::Parse(rebase_traceback(&msg, offset)),
            InterpreterError::Runtime(msg) => {
                InterpreterError::Runtime(rebase_traceback(&msg, offset))
            }
            other => other,
        }
    })?;
    Ok(InterpreterResult {
        output,
        stdout: writer.buffer,
    })
}

fn execute(
    script: String,
    tools: &HashMap<String, ToolFn>,
    resolver: Option<&AsyncResolver>,
    files: Option<&FileFn>,
    limits: ResourceLimits,
    print_writer: &mut PrintWriter<'_>,
) -> Result<Option<Value>, InterpreterError> {
    let _sandbox = limits.max_memory.is_some().then(SandboxScope::enter);
    let mut host = OsHost {
        files,
        pending: Vec::new(),
        cwd: sandbox_cwd()?,
        clock: HostClock {
            spent: Duration::ZERO,
            limit: limits.max_total_sleep,
        },
    };
    let result = drive(script, tools, resolver, &mut host, limits, print_writer);
    // CPython closes open files when a script dies, so their writes land even then.
    match (result, host.flush_all()) {
        (result, Ok(())) => result,
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Err(flush_err)) => {
            warn!(error = %flush_err, "buffered file write failed after the run failed");
            Err(e)
        }
    }
}

fn drive(
    script: String,
    tools: &HashMap<String, ToolFn>,
    resolver: Option<&AsyncResolver>,
    host: &mut OsHost<'_>,
    limits: ResourceLimits,
    print_writer: &mut PrintWriter<'_>,
) -> Result<Option<Value>, InterpreterError> {
    let mut runner = MontyRun::new(script, SCRIPT_NAME, vec![], CompileOptions::default())
        .map_err(|e| InterpreterError::Parse(e.to_string()))?;
    runner.set_cwd(&host.cwd);

    let tracker = ResourceTracker::new(limits);

    let mut progress = runner
        .start(vec![], tracker, print_writer.reborrow())
        .map_err(|e| InterpreterError::Runtime(e.to_string()))?;

    let mut pending_calls: HashMap<u32, PendingCall> = HashMap::new();

    loop {
        match progress {
            RunProgress::Complete(obj) => {
                return Ok((obj != MontyObject::none()).then(|| monty_to_json(obj.as_ref())));
            }
            RunProgress::FunctionCall(call) => {
                let name = call.function_name.clone();
                let args_json: Vec<Value> = call.args.args().map(monty_to_json).collect();
                let kwargs_json: Vec<(String, Value)> = call
                    .args
                    .kwargs()
                    .map(|(k, v)| (k.to_string(), monty_to_json(v)))
                    .collect();

                debug!(
                    function = %name,
                    num_args = args_json.len(),
                    num_kwargs = kwargs_json.len(),
                    "interpreter: function call"
                );

                if resolver.is_some() && tools.contains_key(name.as_str()) {
                    let call_id = call.call_id;
                    pending_calls.insert(
                        call_id,
                        PendingCall {
                            call_id,
                            name,
                            args: args_json,
                            kwargs: kwargs_json,
                        },
                    );
                    progress = call
                        .resume_pending(print_writer.reborrow())
                        .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
                } else if let Some(tool_fn) = tools.get(name.as_str()) {
                    host.flush_all()?;
                    let result = tool_fn(&name, args_json, kwargs_json).map_err(|e| {
                        InterpreterError::ToolCall {
                            tool: name.clone(),
                            message: e,
                        }
                    })?;
                    progress = call
                        .resume(json_to_monty(result), print_writer.reborrow())
                        .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
                } else {
                    progress = call
                        .resume(ExtFunctionResult::NotFound(name), print_writer.reborrow())
                        .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
                }
            }
            RunProgress::NameLookup(lookup) => {
                let name = &lookup.name;
                debug!(name = %name, "interpreter: name lookup");

                let result = if tools.contains_key(name.as_str()) {
                    NameLookupResult::Value(MontyObject::function(name.clone(), None))
                } else {
                    NameLookupResult::Undefined
                };

                progress = lookup
                    .resume(result, print_writer.reborrow())
                    .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
            }
            RunProgress::OsCall(mut call) => {
                if host.files.is_none() {
                    return Err(InterpreterError::Sandboxed(OS_CALLS_DENIED.into()));
                }
                // Not `resume_with`, because `serve` may end the run instead.
                // Taking the call out still moves a big write rather than
                // copying it, and `resume` never looks at the placeholder.
                let os_call = mem::replace(&mut call.function_call, OsFunctionCall::GetEnviron);
                let answer = host.serve(os_call)?;
                progress = call
                    .resume(answer, print_writer.reborrow())
                    .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
            }
            RunProgress::ResolveFutures(state) => {
                let resolver = resolver.ok_or_else(|| {
                    InterpreterError::Sandboxed("async operations are not supported".into())
                })?;

                let ids = state.pending_call_ids().to_vec();
                let batch: Vec<PendingCall> = ids
                    .iter()
                    .filter_map(|id| pending_calls.remove(id))
                    .collect();

                host.flush_all()?;
                let resolved = resolver(batch)?;

                let results: Vec<(u32, ExtFunctionResult)> = resolved
                    .into_iter()
                    .map(|(id, result)| match result {
                        Ok(val) => (id, ExtFunctionResult::Return(json_to_monty(val))),
                        Err(msg) => (
                            id,
                            ExtFunctionResult::Error(MontyException::new(
                                ExcType::RuntimeError,
                                Some(msg),
                            )),
                        ),
                    })
                    .collect();

                progress = state
                    .resume(results, print_writer.reborrow())
                    .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
            }
        }
    }
}

/// Keeps what the script writes with `open()` and `pathlib` in memory until
/// someone could look at the file: a tool call, a read, a metadata query at
/// or above it, or the end of the run. A loop of `f.write` then turns into a single host write,
/// and a cancelled run drops what is still waiting here. Paths are matched once
/// made absolute, so `a` and `./a` share one entry and the last write wins.
struct OsHost<'a> {
    files: Option<&'a FileFn>,
    pending: Vec<FileWrite>,
    cwd: String,
    clock: HostClock,
}

impl OsHost<'_> {
    /// `Err` ends the run before the script resumes, see [`Fault`].
    fn serve(&mut self, call: OsFunctionCall) -> Result<ExtFunctionResult, InterpreterError> {
        match self.dispatch(call) {
            Ok(value) => Ok(ExtFunctionResult::Return(value)),
            Err(Fault::Raise(exc)) => Ok(ExtFunctionResult::Error(exc)),
            Err(Fault::Abort(err)) => Err(err),
        }
    }

    fn dispatch(&mut self, call: OsFunctionCall) -> Result<MontyObject, Fault> {
        match call {
            OsFunctionCall::Open(args) => Ok(self.open(args).map_err(os_error)?),
            OsFunctionCall::ReadText(path) => Ok(self
                .read(self.host_path(path.into_string()))
                .map_err(os_error)?),
            OsFunctionCall::WriteText(args) => Ok(self.write(args, false)),
            OsFunctionCall::AppendText(args) => Ok(self.write(args, true)),
            OsFunctionCall::Exists(path) => self.query(&path, |p| disk::probe(p, Path::exists)),
            OsFunctionCall::IsFile(path) => self.query(&path, |p| disk::probe(p, Path::is_file)),
            OsFunctionCall::IsDir(path) => self.query(&path, |p| disk::probe(p, Path::is_dir)),
            OsFunctionCall::IsSymlink(path) => {
                self.query(&path, |p| disk::probe(p, Path::is_symlink))
            }
            OsFunctionCall::Stat(path) => self.query(&path, disk::stat),
            OsFunctionCall::Iterdir(path) => self.query(&path, disk::iterdir),
            OsFunctionCall::Scan(args) => self.query(&args.path, |p| disk::scan(p, &args)),
            OsFunctionCall::Resolve(path) => {
                self.query(&path, |p| Ok(MontyObject::path(resolved(p))))
            }
            OsFunctionCall::Mkdir(args) => self.mkdir(args),
            OsFunctionCall::Absolute(path) => Ok(MontyObject::path(path.into_string())),
            OsFunctionCall::Getenv(args) => {
                Ok(allowed_env_var(&args.key).map_or(args.default, MontyObject::string))
            }
            OsFunctionCall::GetEnviron => Ok(allowed_environ()),
            OsFunctionCall::Urandom(args) => Ok(random_bytes(args.size)?),
            OsFunctionCall::Sleep(delay)
            | OsFunctionCall::SystemSleep(delay)
            | OsFunctionCall::AsyncSleep(delay)
            | OsFunctionCall::AsyncSystemSleep(delay) => self.sleep(delay),
            OsFunctionCall::ReadBytes(_)
            | OsFunctionCall::WriteBytes(_)
            | OsFunctionCall::AppendBytes(_) => Err(runtime_error(BINARY_FILES_UNSUPPORTED).into()),
            call @ (OsFunctionCall::Unlink(_)
            | OsFunctionCall::Rmdir(_)
            | OsFunctionCall::Rename(_)) => {
                Err(runtime_error(format!("{call}: {REMOVAL_UNSUPPORTED}")).into())
            }
            // Listed by name, so a call that a new monty adds breaks the build
            // here rather than quietly getting a `PermissionError` a walk swallows.
            call @ (OsFunctionCall::DateToday
            | OsFunctionCall::DateTimeNow(_)
            | OsFunctionCall::Time(_)
            | OsFunctionCall::Import(_)
            | OsFunctionCall::ImportStubs(_)) => Err(call.on_no_handler().into()),
        }
    }

    /// The only road to the disk. Waiting writes at or under the path land
    /// first, so the script sees the files it just wrote. The rest stay
    /// queued: flushing them all would turn a report written while checking
    /// files into one append per check. A refusal raises `RuntimeError`, because
    /// walks and globs swallow an `OSError` like CPython does and would show
    /// an empty tree. The host gets the absolute path rather than the script's
    /// spelling, since it would expand `~` or a relative path its own way and
    /// could approve a different file than the one we read.
    fn query<T>(
        &mut self,
        path: &str,
        read: impl FnOnce(&Cleared) -> Result<T, MontyException>,
    ) -> Result<T, Fault> {
        let disk_path = native_path(path, cfg!(windows)).to_owned();
        self.flush_within(&disk_path)?;
        let started = self.clock.admit(Duration::ZERO)?;
        let result = Cleared::new(path, disk_path, |disk| {
            self.call(FileOp::Inspect(disk.to_owned())).map(drop)
        })
        .map_err(runtime_error)
        .and_then(|cleared| read(&cleared));
        self.clock.charge(started);
        Ok(result?)
    }

    /// We look before we create, and that look needs clearance like any other
    /// query. Otherwise `exist_ok` would leak what is on disk behind the host.
    fn mkdir(&mut self, args: MkdirCallArgs) -> Result<MontyObject, Fault> {
        let Some(path) = self.query(&args.path, |p| disk::dir_to_make(p, args.exist_ok))? else {
            return Ok(MontyObject::none());
        };
        self.call(FileOp::Mkdir(DirCreate {
            path,
            parents: args.parents,
        }))
        .map(|_| MontyObject::none())
        .map_err(|e| os_error(e).into())
    }

    fn sleep(&mut self, delay: Duration) -> Result<MontyObject, Fault> {
        let started = self.clock.admit(delay)?;
        thread::sleep(delay);
        self.clock.charge(started);
        Ok(MontyObject::none())
    }

    fn call(&self, op: FileOp) -> Result<String, String> {
        self.files
            .map_or_else(|| Err(OS_CALLS_DENIED.into()), |files| files(op))
    }

    /// Monty leaves what `open()` does to the file up to us. A script can open
    /// with `w` and never write, and the file must still end up empty, so the
    /// open itself queues an empty replacement. A missing file for `r` only
    /// shows up at the first read, which saves reading it twice.
    fn open(&mut self, args: OpenCallArgs) -> Result<MontyObject, String> {
        if args.mode.is_binary() {
            return Err(BINARY_FILES_UNSUPPORTED.into());
        }
        let path = args.path.into_string();
        if args.mode.truncate() {
            self.buffer(FileWrite {
                path: self.host_path(path.clone()),
                content: String::new(),
                append: false,
            });
        }
        Ok(MontyObject::file_handle(MontyFileHandle {
            path,
            mode: args.mode,
            position: 0,
        }))
    }

    fn read(&mut self, path: String) -> Result<MontyObject, String> {
        if let Some(index) = self.pending.iter().position(|w| same_file(&w.path, &path)) {
            self.flush_at(index)?;
        }
        self.call(FileOp::Read(path)).map(MontyObject::string)
    }

    /// Monty moves the file position by the number we return, and text
    /// positions count chars, not bytes.
    fn write(&mut self, args: PathStringDataArgs, append: bool) -> MontyObject {
        let written = args.data.chars().count() as i64;
        self.buffer(FileWrite {
            path: self.host_path(args.path.into_string()),
            content: args.data,
            append,
        });
        MontyObject::int(written)
    }

    fn buffer(&mut self, write: FileWrite) {
        match self
            .pending
            .iter_mut()
            .find(|w| same_file(&w.path, &write.path))
        {
            Some(pending) if write.append => pending.content.push_str(&write.content),
            Some(pending) => *pending = write,
            None => self.pending.push(write),
        }
    }

    /// Stops at the first failure. The files after it stay queued, so the
    /// final flush at the end of the run still writes them.
    fn flush_all(&mut self) -> Result<(), InterpreterError> {
        while !self.pending.is_empty() {
            self.flush_at(0).map_err(InterpreterError::FileWrite)?;
        }
        Ok(())
    }

    fn flush_within(&mut self, dir: &str) -> Result<(), InterpreterError> {
        while let Some(index) = self.pending.iter().position(|w| is_within(&w.path, dir)) {
            self.flush_at(index).map_err(InterpreterError::FileWrite)?;
        }
        Ok(())
    }

    /// The error names the path because it often shows up far from the
    /// `write` that caused it.
    fn flush_at(&mut self, index: usize) -> Result<(), String> {
        let write = self.pending.remove(index);
        let path = write.path.clone();
        self.call(FileOp::Write(write))
            .map(drop)
            .map_err(|e| format!("{path}: {e}"))
    }

    fn host_path(&self, sandbox: String) -> String {
        written_path(&self.cwd, sandbox, cfg!(windows))
    }
}

/// How an OS call fails. A problem with the path the script asked about is
/// raised in the script, like CPython does. Anything else ends the run, since
/// the script could catch it, and a walk or glob would swallow it as an
/// `OSError` (`TimeoutError` is one) and quietly lose a failed buffered write.
enum Fault {
    Raise(MontyException),
    Abort(InterpreterError),
}

impl From<MontyException> for Fault {
    fn from(exc: MontyException) -> Self {
        Self::Raise(exc)
    }
}

impl From<InterpreterError> for Fault {
    fn from(err: InterpreterError) -> Self {
        Self::Abort(err)
    }
}

/// Monty's clock stands still while the host works, so sleeps and disk queries
/// would be free, and a loop of them could run forever. They share this budget
/// instead, and going over ends the run like the timeout does. Reads and writes
/// stay out of it because they go through tools, which keep their own time and
/// may sit on a permission prompt.
struct HostClock {
    spent: Duration,
    limit: Option<Duration>,
}

impl HostClock {
    /// A zero-length disk query is still refused once the budget is spent.
    fn admit(&self, upcoming: Duration) -> Result<Instant, InterpreterError> {
        let over = self.limit.is_some_and(|limit| {
            self.spent >= limit || self.spent.saturating_add(upcoming) > limit
        });
        if over {
            return Err(InterpreterError::Runtime(HOST_TIME_EXCEEDED.into()));
        }
        Ok(Instant::now())
    }

    fn charge(&mut self, started: Instant) {
        self.spent = self.spent.saturating_add(started.elapsed());
    }
}

/// Monty resolves relative paths against its own POSIX cwd, so it gets the
/// process cwd: `os.getcwd()` is then the real one.
fn sandbox_cwd() -> Result<String, InterpreterError> {
    current_dir()
        .map(|dir| to_sandbox_cwd(&dir.to_string_lossy(), cfg!(windows)))
        .map_err(|e| InterpreterError::Runtime(format!("cannot resolve cwd: {e}")))
}

/// Canonical like monty keeps it, so [`written_path`] can strip it. Windows
/// `C:\proj` becomes `/C:/proj`, which monty accepts as an absolute path.
fn to_sandbox_cwd(host: &str, windows: bool) -> String {
    let posix = if windows {
        Cow::Owned(format!("/{}", host.replace('\\', "/")))
    } else {
        Cow::Borrowed(host)
    };
    normalize_virtual_path(&posix).into_owned()
}

/// Monty prepends its cwd to every path not starting with `/`, without
/// normalizing, so cutting it back off gives the path as the script wrote it.
/// That is what the host tools expect: they expand `~`, resolve relative paths
/// and Windows drive paths themselves, and permission rules match the spelling.
fn written_path(cwd: &str, sandbox: String, windows: bool) -> String {
    let written = sandbox.strip_prefix(cwd).and_then(|rest| {
        if cwd.ends_with('/') {
            Some(rest)
        } else {
            rest.strip_prefix('/')
        }
    });
    match written {
        Some(written) => written.to_owned(),
        None => native_path(&sandbox, windows).to_owned(),
    }
}

/// The OS spelling of an absolute sandbox path: a Windows drive path loses
/// the `/` that [`to_sandbox_cwd`] put in front of it.
fn native_path(sandbox: &str, windows: bool) -> &str {
    match sandbox.strip_prefix('/') {
        Some(rest) if windows && is_drive_path(rest) => rest,
        _ => sandbox,
    }
}

fn resolved(path: &Cleared) -> String {
    let real = disk::resolve(path);
    let real = real.to_string_lossy();
    let real = real.strip_prefix(VERBATIM_PREFIX).unwrap_or(&real);
    to_sandbox_cwd(real, cfg!(windows))
}

fn allowed_env_var(key: &str) -> Option<String> {
    ENV_ALLOWLIST
        .contains(&key)
        .then(|| var(key).ok())
        .flatten()
}

fn allowed_environ() -> MontyObject {
    MontyObject::dict(ENV_ALLOWLIST.iter().filter_map(|&key| {
        let value = var(key).ok()?;
        Some((MontyObject::string(key), MontyObject::string(value)))
    }))
}

fn random_bytes(size: u64) -> Result<MontyObject, MontyException> {
    let len = usize::try_from(size)
        .ok()
        .filter(|&len| len <= MAX_URANDOM_BYTES)
        .ok_or_else(|| {
            MontyException::new(
                ExcType::ValueError,
                Some(format!(
                    "os.urandom returns at most {MAX_URANDOM_BYTES} bytes"
                )),
            )
        })?;
    let mut bytes = vec![0; len];
    getrandom::fill(&mut bytes).map_err(|e| os_error(e.to_string()))?;
    Ok(MontyObject::bytes(bytes))
}

fn os_error(message: String) -> MontyException {
    MontyException::new(ExcType::OSError, Some(message))
}

fn runtime_error(message: impl Into<String>) -> MontyException {
    MontyException::new(ExcType::RuntimeError, Some(message.into()))
}

fn is_drive_path(path: &str) -> bool {
    matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
}

/// Lexical only, like the host tools resolve paths, so symlinks still differ.
fn same_file(a: &str, b: &str) -> bool {
    a == b || matches!((absolute(a), absolute(b)), (Ok(a), Ok(b)) if a == b)
}

/// Lexical too. The host expands a written `~` its own way, so such a path,
/// or one we cannot make absolute, counts as inside: a missed flush would show
/// the script a stale disk.
fn is_within(written: &str, dir: &str) -> bool {
    if written.starts_with('~') {
        return true;
    }
    match (lexical_absolute(written), lexical_absolute(dir)) {
        (Some(written), Some(dir)) => written.starts_with(dir),
        _ => true,
    }
}

fn lexical_absolute(path: &str) -> Option<PathBuf> {
    let mut normal = PathBuf::new();
    for component in absolute(path).ok()?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    Some(normal)
}

/// Monty counts lines in the whole script, so user frames come back
/// `preamble_lines` too high. Preamble frames are dropped along with their
/// source excerpt: the caller never wrote those lines, so pointing at them only
/// sends it hunting through code it cannot see.
fn rebase_traceback(msg: &str, preamble_lines: usize) -> String {
    let prefix = format!("  File \"{SCRIPT_NAME}\", line ");
    let mut kept: Vec<Cow<'_, str>> = Vec::new();
    let mut in_preamble_frame = false;
    for line in msg.lines() {
        match line.strip_prefix(&prefix).and_then(split_line_number) {
            Some((number, rest)) => {
                in_preamble_frame = number <= preamble_lines;
                if !in_preamble_frame {
                    kept.push(format!("{prefix}{}{rest}", number - preamble_lines).into());
                }
            }
            None if in_preamble_frame && line.starts_with(' ') => {}
            None => {
                in_preamble_frame = false;
                kept.push(line.into());
            }
        }
    }
    kept.join("\n")
}

fn split_line_number(rest: &str) -> Option<(usize, &str)> {
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok().map(|n| (n, &rest[end..]))
}

pub fn limits(timeout: Duration, max_memory: usize) -> ResourceLimits {
    ResourceLimits::default()
        .max_feed_duration(timeout)
        // Monty leaves enforcing this to the host, see `HostClock`.
        .max_total_sleep(timeout)
        .max_memory(max_memory)
        .max_recursion_depth(DEFAULT_MAX_RECURSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use test_case::test_case;

    const NO_PREAMBLE: &str = "";
    const RAISING_PREAMBLE: &str = "def helper():\n    raise ValueError('inside preamble')\n";
    const DEFAULT_MAX_MEMORY: usize = 50 * 1024 * 1024;
    const SMALL_MAX_MEMORY: usize = 2 * 1024 * 1024;
    const OVER_LIMIT_ALLOC: &str = "x = [0] * 10_000_000";
    const MEMORY_ERR_FRAGMENT: &str = "memory";
    const NESTED_TIMEOUT: Duration = Duration::from_secs(5);
    const NESTED_DEADLOCK: &str = "nested run deadlocked";
    const USER_ERROR_LINE: usize = 2;
    const FILE_PATH: &str = "notes.txt";
    const FILE_CONTENT: &str = "old";
    const MISSING_FILE_ERR: &str = "no such file";
    const LOCKED_PATH: &str = "locked.txt";
    const WRITE_DENIED_ERR: &str = "write denied";
    const PEEK_TOOL: &str = "peek";
    const WRITTEN: &str = "new";
    const UNSERVED_OP_ERR: &str = "not served by this host";
    const TREE_FILE_CONTENT: &str = "abc";
    const DENIED_DIR: &str = "denied";
    const READONLY_DIR: &str = "readonly";
    const INSPECT_DENIED_ERR: &str = "inspect denied";
    const MKDIR_DENIED_ERR: &str = "mkdir denied";
    const MISSING_DIR: &str = "never-created";
    /// Set by cargo for every test process, and no business of a script.
    const HIDDEN_ENV_VAR: &str = "CARGO_MANIFEST_DIR";

    struct MemoryFs {
        files: HashMap<String, String>,
        writes: usize,
    }

    type SharedFs = Rc<RefCell<MemoryFs>>;

    fn run_code(
        code: &str,
        tools: &HashMap<String, ToolFn>,
        resolver: Option<&AsyncResolver>,
        limits: ResourceLimits,
    ) -> Result<InterpreterResult, InterpreterError> {
        run(
            code,
            NO_PREAMBLE,
            tools,
            resolver,
            None,
            limits,
            &mut |_| {},
        )
    }

    fn memory_fs() -> SharedFs {
        Rc::new(RefCell::new(MemoryFs {
            files: HashMap::from([(FILE_PATH.to_owned(), FILE_CONTENT.to_owned())]),
            writes: 0,
        }))
    }

    fn content(fs: &SharedFs) -> Value {
        json!(fs.borrow().files.get(FILE_PATH))
    }

    /// Binds `path`, and `locked` where every write fails. The `peek` tool
    /// returns what `path` holds at the moment it runs.
    fn run_with_files(
        code: &str,
        fs: &SharedFs,
        async_tools: bool,
    ) -> Result<InterpreterResult, InterpreterError> {
        let host_fs = Rc::clone(fs);
        let files: FileFn = Box::new(move |op| {
            let mut fs = host_fs.borrow_mut();
            match op {
                FileOp::Read(path) => fs
                    .files
                    .get(&path)
                    .cloned()
                    .ok_or_else(|| MISSING_FILE_ERR.to_owned()),
                FileOp::Write(write) if write.path == LOCKED_PATH => {
                    Err(WRITE_DENIED_ERR.to_owned())
                }
                FileOp::Write(write) => {
                    fs.writes += 1;
                    let file = fs.files.entry(write.path).or_default();
                    if !write.append {
                        file.clear();
                    }
                    file.push_str(&write.content);
                    Ok(String::new())
                }
                FileOp::Inspect(_) | FileOp::Mkdir(_) => Err(UNSERVED_OP_ERR.to_owned()),
            }
        });
        let peek_fs = Rc::clone(fs);
        let peek: ToolFn = Box::new(move |_, _, _| Ok(content(&peek_fs)));
        let resolver_fs = Rc::clone(fs);
        let resolver: AsyncResolver = Box::new(move |calls| {
            Ok(calls
                .into_iter()
                .map(|c| (c.call_id, Ok(content(&resolver_fs))))
                .collect())
        });
        run(
            code,
            &format!("path = '{FILE_PATH}'\nlocked = '{LOCKED_PATH}'"),
            &HashMap::from([(PEEK_TOOL.to_owned(), peek)]),
            async_tools.then_some(&resolver),
            Some(&files),
            default_limits(),
            &mut |_| {},
        )
    }

    /// `a.txt` and `sub/b.txt`, both holding [`TREE_FILE_CONTENT`].
    fn disk_tree() -> TempDir {
        let tree = tempfile::tempdir().unwrap();
        fs::write(tree.path().join("a.txt"), TREE_FILE_CONTENT).unwrap();
        fs::create_dir(tree.path().join("sub")).unwrap();
        fs::write(tree.path().join("sub/b.txt"), TREE_FILE_CONTENT).unwrap();
        tree
    }

    /// A host on the real disk that refuses to clear any path through
    /// [`DENIED_DIR`] or create one through [`READONLY_DIR`]. `root` is
    /// spelled with `/` so a Windows drive path reaches monty as written.
    fn run_on_disk(
        code: &str,
        root: &Path,
        limits: ResourceLimits,
    ) -> Result<InterpreterResult, InterpreterError> {
        let files: FileFn = Box::new(|op| {
            let done = match op {
                FileOp::Inspect(path) if path.contains(DENIED_DIR) => {
                    return Err(INSPECT_DENIED_ERR.to_owned());
                }
                FileOp::Mkdir(dir) if dir.path.contains(READONLY_DIR) => {
                    return Err(MKDIR_DENIED_ERR.to_owned());
                }
                FileOp::Read(path) => return fs::read_to_string(path).map_err(|e| e.to_string()),
                FileOp::Inspect(_) => Ok(()),
                FileOp::Write(write) => OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(write.append)
                    .truncate(!write.append)
                    .open(&write.path)
                    .and_then(|mut file| file.write_all(write.content.as_bytes())),
                FileOp::Mkdir(dir) if dir.parents => fs::create_dir_all(&dir.path),
                FileOp::Mkdir(dir) => fs::create_dir(&dir.path),
            };
            done.map(|()| String::new()).map_err(|e| e.to_string())
        });
        let root = root.to_string_lossy().replace('\\', "/");
        run(
            code,
            &format!("import os\nfrom pathlib import Path\nroot = {root:?}"),
            &empty_tools(),
            None,
            Some(&files),
            limits,
            &mut |_| {},
        )
    }

    fn run_with_preamble(
        code: &str,
        preamble: &str,
    ) -> Result<InterpreterResult, InterpreterError> {
        run(
            code,
            preamble,
            &empty_tools(),
            None,
            None,
            default_limits(),
            &mut |_| {},
        )
    }

    fn default_limits() -> ResourceLimits {
        limits(Duration::from_secs(30), DEFAULT_MAX_MEMORY)
    }

    fn small_memory_limits() -> ResourceLimits {
        limits(Duration::from_secs(30), SMALL_MAX_MEMORY)
    }

    fn assert_memory_error(err: InterpreterError) {
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains(MEMORY_ERR_FRAGMENT), "got: {msg}");
    }

    fn empty_tools() -> HashMap<String, ToolFn> {
        HashMap::new()
    }

    fn reported_lines(msg: &str) -> Vec<usize> {
        let prefix = format!("File \"{SCRIPT_NAME}\", line ");
        msg.lines()
            .filter_map(|l| l.trim_start().strip_prefix(&prefix))
            .filter_map(split_line_number)
            .map(|(n, _)| n)
            .collect()
    }

    fn stub_tools(names: &[&str]) -> HashMap<String, ToolFn> {
        names
            .iter()
            .map(|&n| {
                let f: ToolFn = Box::new(|_, _, _| Ok(json!(null)));
                (n.into(), f)
            })
            .collect()
    }

    #[test]
    fn memory_limit_raises_memory_error() {
        let err = run_code(
            OVER_LIMIT_ALLOC,
            &empty_tools(),
            None,
            small_memory_limits(),
        )
        .unwrap_err();
        assert_memory_error(err);
    }

    /// A run that rebased the shared baseline on entry would forgive an
    /// already running one its whole usage.
    #[test]
    fn concurrent_memory_limited_runs_both_enforce() {
        let spawn = || {
            std::thread::spawn(|| {
                run_code(
                    OVER_LIMIT_ALLOC,
                    &empty_tools(),
                    None,
                    small_memory_limits(),
                )
                .unwrap_err()
            })
        };
        for handle in [spawn(), spawn()] {
            assert_memory_error(handle.join().unwrap());
        }
    }

    /// Shape of workflow mode: a script awaits `task`, and the subagent it
    /// waits on runs a script of its own. Scopes that serialized would sit
    /// on each other forever.
    #[test]
    fn nested_run_from_a_tool_does_not_deadlock() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let subagent: ToolFn = Box::new(|_, _, _| {
                let inner = std::thread::spawn(|| {
                    run_code("2 + 3", &empty_tools(), None, default_limits()).unwrap()
                });
                Ok(inner.join().unwrap().output.unwrap())
            });
            let tools = HashMap::from([("task".to_owned(), subagent)]);
            let _ = tx.send(run_code("task()", &tools, None, default_limits()));
        });
        let result = rx.recv_timeout(NESTED_TIMEOUT).expect(NESTED_DEADLOCK);
        assert_eq!(result.unwrap().output, Some(json!(5)));
    }

    #[test]
    fn memory_limit_not_tripped_by_small_workload() {
        let result = run_code(
            "sum([i for i in range(1000)])",
            &empty_tools(),
            None,
            small_memory_limits(),
        )
        .unwrap();
        assert_eq!(result.output, Some(json!(499500)));
    }

    #[test]
    fn simple_expression() {
        let result = run_code("2 + 3", &empty_tools(), None, default_limits()).unwrap();
        assert_eq!(result.output, Some(json!(5)));
        assert!(result.stdout.is_empty());
    }

    #[test]
    fn print_output() {
        let result = run_code(
            "print('hello world')",
            &empty_tools(),
            None,
            default_limits(),
        )
        .unwrap();
        assert_eq!(result.stdout.trim(), "hello world");
    }

    #[test]
    fn tool_call_positional() {
        let mut tools: HashMap<String, ToolFn> = HashMap::new();
        tools.insert(
            "echo".into(),
            Box::new(|_, args, _| Ok(args.first().cloned().unwrap_or(json!(null)))),
        );
        let result = run_code("echo(42)", &tools, None, default_limits()).unwrap();
        assert_eq!(result.output, Some(json!(42)));
    }

    #[test]
    fn tool_call_kwargs() {
        let mut tools: HashMap<String, ToolFn> = HashMap::new();
        tools.insert(
            "greet".into(),
            Box::new(|_, _, kwargs| {
                let name = kwargs
                    .iter()
                    .find(|(k, _)| k == "name")
                    .map(|(_, v)| v.as_str().unwrap_or("unknown").to_string())
                    .unwrap_or_default();
                Ok(json!(format!("hello {name}")))
            }),
        );
        let result = run_code("greet(name='world')", &tools, None, default_limits()).unwrap();
        assert_eq!(result.output, Some(json!("hello world")));
    }

    #[test]
    fn parse_error() {
        let err = run_code("def", &empty_tools(), None, default_limits()).unwrap_err();
        assert!(matches!(err, InterpreterError::Parse(_)));
    }

    #[test]
    fn unknown_tool_raises_name_error() {
        let err = run_code("nonexistent()", &empty_tools(), None, default_limits()).unwrap_err();
        assert!(
            matches!(err, InterpreterError::Runtime(_)),
            "expected Runtime NameError, got {err:?}"
        );
    }

    #[test]
    fn tool_error_propagates() {
        let mut tools: HashMap<String, ToolFn> = HashMap::new();
        tools.insert(
            "fail".into(),
            Box::new(|_, _, _| Err("intentional failure".into())),
        );
        let err = run_code("fail()", &tools, None, default_limits()).unwrap_err();
        assert!(matches!(err, InterpreterError::ToolCall { .. }));
    }

    #[test_case("open(path).read()" ; "open_read")]
    #[test_case("from pathlib import Path\nPath(path).read_text()" ; "pathlib_read_text")]
    fn file_reads_are_served_by_the_host(code: &str) {
        let result = run_with_files(code, &memory_fs(), false).unwrap();
        assert_eq!(result.output, Some(json!(FILE_CONTENT)));
    }

    #[test_case("with open(path, 'w') as f:\n    f.write('a')\n    f.write('b')", "ab" ; "write_mode_replaces")]
    #[test_case("open(path, 'w')", "" ; "write_mode_without_writes_empties")]
    #[test_case("with open(path, 'a') as f:\n    f.write('a')\n    f.write('b')", "oldab" ; "append_mode_appends")]
    #[test_case("from pathlib import Path\nPath(path).write_text('ab')", "ab" ; "pathlib_write_text")]
    #[test_case("open(path, 'w').write('1')\nopen('./' + path, 'w').write('2')\nopen(path, 'w').write('3')", "3" ; "one_file_two_spellings")]
    fn buffered_writes_reach_the_host_once(code: &str, expected: &str) {
        let fs = memory_fs();
        run_with_files(code, &fs, false).unwrap();
        assert_eq!(content(&fs), json!(expected));
        assert_eq!(fs.borrow().writes, 1);
    }

    #[test]
    fn buffered_writes_land_when_the_script_raises() {
        let fs = memory_fs();
        let code = format!("open(path, 'w').write('{WRITTEN}')\nraise ValueError()");
        run_with_files(&code, &fs, false).unwrap_err();
        assert_eq!(content(&fs), json!(WRITTEN));
    }

    #[test_case("open(path).read()", false ; "read")]
    #[test_case("peek()", false ; "tool_call")]
    #[test_case("await peek()", true ; "awaited_tool_call")]
    fn buffered_writes_land_before_they_can_be_seen(expr: &str, async_tools: bool) {
        let code = format!("open(path, 'w').write('{WRITTEN}')\n{expr}");
        let result = run_with_files(&code, &memory_fs(), async_tools).unwrap();
        assert_eq!(result.output, Some(json!(WRITTEN)));
    }

    /// A query flushes before the host is asked, so the writes are counted
    /// even though this host refuses every query.
    #[test_case("'elsewhere.txt'", 1 ; "unrelated_path_keeps_the_buffer")]
    #[test_case("path", 4 ; "the_file_itself")]
    #[test_case("'.'", 4 ; "a_dir_above_it")]
    fn queries_flush_only_writes_they_could_see(queried: &str, expected_writes: usize) {
        let fs = memory_fs();
        let code = format!(
            "import os\nf = open(path, 'w')\nfor _ in range(3):\n    try:\n        os.path.exists({queried})\n    except RuntimeError:\n        pass\n    f.write('x')"
        );
        run_with_files(&code, &fs, false).unwrap();
        assert_eq!(content(&fs), json!("xxx"));
        assert_eq!(fs.borrow().writes, expected_writes);
    }

    #[test_case("", false ; "at_the_end")]
    #[test_case("peek()", false ; "before_a_tool_call")]
    #[test_case("await peek()", true ; "before_an_awaited_tool_call")]
    fn failed_buffered_write_fails_the_run(expr: &str, async_tools: bool) {
        let code = format!("open(locked, 'w').write('x')\n{expr}");
        let err = run_with_files(&code, &memory_fs(), async_tools).unwrap_err();
        assert!(
            matches!(&err, InterpreterError::FileWrite(msg) if msg.contains(WRITE_DENIED_ERR)),
            "got {err:?}"
        );
    }

    #[test_case("open('missing.txt').read()", MISSING_FILE_ERR ; "host_error")]
    #[test_case("open(path, 'rb')", BINARY_FILES_UNSUPPORTED ; "binary_mode")]
    #[test_case("open(locked, 'w').write('x'); open(locked).read()", WRITE_DENIED_ERR ; "failed_write_before_a_read")]
    fn file_errors_raise_a_catchable_os_error(expr: &str, message: &str) {
        let code = format!("try:\n    {expr}\nexcept OSError as e:\n    err = str(e)\nerr");
        let result = run_with_files(&code, &memory_fs(), false).unwrap();
        let err = result.output.unwrap();
        assert!(err.as_str().unwrap().contains(message), "got {err}");
    }

    #[test]
    fn os_calls_are_fatal_without_a_file_host() {
        let code = format!("open('{FILE_PATH}')");
        let err = run_code(&code, &empty_tools(), None, default_limits()).unwrap_err();
        assert!(matches!(err, InterpreterError::Sandboxed(_)), "got {err:?}");
    }

    #[test]
    fn getcwd_reports_the_process_cwd() {
        let result = run_code(
            "import os\nos.getcwd()",
            &empty_tools(),
            None,
            default_limits(),
        );
        assert_eq!(result.unwrap().output, Some(json!(sandbox_cwd().unwrap())));
    }

    #[test_case("[os.path.exists(root + '/a.txt'), os.path.exists(root + '/nope'), os.path.isfile(root + '/sub'), Path(root + '/sub').is_dir()]", json!([true, false, false, true]) ; "predicates")]
    #[test_case("os.listdir(root)", json!(["a.txt", "sub"]) ; "listdir")]
    #[test_case("Path(root + '/a.txt').stat().st_size", json!(TREE_FILE_CONTENT.len()) ; "stat")]
    #[test_case("sorted(p.name for p in Path(root).rglob('*.txt'))", json!(["a.txt", "b.txt"]) ; "rglob")]
    #[test_case("[len(files) for _, _, files in os.walk(root)]", json!([1, 1]) ; "walk")]
    #[test_case("Path('x.txt').absolute() == Path(os.getcwd()) / 'x.txt'", json!(true) ; "absolute")]
    #[test_case("p = Path(root + '/sub/../nope/x.txt').resolve()\n[p.name, p.parent.name, p.is_absolute()]", json!(["x.txt", "nope", true]) ; "resolve_missing_tail")]
    #[test_case("open(root + '/new.txt', 'w').write('x')\nos.path.exists(root + '/new.txt')", json!(true) ; "sees_buffered_writes")]
    #[test_case("os.makedirs(root + '/x/y')\nos.path.isdir(root + '/x/y')", json!(true) ; "makedirs")]
    #[test_case("Path(root + '/sub').mkdir(exist_ok=True)\nTrue", json!(true) ; "mkdir_exist_ok")]
    fn disk_queries_answer_like_cpython(code: &str, expected: Value) {
        let tree = disk_tree();
        let result = run_on_disk(code, tree.path(), default_limits()).unwrap();
        assert_eq!(result.output, Some(expected));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_follows_symlinks() {
        let tree = disk_tree();
        std::os::unix::fs::symlink(tree.path().join("sub"), tree.path().join("link")).unwrap();
        let code = "real = Path(root + '/sub/x').resolve()\n[Path(root + '/link/x').resolve() == real, Path(root + '/link/nope/../x').resolve() == real, Path(root + '/link').is_symlink()]";
        let result = run_on_disk(code, tree.path(), default_limits()).unwrap();
        assert_eq!(result.output, Some(json!([true, true, true])));
    }

    /// The host would expand `~` or a relative path its own way, so it must
    /// get the exact absolute path the sandbox reads.
    #[test_case("maki-never-created" ; "relative")]
    #[test_case("~/maki-never-created" ; "tilde_is_literal_like_cpython")]
    fn host_is_asked_about_the_absolute_path(path: &str) {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let record = Rc::clone(&seen);
        let files: FileFn = Box::new(move |op| match op {
            FileOp::Inspect(path) => {
                record.borrow_mut().push(path);
                Ok(String::new())
            }
            FileOp::Mkdir(dir) => {
                record.borrow_mut().push(dir.path);
                Err(MKDIR_DENIED_ERR.to_owned())
            }
            FileOp::Read(_) | FileOp::Write(_) => Err(UNSERVED_OP_ERR.to_owned()),
        });
        let code = format!("import os\ntry:\n    os.mkdir({path:?})\nexcept OSError:\n    pass");
        run(
            &code,
            NO_PREAMBLE,
            &empty_tools(),
            None,
            Some(&files),
            default_limits(),
            &mut |_| {},
        )
        .unwrap();
        let expected = current_dir().unwrap().join(path);
        let seen = seen.borrow();
        assert!(
            seen.len() == 2 && seen.iter().all(|p| Path::new(p) == expected),
            "got {seen:?}"
        );
    }

    /// A refusal must not be an `OSError`, or a walk or predicate swallows it.
    #[test_case("os.stat(root + '/nope')", "FileNotFoundError", "No such file" ; "stat_missing")]
    #[test_case("os.listdir(root + '/a.txt')", "NotADirectoryError", "Not a directory" ; "listdir_on_a_file")]
    #[test_case("os.mkdir(root + '/sub')", "FileExistsError", "File exists" ; "mkdir_existing")]
    #[test_case("Path(root + '/a.txt').mkdir(exist_ok=True)", "FileExistsError", "File exists" ; "mkdir_over_a_file")]
    #[test_case("os.mkdir(root + '/readonly')", "OSError", MKDIR_DENIED_ERR ; "mkdir_refused")]
    #[test_case("os.makedirs(root + '/denied', exist_ok=True)", "RuntimeError", INSPECT_DENIED_ERR ; "mkdir_needs_the_path_cleared")]
    #[test_case("os.path.exists(root + '/denied')", "RuntimeError", INSPECT_DENIED_ERR ; "predicate_refused")]
    #[test_case("list(os.walk(root + '/denied'))", "RuntimeError", INSPECT_DENIED_ERR ; "walk_refused")]
    #[test_case("os.remove(root + '/a.txt')", "RuntimeError", REMOVAL_UNSUPPORTED ; "remove")]
    #[test_case("Path(root + '/a.txt').read_bytes()", "RuntimeError", BINARY_FILES_UNSUPPORTED ; "read_bytes")]
    #[test_case("os.urandom(MAX + 1)", "ValueError", "at most" ; "urandom_over_the_cap")]
    fn os_errors_raise_their_cpython_type(expr: &str, exc_type: &str, fragment: &str) {
        let tree = disk_tree();
        let code = format!(
            "MAX = {MAX_URANDOM_BYTES}\ntry:\n    {expr}\n    err = None\nexcept Exception as e:\n    err = f'{{type(e).__name__}}: {{e}}'\nerr"
        );
        let result = run_on_disk(&code, tree.path(), default_limits()).unwrap();
        let err = result.output.unwrap();
        let err = err.as_str().unwrap();
        assert!(
            err.starts_with(&format!("{exc_type}: ")) && err.contains(fragment),
            "got {err}"
        );
    }

    #[test]
    fn only_allowlisted_env_vars_are_visible() {
        assert!(var(HIDDEN_ENV_VAR).is_ok(), "{HIDDEN_ENV_VAR} must be set");
        let code = format!(
            "[os.getenv('HOME'), os.getenv('{HIDDEN_ENV_VAR}', 'unset'), '{HIDDEN_ENV_VAR}' in os.environ]"
        );
        let result = run_on_disk(&code, Path::new("."), default_limits()).unwrap();
        assert_eq!(
            result.output,
            Some(json!([var("HOME").ok(), "unset", false]))
        );
    }

    #[test_case("len(os.urandom(16))", json!(16) ; "urandom")]
    #[test_case("import time\ntime.sleep(0.001)\nTrue", json!(true) ; "time_sleep")]
    #[test_case("import asyncio\nawait asyncio.sleep(0.001)\nTrue", json!(true) ; "asyncio_sleep")]
    fn host_services_answer(code: &str, expected: Value) {
        let result = run_on_disk(code, Path::new("."), default_limits()).unwrap();
        assert_eq!(result.output, Some(expected));
    }

    /// `time.sleep(1)` is refused before it starts. A 1ms sleep lasts at least
    /// 1ms, so it spends the whole budget and the walk after it is refused.
    #[test_case("time.sleep(1)" ; "sleep_past_the_budget")]
    #[test_case("time.sleep(0.001)\nlist(os.walk(root))" ; "walk_cannot_absorb_it")]
    #[test_case("try:\n    time.sleep(1)\nexcept Exception:\n    pass" ; "script_cannot_catch_it")]
    fn host_time_is_bounded(code: &str) {
        let limits = default_limits().max_total_sleep(Duration::from_millis(1));
        let code = format!("import time\n{code}");
        let err = run_on_disk(&code, Path::new("."), limits).unwrap_err();
        assert!(err.to_string().contains(HOST_TIME_EXCEEDED), "got {err:?}");
    }

    /// If the walk could swallow the failed write as an `OSError`, it would be
    /// lost for good.
    #[test]
    fn failed_write_sent_by_a_walk_ends_the_run() {
        let tree = disk_tree();
        let code = format!(
            "open(root + '/{MISSING_DIR}/x.txt', 'w').write('x')\ntry:\n    list(os.walk(root))\nexcept Exception:\n    pass"
        );
        let err = run_on_disk(&code, tree.path(), default_limits()).unwrap_err();
        assert!(
            matches!(&err, InterpreterError::FileWrite(msg) if msg.contains(MISSING_DIR)),
            "got {err:?}"
        );
    }

    #[test_case("/proj", false, "/proj" ; "unix")]
    #[test_case(r"C:\proj", true, "/C:/proj" ; "windows")]
    #[test_case(r"C:\", true, "/C:" ; "windows_drive_root")]
    #[test_case(r"\\server\share\proj", true, "/server/share/proj" ; "windows_unc")]
    #[test_case(r"\\?\C:\proj", true, "/?/C:/proj" ; "windows_verbatim")]
    fn sandbox_cwd_is_canonical(host: &str, windows: bool, expected: &str) {
        assert_eq!(to_sandbox_cwd(host, windows), expected);
    }

    #[test_case("/proj", "/proj/a.txt", false, "a.txt" ; "relative")]
    #[test_case("/proj", "/proj/./a.txt", false, "./a.txt" ; "dot")]
    #[test_case("/proj", "/proj/../a.txt", false, "../a.txt" ; "above_the_cwd")]
    #[test_case("/proj", "/proj/~/a.txt", false, "~/a.txt" ; "home")]
    #[test_case("/proj", "/projx/a.txt", false, "/projx/a.txt" ; "sibling_sharing_the_prefix")]
    #[test_case("/proj", "/proj", false, "/proj" ; "the_cwd_itself")]
    #[test_case("/proj", "/etc/a.txt", false, "/etc/a.txt" ; "absolute")]
    #[test_case("/", "/a.txt", false, "a.txt" ; "root_cwd")]
    #[test_case("/C:/proj", r"/C:/proj/D:\x\a.txt", true, r"D:\x\a.txt" ; "windows_drive_path")]
    #[test_case("/C:/proj", "/C:/proj/C:a.txt", true, "C:a.txt" ; "windows_drive_relative")]
    #[test_case("/C:/proj", r"/C:/proj/\a.txt", true, r"\a.txt" ; "windows_root_relative")]
    #[test_case("/C:/proj", r"/C:/proj/\\srv\share\a.txt", true, r"\\srv\share\a.txt" ; "windows_unc")]
    #[test_case("/C:", "/C:/a.txt", true, "a.txt" ; "windows_drive_root_cwd")]
    #[test_case("/C:/proj", "/C:/proj", true, "C:/proj" ; "windows_the_cwd_itself")]
    #[test_case("/C:/proj", "/D:/a.txt", true, "D:/a.txt" ; "windows_posix_spelled_drive")]
    #[test_case("/C:/proj", "/tmp/a.txt", true, "/tmp/a.txt" ; "windows_posix_absolute")]
    fn host_gets_the_path_as_written(cwd: &str, sandbox: &str, windows: bool, expected: &str) {
        assert_eq!(written_path(cwd, sandbox.to_owned(), windows), expected);
    }

    #[test_case("a/b.txt", "a", true ; "under")]
    #[test_case("a/b.txt", "a/b.txt", true ; "itself")]
    #[test_case("a/b.txt", ".", true ; "cwd")]
    #[test_case("a/../c/b.txt", "c", true ; "dotdot")]
    #[test_case("ab/c.txt", "a", false ; "sibling_sharing_the_prefix")]
    #[test_case("a", "a/b", false ; "above")]
    #[test_case("~/a.txt", "elsewhere", true ; "home_is_expanded_by_the_host")]
    fn write_is_within_queried_path(written: &str, dir: &str, expected: bool) {
        assert_eq!(is_within(written, dir), expected);
    }

    #[test]
    fn streaming_collects_stdout() {
        let mut called = false;
        let result = run(
            "print('hello')\nprint('world')",
            NO_PREAMBLE,
            &empty_tools(),
            None,
            None,
            default_limits(),
            &mut |_| {
                called = true;
            },
        )
        .unwrap();
        assert_eq!(result.stdout.trim(), "hello\nworld");
        assert!(called);
    }

    #[test]
    fn async_gather_resolves_concurrently() {
        let code = r#"
import asyncio
async def main():
    a, b = await asyncio.gather(tool_a(), tool_b())
    return f'{a}|{b}'
await main()
"#;
        let tools = stub_tools(&["tool_a", "tool_b"]);

        let resolver: AsyncResolver = Box::new(|pending: Vec<PendingCall>| {
            assert_eq!(pending.len(), 2);
            Ok(pending
                .into_iter()
                .map(|pc| {
                    let val = match pc.name.as_str() {
                        "tool_a" => json!("a_val"),
                        "tool_b" => json!("b_val"),
                        _ => json!(null),
                    };
                    (pc.call_id, Ok(val))
                })
                .collect())
        });

        let result = run_code(code, &tools, Some(&resolver), default_limits()).unwrap();
        assert_eq!(result.output, Some(json!("a_val|b_val")));
    }

    #[test]
    fn sequential_await_calls_resolver_per_batch() {
        let code = r#"
import asyncio
async def main():
    a = await tool_a()
    b = await tool_b()
    return f'{a}|{b}'
await main()
"#;
        let tools = stub_tools(&["tool_a", "tool_b"]);

        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = call_count.clone();
        let resolver: AsyncResolver = Box::new(move |pending: Vec<PendingCall>| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Ok(pending
                .into_iter()
                .map(|pc| (pc.call_id, Ok(json!(format!("result:{}", pc.name)))))
                .collect())
        });

        let result = run_code(code, &tools, Some(&resolver), default_limits()).unwrap();
        assert!(result.output.is_some());
        assert!(
            call_count.load(Ordering::SeqCst) >= 2,
            "resolver should be called at least twice for sequential awaits"
        );
    }

    #[test]
    fn resolver_wait_does_not_count_against_timeout() {
        const TIMEOUT: Duration = Duration::from_millis(1000);
        const WAIT: Duration = Duration::from_millis(1100);
        // Monty's tracker must stop the clock while we sit in the resolver.
        // Two awaits so a clock paused only for the first wait still fails,
        // and generous durations so a busy machine cannot tip the balance.
        let code = r#"
async def main():
    a = await slow()
    b = await slow()
    return a + b
await main()
"#;
        let tools = stub_tools(&["slow"]);
        let resolver: AsyncResolver = Box::new(|pending: Vec<PendingCall>| {
            std::thread::sleep(WAIT);
            Ok(pending
                .into_iter()
                .map(|pc| (pc.call_id, Ok(json!("done"))))
                .collect())
        });

        let lims = limits(TIMEOUT, DEFAULT_MAX_MEMORY);
        let result = run_code(code, &tools, Some(&resolver), lims).unwrap();
        assert_eq!(result.output, Some(json!("donedone")));
    }

    #[test_case("x = 1\nprint(boom_undefined)\n", "import re\nimport asyncio\n" ; "runtime_frame")]
    #[test_case("x = 1\nprint(boom_undefined)\n", "import re" ; "preamble_without_trailing_newline")]
    #[test_case("x = 1\ndef\n", "import re\n" ; "parse_error")]
    fn traceback_lines_count_from_the_users_first_line(code: &str, preamble: &str) {
        let err = run_with_preamble(code, preamble).unwrap_err().to_string();
        assert_eq!(reported_lines(&err), [USER_ERROR_LINE], "got: {err}");
    }

    /// The error must survive even though the frame that raised it is gone.
    #[test]
    fn preamble_frames_are_dropped_from_traceback() {
        let err = run_with_preamble("helper()\n", RAISING_PREAMBLE)
            .unwrap_err()
            .to_string();
        assert_eq!(reported_lines(&err), [1], "preamble frame leaked: {err}");
        assert!(!err.contains("raise ValueError"), "excerpt leaked: {err}");
        assert!(err.contains("inside preamble"), "error lost: {err}");
    }

    /// The `gather` helper in the code_execution preamble awaits calls one at a
    /// time so it can catch each failure alone. That stays concurrent only
    /// because every call is already pending when the first await parks, and
    /// here is where we would notice if that stopped being true.
    #[test]
    fn calls_made_before_the_first_await_resolve_in_one_batch() {
        let code = r#"
async def main():
    out = []
    for call in [tool_a(), tool_fail(), tool_b()]:
        try:
            out.append(await call)
        except Exception as e:
            out.append(str(e))
    return out
await main()
"#;
        let tools = stub_tools(&["tool_a", "tool_b", "tool_fail"]);
        let batches = Arc::new(AtomicUsize::new(0));
        let counted = batches.clone();
        let resolver: AsyncResolver = Box::new(move |pending: Vec<PendingCall>| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(pending
                .into_iter()
                .map(|pc| match pc.name.as_str() {
                    "tool_fail" => (pc.call_id, Err("boom".to_owned())),
                    name => (pc.call_id, Ok(json!(name))),
                })
                .collect())
        });

        let result = run_code(code, &tools, Some(&resolver), default_limits()).unwrap();
        assert_eq!(result.output, Some(json!(["tool_a", "boom", "tool_b"])));
        assert_eq!(batches.load(Ordering::SeqCst), 1, "calls must be batched");
    }

    #[test]
    fn async_tool_error_propagates_to_python() {
        let code = r#"
import asyncio
async def main():
    a, b = await asyncio.gather(tool_ok(), tool_fail())
    return 'should not reach'
await main()
"#;
        let tools = stub_tools(&["tool_ok", "tool_fail"]);

        let resolver: AsyncResolver = Box::new(|pending: Vec<PendingCall>| {
            Ok(pending
                .into_iter()
                .map(|pc| match pc.name.as_str() {
                    "tool_fail" => (pc.call_id, Err("boom".into())),
                    _ => (pc.call_id, Ok(json!("ok"))),
                })
                .collect())
        });

        let err = run_code(code, &tools, Some(&resolver), default_limits()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("boom"),
            "expected error message containing 'boom', got {msg}"
        );
    }
}
