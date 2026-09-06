use std::sync::Arc;

use flume::Sender;
use maki_config::RemoteControlConfig;
use tiny_http::{Method, Response, Server};

use crate::state::RemoteState;

pub(crate) const INDEX_HTML: &str = include_str!("index.html");

/// How long a POST waits for the event loop to confirm it handled the
/// request. The loop only flips state or forwards, so this is generous.
/// Prompt bodies carry base64 uploads: ~6MB of files fit with headroom.
const MAX_BODY_BYTES: usize = 32 << 20;
const TOKEN_BYTES: usize = 16;
/// `2 * TOKEN_BYTES` hex chars: 128 bits of entropy, the only gate.
const TOKEN_LEN: usize = 2 * TOKEN_BYTES;

/// A browser upload riding on a prompt.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RemoteFile {
    pub name: String,
    #[serde(default)]
    pub media_type: String,
    /// Standard-alphabet base64 of the bytes.
    pub data: String,
    /// `attach` feeds images to the model; `save` writes into the session's
    /// working directory and references the path in the prompt.
    #[serde(default)]
    pub mode: UploadMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadMode {
    #[default]
    Attach,
    Save,
}

/// One pending action the event loop must perform, answered over `reply`.
/// `session` selects a tab by id; `None` means the focused one.
#[derive(Debug)]
pub enum RemoteRequest {
    Prompt {
        session: Option<String>,
        text: String,
        files: Vec<RemoteFile>,
        reply: Sender<Result<(), String>>,
    },
    Answer {
        session: Option<String>,
        request_id: String,
        answer: String,
        reply: Sender<Result<(), String>>,
    },
    Stop {
        session: Option<String>,
        reply: Sender<Result<(), String>>,
    },
    Command {
        session: Option<String>,
        cmdline: String,
        reply: Sender<Result<(), String>>,
    },
    Sessions {
        reply: Sender<serde_json::Value>,
    },
    ModelGet {
        session: Option<String>,
        reply: Sender<serde_json::Value>,
    },
    ModelSet {
        session: Option<String>,
        spec: Option<String>,
        thinking: Option<String>,
        fast: Option<bool>,
        reply: Sender<Result<serde_json::Value, String>>,
    },
    /// A web client connected and needs the current session state: history,
    /// stats, status. The loop answers with the JSON payload to emit as the
    /// first SSE frame, so a fresh tab opens on the TUI's world.
    Snapshot {
        session: Option<String>,
        reply: Sender<serde_json::Value>,
    },
    /// The slash commands the focused session knows, for the web picker.
    Commands {
        session: Option<String>,
        reply: Sender<serde_json::Value>,
    },
    /// Picker data: available models plus the session's mode/yolo/fast.
    Options {
        session: Option<String>,
        reply: Sender<serde_json::Value>,
    },
    /// Turn a permission flag or the mode on/off; answers with fresh options.
    SetOptions {
        session: Option<String>,
        yolo: Option<bool>,
        mode: Option<String>,
        reply: Sender<serde_json::Value>,
    },
    /// One level of a directory under the session's cwd, for the file panel.
    /// `path` is relative to cwd (empty string for the root).
    FilesList {
        session: Option<String>,
        path: String,
        reply: Sender<Result<serde_json::Value, String>>,
    },
    /// A file's content, capped and UTF-8 checked, for the file panel's viewer.
    FileRead {
        session: Option<String>,
        path: String,
        reply: Sender<Result<serde_json::Value, String>>,
    },
    /// Overwrite an existing file's content from the file panel's editor.
    FileWrite {
        session: Option<String>,
        path: String,
        content: String,
        reply: Sender<Result<(), String>>,
    },
    /// `git status --porcelain` under the session's cwd, for badges in the
    /// file panel's tree.
    GitStatus {
        session: Option<String>,
        reply: Sender<Result<serde_json::Value, String>>,
    },
    /// A unified diff for one file against `HEAD` (or the whole file, marked
    /// added, when it's untracked).
    GitDiff {
        session: Option<String>,
        path: String,
        reply: Sender<Result<serde_json::Value, String>>,
    },
}

impl RemoteRequest {
    /// The tab this request targets; `None` is the focused one.
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Prompt { session, .. }
            | Self::Answer { session, .. }
            | Self::Stop { session, .. }
            | Self::Command { session, .. }
            | Self::ModelGet { session, .. }
            | Self::ModelSet { session, .. }
            | Self::Snapshot { session, .. }
            | Self::Commands { session, .. }
            | Self::Options { session, .. }
            | Self::SetOptions { session, .. }
            | Self::FilesList { session, .. }
            | Self::FileRead { session, .. }
            | Self::FileWrite { session, .. }
            | Self::GitStatus { session, .. }
            | Self::GitDiff { session, .. } => session.as_deref(),
            Self::Sessions { .. } => None,
        }
    }
}

pub struct RemoteServer {
    server: Server,
    dispatcher: crate::dispatch::Dispatcher,
    token: [u8; TOKEN_LEN],
    bound_port: u16,
}

impl RemoteServer {
    /// Binds the listener, returning the server and the public
    /// `https://{domain}/{token}` URL to hand the user.
    pub fn bind(
        config: &RemoteControlConfig,
        requests: Sender<RemoteRequest>,
    ) -> Result<(Arc<Self>, String), crate::RemoteError> {
        let Some(domain) = config.domain.as_deref() else {
            return Err(crate::RemoteError::Bind {
                bind: config.bind.clone(),
                port: config.port,
                source: std::io::Error::other("remote_control.domain is not set in the config"),
            });
        };
        let addr = format!("{}:{}", config.bind, config.port);
        let server = Server::http(&addr).map_err(|e| {
            tracing::warn!(%addr, error = %e, "remote control bind failed");
            crate::RemoteError::Bind {
                bind: config.bind.clone(),
                port: config.port,
                source: std::io::Error::other(e.to_string()),
            }
        })?;
        let bound_port = server
            .server_addr()
            .to_ip()
            .map(|a| a.port())
            .unwrap_or(config.port);
        let token = generate_token();
        let url = format!("https://{domain}/{}", String::from_utf8_lossy(&token));
        let server = Self {
            server,
            dispatcher: crate::dispatch::Dispatcher {
                state: RemoteState::new(),
                requests,
            },
            token,
            bound_port,
        };
        Ok((Arc::new(server), url))
    }

    /// Serves until the process tears the thread down or the listener dies.
    /// tiny_http is synchronous, so this belongs on a dedicated thread.
    pub fn serve(self: &Arc<Self>) {
        loop {
            match self.server.recv() {
                Ok(request) => self.handle(request),
                Err(e) => {
                    tracing::warn!(error = %e, "remote control: accept failed");
                    return;
                }
            }
        }
    }

    /// Unblocks the thread parked in `recv` so it can observe process exit.
    pub fn unblock(&self) {
        self.server.unblock();
    }

    /// Ends every SSE stream and unblocks the serving thread, so `serve`
    /// returns and the listener closes. Idempotent.
    pub fn shutdown(&self) {
        self.dispatcher.state.send_shutdown();
        self.server.unblock();
    }

    /// The live fan-out hub the event loop mirrors session state into.
    pub fn state(&self) -> &RemoteState {
        &self.dispatcher.state
    }

    /// The port actually bound. Differs from the config when the caller
    /// asked for port 0 (ephemeral), which tests do.
    pub fn port(&self) -> u16 {
        self.bound_port
    }

    fn route(
        &self,
        path: &str,
        method: &Method,
    ) -> (Option<String>, Option<crate::dispatch::Route>, String) {
        let Some(rest) = path.strip_prefix('/') else {
            return (None, None, String::new());
        };
        let (token, tail) = match rest.split_once('/') {
            Some((token, tail)) => (token, tail),
            None => (rest, ""),
        };
        if !constant_time_eq(token.as_bytes(), &self.token) {
            return (None, None, String::new());
        }
        crate::dispatch::parse_tail(tail, method.as_str())
    }

    fn handle(&self, mut request: tiny_http::Request) {
        let (session, route, query) = self.route(request.url(), request.method());
        let body = if reads_body(route) {
            match read_body(&mut request) {
                Ok(body) => body,
                Err(_) => {
                    let _ = request.respond(Response::empty(413));
                    return;
                }
            }
        } else {
            String::new()
        };
        // The single token is the whole story in standalone mode: everyone
        // who reaches the page holds it and gets full control.
        match self
            .dispatcher
            .dispatch(route, session, &query, &body, "anon·control")
        {
            crate::dispatch::DispatchOutcome::NotFound => {
                let _ = request.respond(Response::empty(404));
            }
            crate::dispatch::DispatchOutcome::Index => {
                let response = Response::from_string(INDEX_HTML)
                    .with_header(content_type("text/html; charset=utf-8"));
                let _ = request.respond(response);
            }
            // SSE streams live as long as the browser tab, so they must not
            // occupy the accept loop: each gets its own thread and its own
            // subscriber. The snapshot is fetched here so it strictly
            // precedes any event fanned out to this subscriber. Everything
            // else is handled inline.
            crate::dispatch::DispatchOutcome::Events { mut source, .. } => {
                std::thread::Builder::new()
                    .name("remote-control-sse".into())
                    .spawn(move || serve_events(request, &mut source))
                    .expect("spawn SSE thread");
            }
            crate::dispatch::DispatchOutcome::Svg(body) => {
                let response = Response::from_data(body).with_header(content_type("image/svg+xml"));
                let _ = request.respond(response);
            }
            crate::dispatch::DispatchOutcome::Posted(status, error) => {
                let body = match error {
                    Some(reason) => serde_json::json!({"error": reason})
                        .to_string()
                        .into_bytes(),
                    None => Vec::new(),
                };
                let has_error = !body.is_empty();
                let mut response = Response::from_data(body).with_status_code(status);
                if has_error {
                    response = response.with_header(content_type("application/json"));
                }
                let _ = request.respond(response);
            }
            crate::dispatch::DispatchOutcome::Json { status, body } => {
                let response = Response::from_data(body)
                    .with_status_code(status)
                    .with_header(content_type("application/json"));
                let _ = request.respond(response);
            }
        }
    }
}

fn serve_events(request: tiny_http::Request, source: &mut crate::dispatch::SseSource) {
    // Handled by hand instead of `respond`: tiny_http's chunked encoder
    // buffers 8 KiB before sending, which would sit on small SSE frames
    // forever. The raw writer lets each frame flush as it is produced.
    // `X-Accel-Buffering: no` does the same for whatever reverse proxy sits
    // in front (this mode is documented as needing one for TLS) — without
    // it, a buffering proxy (nginx by default) withholds this stream's
    // bytes until its own buffer fills or the connection closes.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Accel-Buffering: no\r\n\r\n";
    let outcome = (|| -> std::io::Result<()> {
        let mut writer = request.into_writer();
        writer.write_all(head.as_bytes())?;
        writer.flush()?;
        while let Some(frame) = source.next_frame() {
            writer.write_all(&frame)?;
            writer.flush()?;
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        tracing::debug!(error = %e, "remote control: SSE stream ended");
    }
}

fn content_type(value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes("Content-Type", value).expect("static content type")
}

/// Routes that carry a request body the HTTP handler must read before
/// dispatch; everything else answers from the path alone.
fn reads_body(route: Option<crate::dispatch::Route>) -> bool {
    matches!(
        route,
        Some(
            crate::dispatch::Route::Prompt
                | crate::dispatch::Route::Answer
                | crate::dispatch::Route::Stop
                | crate::dispatch::Route::Command
                | crate::dispatch::Route::ModelPost
                | crate::dispatch::Route::OptionsPost
        )
    )
}

fn read_body(request: &mut tiny_http::Request) -> Result<String, ()> {
    use std::io::Read;
    let mut body = Vec::new();
    let mut reader = request.as_reader().take(MAX_BODY_BYTES as u64 + 1);
    reader.read_to_end(&mut body).map_err(|_| ())?;
    if body.len() > MAX_BODY_BYTES {
        return Err(());
    }
    String::from_utf8(body).map_err(|_| ())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn generate_token() -> [u8; TOKEN_LEN] {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).expect("rng failed");
    let mut hex = [0u8; TOKEN_LEN];
    for (i, byte) in bytes.iter().enumerate() {
        hex[2 * i] = HEX_TABLE[(*byte >> 4) as usize];
        hex[2 * i + 1] = HEX_TABLE[(*byte & 0xf) as usize];
    }
    hex
}

const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server(token: [u8; TOKEN_LEN]) -> RemoteServer {
        let (tx, _rx) = flume::unbounded();
        let server = Server::http("127.0.0.1:0").unwrap();
        RemoteServer {
            server,
            dispatcher: crate::dispatch::Dispatcher {
                state: RemoteState::new(),
                requests: tx,
            },
            token,
            bound_port: 0,
        }
    }

    fn hex_token(s: &str) -> [u8; TOKEN_LEN] {
        let mut out = [0u8; TOKEN_LEN];
        out.copy_from_slice(s.as_bytes());
        out
    }

    #[test]
    fn token_hex_has_right_shape() {
        let token = generate_token();
        assert!(token.iter().all(|b| HEX_TABLE.contains(b)));
    }

    #[test]
    fn routes_require_exact_token_prefix() {
        let server = test_server(hex_token("abcdef0123456789abcdef0123456789"));
        let get = &Method::Get;
        let post = &Method::Post;
        let cases: &[(&str, &Method, Option<&str>, Option<crate::dispatch::Route>)] = &[
            (
                "/abcdef0123456789abcdef0123456789/",
                get,
                None,
                Some(crate::dispatch::Route::Index),
            ),
            (
                "/abcdef0123456789abcdef0123456789",
                get,
                None,
                Some(crate::dispatch::Route::Index),
            ),
            (
                "/abcdef0123456789abcdef0123456789/events",
                get,
                None,
                Some(crate::dispatch::Route::Events),
            ),
            (
                "/abcdef0123456789abcdef0123456789/prompt",
                post,
                None,
                Some(crate::dispatch::Route::Prompt),
            ),
            (
                "/abcdef0123456789abcdef0123456789/command",
                post,
                None,
                Some(crate::dispatch::Route::Command),
            ),
            (
                "/abcdef0123456789abcdef0123456789/sessions",
                get,
                None,
                Some(crate::dispatch::Route::Sessions),
            ),
            (
                "/abcdef0123456789abcdef0123456789/model",
                get,
                None,
                Some(crate::dispatch::Route::ModelGet),
            ),
            (
                "/abcdef0123456789abcdef0123456789/model",
                post,
                None,
                Some(crate::dispatch::Route::ModelPost),
            ),
            (
                "/abcdef0123456789abcdef0123456789/s/42/events",
                get,
                Some("42"),
                Some(crate::dispatch::Route::Events),
            ),
            (
                "/abcdef0123456789abcdef0123456789/s/42/",
                get,
                Some("42"),
                Some(crate::dispatch::Route::Index),
            ),
            (
                "/abcdef0123456789abcdef0123456789/s/42/prompt",
                post,
                Some("42"),
                Some(crate::dispatch::Route::Prompt),
            ),
            ("/wrongtoken/events", get, None, None),
            ("/abcdef0123456789abcdef0123456788/events", get, None, None),
            ("/abcdef0123456789abcdef0123456789/prompt", get, None, None),
            ("//events", get, None, None),
        ];
        for (path, method, session, route) in cases {
            let (got_session, got_route, _query) = server.route(path, method);
            assert_eq!(&got_session.as_deref(), session, "path {path}");
            assert_eq!(&got_route, route, "path {path}");
        }
    }

    #[test]
    fn constant_time_eq_covers_lengths() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
