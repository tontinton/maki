use std::{
    io::{Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use tungstenite::Message as WsMessage;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Anchor binary under test; overridden by the harness when the bin is not
/// built yet.
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
    port: u16,
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
    AnchorProcess { child, port }
}

fn http(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                .as_bytes(),
        )
        .unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
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

/// Drive a fake instance: WS dial + answer one forwarded GET /{token}/.
fn fake_instance(port: u16, name: &str, token: &str, link: &str) -> Vec<u8> {
    let mut socket = tungstenite::connect(format!("ws://127.0.0.1:{port}/ws"))
        .unwrap()
        .0;
    let hello = serde_json::json!({"instance_name": name, "registration_token": token}).to_string();
    socket.send(WsMessage::text(hello)).unwrap();
    let message = socket.read().unwrap();
    let WsMessage::Text(text) = message else {
        panic!("expected forwarded request, got {message:?}");
    };
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["method"], "GET");
    assert_eq!(parsed["path"], format!("/{link}/"));
    let body = format!("hello from {name}").into_bytes();
    let reply = serde_json::json!({"conn_id": parsed["conn_id"], "status": 200, "body": body, "final": true});
    socket.send(WsMessage::text(reply.to_string())).unwrap();
    body
}

#[test]
fn tunnel_carries_browser_requests() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    thread::sleep(Duration::from_millis(300));

    // Register through the CLI path: insert via direct DB? The binary owns the
    // db. Use `tokens add` CLI against the same db file (WAL allows concurrent).
    let out = Command::new(anchor_bin())
        .args(["tokens", "add", "host-x", "--db", db.to_str().unwrap()])
        .output()
        .expect("run maki-anchor tokens add");
    assert!(out.status.success());
    let reg_token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(reg_token.len(), 32);

    // Mint a link and dial the tunnel as the instance.
    let out = Command::new(anchor_bin())
        .args(["tokens", "link", "host-x", "--db", db.to_str().unwrap()])
        .output()
        .expect("run maki-anchor tokens link");
    assert!(out.status.success());
    let link = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let ws_port = anchor.port + 1;
    let link_for_thread = link.clone();
    // The instance's tunnel stays open (the anchor writer thread holds the
    // command channel), so the dial thread is never joined.
    thread::spawn(move || fake_instance(ws_port, "host-x", &reg_token, &link_for_thread));

    thread::sleep(Duration::from_millis(300));
    let (status, body) = http(anchor.port, "GET", &format!("/{link}/"), &[]);
    assert_eq!(status, 200);
    assert_eq!(body, b"hello from host-x");
}

#[test]
fn anchor_serves_instances_and_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    thread::sleep(Duration::from_millis(300));
    let (status, _) = http(anchor.port, "GET", "/instances", &[]);
    assert_eq!(status, 200);
    let (status, _) = http(anchor.port, "GET", "/sessions", &[]);
    assert_eq!(status, 200);
    let (status, _) = http(anchor.port, "GET", "/nope", &[]);
    assert_eq!(status, 404);
}
