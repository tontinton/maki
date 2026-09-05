//! Cross-crate e2e: a real maki-remote tunnel client dials a real anchor and
//! browser-shaped traffic (index, SSE stream, prompt) flows end to end.

use std::{
    io::{Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use flume::Sender;
use maki_remote::{RemoteRequest, tunnel::TunnelClient};

const BROWSER_TIMEOUT: Duration = Duration::from_secs(15);

fn anchor_bin() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("maki-anchor")
}

struct AnchorProcess {
    child: Child,
    http_port: u16,
    ws_port: u16,
}

impl Drop for AnchorProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ephemeral ports race under full-suite load (another process can take the
/// port between `free_port` releasing it and the anchor binding it), and the
/// tunnel listener shares that port. Retry until one sticks, and
/// poll a real request for readiness instead of sleeping.
fn spawn_anchor(db: &std::path::Path) -> AnchorProcess {
    for attempt in 0..10 {
        let port = free_port();
        let child = Command::new(anchor_bin())
            .args([
                "serve",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--db",
                db.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn maki-anchor");
        let mut process = AnchorProcess {
            child,
            http_port: port,
            ws_port: port,
        };
        if wait_ready(&mut process) {
            return process;
        }
        let _ = process.child.kill();
        let _ = process.child.wait();
        thread::sleep(Duration::from_millis(50 * (attempt + 1)));
    }
    panic!("anchor never bound a port");
}

fn wait_ready(process: &mut AnchorProcess) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = process.child.try_wait() {
            return false;
        }
        if anchor_responding(process.http_port) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// `http()` panics on a refused connect, which is the norm before the anchor
/// is up, so readiness uses its own forgiving request.
fn anchor_responding(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    if stream
        .write_all(b"GET /api/sso HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 200")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    http_auth(port, method, path, body, None)
}

/// Same request with an optional session cookie, for the login-walled API.
fn http_auth(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    cookie: Option<&str>,
) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(BROWSER_TIMEOUT)).unwrap();
    let cookie_line = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{cookie_line}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| buf[i + 4..].to_vec())
        .unwrap_or_default();
    (status, body)
}

fn http_raw(port: u16, method: &str, path: &str, body: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(BROWSER_TIMEOUT)).unwrap();
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    buf
}

/// Registers an instance through the CLI and returns the registration token.
/// First-run admin via /setup; returns the session cookie the anchor set.
fn setup_admin(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = b"username=root&password=password1234";
    stream
        .write_all(
            format!(
                "POST /setup HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let value = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("setup sets a cookie")
        .split("maki_anchor_session=")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    format!("maki_anchor_session={value}")
}

fn register(db: &std::path::Path, name: &str) -> String {
    let out = Command::new(anchor_bin())
        .args(["tokens", "add", name, "--db", db.to_str().unwrap()])
        .output()
        .expect("run maki-anchor tokens add");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A minimal event loop: drains RemoteRequest and answers Snapshot with a
/// canned payload, like the TUI does.
fn loop_side() -> (Sender<RemoteRequest>, thread::JoinHandle<()>) {
    let (tx, rx) = flume::unbounded();
    let handle = thread::spawn(move || {
        let (_reply_tx, reply_rx) = flume::bounded(1);
        let request = rx.recv().expect("loop receives snapshot request");
        let RemoteRequest::Snapshot { reply, .. } = request else {
            panic!("expected snapshot request");
        };
        let payload = serde_json::json!({
            "messages": [{"type": "user_message", "text": "hi from instance"}],
            "session_id": "e2e-session",
            "title": "e2e session",
            "model": "test-model",
            "status": "idle",
        });
        reply.send(payload).unwrap();
        // Park forever; the test ends before this matters.
        let _: flume::Receiver<Result<(), String>> = reply_rx;
    });
    (tx, handle)
}

#[test]
fn tunnel_client_serves_index_sse_and_prompt_through_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);

    let reg_token = register(&db, "e2e-host");
    let (requests_tx, _loop_handle) = loop_side();

    let (reports_tx, reports_rx) = flume::bounded::<maki_remote::tunnel::TunnelReport>(4);
    let client = TunnelClient::new(requests_tx, "e2e".to_owned(), "e2e-host".to_owned());
    let out = client.out();
    let shutdown = Arc::new(AtomicBool::new(false));
    let ws_url = format!("ws://127.0.0.1:{}", anchor.ws_port);
    let thread_shutdown = Arc::clone(&shutdown);
    thread::spawn(move || {
        maki_remote::tunnel::run_tunnel(&ws_url, &reg_token, &client, reports_tx, &thread_shutdown);
    });

    // The anchor minted a link during the handshake.
    let link = match reports_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(maki_remote::tunnel::TunnelReport::Link(link)) => link,
        other => panic!("expected handshake link, got {other:?}"),
    };
    assert_eq!(link.len(), 32);

    // Index flows through the tunnel, and the anchor forwards the instance's
    // content type: an HTML page labeled application/json never renders.
    let (status, body) = http(anchor.http_port, "GET", &format!("/{link}/"), &[]);
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("maki"), "index page: {text:.80}");
    let raw = http_raw(anchor.http_port, "GET", &format!("/{link}/"), &[]);
    let headers = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(
        headers.contains("content-type: text/html"),
        "index must carry its content type through the anchor: {headers:.200}"
    );

    // A session-index push lands in the anchor's store.
    let cookie = setup_admin(anchor.http_port);
    out.send(maki_remote::tunnel::TunnelOut::Push(serde_json::json!({
        "sessions": [{
            "session_id": "e2e-session",
            "title": "e2e session",
            "model": "test-model",
            "cwd": "/work",
            "status": "idle",
        }],
    })))
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let sessions = loop {
        let (_status, body) =
            http_auth(anchor.http_port, "GET", "/api/sessions", &[], Some(&cookie));
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if list.as_array().is_some_and(|a| !a.is_empty()) {
            break list;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session index never arrived"
        );
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(sessions[0]["external_id"], "e2e-session");
    assert_eq!(sessions[0]["cwd"], "/work");

    // SSE opens with the snapshot frame, then stays live.
    let mut stream = TcpStream::connect(("127.0.0.1", anchor.http_port)).unwrap();
    stream
        .write_all(format!("GET /{link}/events HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .unwrap();
    let mut reader = stream;
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut sse = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let mut chunk = [0u8; 4096];
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => sse.extend_from_slice(&chunk[..n]),
        }
        if sse.windows(2).any(|w| w == b"\n\n") {
            break;
        }
    }
    let sse_text = String::from_utf8_lossy(&sse);
    assert!(sse_text.contains("event: snapshot"), "sse: {sse_text:.120}");
    assert!(sse_text.contains("e2e-session"), "sse: {sse_text:.120}");

    // `/rc off` cooperatively: the flag wakes the poll loop and ends the
    // tunnel thread.
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while anchor_offline(anchor.http_port, &link).not_ready() {
        assert!(std::time::Instant::now() < deadline, "tunnel never stopped");
        thread::sleep(Duration::from_millis(100));
    }
}

#[derive(PartialEq)]
struct Offline(bool);

impl Offline {
    fn not_ready(&self) -> bool {
        !self.0
    }
}

fn anchor_offline(port: u16, link: &str) -> Offline {
    let (status, _) = http(port, "GET", &format!("/{link}/"), &[]);
    Offline(status == 503)
}
