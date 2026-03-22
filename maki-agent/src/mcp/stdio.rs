use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex;
use async_process::Child;
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use serde_json::Value;
use smol::channel;
use tracing::{debug, info, warn};

use super::error::McpError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::{BoxFuture, McpTransport};

type PendingMap = HashMap<u64, channel::Sender<Result<Value, McpError>>>;

const MAX_BODY_SIZE: usize = 64 * 1024 * 1024;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.0.id() as i32, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let _ = self.0.kill();
        }
    }
}

pub struct StdioTransport {
    name: Arc<str>,
    stdin: Mutex<async_process::ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
    _reader_task: smol::Task<()>,
    _stderr_task: smol::Task<()>,
    _child: ChildGuard,
}

impl StdioTransport {
    pub fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        environment: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let mut std_cmd = std::process::Command::new(program);
        std_cmd.args(args).envs(environment);

        #[cfg(unix)]
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let mut cmd: async_process::Command = std_cmd.into();
        cmd.stdin(async_process::Stdio::piped())
            .stdout(async_process::Stdio::piped())
            .stderr(async_process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| McpError::StartFailed {
            server: name.into(),
            reason: e.to_string(),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdout".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stderr".into(),
        })?;

        let name: Arc<str> = Arc::from(name);
        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        let reader_task = {
            let name = Arc::clone(&name);
            let alive = Arc::clone(&alive);
            let pending = Arc::clone(&pending);
            smol::spawn(async move {
                let result = Self::reader_loop(&name, &mut BufReader::new(stdout), &pending).await;
                if let Err(e) = &result {
                    warn!(server = &*name, error = %e, "MCP reader loop ended");
                }
                alive.store(false, Ordering::Release);
                for (_, sender) in pending.lock().await.drain() {
                    let _ = sender
                        .send(Err(McpError::ServerDied {
                            server: (*name).into(),
                        }))
                        .await;
                }
            })
        };

        let stderr_task = {
            let name = Arc::clone(&name);
            smol::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                warn!(server = &*name, "{trimmed}");
                            }
                        }
                    }
                }
            })
        };

        Ok(Self {
            name,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            alive,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _child: ChildGuard(child),
        })
    }

    async fn reader_loop(
        name: &Arc<str>,
        reader: &mut (impl AsyncBufReadExt + AsyncReadExt + Unpin),
        pending: &Mutex<PendingMap>,
    ) -> Result<(), McpError> {
        let mut line_buf = String::new();
        loop {
            let content_length = Self::read_headers(reader, &mut line_buf).await?;
            if content_length > MAX_BODY_SIZE {
                return Err(McpError::InvalidResponse {
                    server: (**name).into(),
                    reason: format!("Content-Length {content_length} exceeds {MAX_BODY_SIZE}"),
                });
            }

            let mut body = vec![0u8; content_length];
            reader
                .read_exact(&mut body)
                .await
                .map_err(|e| McpError::InvalidResponse {
                    server: (**name).into(),
                    reason: format!("body read failed: {e}"),
                })?;

            let text = match std::str::from_utf8(&body) {
                Ok(t) => t,
                Err(e) => {
                    warn!(server = &**name, error = %e, len = body.len(), "non-UTF8 body from server");
                    continue;
                }
            };

            match serde_json::from_str::<JsonRpcResponse>(text) {
                Ok(resp) => {
                    if let Some(id) = resp.id
                        && let Some(sender) = pending.lock().await.remove(&id)
                    {
                        let result = if let Some(err) = resp.error {
                            Err(McpError::RpcError {
                                server: (**name).into(),
                                code: err.code,
                                message: err.message,
                            })
                        } else {
                            Ok(resp.result.unwrap_or(Value::Null))
                        };
                        let _ = sender.send(result).await;
                    }
                }
                Err(e) => {
                    debug!(server = &**name, error = %e, body = text, "non-JSON-RPC message from server");
                }
            }
        }
    }

    async fn read_headers(
        reader: &mut (impl AsyncBufReadExt + Unpin),
        buf: &mut String,
    ) -> Result<usize, McpError> {
        let mut content_length: Option<usize> = None;
        loop {
            buf.clear();
            let n = reader
                .read_line(buf)
                .await
                .map_err(|e| McpError::ServerDied {
                    server: format!("header read: {e}"),
                })?;
            if n == 0 {
                return Err(McpError::ServerDied {
                    server: "EOF during headers".into(),
                });
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                match content_length {
                    Some(len) => return Ok(len),
                    None => continue,
                }
            }
            if let Some(val) = trimmed.strip_prefix("Content-Length:") {
                content_length = val.trim().parse::<usize>().ok();
            }
        }
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    async fn write_line(&self, line: &[u8]) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line)
            .await
            .map_err(|e| McpError::WriteFailed {
                server: self.server(),
                reason: e.to_string(),
            })?;
        stdin.flush().await.map_err(|e| McpError::WriteFailed {
            server: self.server(),
            reason: e.to_string(),
        })
    }

    fn server_died(&self) -> McpError {
        McpError::ServerDied {
            server: self.server(),
        }
    }

    fn serialize(&self, value: &impl serde::Serialize) -> Result<Vec<u8>, McpError> {
        let json = serde_json::to_string(value).map_err(|e| McpError::InvalidResponse {
            server: self.server(),
            reason: e.to_string(),
        })?;
        let mut buf = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
        buf.extend_from_slice(json.as_bytes());
        Ok(buf)
    }
}

impl McpTransport for StdioTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            if !self.alive.load(Ordering::Acquire) {
                return Err(self.server_died());
            }

            let start = Instant::now();
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let req = JsonRpcRequest::new(id, method, params);

            let (tx, rx) = smol::channel::bounded(1);
            self.pending.lock().await.insert(id, tx);

            if let Err(e) = self.write_line(&self.serialize(&req)?).await {
                self.pending.lock().await.remove(&id);
                return Err(e);
            }

            let result = futures_lite::future::race(
                async { rx.recv().await.unwrap_or(Err(self.server_died())) },
                async {
                    async_io::Timer::after(self.timeout).await;
                    Err(McpError::Timeout {
                        server: self.server(),
                        timeout_ms: self.timeout.as_millis() as u64,
                    })
                },
            )
            .await;

            if result.is_err() {
                self.pending.lock().await.remove(&id);
            } else {
                info!(server = %self.server(), method, id, duration_ms = start.elapsed().as_millis() as u64, "MCP stdio response");
            }

            result
        })
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let notif = JsonRpcNotification::new(method, params);
            self.write_line(&self.serialize(&notif)?).await
        })
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            self.alive.store(false, Ordering::Release);
            #[cfg(unix)]
            unsafe {
                libc::killpg(self._child.0.id() as i32, libc::SIGTERM);
            }
            smol::Timer::after(Duration::from_millis(200)).await;
            // Drop of self._child (ChildGuard) sends SIGKILL to process group
        })
    }

    fn server_name(&self) -> &Arc<str> {
        &self.name
    }

    fn transport_kind(&self) -> &'static str {
        "stdio"
    }

    fn child_pids(&self) -> Vec<u32> {
        vec![self._child.0.id()]
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn spawn_sleep() -> Child {
        let mut std_cmd = std::process::Command::new("sleep");
        std_cmd.arg("60");
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut cmd: async_process::Command = std_cmd.into();
        cmd.spawn().expect("failed to spawn sleep")
    }

    fn is_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn wait_for_death(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process {pid} still alive after 2s");
    }

    #[test]
    fn drop_kills_child_process() {
        let child = spawn_sleep();
        let pid = child.id();
        assert!(is_alive(pid));
        drop(ChildGuard(child));
        wait_for_death(pid);
    }
}
