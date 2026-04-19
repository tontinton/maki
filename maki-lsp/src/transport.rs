use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_lock::Mutex;
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use serde_json::Value;
use smol::channel;
use tracing::{debug, warn};

use crate::error::LspError;
use crate::protocol::{
    Diagnostic, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, PublishDiagnosticsParams,
};

type PendingMap = HashMap<u64, channel::Sender<Result<Value, LspError>>>;
pub type DiagnosticsCache = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

const CONTENT_LENGTH_PREFIX: &str = "Content-Length: ";

pub struct LspTransport {
    name: Arc<str>,
    stdin: Mutex<async_process::ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
    diagnostics: DiagnosticsCache,
    _reader_task: smol::Task<()>,
    _stderr_task: smol::Task<()>,
    _child: crate::ChildGuard,
}

impl LspTransport {
    pub fn spawn(
        name: &str,
        command: &[String],
        timeout: Duration,
        diagnostics: DiagnosticsCache,
    ) -> Result<Self, LspError> {
        if command.is_empty() {
            return Err(LspError::StartFailed {
                server: name.into(),
                reason: "empty command".into(),
            });
        }

        let program = &command[0];
        let args = &command[1..];

        let mut std_cmd = std::process::Command::new(program);
        std_cmd.args(args);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                std_cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut cmd: async_process::Command = std_cmd.into();
        cmd.stdin(async_process::Stdio::piped())
            .stdout(async_process::Stdio::piped())
            .stderr(async_process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| LspError::StartFailed {
            server: name.into(),
            reason: e.to_string(),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| LspError::StartFailed {
            server: name.into(),
            reason: "no stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| LspError::StartFailed {
            server: name.into(),
            reason: "no stdout".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| LspError::StartFailed {
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
            let diagnostics = Arc::clone(&diagnostics);
            smol::spawn(async move {
                let result =
                    Self::reader_loop(&name, &mut BufReader::new(stdout), &pending, &diagnostics)
                        .await;
                if let Err(e) = &result {
                    warn!(server = &*name, error = %e, "LSP reader loop ended");
                }
                alive.store(false, Ordering::Release);
                for (_, sender) in pending.lock().await.drain() {
                    let _ = sender
                        .send(Err(LspError::ServerDied {
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
                                debug!(server = &*name, "{trimmed}");
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
            diagnostics,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _child: crate::ChildGuard::new(child),
        })
    }

    async fn reader_loop(
        name: &Arc<str>,
        reader: &mut (impl AsyncBufReadExt + AsyncReadExt + Unpin),
        pending: &Mutex<PendingMap>,
        diagnostics: &DiagnosticsCache,
    ) -> Result<(), LspError> {
        let mut header_line = String::new();
        loop {
            let content_length =
                loop {
                    header_line.clear();
                    let n = reader.read_line(&mut header_line).await.map_err(|e| {
                        LspError::ServerDied {
                            server: format!("{}: read failed: {e}", &**name),
                        }
                    })?;
                    if n == 0 {
                        return Err(LspError::ServerDied {
                            server: (**name).into(),
                        });
                    }
                    let trimmed = header_line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(len_str) = trimmed.strip_prefix(CONTENT_LENGTH_PREFIX) {
                        let len: usize =
                            len_str
                                .trim()
                                .parse()
                                .map_err(|_| LspError::InvalidResponse {
                                    server: (**name).into(),
                                    reason: format!("bad Content-Length: {len_str}"),
                                })?;
                        // Read remaining headers until empty line
                        loop {
                            header_line.clear();
                            let n = reader.read_line(&mut header_line).await.map_err(|e| {
                                LspError::ServerDied {
                                    server: format!("{}: read failed: {e}", &**name),
                                }
                            })?;
                            if n == 0 {
                                return Err(LspError::ServerDied {
                                    server: (**name).into(),
                                });
                            }
                            if header_line.trim().is_empty() {
                                break;
                            }
                        }
                        break len;
                    }
                };

            let mut body = vec![0u8; content_length];
            reader
                .read_exact(&mut body)
                .await
                .map_err(|e| LspError::ServerDied {
                    server: format!("{}: read body failed: {e}", &**name),
                })?;

            let msg: JsonRpcResponse = match serde_json::from_slice(&body) {
                Ok(m) => m,
                Err(e) => {
                    debug!(server = &**name, error = %e, "non-JSON-RPC message from LSP server");
                    continue;
                }
            };

            // Notification (no id): handle publishDiagnostics
            if msg.id.is_none() {
                if msg.method.as_deref() == Some("textDocument/publishDiagnostics")
                    && let Some(params) = msg.params
                    && let Ok(diag_params) =
                        serde_json::from_value::<PublishDiagnosticsParams>(params)
                {
                    diagnostics
                        .lock()
                        .await
                        .insert(diag_params.uri, diag_params.diagnostics);
                }
                continue;
            }

            let id = msg.id.unwrap();
            if let Some(sender) = pending.lock().await.remove(&id) {
                let result = if let Some(err) = msg.error {
                    Err(LspError::RequestFailed {
                        server: (**name).into(),
                        message: format!("code {}: {}", err.code, err.message),
                    })
                } else {
                    Ok(msg.result.unwrap_or(Value::Null))
                };
                let _ = sender.send(result).await;
            } else {
                debug!(server = &**name, id, "response for unknown request id");
            }
        }
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(LspError::ServerDied {
                server: self.server(),
            });
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);

        let (tx, rx) = channel::bounded(1);
        self.pending.lock().await.insert(id, tx);

        if let Err(e) = self.write_message(&req).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let result = futures_lite::future::race(
            async {
                rx.recv().await.unwrap_or(Err(LspError::ServerDied {
                    server: self.server(),
                }))
            },
            async {
                async_io::Timer::after(self.timeout).await;
                Err(LspError::Timeout {
                    server: self.server(),
                    timeout_ms: self.timeout.as_millis() as u64,
                })
            },
        )
        .await;

        if result.is_err() {
            self.pending.lock().await.remove(&id);
        }

        result
    }

    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), LspError> {
        let notif = JsonRpcNotification::new(method, params);
        self.write_message(&notif).await
    }

    async fn write_message(&self, value: &impl serde::Serialize) -> Result<(), LspError> {
        let body = serde_json::to_vec(value).map_err(|e| LspError::InvalidResponse {
            server: self.server(),
            reason: e.to_string(),
        })?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| LspError::ServerDied {
                server: format!("{}: write failed: {e}", self.server()),
            })?;
        stdin
            .write_all(&body)
            .await
            .map_err(|e| LspError::ServerDied {
                server: format!("{}: write failed: {e}", self.server()),
            })?;
        stdin.flush().await.map_err(|e| LspError::ServerDied {
            server: format!("{}: flush failed: {e}", self.server()),
        })?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::Release);
    }

    pub fn diagnostics_cache(&self) -> &DiagnosticsCache {
        &self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::io::Cursor;

    use super::*;

    fn make_lsp_message(body: &str) -> Vec<u8> {
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut buf = header.into_bytes();
        buf.extend_from_slice(body.as_bytes());
        buf
    }

    #[test]
    fn parse_single_response() {
        smol::block_on(async {
            let body = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"hello"}}}"#;
            let data = make_lsp_message(body);

            let pending: Mutex<PendingMap> = Mutex::new(HashMap::new());
            let name: Arc<str> = Arc::from("test");
            let diagnostics: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));

            let (tx, rx) = channel::bounded(1);
            pending.lock().await.insert(1, tx);

            let mut reader = BufReader::new(Cursor::new(data));
            let _ = LspTransport::reader_loop(&name, &mut reader, &pending, &diagnostics).await;

            let result = rx.try_recv().unwrap();
            assert!(result.is_ok());
            let val = result.unwrap();
            assert_eq!(val["contents"]["value"], "hello");
        });
    }

    #[test]
    fn parse_notification_updates_diagnostics() {
        smol::block_on(async {
            let body = r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///src/main.rs","diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"severity":1,"message":"error here"}]}}"#;
            let data = make_lsp_message(body);

            let pending: Mutex<PendingMap> = Mutex::new(HashMap::new());
            let name: Arc<str> = Arc::from("test");
            let diagnostics: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));

            let mut reader = BufReader::new(Cursor::new(data));
            let _ = LspTransport::reader_loop(&name, &mut reader, &pending, &diagnostics).await;

            let cache = diagnostics.lock().await;
            let diags = cache.get("file:///src/main.rs").unwrap();
            assert_eq!(diags.len(), 1);
            assert_eq!(diags[0].message, "error here");
        });
    }

    #[test]
    fn write_message_format() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let expected = make_lsp_message(body);
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(expected.starts_with(header.as_bytes()));
        assert!(expected.ends_with(body.as_bytes()));
    }

    #[test]
    fn parse_error_response() {
        smol::block_on(async {
            let body =
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"invalid request"}}"#;
            let data = make_lsp_message(body);

            let pending: Mutex<PendingMap> = Mutex::new(HashMap::new());
            let name: Arc<str> = Arc::from("test");
            let diagnostics: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));

            let (tx, rx) = channel::bounded(1);
            pending.lock().await.insert(1, tx);

            let mut reader = BufReader::new(Cursor::new(data));
            let _ = LspTransport::reader_loop(&name, &mut reader, &pending, &diagnostics).await;

            let result = rx.try_recv().unwrap();
            assert!(matches!(result, Err(LspError::RequestFailed { .. })));
        });
    }
}
