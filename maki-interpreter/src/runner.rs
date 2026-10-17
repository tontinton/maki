//! Drives the monty interpreter through its execution states.
//! Sync tool calls resolve immediately; async (`await`) calls are batched via `ResolveFutures`
//! and dispatched concurrently through [`AsyncResolver`], with results fed back one by one.
//! The sandbox never touches the OS directly. Text files opened with `open()` or `pathlib`
//! are served by the host's [`FileFn`], and every other `OsCall` is refused.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::absolute;
use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyFileHandle, MontyObject,
    NameLookupResult, OpenCallArgs, OsFunctionCall, PathStringDataArgs, PrintWriter,
    PrintWriterCallback, ResourceLimits, ResourceTracker,
};
use serde_json::Value;
use tracing::{debug, warn};

use crate::alloc::SandboxScope;
use crate::convert::{json_to_monty, monty_to_json};
use crate::error::InterpreterError;

const DEFAULT_MAX_RECURSION: usize = 100;
const SCRIPT_NAME: &str = "agent.py";
const OS_CALLS_DENIED: &str = "OS calls are not permitted";
const BINARY_FILES_UNSUPPORTED: &str = "binary files are not supported, open in text mode";

pub type ToolFn =
    Box<dyn Fn(&str, Vec<Value>, Vec<(String, Value)>) -> Result<Value, String> + Send + Sync>;

pub struct FileWrite {
    pub path: String,
    pub content: String,
    pub append: bool,
}

pub enum FileOp {
    Read(String),
    Write(FileWrite),
}

/// `Ok` holds the content for a read and is ignored for a write.
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
    let mut host = FileHost {
        files,
        pending: Vec::new(),
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
    host: &mut FileHost<'_>,
    limits: ResourceLimits,
    print_writer: &mut PrintWriter<'_>,
) -> Result<Option<Value>, InterpreterError> {
    let runner = MontyRun::new(script, SCRIPT_NAME, vec![], CompileOptions::default())
        .map_err(|e| InterpreterError::Parse(e.to_string()))?;

    let tracker = ResourceTracker::new(limits);

    let mut progress = runner
        .start(vec![], tracker, print_writer.reborrow())
        .map_err(|e| InterpreterError::Runtime(e.to_string()))?;

    let mut pending_calls: HashMap<u32, PendingCall> = HashMap::new();

    loop {
        match progress {
            RunProgress::Complete(obj) => {
                let output = match &obj {
                    MontyObject::None => None,
                    _ => Some(monty_to_json(&obj)),
                };
                return Ok(output);
            }
            RunProgress::FunctionCall(call) => {
                let name = call.function_name.clone();
                let args_json: Vec<Value> = call.args.iter().map(monty_to_json).collect();
                let kwargs_json: Vec<(String, Value)> = call
                    .kwargs
                    .iter()
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
                    NameLookupResult::Value(MontyObject::Function {
                        name: name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };

                progress = lookup
                    .resume(result, print_writer.reborrow())
                    .map_err(|e| InterpreterError::Runtime(e.to_string()))?;
            }
            RunProgress::OsCall(call) => {
                if host.files.is_none() {
                    return Err(InterpreterError::Sandboxed(OS_CALLS_DENIED.into()));
                }
                progress = call
                    .resume_with(print_writer.reborrow(), |os_call| host.serve(os_call))
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
/// someone could look at the file: a tool call, a read of that path, or the end
/// of the run. A loop of `f.write` then turns into a single host write, and a
/// cancelled run drops what is still waiting here. Paths are matched once made
/// absolute, so `a` and `./a` share one entry and the last write wins.
struct FileHost<'a> {
    files: Option<&'a FileFn>,
    pending: Vec<FileWrite>,
}

impl FileHost<'_> {
    fn serve(&mut self, call: OsFunctionCall) -> ExtFunctionResult {
        let result = match call {
            OsFunctionCall::Open(args) => self.open(args),
            OsFunctionCall::ReadText(path) => self.read(path.into_string()),
            OsFunctionCall::WriteText(args) => self.write(args, false),
            OsFunctionCall::AppendText(args) => self.write(args, true),
            other => return ExtFunctionResult::Error(other.on_no_handler()),
        };
        match result {
            Ok(value) => ExtFunctionResult::Return(value),
            Err(msg) => ExtFunctionResult::Error(MontyException::new(ExcType::OSError, Some(msg))),
        }
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
                path: path.clone(),
                content: String::new(),
                append: false,
            });
        }
        Ok(MontyObject::FileHandle(MontyFileHandle {
            path,
            mode: args.mode,
            position: 0,
        }))
    }

    fn read(&mut self, path: String) -> Result<MontyObject, String> {
        if let Some(index) = self.pending.iter().position(|w| same_file(&w.path, &path)) {
            self.flush_at(index)?;
        }
        self.call(FileOp::Read(path)).map(MontyObject::String)
    }

    /// Monty moves the file position by the number we return, and text
    /// positions count chars, not bytes.
    fn write(&mut self, args: PathStringDataArgs, append: bool) -> Result<MontyObject, String> {
        let written = args.data.chars().count() as i64;
        self.buffer(FileWrite {
            path: args.path.into_string(),
            content: args.data,
            append,
        });
        Ok(MontyObject::Int(written))
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

    /// The error names the path because it often shows up far from the
    /// `write` that caused it.
    fn flush_at(&mut self, index: usize) -> Result<(), String> {
        let write = self.pending.remove(index);
        let path = write.path.clone();
        self.call(FileOp::Write(write))
            .map(drop)
            .map_err(|e| format!("{path}: {e}"))
    }
}

/// Lexical only, like the host tools resolve paths, so symlinks still differ.
fn same_file(a: &str, b: &str) -> bool {
    a == b || matches!((absolute(a), absolute(b)), (Ok(a), Ok(b)) if a == b)
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
        .max_duration(timeout)
        .max_memory(max_memory)
        .max_recursion_depth(DEFAULT_MAX_RECURSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    struct MemoryFs {
        files: HashMap<String, String>,
        writes: usize,
    }

    // `ToolFn` is `Send + Sync`, so the shared fs has to be too.
    type SharedFs = Arc<Mutex<MemoryFs>>;

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
        Arc::new(Mutex::new(MemoryFs {
            files: HashMap::from([(FILE_PATH.to_owned(), FILE_CONTENT.to_owned())]),
            writes: 0,
        }))
    }

    fn content(fs: &SharedFs) -> Value {
        json!(lock(fs).files.get(FILE_PATH))
    }

    fn lock(fs: &SharedFs) -> std::sync::MutexGuard<'_, MemoryFs> {
        fs.lock().expect("memory fs poisoned")
    }

    /// Binds `path`, and `locked` where every write fails. The `peek` tool
    /// returns what `path` holds at the moment it runs.
    fn run_with_files(
        code: &str,
        fs: &SharedFs,
        async_tools: bool,
    ) -> Result<InterpreterResult, InterpreterError> {
        let host_fs = Arc::clone(fs);
        let files: FileFn = Box::new(move |op| {
            let mut fs = lock(&host_fs);
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
            }
        });
        let peek_fs = Arc::clone(fs);
        let peek: ToolFn = Box::new(move |_, _, _| Ok(content(&peek_fs)));
        let resolver_fs = Arc::clone(fs);
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
        assert_eq!(lock(&fs).writes, 1);
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
