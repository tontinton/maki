//! Drives the monty interpreter through its execution states.
//! Sync tool calls resolve immediately; async (`await`) calls are batched via `ResolveFutures`
//! and dispatched concurrently through [`AsyncResolver`], with results fed back one by one.
//! `OsCall` is always rejected; the sandbox never touches the OS directly.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Duration;

use monty::{
    ExcType, ExtFunctionResult, LimitedTracker, MontyException, MontyObject, MontyRun,
    NameLookupResult, PrintWriter, PrintWriterCallback, ResourceLimits, RunProgress,
};
use serde_json::Value;
use tracing::debug;

use crate::convert::{json_to_monty, monty_to_json};
use crate::error::InterpreterError;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_MEMORY: usize = 50 * 1024 * 1024;
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

pub fn run(
    code: &str,
    tools: &HashMap<String, ToolFn>,
    resolver: Option<&AsyncResolver>,
    limits: ResourceLimits,
) -> Result<InterpreterResult, InterpreterError> {
    let mut stdout = String::new();
    let mut print_writer = PrintWriter::CollectString(&mut stdout);
    let output = run_inner(code, tools, resolver, limits, &mut print_writer)?;
    Ok(InterpreterResult { output, stdout })
}

pub fn run_streaming(
    code: &str,
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
    let mut print_writer = PrintWriter::Callback(&mut writer);
    let output = run_inner(code, tools, resolver, limits, &mut print_writer)?;
    let stdout = writer.buffer;
    Ok(InterpreterResult { output, stdout })
}

fn run_inner(
    code: &str,
    tools: &HashMap<String, ToolFn>,
    resolver: Option<&AsyncResolver>,
    limits: ResourceLimits,
    print_writer: &mut PrintWriter<'_>,
) -> Result<Option<Value>, InterpreterError> {
    let runner = MontyRun::new(code.to_owned(), SCRIPT_NAME, vec![])
        .map_err(|e| InterpreterError::Parse(e.to_string()))?;

    let tracker = LimitedTracker::new(limits);

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

pub fn default_limits() -> ResourceLimits {
    limits_with_timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

pub fn limits_with_timeout(timeout: Duration) -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(timeout)
        .max_memory(DEFAULT_MAX_MEMORY)
        .max_recursion_depth(Some(DEFAULT_MAX_RECURSION))
}

pub fn limits(timeout: Duration, max_memory: usize) -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(timeout)
        .max_memory(max_memory)
        .max_recursion_depth(Some(DEFAULT_MAX_RECURSION))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn empty_tools() -> HashMap<String, ToolFn> {
        HashMap::new()
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
    fn simple_expression() {
        let result = run("2 + 3", &empty_tools(), None, default_limits()).unwrap();
        assert_eq!(result.output, Some(json!(5)));
        assert!(result.stdout.is_empty());
    }

    #[test]
    fn print_output() {
        let result = run(
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
        let result = run("echo(42)", &tools, None, default_limits()).unwrap();
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
        let result = run("greet(name='world')", &tools, None, default_limits()).unwrap();
        assert_eq!(result.output, Some(json!("hello world")));
    }

    #[test]
    fn parse_error() {
        let err = run("def", &empty_tools(), None, default_limits()).unwrap_err();
        assert!(matches!(err, InterpreterError::Parse(_)));
    }

    #[test]
    fn unknown_tool_raises_name_error() {
        let err = run("nonexistent()", &empty_tools(), None, default_limits()).unwrap_err();
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
        let err = run("fail()", &tools, None, default_limits()).unwrap_err();
        assert!(matches!(err, InterpreterError::ToolCall { .. }));
    }

    #[test]
    fn streaming_collects_stdout() {
        let mut called = false;
        let result = run_streaming(
            "print('hello')\nprint('world')",
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

        let result = run(code, &tools, Some(&resolver), default_limits()).unwrap();
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

        let result = run(code, &tools, Some(&resolver), default_limits()).unwrap();
        assert!(result.output.is_some());
        assert!(
            call_count.load(Ordering::SeqCst) >= 2,
            "resolver should be called at least twice for sequential awaits"
        );
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

        let err = run(code, &tools, Some(&resolver), default_limits()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("boom"),
            "expected error message containing 'boom', got {msg}"
        );
    }
}
