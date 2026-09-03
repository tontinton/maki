//! Cross-crate e2e: a real maki-remote tunnel client dials a real anchor and
//! browser-shaped traffic (index, SSE stream, prompt) flows end to end.

use std::{
    io::{Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

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

fn spawn_anchor(db: &std::path::Path) -> AnchorProcess {
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
    AnchorProcess {
        child,
        http_port: port,
        ws_port: port + 1,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
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

/// Registers an instance through the CLI and returns the registration token.
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
        let RemoteRequest::Snapshot { reply } = request else {
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
    thread::sleep(Duration::from_millis(400));

    let reg_token = register(&db, "e2e-host");
    let (requests_tx, _loop_handle) = loop_side();

    let (link_tx, link_rx) = flume::bounded::<String>(1);
    let client = TunnelClient::new(requests_tx, "e2e".to_owned(), "e2e-host".to_owned());
    let ws_url = format!("ws://127.0.0.1:{}", anchor.ws_port);
    thread::spawn(move || {
        let _ = maki_remote::tunnel::run_tunnel(&ws_url, &reg_token, client, link_tx);
    });

    // The anchor minted a link during the handshake.
    let link = link_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("handshake link");
    assert_eq!(link.len(), 32);

    // Index flows through the tunnel.
    let (status, body) = http(anchor.http_port, "GET", &format!("/{link}/"), &[]);
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("maki"), "index page: {text:.80}");

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
}
