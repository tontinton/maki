use std::sync::Arc;

use flume::Sender;
use maki_config::RemoteControlConfig;
use tiny_http::{Method, Response, Server};

use crate::state::RemoteState;

pub(crate) const INDEX_HTML: &str = include_str!("index.html");

/// How long a POST waits for the event loop to confirm it handled the
/// request. The loop only flips state or forwards, so this is generous.
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

    fn route(&self, path: &str, method: &Method) -> Option<crate::dispatch::Route> {
        let rest = path.strip_prefix('/')?;
        let (token, tail) = match rest.split_once('/') {
            Some((token, tail)) => (token, tail),
            None => (rest, ""),
        };
        if !constant_time_eq(token.as_bytes(), &self.token) {
            return None;
        }
        crate::dispatch::Route::from_tail(tail, method.as_str())
    }

    fn handle(&self, mut request: tiny_http::Request) {
        let route = self.route(request.url(), request.method());
        let body = if matches!(
            route,
            Some(
                crate::dispatch::Route::Prompt
                    | crate::dispatch::Route::Answer
                    | crate::dispatch::Route::Stop
            )
        ) {
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
        match self.dispatcher.dispatch(route, &body) {
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
            crate::dispatch::DispatchOutcome::Posted(status) => {
                let _ = request.respond(Response::empty(status));
            }
        }
    }
}

fn serve_events(request: tiny_http::Request, source: &mut crate::dispatch::SseSource) {
    // Handled by hand instead of `respond`: tiny_http's chunked encoder
    // buffers 8 KiB before sending, which would sit on small SSE frames
    // forever. The raw writer lets each frame flush as it is produced.
    let head =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n";
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
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789/", get),
            Some(crate::dispatch::Route::Index)
        ));
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789", get),
            Some(crate::dispatch::Route::Index)
        ));
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789/events", get),
            Some(crate::dispatch::Route::Events)
        ));
        assert!(matches!(
            server.route("/abcdef0123456789abcdef0123456789/prompt", post),
            Some(crate::dispatch::Route::Prompt)
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
            crate::dispatch::parse_json_field(r#"{"text":"hi"}"#, "text"),
            Some("hi".into())
        );
        assert_eq!(
            crate::dispatch::parse_json_field(r#"{"text":42}"#, "text"),
            None
        );
        assert_eq!(crate::dispatch::parse_json_field("not json", "text"), None);
    }

    #[test]
    fn constant_time_eq_covers_lengths() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
