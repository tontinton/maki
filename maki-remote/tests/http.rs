//! Real HTTP round trips against a bound server on an ephemeral port. These
//! are the only tests that see the whole path: routing, token gate, SSE
//! frames (snapshot first), and the request channel the event loop services.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use maki_config::RemoteControlConfig;
use maki_remote::{RemoteRequest, RemoteServer};

const RC_TEST_DOMAIN: &str = "rc.test";
const RC_TEST_SEED: &str = "seeded transcript line";
const RC_TEST_SESSION: &str = "sess-1";
const RC_TEST_STATUS: &str = "working";
const TOKEN_URL_PATH_LEN: usize = 32;
const SSE_HANDSHAKE_MS: u64 = 300;
/// Long enough for the server's own reply timeout (5s) to fire first.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

fn test_config() -> RemoteControlConfig {
    RemoteControlConfig {
        domain: Some(RC_TEST_DOMAIN.into()),
        // Ephemeral: every test owns its listener, so parallel runs and
        // restarts never contend.
        port: 0,
        bind: "127.0.0.1".into(),
        auto_start: false,
    }
}

/// A bound server plus its request surface, standing in for the event loop:
/// snapshots are answered inline, everything else lands on `requests`.
struct TestServer {
    handle: std::thread::JoinHandle<()>,
    url: String,
    requests: flume::Receiver<RemoteRequest>,
    server: Arc<RemoteServer>,
}

impl TestServer {
    fn state(&self) -> &maki_remote::RemoteState {
        self.server.state()
    }
}

impl std::ops::Deref for TestServer {
    type Target = RemoteServer;
    fn deref(&self) -> &RemoteServer {
        &self.server
    }
}

fn spawn_server() -> TestServer {
    let (tx, rx) = flume::unbounded();
    let (server, url) = RemoteServer::bind(&test_config(), tx).expect("bind");
    // Answer snapshot pings the way the event loop would; surface the rest.
    let (surface_tx, surface_rx) = flume::unbounded();
    std::thread::spawn(move || {
        loop {
            match rx.recv() {
                Ok(RemoteRequest::Snapshot { reply, .. }) => {
                    let _ = reply.send(serde_json::json!({
                        "messages": [{"type": "user_message", "text": RC_TEST_SEED}],
                        "status": "idle",
                    }));
                }
                Ok(other) => {
                    let _ = surface_tx.try_send(other);
                }
                Err(_) => return,
            }
        }
    });
    let _ = surface_rx;
    let handle = std::thread::spawn({
        let server = Arc::clone(&server);
        move || server.serve()
    });
    TestServer {
        handle,
        url,
        requests: surface_rx,
        server,
    }
}

fn http_exchange(port: u16, raw: &str) -> String {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).expect("connect to remote control server");
    stream
        .write_all(raw.as_bytes())
        .and_then(|_| stream.flush())
        .expect("write request");
    let mut buf = Vec::new();
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).unwrap();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn http_serves_index_only_behind_token() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();
    assert_eq!(token.len(), TOKEN_URL_PATH_LEN);

    let page = http_exchange(
        server.port(),
        &format!("GET /{token}/ HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\n\r\n"),
    );
    assert!(page.starts_with("HTTP/1.1 200"), "got {page:?}");
    assert!(page.contains("maki remote"));
    // Bug #1 regression: the index must carry text/html. If standalone drops
    // it, the anchor cannot forward it and the page never renders.
    assert!(
        page.to_ascii_lowercase()
            .contains("content-type: text/html"),
        "index must be served as HTML, got header block: {:?}",
        page.lines().take(6).collect::<Vec<_>>()
    );

    let blocked = http_exchange(
        server.port(),
        &format!("GET /wrongtoken HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\n\r\n"),
    );
    assert!(
        blocked.starts_with("HTTP/1.1 404"),
        "wrong token must 404, got {blocked:?}"
    );

    let method = http_exchange(
        server.port(),
        &format!("GET /{token}/prompt HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\n\r\n"),
    );
    assert!(
        method.starts_with("HTTP/1.1 404"),
        "GET on POST route must 404, got {method:?}"
    );
}

#[test]
fn scoped_prompt_carries_the_session_id() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();

    let text = "{\"text\":\"scoped\"}";
    let body = format!(
        "POST /{token}/s/sess-42/prompt HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
        text.len()
    );
    let client = std::thread::spawn({
        let port = server.port();
        move || http_exchange(port, &body)
    });

    let request = server
        .requests
        .recv_timeout(Duration::from_secs(5))
        .expect("scoped request");
    assert_eq!(
        request.session(),
        Some("sess-42"),
        "route prefix must reach the loop as session"
    );
    if let RemoteRequest::Prompt { reply, .. } = request {
        let _ = reply.send(Ok(()));
    }
    assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));
}

#[test]
fn prompt_post_reaches_request_channel() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();

    let text = "{\"text\":\"hi web\"}";
    let body = format!(
        "POST /{token}/prompt HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
        text.len()
    );
    let client = std::thread::spawn({
        let port = server.port();
        move || http_exchange(port, &body)
    });

    let request = server
        .requests
        .recv_timeout(Duration::from_secs(5))
        .expect("request");
    let RemoteRequest::Prompt { text, reply, .. } = request else {
        panic!("expected prompt");
    };
    assert_eq!(text, "hi web");
    reply.send(Err("not now".into())).unwrap();
    let reply = client.join().unwrap();
    assert!(
        reply.starts_with("HTTP/1.1 400"),
        "rejected request must 400, got {reply:?}"
    );
}

#[test]
fn answered_prompt_post_returns_ok() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();
    let surface = server.requests.clone();

    std::thread::spawn(move || {
        if let Ok(RemoteRequest::Stop { reply, .. }) = surface.recv_timeout(Duration::from_secs(5))
        {
            let _ = reply.send(Ok(()));
        }
    });

    let body = format!(
        "POST /{token}/stop HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\nContent-Length: 2\r\n\r\n{{}}"
    );
    let reply = http_exchange(server.port(), &body);
    assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
}

#[test]
fn commands_get_surfaces_and_answers_json() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();
    let surface = server.requests.clone();

    std::thread::spawn(move || {
        if let Ok(RemoteRequest::Commands { reply, .. }) =
            surface.recv_timeout(Duration::from_secs(5))
        {
            let _ = reply.send(serde_json::json!([{"name": "/rc", "description": "remote"}]));
        }
    });

    let reply = http_exchange(
        server.port(),
        &format!("GET /{token}/commands HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\n\r\n"),
    );
    assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
    assert!(
        reply.contains("/rc"),
        "body should carry the command: {reply:?}"
    );
}

#[test]
fn options_post_carries_yolo_and_mode() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();
    let surface = server.requests.clone();

    let client = std::thread::spawn({
        let port = server.port();
        let token = token.to_owned();
        move || {
            let body = "{\"yolo\":true,\"mode\":\"plan\"}";
            http_exchange(
                port,
                &format!(
                    "POST /{token}/options HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
        }
    });

    let request = surface
        .recv_timeout(Duration::from_secs(5))
        .expect("options request");
    let RemoteRequest::SetOptions {
        yolo, mode, reply, ..
    } = request
    else {
        panic!("expected SetOptions");
    };
    assert_eq!(yolo, Some(true));
    assert_eq!(mode.as_deref(), Some("plan"));
    let _ = reply.send(serde_json::json!({"mode": "plan", "yolo": true}));
    assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));
}

#[test]
fn options_post_rejects_empty_body() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();
    let reply = http_exchange(
        server.port(),
        &format!(
            "POST /{token}/options HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\nContent-Length: 2\r\n\r\n{{}}",
        ),
    );
    assert!(
        reply.starts_with("HTTP/1.1 400"),
        "empty set must 400: {reply:?}"
    );
}

#[test]
fn sse_opens_with_snapshot_then_carries_published_frames() {
    let server = spawn_server();
    let token = server.url.rsplit('/').next().unwrap();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .write_all(
            format!(
                "GET /{token}/events HTTP/1.1\r\nHost: {RC_TEST_DOMAIN}\r\nAccept: text/event-stream\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(SSE_HANDSHAKE_MS));

    let headers = {
        let mut buf = [0u8; 512];
        let n = stream.peek(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    };
    assert!(
        headers.to_ascii_lowercase().contains("text/event-stream"),
        "SSE must open event-stream, got {headers:?}"
    );
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("x-accel-buffering: no"),
        "SSE must disable reverse-proxy buffering or a buffering proxy \
         withholds the snapshot frame until more bytes arrive: {headers:?}"
    );

    server.state().send_status(RC_TEST_SESSION, RC_TEST_STATUS);
    let mut buf = Vec::new();
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).unwrap();
    let _ = stream.read_to_end(&mut buf);
    let body = String::from_utf8_lossy(&buf);
    assert!(
        body.contains("event: snapshot"),
        "snapshot frame missing, got {body:?}"
    );
    assert!(
        body.contains(RC_TEST_SEED),
        "snapshot payload missing, got {body:?}"
    );
    assert!(
        body.contains("event: status"),
        "status frame missing, got {body:?}"
    );
    assert!(
        body.contains(RC_TEST_STATUS),
        "status payload missing, got {body:?}"
    );

    server.shutdown();
    server.handle.join().unwrap();
}
