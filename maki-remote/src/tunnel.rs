//! Outbound tunnel to an anchor server. The anchor forwards browser traffic
//! over the WebSocket; this side answers it with the same dispatch logic the
//! standalone server uses, so both modes are identical from the user's view.
//!
//! One thread owns the socket in both directions: reads are bounded by a
//! short socket timeout so the loop can also ship queued replies, keep the
//! connection warm with pings, and notice the shutdown flag. A cloned handle
//! would be impossible over TLS anyway.

use std::{
    io,
    net::TcpStream,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tungstenite::{Message as WsMessage, client::IntoClientRequest, stream::MaybeTlsStream};

use crate::RemoteRequest;

const READ_POLL: Duration = Duration::from_millis(20);
const KEEPALIVE_PING: Duration = Duration::from_secs(30);
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(15);
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// What the tunnel thread tells the UI thread about its lifecycle, so a
/// dropped link reads as "anchor link lost, reconnecting" rather than
/// silence, and a dead token does not retry forever in the background.
#[derive(Debug, Clone)]
pub enum TunnelReport {
    /// Registered; carries the share link the anchor minted for this tunnel.
    /// Arrives again after every successful reconnect (with a new link).
    Link(String),
    /// The socket dropped and a reconnect is coming: carries the reason.
    Lost(String),
    /// The anchor rejected the registration; no further attempts.
    Refused(String),
}

/// Anything the tunnel ships to the anchor: an HTTP reply chunk or an
/// unsolicited instance -> anchor push.
#[derive(Debug)]
pub enum TunnelOut {
    Reply(TunnelReplyWire),
    Push(serde_json::Value),
}

/// A reply frame heading back to the anchor over the tunnel. Bodies ride as
/// base64; a JSON number array would cost four bytes per byte.
#[derive(Debug, serde::Serialize)]
pub struct TunnelReplyWire {
    pub conn_id: u64,
    pub status: u16,
    pub content_type: &'static str,
    #[serde(serialize_with = "serde_b64::serialize")]
    pub body: Vec<u8>,
    #[serde(rename = "final")]
    pub final_chunk: bool,
}

/// Browser traffic forwarded by the anchor: one HTTP-shaped request.
#[derive(Debug, serde::Deserialize)]
pub struct ForwardedRequest {
    pub conn_id: u64,
    pub method: String,
    pub path: String,
    #[serde(default, deserialize_with = "serde_b64::deserialize")]
    pub body: Vec<u8>,
}

/// The anchor's handshake frame: the share link minted for this tunnel.
#[derive(Debug, serde::Deserialize)]
struct LinkFrame {
    link: String,
}

/// Everything the tunnel needs to answer forwarded traffic: the fan-out hub
/// for SSE and the request channel the event loop drains, both reused from
/// standalone mode. Outbound frames (replies, session-index pushes) enter
/// through `out`, which the tunnel thread drains.
pub struct TunnelClient {
    pub state: crate::state::RemoteState,
    pub requests: flume::Sender<RemoteRequest>,
    /// Registration token as 32 lowercase hex chars.
    pub token_hex: String,
    pub instance_name: String,
    dispatcher: crate::dispatch::Dispatcher,
    out_tx: std::sync::mpsc::Sender<TunnelOut>,
    out_rx: Mutex<Option<std::sync::mpsc::Receiver<TunnelOut>>>,
}

impl TunnelClient {
    pub fn new(
        requests: flume::Sender<RemoteRequest>,
        token_hex: String,
        instance_name: String,
    ) -> Self {
        let state = crate::state::RemoteState::new();
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        Self {
            dispatcher: crate::dispatch::Dispatcher {
                state: state.clone(),
                requests: requests.clone(),
            },
            state,
            requests,
            token_hex,
            instance_name,
            out_tx,
            out_rx: Mutex::new(Some(out_rx)),
        }
    }

    /// The sender event loop side uses to queue replies and pushes.
    pub fn out(&self) -> std::sync::mpsc::Sender<TunnelOut> {
        self.out_tx.clone()
    }

    fn take_out_rx(&self) -> Option<std::sync::mpsc::Receiver<TunnelOut>> {
        self.out_rx.lock().unwrap().take()
    }

    fn dispatcher(&self) -> &crate::dispatch::Dispatcher {
        &self.dispatcher
    }
}

/// Runs the outbound tunnel, reconnecting with capped exponential backoff
/// until `shutdown` flips. Share links arrive on `reports` for every
/// successful registration; a refused registration (bad token) is fatal,
/// since retrying a door that will not open only wastes it. Blocking;
/// belongs on a dedicated thread.
pub fn run_tunnel(
    anchor_url: &str,
    registration_token: &str,
    client: &TunnelClient,
    reports: flume::Sender<TunnelReport>,
    shutdown: &AtomicBool,
) {
    let ws_url = normalize_ws_url(anchor_url);
    let out_rx = client.take_out_rx().expect("first take of the out channel");
    let out_tx = client.out();
    let mut attempt = 0u32;
    loop {
        let lost = match tunnel_once(
            &ws_url,
            registration_token,
            client,
            &out_rx,
            &out_tx,
            &reports,
            shutdown,
        ) {
            Ok(()) => return,
            Err(lost) => lost,
        };
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        attempt += 1;
        let delay = backoff_delay(attempt);
        let _ = reports.send(TunnelReport::Lost(format!(
            "{lost} — reconnecting in {}s (attempt {attempt})",
            delay.as_secs()
        )));
        tracing::warn!(attempt, ?delay, reason = %lost, "anchor link lost");
        if sleep_responding(delay, shutdown) {
            return;
        }
    }
}

/// Exponential from 1s, capped at 15s: attempt 1 waits 1s, 2 waits 2s,
/// then 4, 8, 15, 15, …
fn backoff_delay(attempt: u32) -> Duration {
    let scaled = BACKOFF_FIRST.checked_mul(1u32 << attempt.saturating_sub(1).min(5));
    scaled.unwrap_or(BACKOFF_CAP).min(BACKOFF_CAP)
}

/// Sleeps in poll slices so `/rc off` stops the tunnel mid-backoff. Returns
/// whether shutdown was requested.
fn sleep_responding(delay: Duration, shutdown: &AtomicBool) -> bool {
    let until = Instant::now() + delay;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(SHUTDOWN_POLL));
    }
}

/// One connection's whole life: dial, register, serve frames until the
/// anchor or the network ends it. `Ok(())` means stop everything (shutdown
/// or a fatal refusal); `Err` carries a reason worth retrying.
fn tunnel_once(
    ws_url: &str,
    registration_token: &str,
    client: &TunnelClient,
    out_rx: &std::sync::mpsc::Receiver<TunnelOut>,
    out_tx: &std::sync::mpsc::Sender<TunnelOut>,
    reports: &flume::Sender<TunnelReport>,
    shutdown: &AtomicBool,
) -> Result<(), String> {
    let request = ws_url
        .into_client_request()
        .map_err(|e| format!("bad anchor url: {e}"))?;
    let (mut socket, _response) = match tungstenite::connect(request) {
        Ok(pair) => pair,
        Err(source) => {
            return Err(match &source {
                tungstenite::Error::Http(resp) => format!(
                    "anchor answered {}",
                    resp.status().canonical_reason().unwrap_or("an error")
                ),
                other => format!("cannot reach anchor: {other}"),
            });
        }
    };
    let hello = serde_json::json!({
        "instance_name": client.instance_name,
        "registration_token": registration_token,
    })
    .to_string();
    if socket.send(WsMessage::text(hello)).is_err() {
        return Err("anchor hung up during registration".into());
    }
    // First anchor frame hands back the freshly minted control link. Read it
    // without the loop's short poll timeout: the handshake round trip is not
    // something 20ms can promise. Anything other than a link frame is the
    // anchor's plain-text refusal (bad token or name); that is not a
    // transient, so it must not burn in the reconnect loop.
    let first = match socket.read() {
        Ok(WsMessage::Text(text)) if text.starts_with('{') => text,
        Ok(WsMessage::Text(text)) => {
            let reason = if text.contains("auth") {
                "token or instance name rejected by the anchor".into()
            } else {
                format!("anchor said: {text}")
            };
            let _ = reports.send(TunnelReport::Refused(reason));
            return Ok(());
        }
        _ => return Err("anchor hung up during registration".into()),
    };
    let Ok(link_frame) = serde_json::from_str::<LinkFrame>(&first) else {
        return Err("anchor sent an unusable handshake".into());
    };
    let _ = reports.send(TunnelReport::Link(link_frame.link));
    // From here on, reads are bounded so the loop can also ship replies,
    // keepalive-ping, and notice the shutdown flag.
    set_read_timeout(socket.get_ref());

    let mut next_ping = Instant::now() + KEEPALIVE_PING;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            ship_pending(&mut socket, out_rx);
            return Ok(());
        }
        while let Ok(out) = out_rx.try_recv() {
            let frame = match out {
                TunnelOut::Reply(reply) => serde_json::to_string(&reply),
                TunnelOut::Push(push) => serde_json::to_string(&push),
            };
            let Ok(text) = frame else { break };
            if socket.send(WsMessage::text(text)).is_err() {
                return Err("anchor socket died mid-write".into());
            }
            if shutdown.load(Ordering::Relaxed) {
                // `/rc down` queues its revoke push and flips the flag in the
                // same breath; a shutdown exit that skips the backlog kills
                // the message the anchor needs to revoke the link.
                ship_pending(&mut socket, out_rx);
                return Ok(());
            }
        }
        if Instant::now() >= next_ping {
            next_ping = Instant::now() + KEEPALIVE_PING;
            if socket
                .send(WsMessage::Ping(tungstenite::Bytes::new()))
                .is_err()
            {
                return Err("keepalive ping failed".into());
            }
        }
        let _ = socket.flush();
        match socket.read() {
            Ok(WsMessage::Text(text)) => {
                let Ok(parsed) = serde_json::from_str::<ForwardedRequest>(&text) else {
                    continue;
                };
                handle_forwarded(client, parsed, out_tx.clone());
            }
            Ok(WsMessage::Ping(payload)) => {
                let _ = socket.send(WsMessage::Pong(payload));
            }
            Ok(WsMessage::Close(_)) => return Err("anchor closed the link".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err("anchor link dropped".into()),
        }
    }
}

/// Writes everything still queued on the out channel before the socket goes
/// away; a lost goodbye is a link that stays alive for nothing.
fn ship_pending(
    socket: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    out_rx: &std::sync::mpsc::Receiver<TunnelOut>,
) {
    while let Ok(out) = out_rx.try_recv() {
        let text = match out {
            TunnelOut::Reply(reply) => serde_json::to_string(&reply),
            TunnelOut::Push(push) => serde_json::to_string(&push),
        };
        let Ok(text) = text else { return };
        if socket.send(WsMessage::text(text)).is_err() {
            return;
        }
    }
}

/// Config and docs speak in `http(s)://` anchor URLs; the socket speaks ws.
fn normalize_ws_url(anchor_url: &str) -> String {
    let base = anchor_url.trim_end_matches('/');
    let base = match base.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => match base.strip_prefix("http://") {
            Some(rest) => format!("ws://{rest}"),
            None => base.to_owned(),
        },
    };
    format!("{base}/ws")
}

fn set_read_timeout(stream: &MaybeTlsStream<TcpStream>) {
    let tcp = match stream {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(s) => s.get_ref(),
        _ => return,
    };
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(READ_POLL));
}

/// Strips the instance token prefix the standalone mode carries, and converts
/// the body. Byte-safe on purpose: a raw index into a URL the browser chose
/// panics the tunnel thread on multibyte paths.
fn forwarded_tail<'a>(path: &'a str, token_hex: &str) -> &'a str {
    let path = path.strip_prefix('/').unwrap_or(path);
    let tail = path.strip_prefix(token_hex).unwrap_or(path);
    tail.trim_start_matches('/')
}

/// Answers one forwarded request on the tunnel's out channel. Non-SSE routes
/// reply inline; SSE spawns a producer that streams frames until the stream
/// ends.
fn handle_forwarded(
    client: &TunnelClient,
    request: ForwardedRequest,
    out: std::sync::mpsc::Sender<TunnelOut>,
) {
    // The anchor forwards the bare tail (it owns the link token); accept the
    // token-prefixed shape too so the dispatcher stays testable standalone.
    let tail = forwarded_tail(&request.path, &client.token_hex);
    let (session, route, query) = crate::dispatch::parse_tail(tail, &request.method);
    let body = String::from_utf8_lossy(&request.body).into_owned();
    let outcome = client.dispatcher().dispatch(route, session, &query, &body);
    let conn_id = request.conn_id;
    let send = |status: u16, content_type: &'static str, body: Vec<u8>, final_chunk: bool| {
        let _ = out.send(TunnelOut::Reply(TunnelReplyWire {
            conn_id,
            status,
            content_type,
            body,
            final_chunk,
        }));
    };
    match outcome {
        crate::dispatch::DispatchOutcome::NotFound => {
            send(404, "text/plain", b"not found".to_vec(), true)
        }
        crate::dispatch::DispatchOutcome::Index => send(
            200,
            "text/html; charset=utf-8",
            crate::server::INDEX_HTML.as_bytes().to_vec(),
            true,
        ),
        crate::dispatch::DispatchOutcome::Posted(status, error) => match error {
            None => send(status, "text/plain", Vec::new(), true),
            Some(reason) => send(
                status,
                "application/json",
                serde_json::json!({"error": reason})
                    .to_string()
                    .into_bytes(),
                true,
            ),
        },
        crate::dispatch::DispatchOutcome::Json { status, body } => {
            send(status, "application/json", body, true)
        }
        crate::dispatch::DispatchOutcome::Svg(body) => send(200, "image/svg+xml", body, true),
        // SSE: the producer blocks on the fan-out channel and ships frames;
        // the tunnel thread forwards them until the stream ends.
        crate::dispatch::DispatchOutcome::Events { mut source } => {
            if out
                .send(TunnelOut::Reply(TunnelReplyWire {
                    conn_id,
                    status: 200,
                    content_type: "text/event-stream",
                    body: Vec::new(),
                    final_chunk: false,
                }))
                .is_err()
            {
                return;
            }
            std::thread::Builder::new()
                .name("remote-tunnel-sse".into())
                .spawn(move || {
                    while let Some(frame) = source.next_frame() {
                        if out
                            .send(TunnelOut::Reply(TunnelReplyWire {
                                conn_id,
                                status: 200,
                                content_type: "text/event-stream",
                                body: frame,
                                final_chunk: false,
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = out.send(TunnelOut::Reply(TunnelReplyWire {
                        conn_id,
                        status: 200,
                        content_type: "text/event-stream",
                        body: Vec::new(),
                        final_chunk: true,
                    }));
                })
                .expect("spawn tunnel SSE thread");
        }
    }
}

mod serde_b64 {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const REPORT_WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn normalize_ws_url_maps_http_schemes() {
        assert_eq!(
            normalize_ws_url("https://maki.example.com"),
            "wss://maki.example.com/ws"
        );
        assert_eq!(
            normalize_ws_url("http://localhost:8688/"),
            "ws://localhost:8688/ws"
        );
        assert_eq!(normalize_ws_url("ws://127.0.0.1:9"), "ws://127.0.0.1:9/ws");
    }

    #[test]
    fn forwarded_tail_survives_multibyte_before_token() {
        let token = "a".repeat(32);
        // The token-prefixed shape strips.
        assert_eq!(
            forwarded_tail(&format!("/{token}/events"), &token),
            "events"
        );
        // A multibyte path that would panic a raw byte slice passes through.
        assert_eq!(forwarded_tail("/éè/extra/path", &token), "éè/extra/path");
        // Bare anchor-forwarded path.
        assert_eq!(forwarded_tail("/events", &token), "events");
    }

    #[test]
    fn tunnel_once_reports_the_failure_after_rewriting_the_scheme() {
        // An https anchor config must be dialed as wss://.../ws: the failure
        // proves the scheme was rewritten before connecting, without needing
        // a TLS terminator in the test.
        let dead_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (tx, _rx) = flume::unbounded();
        let client = TunnelClient::new(tx, "e".repeat(32), "host".into());
        let out_rx = client.take_out_rx().unwrap();
        let out_tx = client.out();
        let (reports, _sink) = flume::unbounded();
        let lost = tunnel_once(
            &format!("wss://127.0.0.1:{dead_port}/ws"),
            "token",
            &client,
            &out_rx,
            &out_tx,
            &reports,
            &AtomicBool::new(false),
        )
        .expect_err("a dead port must not connect");
        assert!(lost.contains("cannot reach anchor"), "reason: {lost}");
    }

    #[test]
    fn the_tunnel_reconnects_after_the_anchor_drops_the_link() {
        // A stub anchor: accept two connections, hand each a link frame, then
        // slam the door. The supervisor must narrate the drop and come back.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for n in 0..2 {
                let stream = listener.accept().unwrap().0;
                let mut ws = match tungstenite::accept(stream) {
                    Ok(ws) => ws,
                    Err(_) => return,
                };
                let _hello = ws.read().unwrap();
                let link = serde_json::json!({ "link": format!("tok{n}") }).to_string();
                let _ = ws.send(WsMessage::text(link));
                let _ = ws.send(WsMessage::Close(None));
                let _ = ws.flush();
            }
        });
        let (requests, _sink) = flume::unbounded();
        let client = TunnelClient::new(requests, "e".repeat(32), "host".into());
        let (reports, seen) = flume::bounded::<TunnelReport>(8);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let worker = std::thread::spawn(move || {
            run_tunnel(
                &format!("http://127.0.0.1:{port}"),
                "token",
                &client,
                reports,
                &thread_shutdown,
            );
        });
        let first = match seen.recv_timeout(REPORT_WAIT) {
            Ok(TunnelReport::Link(link)) => link,
            other => panic!("expected the first link, got {other:?}"),
        };
        assert_eq!(first, "tok0");
        assert!(matches!(
            seen.recv_timeout(REPORT_WAIT),
            Ok(TunnelReport::Lost(_))
        ));
        let second = match seen.recv_timeout(REPORT_WAIT) {
            Ok(TunnelReport::Link(link)) => link,
            other => panic!("expected the reconnected link, got {other:?}"),
        };
        assert_eq!(second, "tok1", "the client redials and re-registers");
        shutdown.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), BACKOFF_CAP);
        assert_eq!(backoff_delay(64), BACKOFF_CAP, "no shift overflow");
    }

    #[test]
    fn backoff_sleep_releases_on_shutdown() {
        let shutdown = AtomicBool::new(true);
        assert!(
            sleep_responding(Duration::from_secs(30), &shutdown),
            "a flip during /rc off must stop the wait instantly"
        );
        let shutdown = AtomicBool::new(false);
        assert!(
            !sleep_responding(Duration::from_millis(1), &shutdown),
            "a short wait expires normally"
        );
    }

    #[test]
    fn reply_frame_bodies_round_trip_through_base64() {
        let wire = TunnelReplyWire {
            conn_id: 7,
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: vec![0, 255, 128, 10],
            final_chunk: true,
        };
        let text = serde_json::to_string(&wire).unwrap();
        assert!(text.contains("\"body\":\"AP+ACg==\""), "wire: {text}");
        let decoded: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded["content_type"], "text/html; charset=utf-8");
        assert_eq!(decoded["final"], true);
        let request: ForwardedRequest =
            serde_json::from_str(r#"{"conn_id":1,"method":"GET","path":"/x","body":""}"#).unwrap();
        assert!(request.body.is_empty());
    }
}
