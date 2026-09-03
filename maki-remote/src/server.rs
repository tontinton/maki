use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use flume::Sender;
use maki_config::RemoteControlConfig;
use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::state::{PermissionFrame, RemoteState, RemoteUpdate};

const INDEX_HTML: &str = include_str!("index.html");

const SSE_PING_SECS: u64 = 15;
/// How long a POST waits for the event loop to confirm it handled the
/// request. The loop only flips state or forwards, so this is generous.
const REQUEST_REPLY_TIMEOUT_SECS: u64 = 5;
const MAX_BODY_BYTES: usize = 1 << 20;
const TOKEN_BYTES: usize = 16;
/// `2 * TOKEN_BYTES` hex chars: 128 bits of entropy, the only gate.
const TOKEN_LEN: usize = 2 * TOKEN_BYTES;

/// One pending action the event loop must perform, answered over `reply`.
#[derive(Debug)]
pub enum RemoteRequest {
    Prompt {
        text: String,
        reply: Sender<Result<(), String>>,
    },
    Answer {
        request_id: String,
        answer: String,
        reply: Sender<Result<(), String>>,
    },
    Stop {
        reply: Sender<Result<(), String>>,
    },
    /// A web client connected and needs the current session state: history,
    /// stats, status. The loop answers with the JSON payload to emit as the
    /// first SSE frame, so a fresh tab opens on the TUI's world.
    Snapshot {
        reply: Sender<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Copy)]
enum Route {
    Index,
    Events,
    Prompt,
    Answer,
    Stop,
}

pub struct RemoteServer {
    server: Server,
    state: RemoteState,
    requests: Sender<RemoteRequest>,
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
            state: RemoteState::new(),
            requests,
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
        self.state.send_shutdown();
        self.server.unblock();
    }

    /// The live fan-out hub the event loop mirrors session state into.
    pub fn state(&self) -> &RemoteState {
        &self.state
    }

    /// The port actually bound. Differs from the config when the caller
    /// asked for port 0 (ephemeral), which tests do.
    pub fn port(&self) -> u16 {
        self.bound_port
    }

    fn route(&self, path: &str, method: &Method) -> Option<Route> {
        let rest = path.strip_prefix('/')?;
        let (token, tail) = match rest.split_once('/') {
            Some((token, tail)) => (token, tail),
            None => (rest, ""),
        };
        if !constant_time_eq(token.as_bytes(), &self.token) {
            return None;
        }
        match (tail, method) {
            ("", &Method::Get) => Some(Route::Index),
            ("events", &Method::Get) => Some(Route::Events),
            ("prompt", &Method::Post) => Some(Route::Prompt),
            ("answer", &Method::Post) => Some(Route::Answer),
            ("stop", &Method::Post) => Some(Route::Stop),
            _ => None,
        }
    }

    fn handle(&self, mut request: tiny_http::Request) {
        let Some(route) = self.route(request.url(), request.method()) else {
            let _ = request.respond(Response::empty(404));
            return;
        };
        match route {
            Route::Index => {
                let response = Response::from_string(INDEX_HTML)
                    .with_header(content_type("text/html; charset=utf-8"));
                let _ = request.respond(response);
            }
            // SSE streams live as long as the browser tab, so they must not
            // occupy the accept loop: each gets its own thread and its own
            // subscriber. The snapshot is fetched here so it strictly
            // precedes any event fanned out to this subscriber. Everything
            // else is handled inline.
            Route::Events => {
                let subscription = self.state.subscribe();
                let snapshot = self.snapshot_json();
                std::thread::Builder::new()
                    .name("remote-control-sse".into())
                    .spawn(move || serve_events(request, subscription, snapshot))
                    .expect("spawn SSE thread");
            }
            Route::Prompt | Route::Answer | Route::Stop => {
                let body = match read_body(&mut request) {
                    Ok(body) => body,
                    Err(_) => {
                        let _ = request.respond(Response::empty(413));
                        return;
                    }
                };
                let status = match self.dispatch_post(route, body) {
                    Some(()) => 200,
                    None => 400,
                };
                let _ = request.respond(Response::empty(status));
            }
        }
    }

    /// Asks the event loop for the current session snapshot. Empty object if
    /// the loop is wedged; the stream still opens with live events.
    fn snapshot_json(&self) -> serde_json::Value {
        let (tx, rx) = flume::bounded(1);
        if self
            .requests
            .try_send(RemoteRequest::Snapshot { reply: tx })
            .is_err()
        {
            return json!({});
        }
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .unwrap_or_else(|_| json!({}))
    }

    fn dispatch_post(&self, route: Route, body: String) -> Option<()> {
        let (request, reply_rx) = match route {
            Route::Prompt => {
                let text = parse_json_field(&body, "text")?;
                let (tx, rx) = flume::unbounded();
                (RemoteRequest::Prompt { text, reply: tx }, rx)
            }
            Route::Answer => {
                let value: serde_json::Value = serde_json::from_str(&body).ok()?;
                let request_id = value.get("request_id")?.as_str()?.to_owned();
                let answer = value.get("answer")?.as_str()?.to_owned();
                let (tx, rx) = flume::unbounded();
                (
                    RemoteRequest::Answer {
                        request_id,
                        answer,
                        reply: tx,
                    },
                    rx,
                )
            }
            Route::Stop => {
                let (tx, rx) = flume::unbounded();
                (RemoteRequest::Stop { reply: tx }, rx)
            }
            Route::Index | Route::Events => return None,
        };
        self.requests.try_send(request).ok()?;
        // The loop answers fast (it only flips state or forwards); still,
        // bound the wait so a wedged UI thread cannot leak connections.
        match reply_rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS)) {
            Ok(Ok(())) => Some(()),
            Ok(Err(reason)) => {
                tracing::debug!(%reason, "remote control: request rejected");
                None
            }
            Err(_) => None,
        }
    }
}

fn serve_events(
    request: tiny_http::Request,
    subscription: crate::state::Subscription,
    snapshot: serde_json::Value,
) {
    // Handled by hand instead of `respond`: tiny_http's chunked encoder
    // buffers 8 KiB before sending, which would sit on small SSE frames
    // forever. The raw writer lets each frame flush as it is produced.
    let head =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n";
    let outcome = (|| -> std::io::Result<()> {
        let mut writer = request.into_writer();
        writer.write_all(head.as_bytes())?;
        writer.flush()?;
        let mut body = SseBody {
            subscription,
            idle_deadline: None,
            buf: Vec::new(),
        };
        // The snapshot precedes everything fanned out from here on, so a
        // fresh tab renders history first and live events after.
        write_frame(&mut body.buf, "snapshot", &snapshot);
        loop {
            let mut frame = [0u8; 4096];
            let n = body.read(&mut frame)?;
            if n == 0 {
                break;
            }
            writer.write_all(&frame[..n])?;
            writer.flush()?;
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        tracing::debug!(error = %e, "remote control: SSE stream ended");
    }
}

/// Reads the fan-out channel and yields SSE frames. Never returns `Ok(0)`:
/// a zero read would end the stream, so the reader sleeps on the channel
/// instead and emits pings on long idles.
struct SseBody {
    subscription: crate::state::Subscription,
    idle_deadline: Option<std::time::Instant>,
    buf: Vec<u8>,
}

impl Read for SseBody {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.buf.is_empty() {
            let updates = &self.subscription.updates;
            let update = match self.idle_deadline {
                Some(deadline) => match updates.recv_deadline(deadline) {
                    Ok(update) => Some(update),
                    Err(flume::RecvTimeoutError::Timeout) => {
                        self.idle_deadline = None;
                        self.buf.extend_from_slice(b": ping\n\n");
                        None
                    }
                    Err(_) => return Ok(0),
                },
                None => match updates.recv() {
                    Ok(update) => Some(update),
                    Err(_) => return Ok(0),
                },
            };
            if let Some(update) = update {
                self.idle_deadline =
                    Some(std::time::Instant::now() + Duration::from_secs(SSE_PING_SECS));
                match update {
                    RemoteUpdate::Envelope { event, .. } => {
                        write_frame(&mut self.buf, "event", &event);
                    }
                    RemoteUpdate::Status { status, .. } => {
                        write_frame(&mut self.buf, "status", &json!(status));
                    }
                    RemoteUpdate::Permission { frame, .. } => {
                        write_frame(&mut self.buf, "permission", &frame_json(&frame));
                    }
                    RemoteUpdate::PermissionResolved { request_id, .. } => {
                        write_frame(
                            &mut self.buf,
                            "permission_resolved",
                            &json!({ "id": request_id }),
                        );
                    }
                    RemoteUpdate::Shutdown => return Ok(0),
                }
            }
        }
        self.drain_into(out)
    }
}

impl SseBody {
    fn drain_into(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.buf.len().min(out.len());
        if n == 0 {
            // An out slice too small for even one buffered byte would spin;
            // SSE frames are tens of bytes, so this cannot happen in
            // practice, but refuse rather than loop forever.
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "sse buffer larger than read slice",
            ));
        }
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

fn write_frame(buf: &mut Vec<u8>, event: &str, payload: &serde_json::Value) {
    use std::io::Write;
    let _ = write!(buf, "event: {event}\ndata: {payload}\n\n");
}

fn frame_json(frame: &PermissionFrame) -> serde_json::Value {
    json!({
        "id": frame.id,
        "tool": frame.tool,
        "scopes": frame.scopes,
    })
}

fn content_type(value: &str) -> Header {
    header("Content-Type", value)
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn read_body(request: &mut tiny_http::Request) -> Result<String, ()> {
    const LIMIT: u64 = MAX_BODY_BYTES as u64;
    let mut buf = Vec::new();
    let mut reader = request.as_reader().take(LIMIT + 1);
    reader.read_to_end(&mut buf).map_err(|_| ())?;
    if buf.len() as u64 > LIMIT {
        return Err(());
    }
    String::from_utf8(buf).map_err(|_| ())
}

fn parse_json_field(body: &str, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get(field)?.as_str().map(str::to_owned)
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
            state: RemoteState::new(),
            requests: tx,
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
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789/", get),
            Some(Route::Index)
        ));
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789", get),
            Some(Route::Index)
        ));
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789/events", get),
            Some(Route::Events)
        ));
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789/prompt", post),
            Some(Route::Prompt)
        ));
        assert!(server.route("/wrongtoken/events", get).is_none());
        assert!(
            server
                .route("/abcdef0123456789abcdef0123456788/events", get)
                .is_none()
        );
        assert!(
            server
                .route("/abcdef0123456789abcdef0123456789/prompt", get)
                .is_none()
        );
        assert!(server.route("//events", get).is_none());
    }

    #[test]
    fn prompt_body_parses_text_field() {
        assert_eq!(
            parse_json_field(r#"{"text":"hi"}"#, "text"),
            Some("hi".into())
        );
        assert_eq!(parse_json_field(r#"{"text":42}"#, "text"), None);
        assert_eq!(parse_json_field("not json", "text"), None);
    }

    #[test]
    fn constant_time_eq_covers_lengths() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
