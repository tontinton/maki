//! Executes Python in the monty sandbox and bridges tool calls back into the agent.
//!
//! Sync tool calls use `block_on`; async tool calls go through `AsyncResolver` to batch concurrent awaits.
//! Stdout is flushed to the UI on STREAM_FLUSH_INTERVAL so output appears incrementally.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::{Duration, Instant};

use maki_interpreter::runner::{self, ToolFn};
use maki_interpreter::{AsyncResolver, PendingCall};
use maki_tool_macro::Tool;
use serde_json::Value;

use std::sync::Arc;

use crate::cancel::CancelToken;
use crate::permissions::PermissionManager;
use crate::task_set::TaskSet;
use crate::{AgentConfig, AgentEvent, AgentMode, EventSender, ToolInput, ToolOutput};

use smol::future::block_on;

use super::truncate_output;
use super::{Deadline, INTERPRETER_TOOLS};

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const PREAMBLE: &str = "import re\nimport asyncio\nimport sys\nimport os\n";

#[derive(Tool, Debug, Clone)]
pub struct CodeInterpreter {
    #[param(
        description = "Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency."
    )]
    code: String,
    #[param(description = "Timeout in seconds (default 30, max 300)")]
    timeout: Option<u64>,
}

impl CodeInterpreter {
    pub const NAME: &str = "code_execution";
    pub const DESCRIPTION: &str = include_str!("code_execution.md");
    pub const EXAMPLES: Option<&str> = Some(
        r##"[{"code": "files = (await glob(pattern='**/*.rs')).strip().split('\\n')\nresults = await asyncio.gather(*[read(path=f) for f in files if f.strip()])\nfor f, c in zip(files, results):\n    if 'fn main' in c: print(f)"},
            {"code": "result = await grep(pattern='TODO', include='*.rs')\nprint(f\"{len(result.strip().splitlines())} TODOs found\")"},
            {"code": "content = await webfetch(url='https://example.com/docs')\nfor line in content.splitlines():\n    if 'auth' in line.lower(): print(line)"}]"##,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let timeout = Duration::from_secs(
            ctx.deadline.cap_timeout(
                self.timeout
                    .unwrap_or(ctx.config.code_execution_timeout_secs),
            )?,
        );
        let code = self.code.clone();
        let tool_use_id = ctx.tool_use_id.clone();
        let event_tx = ctx.event_tx.clone();
        let mode = ctx.mode.clone();
        let cancel = ctx.cancel.clone();
        let config = ctx.config;
        let permissions = ctx.permissions.clone();
        let deadline = Deadline::after(timeout);
        let limits = runner::limits(timeout, config.interpreter_max_memory_mb * 1024 * 1024);

        // NOTE: cancel races the smol::unblock future. When cancel wins, the
        // blocking thread pool task keeps running until the Python code finishes.
        // There is no safe way to kill a blocking thread.
        ctx.cancel
            .race(smol::unblock(move || {
                let tools =
                    build_tool_fns(&event_tx, &mode, &cancel, deadline, config, &permissions);
                let resolver =
                    build_async_resolver(&event_tx, &mode, &cancel, deadline, config, &permissions);
                let code = format!("{PREAMBLE}{code}");

                let result = if let Some(ref id) = tool_use_id {
                    let mut pending = String::new();
                    let mut last_flush = Instant::now();
                    runner::run_streaming(&code, &tools, Some(&resolver), limits, &mut |line| {
                        pending.push_str(line);
                        if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL && !pending.is_empty() {
                            event_tx.try_send(AgentEvent::ToolOutput {
                                id: id.to_string(),
                                content: pending.clone(),
                            });
                            pending.clear();
                            last_flush = Instant::now();
                        }
                    })
                } else {
                    runner::run(&code, &tools, Some(&resolver), limits)
                }
                .map_err(|e| e.to_string())?;

                let mut output = String::new();
                if !result.stdout.is_empty() {
                    output.push_str(result.stdout.trim_end());
                    output.push('\n');
                }
                if let Some(ref val) = result.output {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    let _ = write!(output, "return: {val}");
                }
                if output.is_empty() {
                    output.push_str("(no output)");
                }

                Ok(ToolOutput::Plain(truncate_output(
                    output,
                    config.max_output_lines,
                    config.max_output_bytes,
                )))
            }))
            .await?
    }

    pub fn start_summary(&self) -> String {
        let lines = self.code.lines().count();
        format!("{lines} lines")
    }
}

impl super::ToolDefaults for CodeInterpreter {
    fn start_input(&self) -> Option<ToolInput> {
        Some(ToolInput::Script {
            language: "python".into(),
            code: self.code.clone(),
        })
    }

    fn start_annotation(&self) -> Option<String> {
        Some(super::timeout_annotation(self.timeout.unwrap_or(30)))
    }

    fn augment_description(description: &mut String, _ctx: &super::DescriptionContext) {
        description.push_str(&super::build_interpreter_tools_description());
    }

    fn permission(&self) -> Option<String> {
        Some("code_execution".into())
    }
}

fn build_tool_fns(
    event_tx: &EventSender,
    mode: &AgentMode,
    cancel: &CancelToken,
    deadline: Deadline,
    config: AgentConfig,
    permissions: &Arc<PermissionManager>,
) -> HashMap<String, ToolFn> {
    let mut tools: HashMap<String, ToolFn> = HashMap::new();

    for &tool_name in INTERPRETER_TOOLS {
        let name = tool_name.to_string();
        let tx = event_tx.clone();
        let mode = mode.clone();
        let cancel = cancel.clone();
        let permissions = Arc::clone(permissions);

        tools.insert(
            name.clone(),
            Box::new(
                move |fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
                    deadline.check()?;

                    let input = build_tool_input(&args, &kwargs)?;
                    let call = super::ToolCall::from_api(fn_name, &input)
                        .map_err(|e| format!("tool parse error: {e}"))?;

                    let mut inner_ctx = super::interpreter_ctx(
                        &mode,
                        &tx,
                        cancel.clone(),
                        Arc::clone(&permissions),
                    );
                    inner_ctx.deadline = deadline;
                    inner_ctx.config = config;
                    let done = block_on(call.execute(&inner_ctx, String::new()));
                    if done.is_error {
                        Err(done.output.as_text())
                    } else {
                        Ok(Value::String(done.output.as_text()))
                    }
                },
            ),
        );
    }

    tools
}

fn build_async_resolver(
    event_tx: &EventSender,
    mode: &AgentMode,
    cancel: &CancelToken,
    deadline: Deadline,
    config: AgentConfig,
    permissions: &Arc<PermissionManager>,
) -> AsyncResolver {
    let tx = event_tx.clone();
    let mode = mode.clone();
    let cancel = cancel.clone();
    let permissions = Arc::clone(permissions);

    Box::new(move |pending_calls: Vec<PendingCall>| {
        smol::future::block_on(async {
            let call_ids: Vec<u32> = pending_calls.iter().map(|pc| pc.call_id).collect();
            let mut set = TaskSet::new();
            for pc in pending_calls {
                let tx = tx.clone();
                let mode = mode.clone();
                let cancel = cancel.clone();
                let permissions = Arc::clone(&permissions);

                set.spawn(async move {
                    if let Err(e) = deadline.check() {
                        return (pc.call_id, Err(e));
                    }

                    let input = match build_tool_input(&pc.args, &pc.kwargs) {
                        Ok(v) => v,
                        Err(e) => return (pc.call_id, Err(e)),
                    };
                    let call = match super::ToolCall::from_api(&pc.name, &input) {
                        Ok(c) => c,
                        Err(e) => return (pc.call_id, Err(format!("tool parse error: {e}"))),
                    };

                    let mut inner_ctx =
                        super::interpreter_ctx(&mode, &tx, cancel, Arc::clone(&permissions));
                    inner_ctx.deadline = deadline;
                    inner_ctx.config = config;
                    let done = call.execute(&inner_ctx, String::new()).await;

                    let result = if done.is_error {
                        Err(done.output.as_text())
                    } else {
                        Ok(Value::String(done.output.as_text()))
                    };
                    (pc.call_id, result)
                });
            }

            let results: Vec<_> = set
                .join_all()
                .await
                .into_iter()
                .zip(&call_ids)
                .map(|(r, &call_id)| {
                    r.unwrap_or_else(|msg| {
                        tracing::error!(error = %msg, "code_execution inner tool panicked");
                        (call_id, Err(format!("tool panicked: {msg}")))
                    })
                })
                .collect();

            Ok(results)
        })
    })
}

fn build_tool_input(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    if let Some(first) = args.first()
        && first.is_object()
    {
        return Ok(first.clone());
    }

    if !kwargs.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in kwargs {
            obj.insert(k.clone(), v.clone());
        }
        return Ok(Value::Object(obj));
    }

    if args.is_empty() {
        return Ok(serde_json::json!({}));
    }

    Err("pass arguments as keyword arguments (e.g. read(path='/file'))".into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use test_case::test_case;

    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    use super::*;

    #[test]
    fn read_tool_via_interpreter() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("test.txt");
            fs::write(&path, "line1\nline2\n").unwrap();
            let path_str = path.to_string_lossy();

            let ctx = stub_ctx(&AgentMode::Build);
            let ci = CodeInterpreter {
                code: format!("result = await read(path='{path_str}')\nprint(result)"),
                timeout: None,
            };
            let output = ci.execute(&ctx).await.unwrap().as_text();
            assert!(output.contains("line1"));
        });
    }

    #[test_case(&[], &[("path".into(), json!("/foo"))],  json!({"path": "/foo"}) ; "kwargs")]
    #[test_case(&[json!({"path": "/foo"})], &[],         json!({"path": "/foo"}) ; "dict_passthrough")]
    #[test_case(&[], &[],                                json!({})               ; "no_args")]
    fn build_tool_input_cases(args: &[Value], kwargs: &[(String, Value)], expected: Value) {
        assert_eq!(build_tool_input(args, kwargs).unwrap(), expected);
    }

    #[test]
    fn cancel_returns_error() {
        smol::block_on(async {
            let (trigger, cancel) = crate::cancel::CancelToken::new();
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.cancel = cancel;
            let ci = CodeInterpreter {
                code: "1 + 1".into(),
                timeout: None,
            };
            trigger.cancel();
            let result = ci.execute(&ctx).await;
            assert!(result.is_err());
        });
    }
}
