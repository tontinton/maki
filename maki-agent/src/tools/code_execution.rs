//! Python execution bridge for the agent.
//!
//! We run Python in the monty sandbox, and bridge tool calls back to the agent.
//! Async awaits get batched via AsyncResolver so concurrent tool calls actually run in parallel.
//! Stdout streams to the UI every STREAM_FLUSH_INTERVAL so you see output as it happens.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::{Duration, Instant};

use maki_interpreter::runner::{self, ToolFn};
use maki_interpreter::{AsyncResolver, PendingCall};
use maki_tool_macro::Tool;
use serde::Deserialize;
use serde_json::Value;

use std::sync::Arc;

use crate::agent::tool_dispatch::Emit;
use crate::cancel::CancelToken;
use crate::permissions::PermissionManager;
use crate::task_set::TaskSet;
use crate::{AgentConfig, AgentEvent, AgentMode, EventSender, ToolInput, ToolOutput};
use async_lock::Mutex;

use smol::future::block_on;

use super::truncate_output;
use super::{Deadline, FileReadTracker};
use crate::tools::{ToolAudience, ToolRegistry};

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const PREAMBLE: &str = "import re\nimport asyncio\nimport sys\nimport os\nimport json\n";

struct InterpreterEnv {
    event_tx: EventSender,
    mode: AgentMode,
    cancel: CancelToken,
    deadline: Deadline,
    config: AgentConfig,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    user_response_rx: Option<Arc<Mutex<flume::Receiver<String>>>>,
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct CodeExecution {
    #[param(
        description = "Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency."
    )]
    code: String,
    #[param(description = "Timeout in seconds (default 30, max 300)")]
    timeout: Option<u64>,
}

impl CodeExecution {
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
        let config = ctx.config.clone();
        let deadline = Deadline::after(timeout);
        let limits = runner::limits(timeout, config.interpreter_max_memory_mb * 1024 * 1024);
        let env = InterpreterEnv {
            event_tx: ctx.event_tx.clone(),
            mode: ctx.mode.clone(),
            cancel: ctx.cancel.clone(),
            deadline,
            config,
            permissions: ctx.permissions.clone(),
            file_tracker: ctx.file_tracker.clone(),
            user_response_rx: ctx.user_response_rx.clone(),
        };

        // We race cancel against the blocking thread. If cancel wins, the Python thread
        // keeps running till it finishes. Threads can not be killed safely.
        ctx.cancel
            .race(smol::unblock(move || {
                let tools = build_tool_fns(&env);
                let resolver = build_async_resolver(&env);
                let code = format!("{PREAMBLE}{code}");

                let result = if let Some(ref id) = tool_use_id {
                    let mut pending = String::new();
                    let mut last_flush = Instant::now();
                    runner::run_streaming(&code, &tools, Some(&resolver), limits, &mut |line| {
                        pending.push_str(line);
                        if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL && !pending.is_empty() {
                            env.event_tx.try_send(AgentEvent::ToolOutput {
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
                    env.config.max_output_lines,
                    env.config.max_output_bytes,
                )))
            }))
            .await?
    }

    pub fn start_header(&self) -> String {
        let lines = self.code.lines().count();
        format!("{lines} lines")
    }
}

super::impl_tool!(
    CodeExecution,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::RESEARCH_SUB
        | super::ToolAudience::GENERAL_SUB,
    augment = |desc: &mut String, ctx: &super::DescriptionContext| {
        desc.push_str(&super::build_interpreter_tools_description(ctx.filter));
    },
);

impl super::ToolInvocation for CodeExecution {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(CodeExecution::start_header(
            self,
        )))
    }
    fn start_input(&self) -> Option<ToolInput> {
        Some(ToolInput::Script {
            language: "python".into(),
            code: self.code.clone(),
        })
    }
    fn start_annotation(&self) -> Option<String> {
        Some(super::timeout_annotation(self.timeout.unwrap_or(30)))
    }
    fn permission_scope(&self) -> Option<String> {
        None
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { CodeExecution::execute(&self, ctx).await })
    }
}

fn build_tool_fns(env: &InterpreterEnv) -> HashMap<String, ToolFn> {
    let mut tools: HashMap<String, ToolFn> = HashMap::new();

    for entry in ToolRegistry::native().iter().iter() {
        let tool_name = entry.name();
        if !entry.tool.audience().contains(ToolAudience::INTERPRETER) {
            continue;
        }
        if !super::is_tool_enabled(&env.config, tool_name) {
            continue;
        }
        let name = tool_name.to_string();
        let tx = env.event_tx.clone();
        let mode = env.mode.clone();
        let cancel = env.cancel.clone();
        let deadline = env.deadline;
        let permissions = Arc::clone(&env.permissions);
        let config = env.config.clone();
        let file_tracker = Arc::clone(&env.file_tracker);
        let user_response_rx = env.user_response_rx.clone();

        tools.insert(
            name.clone(),
            Box::new(
                move |fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
                    deadline.check()?;

                    let input = build_tool_input(&args, &kwargs)?;

                    let mut inner_ctx = super::interpreter_ctx(
                        &mode,
                        &tx,
                        cancel.clone(),
                        Arc::clone(&permissions),
                        Arc::clone(&file_tracker),
                        user_response_rx.clone(),
                    );
                    inner_ctx.deadline = deadline;
                    inner_ctx.config = config.clone();
                    let done = block_on(crate::agent::tool_dispatch::run(
                        ToolRegistry::native(),
                        inner_ctx.mcp.as_ref(),
                        String::new(),
                        fn_name,
                        &input,
                        &inner_ctx,
                        Emit::Silent,
                    ));
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

fn build_async_resolver(env: &InterpreterEnv) -> AsyncResolver {
    let tx = env.event_tx.clone();
    let mode = env.mode.clone();
    let cancel = env.cancel.clone();
    let deadline = env.deadline;
    let config = env.config.clone();
    let permissions = Arc::clone(&env.permissions);
    let file_tracker = Arc::clone(&env.file_tracker);
    let user_response_rx = env.user_response_rx.clone();

    Box::new(move |pending_calls: Vec<PendingCall>| {
        let config = config.clone();
        let file_tracker = Arc::clone(&file_tracker);
        let user_response_rx = user_response_rx.clone();
        smol::future::block_on(async {
            let call_ids: Vec<u32> = pending_calls.iter().map(|pc| pc.call_id).collect();
            let mut set = TaskSet::new();
            for pc in pending_calls {
                let tx = tx.clone();
                let mode = mode.clone();
                let cancel = cancel.clone();
                let permissions = Arc::clone(&permissions);
                let config = config.clone();
                let file_tracker = Arc::clone(&file_tracker);
                let user_response_rx = user_response_rx.clone();

                set.spawn(async move {
                    if let Err(e) = deadline.check() {
                        return (pc.call_id, Err(e));
                    }

                    let input = match build_tool_input(&pc.args, &pc.kwargs) {
                        Ok(v) => v,
                        Err(e) => return (pc.call_id, Err(e)),
                    };

                    let mut inner_ctx = super::interpreter_ctx(
                        &mode,
                        &tx,
                        cancel,
                        Arc::clone(&permissions),
                        file_tracker,
                        user_response_rx,
                    );
                    inner_ctx.deadline = deadline;
                    inner_ctx.config = config;
                    let done = crate::agent::tool_dispatch::run(
                        ToolRegistry::native(),
                        inner_ctx.mcp.as_ref(),
                        String::new(),
                        &pc.name,
                        &input,
                        &inner_ctx,
                        Emit::Silent,
                    )
                    .await;

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
            let ci = CodeExecution {
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
            let ci = CodeExecution {
                code: "1 + 1".into(),
                timeout: None,
            };
            trigger.cancel();
            let result = ci.execute(&ctx).await;
            assert!(result.is_err());
        });
    }
}
