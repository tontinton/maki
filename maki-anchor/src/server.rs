use std::{
    io::Write,
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, channel},
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use tungstenite::{Message as WsMessage, protocol::WebSocket};

use crate::{
    http::{self, Header, Response},
    hub::{self, Hub, TunnelCommand, TunnelPush},
    store::{self, Role, SessionRow, Store},
};

const TUNNEL_LINK_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// One thread per connection; cap the counts so a flood costs 503s and
/// refused sockets instead of unbounded threads.
const MAX_CONCURRENT_CONNECTIONS: usize = 512;
const MAX_CONCURRENT_TUNNELS: usize = 64;
const INSTALL_SH: &str = include_str!("../../install.sh");
const INSTALL_PS1: &str = include_str!("../../install.ps1");

/// Mints a share link, returning the raw token.
pub fn mint_link(
    store: &Store,
    instance_id: i64,
    session_id: Option<&str>,
    rights: Role,
    ttl: Duration,
) -> Result<String, store::StoreError> {
    let token = new_link_token();
    store.create_link(&token, instance_id, session_id, rights.as_str(), ttl)?;
    Ok(token)
}

fn request_path_session(tail: &str) -> Option<String> {
    let rest = tail.strip_prefix("s/")?;
    Some(rest.split('/').next().unwrap_or(rest).to_owned())
}

fn buffered(
    (status, content_type, body): (u16, String, Vec<u8>),
    request: http::Request,
) -> RouteOutcome {
    RouteOutcome::Buffered(Box::new(BufferedResponse {
        status,
        content_type,
        body,
        request,
    }))
}

fn new_link_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("rng failed");
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Outcome of routing one browser request.
enum RouteOutcome {
    /// Write this response to the request.
    Buffered(Box<BufferedResponse>),
    /// The response was already written (SSE stream); nothing to do.
    Streamed,
}

struct BufferedResponse {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    request: http::Request,
}

const MAX_BODY: usize = 10 * 1024 * 1024;

#[derive(Debug, serde::Deserialize)]
struct HelloFrame {
    instance_name: String,
    registration_token: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum TunnelWireFrame {
    Response {
        conn_id: u64,
        status: u16,
        #[serde(default)]
        content_type: Option<String>,
        #[serde(deserialize_with = "serde_b64::deserialize")]
        body: Vec<u8>,
        #[serde(rename = "final")]
        final_chunk: bool,
    },
}

mod serde_b64 {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize as _, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(&text).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Strip the leading `/{token}` path component used by the standalone remote UI.
/// Tokens are 32 hex chars (128-bit), so `/api/*` etc. are not mistaken for a token.
fn split_token_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix('/')?;
    let (token, tail) = rest.split_once('/')?;
    if token.len() != 32 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((token, tail))
}

pub fn serve(
    addr: &str,
    store: Arc<Store>,
    oidc: Option<crate::oidc::OidcConfig>,
    allow_local: bool,
    mint_tokens: store::MintTokens,
) -> Result<(), ServerError> {
    let listener = TcpListener::bind(addr).map_err(|source| ServerError::Bind {
        addr: addr.to_string(),
        source,
    })?;
    let hub = Hub::new();
    let auth = Arc::new(crate::auth::Auth::new(
        Arc::clone(&store),
        oidc,
        allow_local,
        mint_tokens,
    ));
    // One port, one accept loop: every connection is either a WebSocket
    // upgrade on /ws (an instance tunnel) or one HTTP request. The
    // connection cap bounds threads; the tunnel cap bounds how many
    // long-lived sockets a flood can pin.
    let connection_slots = Arc::new(AtomicUsize::new(0));
    let tunnel_slots = Arc::new(AtomicUsize::new(0));
    tracing::info!(addr, "anchor listening");
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                continue;
            }
        };
        if connection_slots.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
            connection_slots.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let hub = Arc::clone(&hub);
        let store = Arc::clone(&store);
        let auth = Arc::clone(&auth);
        let connections = Arc::clone(&connection_slots);
        let tunnels = Arc::clone(&tunnel_slots);
        thread::spawn(move || {
            let _slot = SlotGuard(connections);
            handle_connection(stream, peer, hub, store, auth, tunnels);
        });
    }
}

/// Frees a concurrency slot when the thread holding it ends.
struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn handle_connection(
    stream: std::net::TcpStream,
    peer: std::net::SocketAddr,
    hub: Arc<Hub>,
    store: Arc<Store>,
    auth: Arc<crate::auth::Auth>,
    tunnel_slots: Arc<AtomicUsize>,
) {
    let _ = stream.set_read_timeout(Some(http::HEAD_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let sink = match stream.try_clone() {
        Ok(sink) => sink,
        Err(_) => return,
    };
    let mut reader = std::io::BufReader::new(stream);
    let head = match http::read_head(&mut reader) {
        Ok(head) => head,
        Err(reject) => {
            if let Some(response) = reject.response() {
                let mut sink = sink;
                let _ = http::write_response(&mut sink, &response);
            }
            return;
        }
    };
    if head.is_upgrade() {
        if tunnel_slots.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT_TUNNELS {
            tunnel_slots.fetch_sub(1, Ordering::Relaxed);
            let mut sink = sink;
            let _ = http::write_response(
                &mut sink,
                &Response::from_string("too many tunnels").with_status_code(503),
            );
            return;
        }
        let _tunnel_slot = SlotGuard(tunnel_slots);
        accept_tunnel(reader, sink, head.replay, hub, store);
        return;
    }
    if head.content_length > 0
        && head
            .header("expect")
            .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
        && let Ok(mut cont) = sink.try_clone()
    {
        let _ = cont.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = cont.flush();
    }
    // Read the body up to the cap plus one byte, so handlers keep owning the
    // 413 decision exactly as before; the rest dies with the connection.
    let mut body = Vec::new();
    if head.content_length > 0 {
        use std::io::Read;
        let want = head.content_length.min(MAX_BODY + 1);
        let _ = (&mut reader).take(want as u64).read_to_end(&mut body);
    }
    let request = http::Request::new(head, body, sink, peer);
    handle_request(request, hub, store, auth);
}

/// One port's two souls: reads replay the already-consumed handshake bytes,
/// writes go straight to the socket. Cloning hands each direction to its own
/// thread, replacing the `try_clone` the raw listener used.
#[derive(Clone)]
struct HalfDuplex {
    read: Arc<Mutex<Replay<std::io::BufReader<std::net::TcpStream>>>>,
    write: Arc<Mutex<std::net::TcpStream>>,
}

struct Replay<R> {
    prefix: Vec<u8>,
    pos: usize,
    inner: R,
}

impl<R: std::io::Read> std::io::Read for Replay<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

impl std::io::Read for HalfDuplex {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read.lock().unwrap().read(buf)
    }
}

impl std::io::Write for HalfDuplex {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.write.lock().unwrap().flush()
    }
}

fn accept_tunnel(
    reader: std::io::BufReader<std::net::TcpStream>,
    sink: std::net::TcpStream,
    replay: Vec<u8>,
    hub: Arc<Hub>,
    store: Arc<Store>,
) {
    let duplex = HalfDuplex {
        read: Arc::new(Mutex::new(Replay {
            prefix: replay,
            pos: 0,
            inner: reader,
        })),
        write: Arc::new(Mutex::new(sink)),
    };
    // The handshake reads through the replay under the head timeout still set
    // on the socket; a client that sends headers and stalls cannot pin a slot.
    let Ok(accepted) = tungstenite::accept(duplex.clone()) else {
        return;
    };
    // The tunnel idles between pings by design: no more read deadlines.
    let _ = accepted
        .get_ref()
        .write
        .lock()
        .unwrap()
        .set_read_timeout(None);
    let stream = accepted.into_inner();
    let writer = Arc::new(Mutex::new(WebSocket::from_raw_socket(
        stream.clone(),
        tungstenite::protocol::Role::Server,
        None,
    )));
    let reader = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
    drive_tunnel(reader, writer, hub, store);
}

fn handle_request(
    request: http::Request,
    hub: Arc<Hub>,
    store: Arc<Store>,
    auth: Arc<crate::auth::Auth>,
) {
    let method = request.method().to_owned();
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("");

    if path == "/ws" {
        // Reached only by non-upgrade requests; tunnels arrive via the demux.
        let response = Response::from_string("websockets connect with an upgrade")
            .with_status_code(426)
            .with_header(Header::from_bytes("Content-Type", "text/plain").unwrap());
        let _ = request.respond(response);
        return;
    }

    let outcome = route(path, request, &hub, &store, &auth);
    match outcome {
        RouteOutcome::Buffered(buffered) => {
            let response = Response::from_data(buffered.body)
                .with_status_code(buffered.status)
                .with_header(
                    Header::from_bytes("Content-Type", buffered.content_type.as_bytes()).unwrap(),
                );
            let _ = buffered.request.respond(response);
            tracing::debug!(method, path, status = buffered.status, "request");
        }
        RouteOutcome::Streamed => {}
    }
}

fn route(
    path: &str,
    request: http::Request,
    hub: &Hub,
    store: &Arc<Store>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if path == "/install.sh" {
        return buffered(
            (
                200,
                "text/x-shellscript".to_string(),
                INSTALL_SH.as_bytes().to_vec(),
            ),
            request,
        );
    }
    if path == "/install.ps1" {
        return buffered(
            (
                200,
                "text/x-powershell".to_string(),
                INSTALL_PS1.as_bytes().to_vec(),
            ),
            request,
        );
    }
    if path == "/api/sso" {
        let body = if let Some(oidc) = &auth.oidc {
            serde_json::json!({
                "enabled": true,
                "issuer": oidc.issuer,
                "origin": oidc.origin,
                "client_id": oidc.client_id,
            })
            .to_string()
            .into_bytes()
        } else {
            serde_json::json!({"enabled": false})
                .to_string()
                .into_bytes()
        };
        return buffered((200, "application/json".to_string(), body), request);
    }
    if path == "/qr" {
        let query = request.url().split_once('?').map(|(_, q)| q).unwrap_or("");
        let text = query_param(query, "text").unwrap_or_default();
        let outcome = if text.len() > 512
            || !text
                .split('/')
                .any(|seg| seg.len() == 32 && seg.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            (
                400,
                "text/plain".to_string(),
                b"qr text must contain a share token".to_vec(),
            )
        } else {
            match fast_qr::QRBuilder::new(text).build() {
                Ok(code) => (
                    200,
                    "image/svg+xml".to_string(),
                    fast_qr::convert::svg::SvgBuilder::default()
                        .to_str(&code)
                        .into_bytes(),
                ),
                Err(_) => (
                    400,
                    "text/plain".to_string(),
                    b"text out of qr capacity".to_vec(),
                ),
            }
        };
        return buffered(outcome, request);
    }
    if let Some((token, tail)) = split_token_path(path) {
        let user = auth.user_from_cookie(cookie_header(&request));
        return proxy_remote(token, tail, request, hub, store, user.as_ref());
    }
    // Auth endpoints work without a session; everything user-facing below
    // needs one when OIDC is on.
    let local_login = auth.local_login_allowed();
    if path == "/login" {
        if request.method() == "POST" && local_login {
            return handle_local_login(request, auth);
        }
        // GET /login: if OIDC+local enabled, show chooser; if OIDC only, redirect; if local only, show form
        if auth.enabled() && local_login {
            return render_login_page(request, auth);
        }
        if auth.enabled() {
            return start_login(request, auth);
        }
        if local_login {
            return render_login_page(request, auth);
        }
        return buffered(
            (
                404,
                "text/plain".to_string(),
                b"no login configured".to_vec(),
            ),
            request,
        );
    }
    if path == "/api/login" && local_login {
        return handle_api_local_login(request, auth);
    }
    if path == "/callback" {
        return finish_login(request, auth);
    }
    if path == "/logout" {
        let cookie_header = cookie_header(&request);
        auth.logout(cookie_header);
        let response = Response::empty(302)
            .with_header(Header::from_bytes("Location", "/").unwrap())
            .with_header(
                Header::from_bytes("Set-Cookie", crate::auth::Auth::clear_cookie()).unwrap(),
            );
        let _ = request.respond(response);
        return RouteOutcome::Streamed;
    }
    let user = auth.user_from_cookie(cookie_header(&request));
    // An anchor with no users is not a dashboard yet, it is a setup page:
    // nothing but /setup renders until the first admin exists.
    if !auth.has_users() {
        return match path {
            "/setup" => handle_setup(request, auth),
            _ => redirect(request, "/setup"),
        };
    }
    // From here every management surface requires a session; share links
    // are capability-gated by their token and answered far above.
    if user.is_none() {
        return redirect_to_login(request);
    }
    if path == "/setup" {
        return redirect(request, "/");
    }
    route_authorized(path, request, hub, store, user, auth)
}

/// 302 that also logs the browser in.
fn redirect_with_cookie(request: http::Request, to: &str, cookie: &str) -> RouteOutcome {
    let response = Response::empty(302)
        .with_header(Header::from_bytes("Location", to.as_bytes()).unwrap())
        .with_header(
            Header::from_bytes(
                "Set-Cookie",
                crate::auth::Auth::session_set_cookie(cookie).as_bytes(),
            )
            .unwrap(),
        );
    let _ = request.respond(response);
    RouteOutcome::Streamed
}

/// 302 to a path on this anchor.
fn redirect(request: http::Request, to: &str) -> RouteOutcome {
    let response =
        Response::empty(302).with_header(Header::from_bytes("Location", to.as_bytes()).unwrap());
    let _ = request.respond(response);
    RouteOutcome::Streamed
}

/// First-run admin creation: a form while the store is empty, an account and
/// live cookie on submit, and a bounce as soon as anyone exists (race
/// attempts land on the login page through the wall above anyway).
fn handle_setup(mut request: http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    const MIN_PASSWORD: usize = 8;
    if request.method() != "POST" {
        let body = format!(
            r#"<h2>Setup</h2>
             <p>No users exist yet. Create the first administrator; this page closes behind it.</p>
             <form method="post" action="/setup">
             <p><label>Username<br><input name="username" autocomplete="username" required></label></p>
             <p><label>Password (at least {MIN_PASSWORD} chars)<br><input name="password" type="password" autocomplete="new-password" required minlength="{MIN_PASSWORD}"></label></p>
             <button class="primary" type="submit">Create admin</button>
             </form>"#
        );
        return buffered(
            crate::dashboard::standalone_page(200, "maki anchor — setup", &body),
            request,
        );
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (413, "text/plain".to_string(), b"body too large".to_vec()),
            request,
        );
    }
    let params = parse_form(&String::from_utf8_lossy(&body));
    let username = params.get("username").map(|s| s.as_str()).unwrap_or("");
    let password = params.get("password").map(|s| s.as_str()).unwrap_or("");
    if username.trim().is_empty() || username.len() > 64 || password.len() < MIN_PASSWORD {
        return setup_failure(
            request,
            "Username and a password of at least 8 characters are required.",
        );
    }
    let origin = remote_origin(&request);
    match auth.setup_admin(&origin, username.trim(), password) {
        Ok(cookie) => {
            tracing::info!(username, "first admin created");
            redirect_with_cookie(request, "/", &cookie)
        }
        Err(crate::auth::AuthError::AlreadySetup) => redirect(request, "/login"),
        Err(err) => setup_failure(request, &format!("Setup failed: {err}")),
    }
}

fn setup_failure(request: http::Request, message: &str) -> RouteOutcome {
    let content = format!(
        "<h2>Setup</h2><p>{}</p><p><a href=\"/setup\">Try again</a></p>",
        message.replace('<', "&lt;")
    );
    buffered(
        crate::dashboard::standalone_page(400, "maki anchor — setup", &content),
        request,
    )
}
fn cookie_header(request: &http::Request) -> Option<&str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.eq_ignore_ascii_case("Cookie"))
        .map(|h| h.value.as_str())
}

/// Best-effort client identity for login rate limiting. IP only (ports churn
/// per connection), and behind a reverse proxy every request shares one
/// address, so the limiter keys per proxy; local brute-forcing still trips it.
fn remote_origin(request: &http::Request) -> String {
    request
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn start_login(request: http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    match auth.begin_login() {
        Ok(url) => {
            let response = Response::empty(302)
                .with_header(Header::from_bytes("Location", url.as_bytes()).unwrap());
            let _ = request.respond(response);
            RouteOutcome::Streamed
        }
        Err(err) => buffered(
            (
                502,
                "text/plain".to_string(),
                format!("login: {err}").into_bytes(),
            ),
            request,
        ),
    }
}

fn render_login_page(request: http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    let has_oidc = auth.enabled();
    let allow_local = auth.local_login_allowed();
    // If ?oidc=1 is present, do OIDC redirect directly
    if has_oidc && request.url().contains("oidc=1") {
        return start_login(request, auth);
    }
    let mut body = String::from("<h2>Log in</h2>");
    if has_oidc {
        body.push_str("<p><a class=\"btn\" href=\"/login?oidc=1\">Log in with SSO</a></p>");
    }
    if allow_local {
        body.push_str(
            "<form method=\"post\" action=\"/login\">\
             <p><label>Username<br><input name=\"username\" autocomplete=\"username\" required></label></p>\
             <p><label>Password<br><input type=\"password\" name=\"password\" autocomplete=\"current-password\" required></label></p>\
             <button class=\"primary\" type=\"submit\">Log in</button>\
             </form>",
        );
    }
    if !has_oidc && !allow_local {
        body.push_str("<p>No login is configured. OIDC and local auth are both off.</p>");
    }
    let page = crate::dashboard::standalone_page(200, "maki anchor — login", &body);
    buffered(page, request)
}

fn handle_local_login(mut request: http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (413, "text/plain".to_string(), b"body too large".to_vec()),
            request,
        );
    }
    let body_str = String::from_utf8_lossy(&body);
    let params = parse_form(&body_str);
    let username = params.get("username").map(|s| s.as_str()).unwrap_or("");
    let password = params.get("password").map(|s| s.as_str()).unwrap_or("");
    let origin = remote_origin(&request);
    match auth.login_local(&origin, username, password) {
        Ok(cookie) => redirect_with_cookie(request, "/", &cookie),
        Err(crate::auth::AuthError::RateLimited) => buffered(
            (
                429,
                "text/html".to_string(),
                crate::dashboard::standalone_page(
                    429,
                    "maki anchor — login",
                    "<h2>Slow down</h2><p>Too many failed logins. Wait a while, then <a href=\"/login\">try again</a>.</p>",
                )
                .2
                    .to_vec(),
            ),
            request,
        ),
        Err(_) => buffered(
            (
                401,
                "text/html".to_string(),
                crate::dashboard::standalone_page(
                    401,
                    "maki anchor — login",
                    "<h2>Wrong username or password</h2><p><a href=\"/login\">Back to the login form</a></p>",
                )
                .2
                    .to_vec(),
            ),
            request,
        ),
    }
}

fn handle_api_local_login(mut request: http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Try form fallback
            let s = String::from_utf8_lossy(&body);
            let p = parse_form(&s);
            if let (Some(u), Some(p)) = (p.get("username"), p.get("password")) {
                serde_json::json!({"username": u, "password": p})
            } else {
                return buffered(
                    (
                        400,
                        "application/json".to_string(),
                        br#"{"error":"invalid json"}"#.to_vec(),
                    ),
                    request,
                );
            }
        }
    };
    let username = value.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let password = value.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let origin = remote_origin(&request);
    match auth.login_local(&origin, username, password) {
        Ok(cookie) => {
            // For API, return JSON and also set cookie if request is browser fetch with credentials
            let body = br#"{"ok":true}"#.to_vec();
            let response = Response::from_data(body)
                .with_status_code(200)
                .with_header(Header::from_bytes("Content-Type", b"application/json").unwrap())
                .with_header(
                    Header::from_bytes(
                        "Set-Cookie",
                        crate::auth::Auth::session_set_cookie(&cookie).as_bytes(),
                    )
                    .unwrap(),
                );
            let _ = request.respond(response);
            RouteOutcome::Streamed
        }
        Err(crate::auth::AuthError::RateLimited) => buffered(
            (
                429,
                "application/json".to_string(),
                br#"{"error":"too many failed logins, wait a while"}"#.to_vec(),
            ),
            request,
        ),
        Err(_) => buffered(
            (
                401,
                "application/json".to_string(),
                br#"{"error":"invalid credentials"}"#.to_vec(),
            ),
            request,
        ),
    }
}

fn parse_form(body: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let k = url_decode(k);
            let v = url_decode(v);
            map.insert(k, v);
        }
    }
    map
}

/// Percent-decode plus form `+`-as-space. Decodes to bytes then UTF-8, so
/// multibyte sequences survive instead of becoming one-char-per-byte mojibake.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn finish_login(request: http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    let query = request.url().split('?').nth(1).unwrap_or("");
    let params = |key: &str| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| url_decode(v))
        })
    };
    let Some(code) = params("code") else {
        return buffered(
            (400, "text/plain".to_string(), b"missing code".to_vec()),
            request,
        );
    };
    let Some(state) = params("state") else {
        return buffered(
            (400, "text/plain".to_string(), b"missing state".to_vec()),
            request,
        );
    };
    match auth.finish_login(&code, &state) {
        Ok(cookie_value) => {
            let response = Response::empty(302)
                .with_header(Header::from_bytes("Location", "/".as_bytes()).unwrap())
                .with_header(
                    Header::from_bytes(
                        "Set-Cookie",
                        crate::auth::Auth::session_set_cookie(&cookie_value).as_bytes(),
                    )
                    .unwrap(),
                );
            let _ = request.respond(response);
            RouteOutcome::Streamed
        }
        Err(err) => buffered(
            (
                401,
                "text/plain".to_string(),
                format!("login failed: {err}").into_bytes(),
            ),
            request,
        ),
    }
}

/// GET /links?instance=&session=&rights=&hours=: mint and display a link.
fn render_link(
    request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    let query = request.url().split('?').nth(1).unwrap_or("");
    let params = |key: &str| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| url_decode(v))
        })
    };
    let Some(instance) = params("instance") else {
        return buffered(
            (400, "text/plain".to_string(), b"missing instance".to_vec()),
            request,
        );
    };
    let hours = params("hours")
        .and_then(|h| h.parse::<u64>().ok())
        .unwrap_or(2);
    let outcome = crate::dashboard::render_link(
        store,
        user,
        &instance,
        params("session").as_deref(),
        params("rights").as_deref().unwrap_or("view"),
        hours,
    );
    buffered(outcome, request)
}

fn redirect_to_login(request: http::Request) -> RouteOutcome {
    let response = Response::empty(302)
        .with_header(Header::from_bytes("Location", "/login".as_bytes()).unwrap());
    let _ = request.respond(response);
    RouteOutcome::Streamed
}

/// A logged-in non-admin. Anonymous (LAN-trust / CLI) callers are not
/// covered; the dashboard admin tables are simply open to everyone there.
fn is_non_admin(user: Option<&crate::store::UserRow>) -> bool {
    user.is_some_and(|u| !u.is_admin)
}

fn forbidden_json(request: http::Request) -> RouteOutcome {
    buffered(
        (
            403,
            "application/json".to_string(),
            br#"{"error":"admin required"}"#.to_vec(),
        ),
        request,
    )
}

fn handle_api_create_instance(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if request.method() != "POST" {
        return buffered(
            (
                405,
                "application/json".to_string(),
                br#"{"error":"method not allowed"}"#.to_vec(),
            ),
            request,
        );
    }
    if !auth.can_mint_tokens(user) {
        let msg = match auth.effective_mint_tokens() {
            store::MintTokens::Admin => "admin required to mint tokens",
            store::MintTokens::User => "authentication required",
            store::MintTokens::Any => "authentication required",
        };
        let status = if auth.effective_mint_tokens() == store::MintTokens::Admin {
            403
        } else {
            401
        };
        return buffered(
            (
                status,
                "application/json".to_string(),
                serde_json::json!({"error": msg}).to_string().into_bytes(),
            ),
            request,
        );
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    if name.is_empty() {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing name"}"#.to_vec(),
            ),
            request,
        );
    }
    if !store::valid_instance_name(&name) {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"name must be 1-64 chars: alphanumeric, -, _, ."}"#.to_vec(),
            ),
            request,
        );
    }
    let token = new_link_token();
    let hash = store::hash_token(&token);
    match store.create_instance(&name, &hash) {
        Ok(_) => {}
        Err(store::StoreError::Exists) => {
            return buffered(
                (
                    409,
                    "application/json".to_string(),
                    br#"{"error":"instance exists; rotate its token with `maki-anchor tokens add`"}"#.to_vec(),
                ),
                request,
            );
        }
        Err(e) => {
            return buffered(
                (
                    500,
                    "application/json".to_string(),
                    serde_json::json!({"error": format!("store error: {e}")})
                        .to_string()
                        .into_bytes(),
                ),
                request,
            );
        }
    }
    let body = serde_json::json!({"name": name, "token": token})
        .to_string()
        .into_bytes();
    buffered((200, "application/json".to_string(), body), request)
}

fn json_list_users(store: &Arc<Store>) -> (u16, String, Vec<u8>) {
    match store.list_users() {
        Ok(rows) => {
            let body = serde_json::to_vec(&rows).unwrap_or_default();
            (200, "application/json".to_string(), body)
        }
        Err(err) => (
            500,
            "application/json".to_string(),
            serde_json::json!({"error": format!("store error: {err}")})
                .to_string()
                .into_bytes(),
        ),
    }
}

fn json_list_grants(store: &Arc<Store>) -> (u16, String, Vec<u8>) {
    match store.list_grants() {
        Ok(rows) => {
            let body = serde_json::to_vec(&rows).unwrap_or_default();
            (200, "application/json".to_string(), body)
        }
        Err(err) => (
            500,
            "application/json".to_string(),
            serde_json::json!({"error": format!("store error: {err}")})
                .to_string()
                .into_bytes(),
        ),
    }
}

fn handle_api_set_grant(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return buffered(
            (
                405,
                "application/json".to_string(),
                br#"{"error":"method not allowed"}"#.to_vec(),
            ),
            request,
        );
    }
    if let Some(u) = user
        && !u.is_admin
    {
        return buffered(
            (
                403,
                "application/json".to_string(),
                br#"{"error":"admin required"}"#.to_vec(),
            ),
            request,
        );
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let user_id = value.get("user_id").and_then(|v| v.as_i64());
    let instance = value
        .get("instance")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let rights = value
        .get("rights")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let Some(user_id) = user_id else {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing user_id"}"#.to_vec(),
            ),
            request,
        );
    };
    if instance.is_empty() {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing instance"}"#.to_vec(),
            ),
            request,
        );
    }
    let Some(role) = Role::parse(&rights) else {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"rights must be view or control"}"#.to_vec(),
            ),
            request,
        );
    };
    let instance_row = match store.instance_by_name(&instance) {
        Ok(r) => r,
        Err(_) => {
            return buffered(
                (
                    404,
                    "application/json".to_string(),
                    br#"{"error":"unknown instance"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    if store.user_by_id(user_id).is_err() {
        return buffered(
            (
                404,
                "application/json".to_string(),
                br#"{"error":"unknown user"}"#.to_vec(),
            ),
            request,
        );
    }
    if let Err(e) = store.set_grant(user_id, instance_row.id, role) {
        return buffered(
            (
                500,
                "application/json".to_string(),
                serde_json::json!({"error": format!("store error: {e}")})
                    .to_string()
                    .into_bytes(),
            ),
            request,
        );
    }
    buffered(
        (
            200,
            "application/json".to_string(),
            br#"{"ok":true}"#.to_vec(),
        ),
        request,
    )
}

/// Everything the control center and the management pages need to know about
/// one share link: its instance, the effective rights of the caller, and
/// whether the caller may manage it.
struct CenterView {
    instance: store::InstanceRow,
    rights: String,
    can_manage: bool,
}

fn center_view(
    store: &Store,
    link_token: &str,
    user: Option<&crate::store::UserRow>,
) -> Option<CenterView> {
    let link = store.link_by_token(link_token).ok()?;
    let instance = store.instance_by_id(link.instance_id).ok()??;
    let grant = user.and_then(|u| store.grant_for(u.id, link.instance_id).ok().flatten());
    // Grants raise the floor, never lower it, exactly like proxy_remote.
    let rights =
        if link.rights == Role::Viewer.as_str() && grant.is_some_and(|r| r == Role::Controller) {
            Role::Controller.as_str().to_owned()
        } else {
            link.rights.clone()
        };
    let can_manage =
        user.is_some_and(|u| u.is_admin || grant.is_some_and(|r| r == Role::Controller));
    Some(CenterView {
        instance,
        rights,
        can_manage,
    })
}

fn center_json(status: u16, body: serde_json::Value) -> (u16, String, Vec<u8>) {
    (
        status,
        "application/json".to_string(),
        body.to_string().into_bytes(),
    )
}

fn json_error(request: http::Request, status: u16, reason: &str) -> RouteOutcome {
    buffered(
        center_json(status, serde_json::json!({"error": reason})),
        request,
    )
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| url_decode(v))
    })
}

/// GET /api/center?link=<token>: the remote page's control-center feed.
/// Anonymous callers learn nothing (the page then falls back to instance
/// data); a logged-in user learns their rights, and managers also get the
/// instance's live links and grants.
fn handle_api_center(
    request: http::Request,
    store: &Arc<Store>,
    hub: &Hub,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    let query = request.url().split_once('?').map(|(_, q)| q).unwrap_or("");
    let Some(link_token) = query_param(query, "link") else {
        return json_error(request, 400, "link required");
    };
    let Some(view) = center_view(store, &link_token, user) else {
        return json_error(request, 404, "invalid or expired link");
    };
    let me = user.map(|u| {
        serde_json::json!({
            "id": u.id,
            "name": u.name.clone().or_else(|| u.email.clone()).unwrap_or_else(|| format!("user {}", u.id)),
            "admin": u.is_admin,
        })
    });
    let online = hub.is_online(view.instance.id);
    let mut body = serde_json::json!({
        "instance": {"id": view.instance.id, "name": view.instance.name, "last_seen": view.instance.last_seen},
        "online": online,
        "rights": view.rights,
        "can_manage": view.can_manage,
        "me": me,
    });
    if view.can_manage {
        let links: Vec<serde_json::Value> = store
            .list_links()
            .unwrap_or_default()
            .into_iter()
            .filter(|l| l.instance_id == view.instance.id)
            .map(|l| {
                serde_json::json!({
                    "token": l.token,
                    "hash": l.token_hash,
                    "rights": l.rights,
                    "session": l.external_session_id,
                    "expires": l.expires_at,
                })
            })
            .collect();
        let grants: Vec<serde_json::Value> = store
            .list_grants()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, instance, _)| *instance == view.instance.name)
            .map(|(user_id, _, rights)| serde_json::json!({"user_id": user_id, "rights": rights}))
            .collect();
        body["links"] = serde_json::Value::Array(links);
        body["grants"] = serde_json::Value::Array(grants);
    }
    buffered(center_json(200, body), request)
}

fn read_json(request: &mut http::Request) -> Result<serde_json::Value, String> {
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return Err("body too large".to_owned());
    }
    serde_json::from_slice(&body).map_err(|_| "invalid json".to_owned())
}

/// POST /api/links/mint {link, rights, hours, session?}: managers create
/// invite links for their instance.
fn handle_api_center_invite(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return json_error(request, 405, "method not allowed");
    }
    let value = match read_json(&mut request) {
        Ok(v) => v,
        Err(e) => return json_error(request, 400, &e),
    };
    let Some(link) = value.get("link").and_then(|v| v.as_str()) else {
        return json_error(request, 400, "link required");
    };
    let Some(view) = center_view(store, link, user) else {
        return json_error(request, 404, "invalid or expired link");
    };
    if !view.can_manage {
        return json_error(request, 403, "managers only: a controller grant is needed");
    }
    let rights = value
        .get("rights")
        .and_then(|v| v.as_str())
        .unwrap_or("view");
    let Some(role) = Role::parse(rights) else {
        return json_error(request, 400, "rights must be view or control");
    };
    let hours = value
        .get("hours")
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .clamp(1, crate::dashboard::MAX_LINK_HOURS);
    let session = value
        .get("session")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let token = match mint_link(
        store,
        view.instance.id,
        session.as_deref(),
        role,
        std::time::Duration::from_secs(hours * 3600),
    ) {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!(error = %err, "invite mint failed");
            return json_error(request, 500, "mint failed");
        }
    };
    let path = match &session {
        Some(s) => format!("/{token}/s/{s}/"),
        None => format!("/{token}/"),
    };
    buffered(
        center_json(
            200,
            serde_json::json!({"token": token, "path": path, "hours": hours}),
        ),
        request,
    )
}

/// POST /api/links/close {link}: managers kill this very share URL and drop
/// the tunnel behind it, so a leaked link stops serving immediately. The
/// instance notices and registers again under a fresh link.
fn handle_api_center_close(
    mut request: http::Request,
    store: &Arc<Store>,
    hub: &Hub,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return json_error(request, 405, "method not allowed");
    }
    let value = match read_json(&mut request) {
        Ok(v) => v,
        Err(e) => return json_error(request, 400, &e),
    };
    let Some(link) = value.get("link").and_then(|v| v.as_str()) else {
        return json_error(request, 400, "link required");
    };
    let Some(view) = center_view(store, link, user) else {
        return json_error(request, 404, "invalid or expired link");
    };
    if !view.can_manage {
        return json_error(request, 403, "managers only: a controller grant is needed");
    }
    store.revoke_link(&store::hash_token(link)).ok();
    hub.disconnect(view.instance.id);
    tracing::info!(
        instance = view.instance.name,
        "share link closed from the control center"
    );
    buffered(
        center_json(200, serde_json::json!({"closed": true})),
        request,
    )
}

/// Revokes a live link by hash. Admins may revoke anything (the dashboard
/// sends just the hash); controllers may revoke links belonging to instances
/// they manage, and must name the link that gives them that right.
fn handle_api_revoke_link(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return json_error(request, 405, "method not allowed");
    }
    if user.is_none() {
        return json_error(request, 401, "login required");
    }
    let value = match read_json(&mut request) {
        Ok(v) => v,
        Err(e) => return json_error(request, 400, &e),
    };
    let Some(hash) = value.get("token_hash").and_then(|v| v.as_str()) else {
        return json_error(request, 400, "token_hash required");
    };
    let is_admin = user.is_some_and(|u| u.is_admin);
    if !is_admin {
        let Some(link) = value.get("link").and_then(|v| v.as_str()) else {
            return json_error(request, 403, "admin required");
        };
        let Some(view) = center_view(store, link, user) else {
            return json_error(request, 404, "invalid or expired link");
        };
        if !view.can_manage {
            return json_error(request, 403, "managers only");
        }
        let owns = store
            .list_links()
            .unwrap_or_default()
            .into_iter()
            .any(|l| l.token_hash == hash && l.instance_id == view.instance.id);
        if !owns {
            return json_error(request, 403, "that link is not on your instance");
        }
    }
    match store.revoke_link(hash) {
        Ok(true) => buffered(
            center_json(200, serde_json::json!({"revoked": true})),
            request,
        ),
        Ok(false) => json_error(request, 404, "link not live"),
        Err(err) => json_error(request, 500, &err.to_string()),
    }
}

fn handle_api_revoke_grant(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return buffered(
            (
                405,
                "application/json".to_string(),
                br#"{"error":"method not allowed"}"#.to_vec(),
            ),
            request,
        );
    }
    if let Some(u) = user
        && !u.is_admin
    {
        return buffered(
            (
                403,
                "application/json".to_string(),
                br#"{"error":"admin required"}"#.to_vec(),
            ),
            request,
        );
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let user_id = value.get("user_id").and_then(|v| v.as_i64());
    let instance = value
        .get("instance")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let Some(user_id) = user_id else {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing user_id"}"#.to_vec(),
            ),
            request,
        );
    };
    if instance.is_empty() {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing instance"}"#.to_vec(),
            ),
            request,
        );
    }
    let instance_row = match store.instance_by_name(&instance) {
        Ok(r) => r,
        Err(_) => {
            return buffered(
                (
                    404,
                    "application/json".to_string(),
                    br#"{"error":"unknown instance"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    match store.delete_grant(user_id, instance_row.id) {
        Ok(true) => buffered(
            (
                200,
                "application/json".to_string(),
                br#"{"ok":true}"#.to_vec(),
            ),
            request,
        ),
        Ok(false) => buffered(
            (
                404,
                "application/json".to_string(),
                br#"{"error":"grant not found"}"#.to_vec(),
            ),
            request,
        ),
        Err(e) => buffered(
            (
                500,
                "application/json".to_string(),
                serde_json::json!({"error": format!("store error: {e}")})
                    .to_string()
                    .into_bytes(),
            ),
            request,
        ),
    }
}

fn handle_api_create_user(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if !auth.allow_local {
        return buffered(
            (
                403,
                "application/json".to_string(),
                br#"{"error":"local users disabled"}"#.to_vec(),
            ),
            request,
        );
    }
    // User creation is admin-only. The one exception is bootstrapping an
    // empty deployment: with no users at all, the first call creates the
    // first admin. Mint policy governs share links, not account creation,
    // so `any` can never be used to add admins to an established anchor.
    let is_admin_caller = user.as_ref().is_some_and(|u| u.is_admin);
    if !is_admin_caller {
        let has_users = store.list_users().map(|u| !u.is_empty()).unwrap_or(true);
        if has_users {
            let status = if user.is_some() { 403 } else { 401 };
            return buffered(
                (
                    status,
                    "application/json".to_string(),
                    br#"{"error":"admin required to create users"}"#.to_vec(),
                ),
                request,
            );
        }
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let username = value
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let password = value
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let email = value
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let is_admin = value
        .get("is_admin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if username.is_empty() || password.is_empty() {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing username or password"}"#.to_vec(),
            ),
            request,
        );
    }
    if username.len() > 32
        || !username
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"username must be 1-32 alphanumeric/_-."}"#.to_vec(),
            ),
            request,
        );
    }
    if password.len() < 8 {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"password must be at least 8 chars"}"#.to_vec(),
            ),
            request,
        );
    }
    match store.create_local_user(
        &username,
        &password,
        email.as_deref(),
        name.as_deref(),
        is_admin,
    ) {
        Ok(u) => {
            let body =
                serde_json::json!({"id": u.id, "username": username, "is_admin": u.is_admin})
                    .to_string()
                    .into_bytes();
            buffered((200, "application/json".to_string(), body), request)
        }
        Err(e) => buffered(
            (
                500,
                "application/json".to_string(),
                serde_json::json!({"error": format!("store error: {e}")})
                    .to_string()
                    .into_bytes(),
            ),
            request,
        ),
    }
}

fn handle_api_set_admin(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return buffered(
            (
                405,
                "application/json".to_string(),
                br#"{"error":"method not allowed"}"#.to_vec(),
            ),
            request,
        );
    }
    if let Some(u) = user
        && !u.is_admin
    {
        return buffered(
            (
                403,
                "application/json".to_string(),
                br#"{"error":"admin required"}"#.to_vec(),
            ),
            request,
        );
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let username = value
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let is_admin = value
        .get("is_admin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if username.is_empty() {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing username"}"#.to_vec(),
            ),
            request,
        );
    }
    let user_row = match store
        .local_user_by_username(&username)
        .or_else(|_| store.user_by_sub(&format!("local:{username}")))
    {
        Ok(u) => u,
        Err(_) => {
            return buffered(
                (
                    404,
                    "application/json".to_string(),
                    br#"{"error":"unknown user"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    if let Err(e) = store.set_user_admin(user_row.id, is_admin) {
        return buffered(
            (
                500,
                "application/json".to_string(),
                serde_json::json!({"error": format!("store error: {e}")})
                    .to_string()
                    .into_bytes(),
            ),
            request,
        );
    }
    let body = serde_json::json!({"ok": true, "is_admin": is_admin})
        .to_string()
        .into_bytes();
    buffered((200, "application/json".to_string(), body), request)
}

fn handle_api_delete_user(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method() != "POST" {
        return buffered(
            (
                405,
                "application/json".to_string(),
                br#"{"error":"method not allowed"}"#.to_vec(),
            ),
            request,
        );
    }
    if let Some(u) = user
        && !u.is_admin
    {
        return buffered(
            (
                403,
                "application/json".to_string(),
                br#"{"error":"admin required"}"#.to_vec(),
            ),
            request,
        );
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let username = value
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    if username.is_empty() {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"missing username"}"#.to_vec(),
            ),
            request,
        );
    }
    let user_row = match store
        .local_user_by_username(&username)
        .or_else(|_| store.user_by_sub(&format!("local:{username}")))
    {
        Ok(u) => u,
        Err(_) => {
            return buffered(
                (
                    404,
                    "application/json".to_string(),
                    br#"{"error":"unknown user"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    // Prevent deleting self if admin
    if let Some(cur) = user
        && cur.id == user_row.id
    {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"cannot delete self"}"#.to_vec(),
            ),
            request,
        );
    }
    match store.delete_user(user_row.id) {
        Ok(true) => buffered(
            (
                200,
                "application/json".to_string(),
                br#"{"ok":true}"#.to_vec(),
            ),
            request,
        ),
        Ok(false) => buffered(
            (
                404,
                "application/json".to_string(),
                br#"{"error":"not found"}"#.to_vec(),
            ),
            request,
        ),
        Err(e) => buffered(
            (
                500,
                "application/json".to_string(),
                serde_json::json!({"error": format!("store error: {e}")})
                    .to_string()
                    .into_bytes(),
            ),
            request,
        ),
    }
}

fn handle_api_set_mint_tokens(
    mut request: http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if request.method() != "POST" {
        return buffered(
            (
                405,
                "application/json".to_string(),
                br#"{"error":"method not allowed"}"#.to_vec(),
            ),
            request,
        );
    }
    let is_admin = user.as_ref().is_some_and(|u| u.is_admin);
    if !is_admin {
        let has_users = store.list_users().map(|u| !u.is_empty()).unwrap_or(false);
        if has_users {
            return buffered(
                (
                    403,
                    "application/json".to_string(),
                    br#"{"error":"admin required"}"#.to_vec(),
                ),
                request,
            );
        }
    }
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (
                413,
                "application/json".to_string(),
                br#"{"error":"body too large"}"#.to_vec(),
            ),
            request,
        );
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return buffered(
                (
                    400,
                    "application/json".to_string(),
                    br#"{"error":"invalid json"}"#.to_vec(),
                ),
                request,
            );
        }
    };
    let mint = value
        .get("mint_tokens")
        .or_else(|| value.get("mint"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let parsed = crate::store::MintTokens::parse(mint);
    let Some(parsed) = parsed else {
        return buffered(
            (
                400,
                "application/json".to_string(),
                br#"{"error":"mint_tokens must be any, user, or admin"}"#.to_vec(),
            ),
            request,
        );
    };
    if let Err(e) = store.set_setting("mint_tokens", parsed.as_str()) {
        return buffered(
            (
                500,
                "application/json".to_string(),
                serde_json::json!({"error": format!("store error: {e}")})
                    .to_string()
                    .into_bytes(),
            ),
            request,
        );
    }
    // Also update in-memory auth for this process (next request will read from DB anyway)
    let _ = auth;
    let body = serde_json::json!({"mint_tokens": parsed.as_str()})
        .to_string()
        .into_bytes();
    buffered((200, "application/json".to_string(), body), request)
}

/// User-facing routes: dashboard and JSON endpoints. The user row (None in
/// LAN-trust mode) is available for future per-instance filtering.
fn route_authorized(
    path: &str,
    request: http::Request,
    hub: &Hub,
    store: &Arc<Store>,
    user: Option<crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if path == "/" {
        return buffered(
            crate::dashboard::render_sessions(store, hub, user.as_ref()),
            request,
        );
    }
    if path == "/instances" {
        return buffered(
            crate::dashboard::render_instances(store, user.as_ref()),
            request,
        );
    }
    if path == "/links" {
        if request.url().contains("instance=") {
            return render_link(request, store, user.as_ref());
        }
        return buffered(
            crate::dashboard::render_links(store, hub, user.as_ref()),
            request,
        );
    }
    if path == "/api/links/revoke" {
        return handle_api_revoke_link(request, store, user.as_ref());
    }
    if path == "/api/center" {
        return handle_api_center(request, store, hub, user.as_ref());
    }
    if path == "/api/links/mint" {
        return handle_api_center_invite(request, store, user.as_ref());
    }
    if path == "/api/links/close" {
        return handle_api_center_close(request, store, hub, user.as_ref());
    }
    if path == "/api/instances" {
        return match request.method() {
            "GET" => buffered(json_list_instances_for(store, user.as_ref()), request),
            "POST" => handle_api_create_instance(request, store, user.as_ref(), auth),
            _ => buffered(
                (
                    405,
                    "application/json".to_string(),
                    br#"{"error":"method not allowed"}"#.to_vec(),
                ),
                request,
            ),
        };
    }
    if path == "/api/sessions" {
        return buffered(json_list_sessions_for(store, user.as_ref()), request);
    }
    if path == "/api/users" {
        return match request.method() {
            "GET" => {
                if is_non_admin(user.as_ref()) {
                    return forbidden_json(request);
                }
                buffered(json_list_users(store), request)
            }
            "POST" => handle_api_create_user(request, store, user.as_ref(), auth),
            _ => buffered(
                (
                    405,
                    "application/json".to_string(),
                    br#"{"error":"method not allowed"}"#.to_vec(),
                ),
                request,
            ),
        };
    }
    if path == "/api/users/set-admin" {
        return handle_api_set_admin(request, store, user.as_ref());
    }
    if path == "/api/users/delete" {
        return handle_api_delete_user(request, store, user.as_ref());
    }
    if path == "/api/grants" {
        return match request.method() {
            "GET" => {
                if is_non_admin(user.as_ref()) {
                    return forbidden_json(request);
                }
                buffered(json_list_grants(store), request)
            }
            "POST" => handle_api_set_grant(request, store, user.as_ref()),
            _ => buffered(
                (
                    405,
                    "text/plain".to_string(),
                    b"method not allowed".to_vec(),
                ),
                request,
            ),
        };
    }
    if path == "/api/grants/revoke" {
        return handle_api_revoke_grant(request, store, user.as_ref());
    }
    if path == "/admin" {
        match &user {
            Some(u) if u.is_admin => {
                return buffered(
                    crate::dashboard::render_admin(store, user.as_ref(), auth),
                    request,
                );
            }
            Some(_) => {
                return buffered(
                    (
                        403,
                        "text/html".to_string(),
                        crate::dashboard::standalone_page(
                            403,
                            "maki anchor — admin",
                            "<h2>Admins only</h2><p>This page is for anchor administrators. <a href=\"/\">Back to sessions</a>.</p>",
                        )
                        .2,
                    ),
                    request,
                );
            }
            None => return redirect_to_login(request),
        }
    }
    if path == "/api/config/mint_tokens" {
        return match request.method() {
            "GET" => {
                let v = auth.effective_mint_tokens().as_str().to_owned();
                let body = serde_json::json!({"mint_tokens": v})
                    .to_string()
                    .into_bytes();
                buffered((200, "application/json".to_string(), body), request)
            }
            "POST" => handle_api_set_mint_tokens(request, store, user.as_ref(), auth),
            _ => buffered(
                (
                    405,
                    "application/json".to_string(),
                    br#"{"error":"method not allowed"}"#.to_vec(),
                ),
                request,
            ),
        };
    }
    let outcome = if path == "/api/sessions" {
        json_list_sessions_for(store, user.as_ref())
    } else if let Some(rest) = path.strip_prefix("/api/sessions/") {
        match rest.parse::<i64>() {
            Ok(instance_id) => {
                if let Some(u) = &user
                    && !u.is_admin
                    && let Ok(visible) = store.instances_for_user(u.id, u.is_admin)
                    && !visible.iter().any(|i| i.id == instance_id)
                {
                    return buffered(
                        (
                            403,
                            "application/json".to_string(),
                            br#"{"error":"forbidden"}"#.to_vec(),
                        ),
                        request,
                    );
                }
                match store.sessions_for_instance(instance_id) {
                    Ok(rows) => {
                        let body = serde_json::to_vec(&rows).unwrap_or_default();
                        (200, "application/json".to_string(), body)
                    }
                    Err(err) => (
                        500,
                        "application/json".to_string(),
                        serde_json::json!({"error": format!("store error: {err}")})
                            .to_string()
                            .into_bytes(),
                    ),
                }
            }
            Err(_) => (
                400,
                "application/json".to_string(),
                br#"{"error":"bad instance id"}"#.to_vec(),
            ),
        }
    } else {
        (404, "text/plain".to_string(), b"not found".to_vec())
    };
    buffered(outcome, request)
}

fn json_list_instances_for(
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> (u16, String, Vec<u8>) {
    let res = match user {
        Some(u) => store.instances_for_user(u.id, u.is_admin),
        None => store.list_instances(),
    };
    match res {
        Ok(rows) => {
            let body = serde_json::to_vec(&rows).unwrap_or_default();
            (200, "application/json".to_string(), body)
        }
        Err(err) => (
            500,
            "application/json".to_string(),
            serde_json::json!({"error": format!("store error: {err}")})
                .to_string()
                .into_bytes(),
        ),
    }
}

fn json_list_sessions_for(
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> (u16, String, Vec<u8>) {
    let res = match user {
        Some(u) => store.sessions_for_user(u.id, u.is_admin),
        None => store.list_sessions(),
    };
    match res {
        Ok(rows) => {
            let body = serde_json::to_vec(&rows).unwrap_or_default();
            (200, "application/json".to_string(), body)
        }
        Err(err) => (
            500,
            "application/json".to_string(),
            serde_json::json!({"error": format!("store error: {err}")})
                .to_string()
                .into_bytes(),
        ),
    }
}

/// Persist a session-index push from an instance. The owning instance is the
/// authenticated tunnel, so a registered host can never rewrite another's
/// index by claiming a name in the frame.
fn handle_push(store: &Arc<Store>, hub: &Hub, instance_id: i64, push: TunnelPush) {
    let sessions = match push {
        TunnelPush::LinkRevoke { link_revoke } => {
            let hash = store::hash_token(&link_revoke);
            match store.revoke_link(&hash) {
                Ok(true) => {
                    tracing::info!(instance_id, "tunnel closed its own link");
                    // The link is gone; nothing left for the tunnel to serve.
                    hub.disconnect(instance_id);
                }
                Ok(false) => tracing::debug!(instance_id, "revoke for a link that was not live"),
                Err(err) => tracing::warn!(error = %err, instance_id, "link revoke failed"),
            }
            return;
        }
        TunnelPush::SessionIndex { sessions } => sessions,
    };
    for entry in sessions {
        let row = SessionRow {
            instance_id,
            external_id: entry.session_id,
            title: entry.title,
            model: entry.model,
            cwd: entry.cwd,
            status: entry.status,
            cost_cents: entry.cost_cents,
            tokens_in: entry.tokens_in,
            tokens_out: entry.tokens_out,
            context_window: entry.context_window,
            updated_at: crate::store::now_unix(),
        };
        if let Err(err) = store.upsert_session(&row) {
            tracing::warn!(error = %err, instance_id, "session upsert failed");
        }
    }
}

/// The route under an optional `s/<session>/` prefix.
fn tail_under_session(tail: &str) -> &str {
    tail.strip_prefix("s/")
        .map(|rest| rest.split_once('/').map(|(_, sub)| sub).unwrap_or(""))
        .unwrap_or(tail)
}

fn proxy_remote(
    token: &str,
    tail: &str,
    request: http::Request,
    hub: &Hub,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    let link = match store.link_by_token(token) {
        Ok(link) => link,
        Err(_) => {
            return buffered(
                (
                    404,
                    "text/plain".to_string(),
                    b"invalid or expired link".to_vec(),
                ),
                request,
            );
        }
    };
    if !hub.is_online(link.instance_id) {
        return buffered(
            (503, "text/plain".to_string(), b"instance offline".to_vec()),
            request,
        );
    }

    // A session-scoped link only opens that session: the bare index bounces
    // into the scoped path (so the UI's relative requests carry it), and
    // anything outside it is refused.
    if let Some(session_id) = link.external_session_id.as_deref() {
        if tail.is_empty() {
            let response = Response::empty(302).with_header(
                Header::from_bytes("Location", format!("/{token}/s/{session_id}/").as_bytes())
                    .unwrap(),
            );
            let _ = request.respond(response);
            return RouteOutcome::Streamed;
        }
        let scoped = request_path_session(tail);
        if scoped.as_deref() != Some(session_id) {
            return buffered(
                (
                    404,
                    "text/plain".to_string(),
                    b"link is scoped to another session".to_vec(),
                ),
                request,
            );
        }
    }

    let method = request.method().to_owned();
    let write = matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    // Grants raise the floor, never lower it: a controller grant upgrades a
    // view link for the logged-in user.
    let rights = if write
        && link.rights == Role::Viewer.as_str()
        && user.is_some_and(|u| {
            store
                .grant_for(u.id, link.instance_id)
                .ok()
                .flatten()
                .is_some_and(|r| r == Role::Controller)
        }) {
        Role::Controller.as_str()
    } else {
        link.rights.as_str()
    };
    if write && rights != Role::Controller.as_str() {
        return buffered(
            (403, "text/plain".to_string(), b"link is view-only".to_vec()),
            request,
        );
    }
    let mut body = Vec::new();
    let mut request = request;
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return buffered(
            (413, "text/plain".to_string(), b"body too large".to_vec()),
            request,
        );
    }

    // The anchor owns the link token: the instance sees the bare tail, so its
    // own token check (standalone mode) stays independent of anchor links.
    let forwarded = serde_json::json!({
        "method": method,
        "path": format!("/{tail}"),
        "headers": {},
        "body": STANDARD.encode(&body),
    })
    .to_string();

    let (_conn_id, rx) = match hub.request(link.instance_id, forwarded) {
        Ok(pair) => pair,
        Err(err) => {
            return buffered(
                (
                    502,
                    "text/plain".to_string(),
                    format!("tunnel: {err}").into_bytes(),
                ),
                request,
            );
        }
    };
    // Traffic on the tunnel's own control link slides its expiry, so it stays
    // alive as long as the tunnel does.
    if hub.link_hash(link.instance_id).as_deref() == Some(link.token_hash.as_str())
        && let Err(err) = store.extend_link(&link.token_hash, TUNNEL_LINK_TTL)
    {
        tracing::warn!(error = %err, "link extend failed");
    }
    let first = match hub.wait_first(&rx) {
        Ok(first) => first,
        Err(err) => {
            return buffered(
                (
                    502,
                    "text/plain".to_string(),
                    format!("tunnel: {err}").into_bytes(),
                ),
                request,
            );
        }
    };
    let status = first.status;
    // SSE streams chunk as they are produced; everything else buffers. The
    // first chunk is handed to the streamer so an already-final stream cannot
    // leave the browser waiting on the chunk timeout.
    if tail_under_session(tail) == "events" && status == 200 {
        return stream_to_browser(request, hub, rx, first);
    }
    let content_type = first
        .content_type
        .unwrap_or_else(|| "application/json".to_string());
    let mut acc = first.body;
    if !first.final_chunk {
        while let Ok(chunk) = hub.wait_chunk(&rx) {
            acc.extend_from_slice(&chunk.body);
            if chunk.final_chunk {
                break;
            }
        }
    }
    buffered((status, content_type, acc), request)
}

/// Streams tunnel SSE chunks to the browser with tiny_http bypassed: its
/// chunked encoder buffers, and SSE frames must flush as they arrive.
fn stream_to_browser(
    request: http::Request,
    hub: &Hub,
    rx: Receiver<hub::TunnelResponse>,
    first: hub::TunnelResponse,
) -> RouteOutcome {
    let status = first.status;
    let mut writer = request.into_writer();
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
    );
    if writer.write_all(head.as_bytes()).is_err() || writer.flush().is_err() {
        return RouteOutcome::Streamed;
    }
    let mut first = Some(first);
    loop {
        let chunk = match first.take() {
            Some(chunk) => chunk,
            None => match hub.wait_chunk(&rx) {
                Ok(chunk) => chunk,
                Err(_) => break,
            },
        };
        let final_chunk = chunk.final_chunk;
        if writer.write_all(&chunk.body).is_err() || writer.flush().is_err() || final_chunk {
            break;
        }
    }
    RouteOutcome::Streamed
}

fn drive_tunnel(
    mut reader: WebSocket<HalfDuplex>,
    writer: Arc<Mutex<WebSocket<HalfDuplex>>>,
    hub: Arc<Hub>,
    store: Arc<Store>,
) {
    let hello = match reader.read() {
        Ok(WsMessage::Text(text)) => text,
        _ => return,
    };
    let Ok(parsed) = serde_json::from_str::<HelloFrame>(&hello) else {
        return;
    };
    let instance = match store.instance_by_registration_token(&parsed.registration_token) {
        Ok(instance) if instance.name == parsed.instance_name => instance,
        _ => {
            let _ = writer.lock().unwrap().send(WsMessage::text("auth failed"));
            return;
        }
    };
    store.touch_instance(instance.id).ok();
    // A tunnel reuses the instance's still-live control link, so reconnects
    // and repeated /rc calls keep the URL people have already shared; only a
    // dead link (expired, revoked, or pre-plaintext legacy) gets a fresh mint.
    // Traffic on it slides the expiry (see proxy_remote) so it lives as long
    // as the tunnel does.
    let (control_link, link_hash) = match store.live_control_link(instance.id).ok().flatten() {
        Some(live) => {
            store.extend_link(&live.token_hash, TUNNEL_LINK_TTL).ok();
            (live.token, live.token_hash)
        }
        None => match mint_link(
            store.as_ref(),
            instance.id,
            None,
            Role::Controller,
            TUNNEL_LINK_TTL,
        ) {
            Ok(link) => (link.clone(), store::hash_token(&link)),
            Err(err) => {
                tracing::warn!(error = %err, instance_id = instance.id, "link mint failed");
                return;
            }
        },
    };
    // Attach first, then hand the link to the instance: a client that holds
    // the link must find the tunnel already online, or a request fired the
    // instant the link arrives races a 503 against the hub insert.
    let (cmd_tx, cmd_rx) = channel::<TunnelCommand>();
    let epoch = hub.attach(instance.id, cmd_tx, link_hash.clone());
    let _ = writer.lock().unwrap().send(WsMessage::text(
        serde_json::json!({"link": control_link}).to_string(),
    ));

    // Only the writer thread touches the write side; this thread keeps reading.
    let writer_hub = Arc::clone(&hub);
    let writer_id = instance.id;
    let writer_handle = Arc::clone(&writer);
    let _ = thread::spawn(move || {
        while let Ok(command) = cmd_rx.recv() {
            let TunnelCommand::Request { conn_id, request } = command;
            let mut wire = serde_json::Map::new();
            wire.insert("conn_id".into(), conn_id.into());
            if let Ok(inner) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&request)
            {
                wire.extend(inner);
            }
            let wire = serde_json::Value::Object(wire).to_string();
            let mut guard = writer_handle.lock().unwrap();
            if guard.send(WsMessage::text(wire)).is_err() {
                break;
            }
        }
        // Only drops the hub entry if this is still the live tunnel.
        writer_hub.detach(writer_id, epoch);
    });

    loop {
        let message = match reader.read() {
            Ok(message) => message,
            Err(_) => break,
        };
        match message {
            WsMessage::Text(text) => {
                if let Ok(push) = serde_json::from_str::<TunnelPush>(&text) {
                    handle_push(&store, &hub, instance.id, push);
                    continue;
                }
                let Ok(frame) = serde_json::from_str::<TunnelWireFrame>(&text) else {
                    continue;
                };
                let TunnelWireFrame::Response {
                    conn_id,
                    status,
                    content_type,
                    body,
                    final_chunk,
                } = frame;
                hub.deliver_response(
                    instance.id,
                    conn_id,
                    hub::TunnelResponse {
                        status,
                        content_type,
                        body,
                        final_chunk,
                    },
                );
            }
            WsMessage::Ping(payload) => {
                // Instance keepalive: proof of life for the dashboard and the
                // tunnel's control link.
                store.touch_instance(instance.id).ok();
                store.extend_link(&link_hash, TUNNEL_LINK_TTL).ok();
                let _ = writer.lock().unwrap().send(WsMessage::Pong(payload));
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    if hub.detach(instance.id, epoch) {
        store.touch_instance(instance.id).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_token_path_rejects_api_paths() {
        assert_eq!(split_token_path("/api/instances"), None);
        assert_eq!(split_token_path("/api/users"), None);
        assert_eq!(split_token_path("/api/sso"), None);
        assert_eq!(split_token_path("/install.sh"), None);
        assert_eq!(split_token_path("/login"), None);
    }

    #[test]
    fn split_token_path_accepts_valid_token() {
        let token = "a".repeat(32);
        assert_eq!(
            split_token_path(&format!("/{token}/events")),
            Some((token.as_str(), "events"))
        );
        assert_eq!(
            split_token_path(&format!("/{token}/prompt")),
            Some((token.as_str(), "prompt"))
        );
        // Non-hex or wrong length rejected
        assert_eq!(split_token_path("/short/events"), None);
        assert_eq!(
            split_token_path("/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz/events"),
            None
        );
        assert_eq!(split_token_path("//events"), None);
        assert_eq!(split_token_path("/api"), None);
    }

    #[test]
    fn request_path_session_extracts_scoped_id() {
        assert_eq!(request_path_session("s/abc/events").as_deref(), Some("abc"));
        assert_eq!(request_path_session("s/abc").as_deref(), Some("abc"));
        assert_eq!(request_path_session("events"), None);
        assert_eq!(request_path_session("s/").as_deref(), Some(""));
    }

    #[test]
    fn tail_under_session_strips_scope_for_route_match() {
        assert_eq!(tail_under_session("s/abc/events"), "events");
        assert_eq!(tail_under_session("events"), "events");
        // The scope with no route under it is the index.
        assert_eq!(tail_under_session("s/abc/"), "");
        assert_eq!(tail_under_session("s/abc"), "");
    }

    #[test]
    fn non_admin_gate_spares_anonymous_and_admins() {
        let admin = crate::store::UserRow {
            id: 1,
            oidc_sub: "a".into(),
            email: None,
            name: None,
            is_admin: true,
        };
        let user = crate::store::UserRow {
            is_admin: false,
            ..admin.clone()
        };
        assert!(
            !is_non_admin(None),
            "LAN-trust anonymous is not a non-admin"
        );
        assert!(!is_non_admin(Some(&admin)));
        assert!(is_non_admin(Some(&user)));
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%3A"), ":");
        // Multibyte: %C3%A9 is é, which the old byte-into-char decode mangled.
        assert_eq!(url_decode("%C3%A9"), "é");
        // A stray percent stays literal instead of eating following chars.
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("bad%zz"), "bad%zz");
    }
}
