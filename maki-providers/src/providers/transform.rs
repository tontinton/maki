use std::io;
use std::path::Path;
use std::time::Duration;

use async_lock::Mutex;
use async_process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use serde_json::{Value, json};
use tracing::warn;

use crate::model::TokenUsage;
use crate::types::StopReason;
use crate::{AgentError, Message, ProviderEvent, StreamResponse};

type LocalBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

const REQUEST_STAGE: &str = "request";
const RESPONSE_DELTA_STAGE: &str = "response_delta";
const RESPONSE_END_STAGE: &str = "response_end";
const LINE_TIMEOUT: Duration = Duration::from_secs(30);

const NO_TRANSFORM: &str = "transform subprocess produced no output line";

/// Outcome of an `response_end`-stage transform. Each field is `Some` only
/// when the transform returned a replacement value; `None` means passthrough.
pub(crate) struct TransformedEnd {
    pub message: Option<Message>,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<StopReason>,
}

pub(crate) struct TransformProcess {
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    child: Option<Child>,
}

impl TransformProcess {
    pub(crate) fn spawn(script: &Path) -> Result<Self, AgentError> {
        let mut child = Command::new(script)
            .arg("transform")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| transform_start_error(script, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| transform_pipe_error(script))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| transform_pipe_error(script))?;

        let stderr = child.stderr.take();
        if let Some(stderr) = stderr {
            smol::spawn(stderr_drain(stderr)).detach();
        }

        Ok(Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            child: Some(child),
        })
    }

    /// Spawns a transform for `path` (when set) and rewrites `body` through it.
    /// Returns the spawned process (if any) alongside the transformed body.
    pub(crate) async fn spawn_and_request(
        path: Option<&Path>,
        body: Value,
        headers: &[(String, String)],
        model: &str,
    ) -> Result<(Option<Self>, Value), AgentError> {
        let Some(path) = path else {
            return Ok((None, body));
        };
        let tf = Self::spawn(path)?;
        let out = tf.request(&body, headers, model).await?;
        Ok((Some(tf), out))
    }

    pub(crate) async fn request(
        &self,
        body: &Value,
        headers: &[(String, String)],
        model: &str,
    ) -> Result<Value, AgentError> {
        let payload = json!({
            "stage": REQUEST_STAGE,
            "body": body,
            "headers": serde_json::to_value(headers)?,
            "model": model,
        });
        let reply = self.exchange(&payload).await?;
        reply
            .get("body")
            .cloned()
            .ok_or_else(|| AgentError::Config {
                message: NO_TRANSFORM.into(),
            })
    }

    pub(crate) async fn delta(
        &self,
        event: &ProviderEvent,
    ) -> Result<Option<ProviderEvent>, AgentError> {
        let payload = serde_json::to_value(event)?;
        let payload = json!({ "stage": RESPONSE_DELTA_STAGE, "event": payload });
        let reply = self.exchange(&payload).await?;
        if reply.is_null() || reply.as_object().is_some_and(|m| m.is_empty()) {
            return Ok(None);
        }
        if let Some(ev) = reply.get("event") {
            if ev.is_null() {
                return Ok(None);
            }
            return Ok(Some(serde_json::from_value(ev.clone())?));
        }
        Ok(None)
    }

    pub(crate) async fn end(
        &self,
        response: &StreamResponse,
    ) -> Result<TransformedEnd, AgentError> {
        let payload = json!({
            "stage": RESPONSE_END_STAGE,
            "message": serde_json::to_value(&response.message)?,
            "usage": serde_json::to_value(response.usage)?,
            "stop_reason": serde_json::to_value(response.stop_reason)?,
        });
        let reply = self.exchange(&payload).await?;
        let message = match reply.get("message") {
            Some(m) if !m.is_null() => Some(serde_json::from_value(m.clone())?),
            _ => None,
        };
        let usage = match reply.get("usage") {
            Some(u) if !u.is_null() => Some(serde_json::from_value(u.clone())?),
            _ => None,
        };
        let stop_reason = match reply.get("stop_reason") {
            Some(s) if !s.is_null() => Some(serde_json::from_value(s.clone())?),
            _ => None,
        };
        Ok(TransformedEnd {
            message,
            usage,
            stop_reason,
        })
    }

    async fn exchange(&self, payload: &Value) -> Result<Value, AgentError> {
        let mut buf = serde_json::to_vec(payload)?;
        buf.push(b'\n');

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&buf).await.map_err(transform_write_error)?;
        stdin.flush().await.map_err(transform_write_error)?;
        drop(stdin);

        let mut stdout = self.stdout.lock().await;
        let line = read_line(&mut *stdout, LINE_TIMEOUT).await?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(AgentError::Config {
                message: NO_TRANSFORM.into(),
            });
        }
        serde_json::from_str::<Value>(trimmed).map_err(|e| AgentError::Config {
            message: format!("transform subprocess produced invalid JSON: {e}"),
        })
    }

    pub(crate) async fn shutdown(mut self) {
        let _ = self.stdin.lock().await.close().await;
        if let Some(child) = self.child.as_mut() {
            let _ = futures_lite::future::or(
                async {
                    let _ = child.status().await;
                },
                async {
                    smol::Timer::after(Duration::from_secs(2)).await;
                },
            )
            .await;
        }
    }
}

impl Drop for TransformProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_process_group(child.id());
            let _ = child.try_status();
        }
    }
}

async fn stderr_drain(stderr: async_process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(target: "transform", "{trimmed}");
                }
            }
        }
    }
}

async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    deadline: Duration,
) -> Result<String, AgentError> {
    let mut line = String::new();
    futures_lite::future::or(
        async {
            reader
                .read_line(&mut line)
                .await
                .map_err(transform_read_error)?;
            Ok(line)
        },
        async {
            smol::Timer::after(deadline).await;
            Err(AgentError::Config {
                message: format!(
                    "transform subprocess timed out after {}s",
                    deadline.as_secs()
                ),
            })
        },
    )
    .await
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe {
        libc::killpg(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

fn transform_start_error(script: &Path, e: io::Error) -> AgentError {
    AgentError::Config {
        message: format!("failed to spawn {} transform: {e}", script.display()),
    }
}

fn transform_pipe_error(script: &Path) -> AgentError {
    AgentError::Config {
        message: format!("{} transform: no stdio pipe captured", script.display()),
    }
}

fn transform_write_error(e: io::Error) -> AgentError {
    AgentError::Config {
        message: format!("transform subprocess write failed: {e}"),
    }
}

fn transform_read_error(e: io::Error) -> AgentError {
    AgentError::Config {
        message: format!("transform subprocess read failed: {e}"),
    }
}

pub(crate) struct TransformingSender<'a> {
    inner: &'a Sender<ProviderEvent>,
    transform: &'a TransformProcess,
}

impl<'a> TransformingSender<'a> {
    pub(crate) fn new(inner: &'a Sender<ProviderEvent>, transform: &'a TransformProcess) -> Self {
        Self { inner, transform }
    }

    pub(crate) async fn send(&self, event: ProviderEvent) -> Result<(), AgentError> {
        match self.transform.delta(&event).await {
            Ok(Some(transformed)) => {
                self.inner.send_async(transformed).await?;
            }
            Ok(None) => {
                self.inner.send_async(event).await?;
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }
}

pub(crate) async fn forward_events(
    bridge_rx: flume::Receiver<ProviderEvent>,
    sender: TransformingSender<'_>,
) {
    while let Ok(event) = bridge_rx.recv_async().await {
        if sender.send(event).await.is_err() {
            break;
        }
    }
}

const BRIDGE_BOUND: usize = 64;

/// Drives a streaming parse through an optional transform subprocess.
///
/// When a transform is set, `parse` owns the bridge sender so that returning
/// (or erroring) drops it and unblocks [`forward_events`] on the receiver.
/// The transform's [`TransformProcess::end`] rewrites the final message.
pub(crate) async fn stream_with_transform(
    event_tx: &Sender<ProviderEvent>,
    transform: Option<TransformProcess>,
    parse: impl FnOnce(
        Sender<ProviderEvent>,
    ) -> LocalBoxFuture<'static, Result<StreamResponse, AgentError>>,
) -> Result<StreamResponse, AgentError> {
    let Some(tf) = transform else {
        return parse(event_tx.clone()).await;
    };
    let (bridge_tx, bridge_rx) = flume::bounded::<ProviderEvent>(BRIDGE_BOUND);
    let sender = TransformingSender::new(event_tx, &tf);
    let (r, _) =
        futures_lite::future::zip(parse(bridge_tx), forward_events(bridge_rx, sender)).await;
    match r {
        Ok(mut response) => {
            let end = tf.end(&response).await?;
            if let Some(msg) = end.message {
                response.message = msg;
            }
            if let Some(usage) = end.usage {
                response.usage = usage;
            }
            if let Some(stop_reason) = end.stop_reason {
                response.stop_reason = Some(stop_reason);
            }
            tf.shutdown().await;
            Ok(response)
        }
        Err(e) => {
            tf.shutdown().await;
            Err(e)
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_sh_transform(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let script = format!("#!/bin/sh\n{body}\n");
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn request_transform_passthrough_via_cat() {
        let tmp = TempDir::new().unwrap();
        let script = write_sh_transform(tmp.path(), "passthrough", "awk '{ print; fflush() }'");
        smol::block_on(async {
            let process = TransformProcess::spawn(&script).unwrap();
            let body = json!({"prompt": "hello"});
            let result = process
                .request(
                    &body,
                    &[("authorization".into(), "Bearer x".into())],
                    "test-model",
                )
                .await
                .unwrap();
            assert_eq!(result["prompt"], "hello");
            process.shutdown().await;
        });
    }

    #[test]
    fn request_transform_mutates_body() {
        let tmp = TempDir::new().unwrap();
        let script = write_sh_transform(
            tmp.path(),
            "mutator",
            r#"
while IFS= read -r line; do
  echo "$line" | stdbuf -oL jq -c '.body.metadata = {"user_id":"u123"} | {body: .body}'
done
"#,
        );
        smol::block_on(async {
            let process = TransformProcess::spawn(&script).unwrap();
            let body = json!({"messages": []});
            let result = process.request(&body, &[], "m").await.unwrap();
            assert_eq!(result["metadata"]["user_id"], "u123");
            process.shutdown().await;
        });
    }

    #[test]
    fn request_transform_crash_aborts() {
        let tmp = TempDir::new().unwrap();
        let script = write_sh_transform(tmp.path(), "crasher", "exit 1");
        smol::block_on(async {
            let process = TransformProcess::spawn(&script).unwrap();
            let result = process.request(&json!({}), &[], "m").await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn request_transform_invalid_json_errors() {
        let tmp = TempDir::new().unwrap();
        let script = write_sh_transform(tmp.path(), "garbage", r#"echo 'not json'"#);
        smol::block_on(async {
            let process = TransformProcess::spawn(&script).unwrap();
            let result = process.request(&json!({}), &[], "m").await;
            assert!(matches!(result, Err(AgentError::Config { .. })));
            process.shutdown().await;
        });
    }

    #[test]
    fn delta_transform_passthrough() {
        let tmp = TempDir::new().unwrap();
        let script = write_sh_transform(tmp.path(), "delta-id", "awk '{ print; fflush() }'");
        smol::block_on(async {
            let process = TransformProcess::spawn(&script).unwrap();
            let event = ProviderEvent::TextDelta { text: "x".into() };
            let result = process.delta(&event).await.unwrap();
            assert!(result.is_some());
            let transformed = result.unwrap();
            match transformed {
                ProviderEvent::TextDelta { text } => assert_eq!(text, "x"),
                _ => panic!("wrong variant"),
            }
            process.shutdown().await;
        });
    }
}
