//! Runs up to 25 tool calls concurrently in a single agent turn.
//!
//! Nesting (batch inside batch) is explicitly rejected.
//! `BatchProgress` events are emitted as each child tool completes so the UI can update in real time.

use std::fmt::Write;

use crate::agent::{ResolvedCall, resolve_tool};
use crate::{AgentEvent, BatchProgressEvent, BatchToolEntry, BatchToolStatus, ToolOutput};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::task_set::TaskSet;
use maki_tool_macro::{Args, Tool};
use tracing::{error, info};

use std::time::Instant;

use super::{ToolCall, ToolContext};

const MAX_BATCH_SIZE: usize = 25;

#[derive(Args, Debug, Clone)]
pub(super) struct BatchEntry {
    #[param(description = "The name of the tool to execute")]
    tool: String,
    #[param(description = "Parameters for the tool")]
    parameters: Value,
}

/// Models sometimes send batch entries with flat fields:
///   `{"tool": "glob", "path": "/tmp", "pattern": "*.rs"}`
/// instead of the expected nested format:
///   `{"tool": "glob", "parameters": {"path": "/tmp", "pattern": "*.rs"}}`
///
/// This custom deserializer accepts both. When `parameters` is missing,
/// every field that isn't `tool` is collected into a `parameters` object.
/// When both `parameters` and flat fields are present, they are merged
/// (duplicate keys are rejected).
impl<'de> Deserialize<'de> for BatchEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BatchEntryVisitor;

        impl<'de> Visitor<'de> for BatchEntryVisitor {
            type Value = BatchEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a batch entry with 'tool' and either 'parameters' or flat tool params")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<BatchEntry, M::Error> {
                let mut tool: Option<String> = None;
                let mut parameters: Option<Value> = None;
                let mut rest = serde_json::Map::new();

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "tool" => {
                            tool = Some(map.next_value()?);
                        }
                        "parameters" => {
                            parameters = Some(map.next_value()?);
                        }
                        _ => {
                            rest.insert(key, map.next_value()?);
                        }
                    }
                }

                let tool = tool.ok_or_else(|| de::Error::missing_field("tool"))?;
                let parameters = match parameters {
                    Some(p) if rest.is_empty() => p,
                    Some(Value::Object(mut obj)) => {
                        for (k, v) in rest {
                            if obj.contains_key(&k) {
                                return Err(de::Error::custom(format_args!(
                                    "duplicate parameter '{k}' in both 'parameters' and flat fields"
                                )));
                            }
                            obj.insert(k, v);
                        }
                        Value::Object(obj)
                    }
                    Some(_) => {
                        return Err(de::Error::custom(
                            "'parameters' must be an object when flat fields are also present",
                        ));
                    }
                    None if !rest.is_empty() => Value::Object(rest),
                    None => return Err(de::Error::missing_field("parameters")),
                };

                Ok(BatchEntry { tool, parameters })
            }
        }

        deserializer.deserialize_map(BatchEntryVisitor)
    }
}

impl BatchEntry {
    fn to_batch_entry(
        &self,
        status: BatchToolStatus,
        output: Option<ToolOutput>,
    ) -> BatchToolEntry {
        let call = ToolCall::from_api(&self.tool, &self.parameters).ok();
        BatchToolEntry {
            tool: self.tool.clone(),
            summary: call.as_ref().map(|c| c.start_summary()).unwrap_or_default(),
            status,
            input: call.and_then(|c| c.start_input()),
            output,
            annotation: None,
        }
    }
}

#[derive(Tool, Debug, Clone)]
pub struct Batch {
    #[param(description = "Array of tool calls to execute in parallel")]
    tool_calls: Vec<BatchEntry>,
}

impl Batch {
    pub const NAME: &str = "batch";
    pub const DESCRIPTION: &str = include_str!("batch.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"tool_calls": [
  {"tool": "glob", "parameters": {"pattern": "src/**/*.ts"}},
  {"tool": "grep", "parameters": {"pattern": "import", "include": "*.ts"}},
  {"tool": "index", "parameters": {"path": "/project/index.ts"}}
]}]"#,
    );

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        if self.tool_calls.is_empty() {
            return Err("provide at least one tool call".into());
        }

        let batch_id = ctx.tool_use_id.clone().unwrap_or_default();

        let active = &self.tool_calls[..self.tool_calls.len().min(MAX_BATCH_SIZE)];
        let discarded = &self.tool_calls[active.len()..];

        let mcp = ctx.mcp.as_ref();

        let parsed: Vec<Result<ResolvedCall, String>> = active
            .iter()
            .map(|entry| {
                if entry.tool == Self::NAME {
                    return Err("cannot nest batch inside batch".into());
                }
                resolve_tool(&entry.tool, &entry.parameters, mcp).map_err(|e| e.to_string())
            })
            .collect();

        let inner_id = |i: usize| format!("{batch_id}__{i}");

        let start = Instant::now();
        let mut set = TaskSet::new();
        for (i, parsed_call) in parsed.iter().enumerate() {
            let id = inner_id(i);
            let batch_id = batch_id.clone();
            let ctx = ctx.clone();
            let parsed_call = parsed_call.clone();
            set.spawn(async move {
                ctx.event_tx
                    .try_send(AgentEvent::BatchProgress(Box::new(BatchProgressEvent {
                        batch_id: batch_id.clone(),
                        index: i,
                        status: BatchToolStatus::InProgress,
                        output: None,
                    })));
                let (result, output) = match parsed_call {
                    Ok(call) => {
                        let inner_ctx = ToolContext {
                            tool_use_id: Some(id.clone()),
                            ..ctx.clone()
                        };
                        let done = call.execute(&inner_ctx, id).await;
                        ctx.event_tx
                            .try_send(AgentEvent::ToolDone(Box::new(done.clone())));
                        let text = done.output.as_text();
                        let result = if done.is_error { Err(text) } else { Ok(text) };
                        (result, Some(done.output))
                    }
                    Err(e) => (Err(e.to_string()), None),
                };
                let status = if result.is_ok() {
                    BatchToolStatus::Success
                } else {
                    BatchToolStatus::Error
                };
                ctx.event_tx
                    .try_send(AgentEvent::BatchProgress(Box::new(BatchProgressEvent {
                        batch_id,
                        index: i,
                        status,
                        output: output.clone(),
                    })));
                (i, result, output)
            });
        }

        let mut results: Vec<(Result<String, String>, Option<ToolOutput>)> =
            vec![(Err("tool task panicked".into()), None); parsed.len()];
        let all = ctx.cancel.race(set.join_all()).await?;
        for (r, i) in all.into_iter().zip(0..) {
            match r {
                Ok((idx, result, output)) => results[idx] = (result, output),
                Err(e) => {
                    error!(error = %e, "batch tool task panicked");
                    results[i] = (Err(format!("tool task panicked: {e}")), None);
                }
            }
        }

        let total = results.len() + discarded.len();
        let mut failed = discarded.len();
        let mut output = String::new();
        let mut entries: Vec<BatchToolEntry> = active
            .iter()
            .zip(&results)
            .map(|(entry, (result, output))| {
                let status = if result.is_ok() {
                    BatchToolStatus::Success
                } else {
                    BatchToolStatus::Error
                };
                entry.to_batch_entry(status, output.clone())
            })
            .collect();

        for (entry, (result, _)) in active.iter().zip(&results) {
            let _ = writeln!(output, "## {}", entry.tool);
            match result {
                Ok(content) => output.push_str(content),
                Err(err) => {
                    failed += 1;
                    let _ = write!(output, "[ERROR] {err}");
                }
            }
            output.push_str("\n\n");
        }

        for entry in discarded {
            let _ = write!(
                output,
                "## {}\n[ERROR] maximum of {MAX_BATCH_SIZE} tools per batch\n\n",
                entry.tool
            );
            entries.push(entry.to_batch_entry(BatchToolStatus::Error, None));
        }

        let succeeded = total - failed;
        info!(
            succeeded,
            failed,
            total,
            duration_ms = start.elapsed().as_millis() as u64,
            "batch completed"
        );
        if failed > 0 {
            let _ = write!(
                output,
                "Executed {succeeded}/{total} successfully. {failed} failed."
            );
        } else {
            let _ = write!(output, "All {total} tools executed successfully.");
        }

        Ok(ToolOutput::Batch {
            entries,
            text: output,
        })
    }

    pub fn start_summary(&self) -> String {
        format!("{} tools", self.tool_calls.len())
    }
}

impl super::ToolDefaults for Batch {
    fn start_output(&self) -> Option<ToolOutput> {
        let entries = self
            .tool_calls
            .iter()
            .map(|entry| entry.to_batch_entry(BatchToolStatus::Pending, None))
            .collect();
        Some(ToolOutput::Batch {
            entries,
            text: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::BatchToolEntry;
    use serde_json::json;

    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    use super::*;

    async fn run_batch(input: Value) -> (Vec<BatchToolEntry>, String) {
        let ctx = stub_ctx(&AgentMode::Build);
        execute_batch(&ctx, input).await
    }

    async fn execute_batch(ctx: &ToolContext, input: Value) -> (Vec<BatchToolEntry>, String) {
        let batch = Batch::parse_input(&input).unwrap();
        match batch.execute(ctx).await.unwrap() {
            ToolOutput::Batch { entries, text } => (entries, text),
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn empty_batch_returns_error() {
        smol::block_on(async {
            let ctx = stub_ctx(&AgentMode::Build);
            let batch = Batch::parse_input(&json!({"tool_calls": []})).unwrap();
            assert!(batch.execute(&ctx).await.is_err());
        });
    }

    #[test]
    fn nested_batch_rejected() {
        smol::block_on(async {
            let (entries, _) = run_batch(json!({
                "tool_calls": [{"tool": "batch", "parameters": {"tool_calls": []}}]
            }))
            .await;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].status, BatchToolStatus::Error);
            assert_eq!(entries[0].tool, "batch");
        });
    }

    #[test]
    fn parallel_execution_with_mixed_results() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let f = dir.path().join("a.txt");
            std::fs::write(&f, "content").unwrap();

            let (entries, text) = run_batch(json!({
                "tool_calls": [
                    {"tool": "read", "parameters": {"path": f.to_str().unwrap()}},
                    {"tool": "read", "parameters": {"path": "/nonexistent/path.txt"}}
                ]
            }))
            .await;
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].status, BatchToolStatus::Success);
            assert_eq!(entries[1].status, BatchToolStatus::Error);
            assert!(text.contains("content"));
        });
    }

    #[test]
    fn cancel_stops_batch() {
        smol::block_on(async {
            let (trigger, cancel) = crate::cancel::CancelToken::new();
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.cancel = cancel;
            let batch = Batch::parse_input(&json!({
                "tool_calls": [
                    {"tool": "read", "parameters": {"path": "/dev/null"}}
                ]
            }))
            .unwrap();
            trigger.cancel();
            let err = batch.execute(&ctx).await.unwrap_err();
            assert!(err.contains("cancelled"));
        });
    }

    #[test]
    fn exceeds_max_batch_size_discards_excess() {
        smol::block_on(async {
            let calls: Vec<Value> = (0..MAX_BATCH_SIZE + 2)
                .map(|_| json!({"tool": "read", "parameters": {"path": "/tmp"}}))
                .collect();
            let (entries, _) = run_batch(json!({"tool_calls": calls})).await;
            assert_eq!(entries.len(), MAX_BATCH_SIZE + 2);
            let discarded: Vec<_> = entries[MAX_BATCH_SIZE..].iter().collect();
            assert!(discarded.iter().all(|e| e.status == BatchToolStatus::Error));
        });
    }

    #[test]
    fn inner_tool_emits_tool_done_event() {
        use crate::tools::test_support::stub_ctx_with;
        use crate::{Envelope, EventSender};

        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let f = dir.path().join("hello.txt");
            std::fs::write(&f, "hello").unwrap();

            let (tx, rx) = flume::unbounded::<Envelope>();
            let event_tx = EventSender::new(tx, 0);
            let ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), None);
            execute_batch(
                &ctx,
                json!({
                    "tool_calls": [{"tool": "read", "parameters": {"path": f.to_str().unwrap()}}]
                }),
            )
            .await;

            assert!(
                rx.drain()
                    .any(|env| matches!(&env.event, AgentEvent::ToolDone(done) if !done.is_error)),
                "batch inner tool must emit ToolDone"
            );
        });
    }

    #[test]
    fn flat_batch_entry_deserialized_as_nested() {
        let flat = json!({
            "tool_calls": [
                {"tool": "glob", "path": "/tmp", "pattern": "*.rs"},
                {"tool": "read", "path": "/tmp/foo.txt"}
            ]
        });
        let batch = Batch::parse_input(&flat).unwrap();
        assert_eq!(batch.tool_calls.len(), 2);
        assert_eq!(batch.tool_calls[0].tool, "glob");
        assert_eq!(batch.tool_calls[0].parameters["path"], "/tmp");
        assert_eq!(batch.tool_calls[0].parameters["pattern"], "*.rs");
        assert_eq!(batch.tool_calls[1].tool, "read");
        assert_eq!(batch.tool_calls[1].parameters["path"], "/tmp/foo.txt");
    }

    #[test]
    fn nested_batch_entry_still_works() {
        let nested = json!({
            "tool_calls": [
                {"tool": "glob", "parameters": {"path": "/tmp", "pattern": "*.rs"}}
            ]
        });
        let batch = Batch::parse_input(&nested).unwrap();
        assert_eq!(batch.tool_calls[0].tool, "glob");
        assert_eq!(batch.tool_calls[0].parameters["path"], "/tmp");
    }

    #[test]
    fn mixed_nested_and_flat_fields_merged() {
        let mixed = json!({
            "tool_calls": [
                {"tool": "glob", "parameters": {"path": "/tmp"}, "pattern": "*.rs"}
            ]
        });
        let batch = Batch::parse_input(&mixed).unwrap();
        assert_eq!(batch.tool_calls[0].tool, "glob");
        assert_eq!(batch.tool_calls[0].parameters["path"], "/tmp");
        assert_eq!(batch.tool_calls[0].parameters["pattern"], "*.rs");
    }

    #[test]
    fn mixed_with_duplicate_key_is_error() {
        let dup = json!({
            "tool_calls": [
                {"tool": "glob", "parameters": {"pattern": "*.rs"}, "pattern": "*.txt"}
            ]
        });
        assert!(Batch::parse_input(&dup).is_err());
    }

    #[test]
    fn batch_entry_missing_tool_is_error() {
        let no_tool = json!({"tool_calls": [{"parameters": {"path": "/tmp"}}]});
        assert!(Batch::parse_input(&no_tool).is_err());
    }

    #[test]
    fn batch_entry_missing_tool_and_params_is_error() {
        let empty = json!({"tool_calls": [{}]});
        assert!(Batch::parse_input(&empty).is_err());
    }

    #[test]
    fn flat_batch_entries_actually_execute() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
            let dir_str = dir.path().to_string_lossy().to_string();

            let (entries, text) = run_batch(json!({
                "tool_calls": [
                    {"tool": "glob", "path": dir_str, "pattern": "*.txt"}
                ]
            }))
            .await;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].status, BatchToolStatus::Success);
            assert!(text.contains("a.txt"));
        });
    }
}
