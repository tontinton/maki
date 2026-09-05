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

/// Ephemeral ports are a race under full-suite load: another test can take
/// the port between `free_port` dropping its listener and the anchor binding.
/// So retry until one that sticks is found, and wait for readiness instead
/// of sleeping.
fn spawn_anchor(db: &std::path::Path) -> AnchorProcess {
    for attempt in 0..10 {
        let port = free_port();
        let mut child = Command::new(anchor_bin())
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
        match wait_ready(&mut child, port) {
            Ok(()) => return AnchorProcess { child, port },
            Err(()) => {
                let _ = child.kill();
                let _ = child.wait();
                thread::sleep(Duration::from_millis(50 * (attempt + 1)));
            }
        }
    }
    panic!("anchor never bound a port");
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

/// A plain TCP connect is not readiness: the kernel queues connections into
/// the listener backlog before the server loop runs, and the anchor still
/// tunnel shares the one port now. A real request succeeds only
/// once both ports are bound and the loop is serving.
fn wait_ready(child: &mut Child, port: u16) -> Result<(), ()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return Err(());
        }
        if anchor_responding(port) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(())
}

/// Poll until the anchor stops reporting the instance offline, so tunnel
/// attachment never depends on a fixed sleep.
fn wait_online(port: u16, link: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let (status, _) = http(port, "GET", &format!("/{link}/"), &[]);
        if status != 503 {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("instance never came online behind {link}");
}

fn http(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    http_auth(port, method, path, body, None)
}

/// Same request with an optional session cookie, for the login-walled pages.
fn http_auth(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    cookie: Option<&str>,
) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let cookie_line = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{cookie_line}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())
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

/// Drive a fake instance: WS dial, then answer every forwarded request.
/// `wait_online` probes this too, so it must keep serving, not stop after one.
fn fake_instance(port: u16, name: &str, token: &str) -> Vec<u8> {
    let mut socket = tungstenite::connect(format!("ws://127.0.0.1:{port}/ws"))
        .unwrap()
        .0;
    let hello = serde_json::json!({"instance_name": name, "registration_token": token}).to_string();
    socket.send(WsMessage::text(hello)).unwrap();
    let link_frame = socket.read().unwrap();
    let WsMessage::Text(link_text) = link_frame else {
        panic!("expected link frame, got {link_frame:?}");
    };
    let minted: serde_json::Value = serde_json::from_str(&link_text).unwrap();
    assert_eq!(minted["link"].as_str().map(str::len), Some(32));
    let body = format!("hello from {name}").into_bytes();
    while let Ok(WsMessage::Text(text)) = socket.read() {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(conn_id) = parsed.get("conn_id").and_then(|v| v.as_u64()) else {
            continue;
        };
        let reply = serde_json::json!({
            "conn_id": conn_id,
            "status": 200,
            "content_type": "text/plain",
            "body": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &body),
            "final": true,
        });
        if socket.send(WsMessage::text(reply.to_string())).is_err() {
            break;
        }
    }
    body
}

#[test]
fn tunnel_carries_browser_requests() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);

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

    let ws_port = anchor.port;
    // The instance's tunnel stays open (the anchor writer thread holds the
    // command channel), so the dial thread is never joined.
    thread::spawn(move || fake_instance(ws_port, "host-x", &reg_token));

    wait_online(anchor.port, &link);
    let (status, body) = http(anchor.port, "GET", &format!("/{link}/"), &[]);
    assert_eq!(status, 200);
    assert_eq!(body, b"hello from host-x");
}

#[test]
fn the_paged_management_ui_needs_a_session_and_the_json_api_lives_under_api() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let cookie = setup_admin(anchor.port);
    for path in ["/", "/instances", "/links", "/admin"] {
        let (status, body) = http_auth(anchor.port, "GET", path, &[], Some(&cookie));
        assert_eq!(status, 200, "{path} must render for the admin");
        assert!(
            String::from_utf8_lossy(&body).contains("<nav"),
            "{path} is a page"
        );
    }
    let (status, body) = http_auth(anchor.port, "GET", "/api/instances", &[], Some(&cookie));
    assert_eq!(status, 200);
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    let (status, body) = http_auth(anchor.port, "GET", "/api/sessions", &[], Some(&cookie));
    assert_eq!(status, 200);
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    let (status, _) = http_auth(anchor.port, "GET", "/nope", &[], Some(&cookie));
    assert_eq!(status, 404);
}

#[test]
fn first_run_setup_closes_behind_itself_and_every_page_demands_login() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let full = http_full(anchor.port, "GET", "/", b"");
    assert!(full.starts_with("HTTP/1.1 302"), "empty store: {full}");
    assert!(
        full.to_lowercase().contains("location: /setup"),
        "forced to setup: {full}"
    );
    let (status, body) = http(anchor.port, "GET", "/setup", &[]);
    assert_eq!(status, 200);
    assert!(
        String::from_utf8_lossy(&body).contains("Create admin"),
        "setup form: {status}"
    );
    let cookie = setup_admin(anchor.port);
    assert_eq!(
        http(anchor.port, "GET", "/", &[]).0,
        302,
        "no cookie, no dashboard"
    );
    let (status, _) = http_auth(anchor.port, "GET", "/setup", &[], Some(&cookie));
    assert_eq!(status, 302, "the setup door shut behind the admin");
    let (status, _) = http_auth(anchor.port, "GET", "/admin", &[], Some(&cookie));
    assert_eq!(status, 200, "the admin who set up reaches the admin page");
    let (status, _) = http(anchor.port, "GET", "/install.sh", &[]);
    assert_eq!(status, 200, "installers stay open for hosts to join");
}

#[test]
fn api_instances_is_not_treated_as_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let cookie = setup_admin(anchor.port);
    // POST /api/instances should be JSON API, not proxy 404 "invalid or expired link"
    let (status, body) = http_auth(
        anchor.port,
        "POST",
        "/api/instances",
        br#"{"name":"test"}"#,
        Some(&cookie),
    );
    // Should be 200 (any) or 401/403 JSON, never 404 text/plain "invalid or expired link"
    assert_ne!(status, 404);
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.starts_with("invalid or expired"),
        "API was mistaken for token path: {text}"
    );
    // Body must be JSON
    assert!(
        serde_json::from_slice::<serde_json::Value>(&body).is_ok(),
        "API must return JSON, got: {text}"
    );
}

#[test]
fn api_users_and_grants_require_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let cookie = setup_admin(anchor.port);
    let (status, body) = http_auth(anchor.port, "GET", "/api/users", &[], Some(&cookie));
    assert_eq!(status, 200);
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    let (status, body) = http_auth(anchor.port, "GET", "/api/grants", &[], Some(&cookie));
    assert_eq!(status, 200);
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
}

const COOKIE_NAME: &str = "maki_anchor_session";

/// Runs the first-run setup as a brand-new admin and returns the session
/// cookie the anchor handed back.
fn setup_admin(port: u16) -> String {
    let full = http_full(
        port,
        "POST",
        "/setup",
        b"username=root&password=password1234",
    );
    assert!(
        full.starts_with("HTTP/1.1 302"),
        "setup must land an admin in: {full}"
    );
    let set_cookie = full
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("setup must set a session cookie");
    let value = set_cookie
        .split(&format!("{COOKIE_NAME}="))
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    format!("{COOKIE_NAME}={value}")
}

/// Full response (headers included) for tests that assert on them.
fn http_full(port: u16, method: &str, path: &str, body: &[u8]) -> String {
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
    String::from_utf8_lossy(&buf).into_owned()
}

/// Fake instance that stays up and answers every forwarded request.
fn fake_instance_serving(port: u16, name: &str, token: &str) {
    let Ok((mut socket, _)) = tungstenite::connect(format!("ws://127.0.0.1:{port}/ws")) else {
        return;
    };
    let hello = serde_json::json!({"instance_name": name, "registration_token": token}).to_string();
    socket.send(WsMessage::text(hello)).unwrap();
    let _link = socket.read().unwrap();
    while let Ok(WsMessage::Text(text)) = socket.read() {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(conn_id) = parsed.get("conn_id").and_then(|v| v.as_u64()) else {
            continue;
        };
        let reply = serde_json::json!({
            "conn_id": conn_id,
            "status": 200,
            "content_type": "application/json",
            "body": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"{\"ok\":true}"),
            "final": true,
        });
        if socket.send(WsMessage::text(reply.to_string())).is_err() {
            break;
        }
    }
}

fn cli(db: &std::path::Path, args: &[&str]) -> String {
    let mut full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    full.push("--db".into());
    full.push(db.to_str().unwrap().into());
    let out = Command::new(anchor_bin())
        .args(&full)
        .output()
        .expect("run maki-anchor");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn instance_api_cannot_rotate_an_existing_instances_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let cookie = setup_admin(anchor.port);
    let (status, body) = http_auth(
        anchor.port,
        "POST",
        "/api/instances",
        br#"{"name":"dup-host"}"#,
        Some(&cookie),
    );
    assert_eq!(
        status,
        200,
        "first create must succeed: {}",
        String::from_utf8_lossy(&body)
    );
    let (status, body) = http_auth(
        anchor.port,
        "POST",
        "/api/instances",
        br#"{"name":"dup-host"}"#,
        Some(&cookie),
    );
    assert_eq!(status, 409, "re-creating must not rotate the token");
    let text = String::from_utf8_lossy(&body).to_lowercase();
    assert!(
        text.contains("tokens add"),
        "error should point at the CLI: {text}"
    );
}

#[test]
fn the_control_center_feeds_managers_and_shrugs_at_anonymity() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let cookie = setup_admin(anchor.port);
    let instance = cli(&db, &["tokens", "add", "center-host"]);
    let _ = instance; // cli prints the registration token
    let link = cli(
        &db,
        &[
            "tokens",
            "link",
            "center-host",
            "control",
            "--ttl-hours",
            "2",
        ],
    );

    // The admin holding the link learns rights, instance and the link list.
    let (status, body) = http_auth(
        anchor.port,
        "GET",
        &format!("/api/center?link={link}"),
        &[],
        Some(&cookie),
    );
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["instance"]["name"], "center-host");
    assert_eq!(json["can_manage"], true);
    assert_eq!(json["rights"], "controller");
    assert!(
        !json["links"].as_array().unwrap().is_empty(),
        "links listed"
    );
    // The invite round trip: mint via api, the fresh link resolves.
    let (status, body) = http_auth(
        anchor.port,
        "POST",
        "/api/links/mint",
        serde_json::json!({"link": link, "rights": "view", "hours": 1})
            .to_string()
            .as_bytes(),
        Some(&cookie),
    );
    assert_eq!(
        status,
        200,
        "invite mint: {}",
        String::from_utf8_lossy(&body)
    );
    let invite: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let invite_token = invite["token"].as_str().unwrap().to_owned();
    assert_eq!(invite["path"], format!("/{invite_token}/"));
    let (status, _) = http(anchor.port, "GET", &format!("/{invite_token}/"), &[]);
    assert_eq!(status, 503, "a valid invite on an offline instance waits");
    let (status, _) = http(anchor.port, "GET", &format!("/{link}nope/"), &[]);
    assert_eq!(
        status, 302,
        "a mangled path is management territory, behind the wall"
    );

    // Closing kills the URL immediately.
    let (status, _) = http_auth(
        anchor.port,
        "POST",
        "/api/links/close",
        serde_json::json!({"link": invite_token})
            .to_string()
            .as_bytes(),
        Some(&cookie),
    );
    assert_eq!(status, 200);
    let (status, _) = http(anchor.port, "GET", &format!("/{invite_token}/"), &[]);
    assert_eq!(status, 404, "closed means gone, not offline");

    // No cookie: the wall bounces the management API.
    let (status, _) = http(anchor.port, "GET", &format!("/api/center?link={link}"), &[]);
    assert_eq!(status, 302, "anonymous callers do not get the center feed");
}

#[test]
fn oidc_can_be_configured_from_the_admin_page() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let cookie = setup_admin(anchor.port);
    let post =
        |body: &[u8]| http_auth(anchor.port, "POST", "/api/config/oidc", body, Some(&cookie));
    let (status, _) = post(br#"{"issuer":"https://auth.test/realm","client_id":"anchor"}"#);
    assert_eq!(status, 400, "a half-configured SSO is refused");
    let (status, body) = post(
        br#"{"issuer":"https://auth.test/realm","client_id":"anchor","client_secret":"shh","origin":"https://maki.test"}"#,
    );
    assert_eq!(status, 200, "save: {}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("restart"));
    let (status, body) = post(br#"{"issuer":"","client_id":"","client_secret":"","origin":""}"#);
    assert_eq!(status, 200, "clear: {}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("cleared"));
}

#[test]
fn the_qr_endpoint_renders_share_links_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    let token = "ab".repeat(16);
    let (status, body) = http(
        anchor.port,
        "GET",
        &format!("/qr?text=https%3A%2F%2Fhost%2F{token}%2F"),
        &[],
    );
    assert_eq!(status, 200);
    assert!(
        String::from_utf8_lossy(&body).starts_with("<svg"),
        "svg body"
    );
    let (status, _) = http(anchor.port, "GET", "/qr?text=hello+world", &[]);
    assert_eq!(status, 400, "not a general-purpose qr service");
}

#[test]
fn scoped_and_view_links_are_enforced_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    thread::sleep(Duration::from_millis(300));
    let reg = cli(&db, &["tokens", "add", "gate-host"]);
    let scoped = cli(
        &db,
        &["tokens", "link", "gate-host", "view", "--session", "sid-1"],
    );
    let view = cli(&db, &["tokens", "link", "gate-host", "view"]);
    let control = cli(&db, &["tokens", "link", "gate-host", "control"]);
    thread::spawn(move || fake_instance_serving(anchor.port, "gate-host", &reg));
    wait_online(anchor.port, &control);

    // A scoped link's bare index bounces into the session path, and anything
    // for another session is refused.
    let response = http_full(anchor.port, "GET", &format!("/{scoped}/"), &[]);
    assert!(
        response.starts_with("HTTP/1.1 302"),
        "scoped root: {response:.60}"
    );
    assert!(
        response.contains(&format!("Location: /{scoped}/s/sid-1/")),
        "redirect target: {response}"
    );
    let (status, _) = http(
        anchor.port,
        "GET",
        &format!("/{scoped}/s/other/events"),
        &[],
    );
    assert_eq!(status, 404, "foreign session must be refused");
    let (status, _) = http(
        anchor.port,
        "GET",
        &format!("/{scoped}/s/sid-1/events"),
        &[],
    );
    assert_eq!(status, 200, "own session must pass");

    // View links refuse writes; control links allow them.
    let (status, _) = http(
        anchor.port,
        "POST",
        &format!("/{view}/prompt"),
        br#"{"text":"hi"}"#,
    );
    assert_eq!(status, 403, "view link must not accept prompts");
    let (status, _) = http(
        anchor.port,
        "POST",
        &format!("/{control}/prompt"),
        br#"{"text":"hi"}"#,
    );
    assert_eq!(status, 200, "control link must accept prompts");
}

#[test]
fn local_login_locks_out_after_five_failures() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);
    for _ in 0..5 {
        let (status, _) = http(
            anchor.port,
            "POST",
            "/api/login",
            br#"{"username":"ghost","password":"guessed"}"#,
        );
        assert_eq!(status, 401);
    }
    let (status, body) = http(
        anchor.port,
        "POST",
        "/api/login",
        br#"{"username":"ghost","password":"guessed"}"#,
    );
    assert_eq!(status, 429, "sixth guess must be rate limited, not checked");
    assert!(String::from_utf8_lossy(&body).contains("too many"));
}

/// Request with extra headers (cookies).
fn http_with_headers(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &[String],
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for header in headers {
        raw.push_str(header);
        raw.push_str("\r\n");
    }
    raw.push_str("\r\n");
    stream.write_all(raw.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

/// Log in over the API and return the session cookie header value.
fn login(port: u16, username: &str, password: &str) -> String {
    let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
    let (status, text) = http_with_headers(
        port,
        "POST",
        "/api/login",
        body.as_bytes(),
        &["Content-Type: application/json".to_owned()],
    );
    assert_eq!(status, 200, "login {username}: {text:.120}");
    let line = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("login sets a cookie");
    let value = line["set-cookie:".len()..]
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_owned();
    assert!(value.starts_with("maki_anchor_session="), "cookie: {value}");
    value
}

#[test]
fn local_users_log_in_and_the_links_gate_follows_grants() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let anchor = spawn_anchor(&db);

    // root is created first, so the CLI's --admin sticks; peon stays plain.
    cli_stdin(
        &db,
        &["users", "add", "root", "--admin"],
        "RootPass123\nRootPass123\n",
    );
    cli_stdin(&db, &["users", "add", "peon"], "PeonPass123\nPeonPass123\n");
    cli(&db, &["tokens", "add", "host-g"]);

    // Wrong password is refused; right password yields a session cookie.
    let body = br#"{"username":"peon","password":"wrong-password"}"#;
    let (status, _) = http_with_headers(
        anchor.port,
        "POST",
        "/api/login",
        body,
        &["Content-Type: application/json".to_owned()],
    );
    assert_eq!(
        status, 401,
        "argon verification must reject a wrong password"
    );
    let peon = login(anchor.port, "peon", "PeonPass123");

    // peon exists but has no grant: minting for host-g is refused.
    let (status, text) = http_with_headers(
        anchor.port,
        "GET",
        "/links?instance=host-g&rights=view",
        &[],
        &[format!("Cookie: {peon}")],
    );
    assert_eq!(status, 403, "no grant must not mint: {text:.120}");

    // Grant peon view access and the same request mints a link.
    let lookup = cli(&db, &["grants", "lookup", "local:peon"]);
    let peon_id = lookup.split_whitespace().next().expect("id sub");
    cli(&db, &["grants", "set", peon_id, "host-g", "view"]);
    let (status, text) = http_with_headers(
        anchor.port,
        "GET",
        "/links?instance=host-g&rights=view",
        &[],
        &[format!("Cookie: {peon}")],
    );
    assert_eq!(status, 200, "a grant opens minting: {text:.200}");
    assert!(text.contains("expires in 2h"), "link page expected");

    // An admin mints control links without needing a grant.
    let root = login(anchor.port, "root", "RootPass123");
    let (status, _) = http_with_headers(
        anchor.port,
        "GET",
        "/links?instance=host-g&rights=control",
        &[],
        &[format!("Cookie: {root}")],
    );
    assert_eq!(status, 200, "admin mints control links");
}

/// `users add` prompts on stdin; drive it with piped passwords.
fn cli_stdin(db: &std::path::Path, args: &[&str], stdin: &str) -> String {
    let mut full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    full.push("--db".into());
    full.push(db.to_str().unwrap().into());
    let mut child = Command::new(anchor_bin())
        .args(&full)
        .stdin(std::process::Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn maki-anchor");
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("wait maki-anchor");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
