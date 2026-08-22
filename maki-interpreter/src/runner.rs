//! Drives the monty interpreter through its execution states.
//! Sync tool calls resolve immediately; async (`await`) calls are batched via `ResolveFutures`
//! and dispatched concurrently through [`AsyncResolver`], with results fed back one by one.
//! `OsCall` is always rejected; the sandbox never touches the OS directly.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult,
    PrintWriter, PrintWriterCallback, ResourceLimits, ResourceTracker,
};
use serde_json::Value;
use tracing::debug;

use crate::alloc::SandboxScope;
use crate::convert::{json_to_monty, monty_to_json};
use crate::error::InterpreterError;

const DEFAULT_MAX_RECURSION: usize = 100;
const SCRIPT_NAME: &str = "agent.py";

pub type ToolFn = Box<dyn Fn(&str, Vec<Value>, Vec<(String, Value)>) -> Result<Value, String>>;

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
    limits: ResourceLimits,
    print_writer: &mut PrintWriter<'_>,
) -> Result<Option<Value>, InterpreterError> {
    let _sandbox = limits.max_memory.is_some().then(SandboxScope::enter);
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
            RunProgress::OsCall(_) => {
                return Err(InterpreterError::Sandboxed(
                    "OS calls are not permitted".into(),
                ));
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

    fn run_code(
        code: &str,
        tools: &HashMap<String, ToolFn>,
        resolver: Option<&AsyncResolver>,
        limits: ResourceLimits,
    ) -> Result<InterpreterResult, InterpreterError> {
        run(code, NO_PREAMBLE, tools, resolver, limits, &mut |_| {})
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

    #[test]
    fn streaming_collects_stdout() {
        let mut called = false;
        let result = run(
            "print('hello')\nprint('world')",
            NO_PREAMBLE,
            &empty_tools(),
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
