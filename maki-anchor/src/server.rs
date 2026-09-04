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
use tiny_http::{Header, Response, Server};
use tungstenite::{Message as WsMessage, protocol::WebSocket};

use crate::{
    hub::{self, Hub, TunnelCommand, TunnelPush},
    store::{self, Role, SessionRow, Store},
};

const TUNNEL_LINK_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// Browser requests and instance tunnels each get a thread; cap the counts so
/// a flood costs 503s instead of unbounded threads.
const MAX_CONCURRENT_REQUESTS: usize = 256;
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
    store.create_link(
        &store::hash_token(&token),
        instance_id,
        session_id,
        rights.as_str(),
        ttl,
    )?;
    Ok(token)
}

fn request_path_session(tail: &str) -> Option<String> {
    let rest = tail.strip_prefix("s/")?;
    Some(rest.split('/').next().unwrap_or(rest).to_owned())
}

fn buffered(
    (status, content_type, body): (u16, String, Vec<u8>),
    request: tiny_http::Request,
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
    request: tiny_http::Request,
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

/// Tunnel listen address: an explicit `ws_bind` host (with `:port`) wins,
/// otherwise the HTTP host with `http_port + 1`. Keeps the default behavior
/// while letting operators bind the tunnel to loopback only.
fn resolve_ws_addr(ws_bind: Option<&str>, addr: &str, http_port: u16) -> String {
    match ws_bind {
        None => format!(
            "{}:{}",
            addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr),
            http_port + 1
        ),
        Some(bind) => match bind.rsplit_once(':') {
            Some((_host, port)) if port.parse::<u16>().is_ok() => bind.to_owned(),
            _ => format!("{bind}:{}", http_port + 1),
        },
    }
}

pub fn serve(
    addr: &str,
    ws_bind: Option<&str>,
    store: Arc<Store>,
    oidc: Option<crate::oidc::OidcConfig>,
    allow_local: bool,
    mint_tokens: store::MintTokens,
) -> Result<(), ServerError> {
    let listener = TcpListener::bind(addr).map_err(|source| ServerError::Bind {
        addr: addr.to_string(),
        source,
    })?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let server = Arc::new(Server::from_listener(listener, None).expect("tiny_http server"));
    let hub = Hub::new();

    // The instance tunnel lives on its own listener: browser traffic goes
    // through tiny_http, tunnels through a plain socket we drive directly.
    // `ws_bind` overrides the host (and optionally the port) so operators
    // fronting one public origin can pin the tunnel to loopback.
    let ws_addr = resolve_ws_addr(ws_bind, addr, port);
    let ws_listener = TcpListener::bind(&ws_addr).map_err(|source| ServerError::Bind {
        addr: ws_addr.clone(),
        source,
    })?;
    let auth = Arc::new(crate::auth::Auth::new(
        Arc::clone(&store),
        oidc,
        allow_local,
        mint_tokens,
    ));
    let ws_store = Arc::clone(&store);
    let ws_hub = Arc::clone(&hub);
    let tunnel_slots = Arc::new(AtomicUsize::new(0));
    let ws_slots = Arc::clone(&tunnel_slots);
    thread::spawn(move || {
        for socket in ws_listener.incoming() {
            let Ok(socket) = socket else {
                continue;
            };
            if ws_slots.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT_TUNNELS {
                ws_slots.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            let _ = socket.set_nodelay(true);
            let hub = Arc::clone(&ws_hub);
            let store = Arc::clone(&ws_store);
            let slots = Arc::clone(&ws_slots);
            thread::spawn(move || {
                let _slot = SlotGuard(slots);
                let Ok(websocket) = tungstenite::accept(socket) else {
                    return;
                };
                drive_tunnel(websocket, hub, store);
            });
        }
    });

    tracing::info!(addr, ws_addr, "anchor listening");

    let request_slots = Arc::new(AtomicUsize::new(0));
    loop {
        let request = match server.recv() {
            Ok(request) => request,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                continue;
            }
        };
        if request_slots.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT_REQUESTS {
            request_slots.fetch_sub(1, Ordering::Relaxed);
            let response = Response::from_string("too many requests")
                .with_status_code(503)
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], b"text/plain".as_ref()).unwrap(),
                );
            let _ = request.respond(response);
            continue;
        }
        let hub = Arc::clone(&hub);
        let store = Arc::clone(&store);
        let auth = Arc::clone(&auth);
        let slots = Arc::clone(&request_slots);
        thread::spawn(move || {
            let _slot = SlotGuard(slots);
            handle_request(request, hub, store, auth);
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

fn handle_request(
    request: tiny_http::Request,
    hub: Arc<Hub>,
    store: Arc<Store>,
    auth: Arc<crate::auth::Auth>,
) {
    let method = request.method().to_string();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("");

    if path == "/ws" {
        // The tunnel endpoint lives on the WS listener (HTTP port + 1).
        let response = Response::from_string("use the ws port")
            .with_status_code(426)
            .with_header(Header::from_bytes(&b"Content-Type"[..], b"text/plain".as_ref()).unwrap());
        let _ = request.respond(response);
        return;
    }

    let outcome = route(path, request, &hub, &store, &auth);
    match outcome {
        RouteOutcome::Buffered(buffered) => {
            let response = Response::from_data(buffered.body)
                .with_status_code(buffered.status)
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], buffered.content_type.as_bytes())
                        .unwrap(),
                );
            let _ = buffered.request.respond(response);
            tracing::debug!(method, path, status = buffered.status, "request");
        }
        RouteOutcome::Streamed => {}
    }
}

fn route(
    path: &str,
    request: tiny_http::Request,
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
    if let Some((token, tail)) = split_token_path(path) {
        let user = auth.user_from_cookie(cookie_header(&request));
        return proxy_remote(token, tail, request, hub, store, user.as_ref());
    }
    // Auth endpoints work without a session; everything user-facing below
    // needs one when OIDC is on.
    if path == "/login" {
        if request.method().as_str() == "POST" && auth.allow_local {
            return handle_local_login(request, auth);
        }
        // GET /login: if OIDC+local enabled, show chooser; if OIDC only, redirect; if local only, show form
        if auth.enabled() && auth.allow_local {
            return render_login_page(request, auth);
        }
        if auth.enabled() {
            return start_login(request, auth);
        }
        if auth.allow_local {
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
    if path == "/api/login" && auth.allow_local {
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
    if auth.enabled() && user.is_none() {
        return redirect_to_login(request);
    }
    if auth.allow_local && auth.effective_mint_tokens() != store::MintTokens::Any && user.is_none()
    {
        // Only require auth for dashboard if there is at least one user (otherwise bootstrapping)
        let has_users = store.list_users().map(|u| !u.is_empty()).unwrap_or(false);
        if has_users {
            return redirect_to_login(request);
        }
    }
    route_authorized(path, request, hub, store, user, auth)
}

fn cookie_header(request: &tiny_http::Request) -> Option<&str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str() == "Cookie")
        .map(|h| h.value.as_str())
}

/// Best-effort client identity for login rate limiting. IP only (ports churn
/// per connection), and behind a reverse proxy every request shares one
/// address, so the limiter keys per proxy; local brute-forcing still trips it.
fn remote_origin(request: &tiny_http::Request) -> String {
    request
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn start_login(request: tiny_http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
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

fn render_login_page(request: tiny_http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
    let has_oidc = auth.enabled();
    let allow_local = auth.allow_local;
    // If ?oidc=1 is present, do OIDC redirect directly
    if has_oidc && request.url().contains("oidc=1") {
        return start_login(request, auth);
    }
    let mut body = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>login</title>\
         <style>body{font-family:system-ui;margin:2rem auto;max-width:40rem;padding:0 1rem} form{margin:1rem 0;padding:1rem;border:1px solid #ddd;border-radius:.5rem} input{padding:.4rem .6rem;border:1px solid #ccc;border-radius:.3rem;width:100%;box-sizing:border-box} button{padding:.5rem 1rem;margin-top:.5rem}</style></head><body><h1>maki anchor — login</h1>",
    );
    if has_oidc {
        body.push_str("<p><a href=\"/login?oidc=1\" style=\"display:inline-block;padding:.6rem 1rem;background:#0072ff;color:#fff;text-decoration:none;border-radius:.3rem\">Log in with SSO</a></p>");
        if allow_local {
            body.push_str("<hr>");
        }
    }
    if allow_local {
        body.push_str(
            r#"<form method="post" action="/login">
            <h3>Local login</h3>
            <label>Username<br><input name="username" required></label><br><br>
            <label>Password<br><input type="password" name="password" required></label><br>
            <button type="submit">Log in</button>
            </form>
            <p><small>First local user becomes admin. Create users on server: <code>maki-anchor users add &lt;username&gt; --admin</code></small></p>"#,
        );
    }
    if !has_oidc && !allow_local {
        body.push_str("<p>No login configured (OIDC and local disabled).</p>");
    }
    body.push_str("</body></html>");
    buffered((200, "text/html".to_string(), body.into_bytes()), request)
}

fn handle_local_login(mut request: tiny_http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
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
        Ok(cookie) => {
            let response = Response::empty(302)
                .with_header(Header::from_bytes("Location", "/".as_bytes()).unwrap())
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
                "text/html".to_string(),
                b"<html><body>too many failed logins, wait a while <a href=\"/login\">back</a></body></html>"
                    .to_vec(),
            ),
            request,
        ),
        Err(_) => buffered(
            (
                401,
                "text/html".to_string(),
                b"<html><body>invalid credentials <a href=\"/login\">back</a></body></html>"
                    .to_vec(),
            ),
            request,
        ),
    }
}

fn handle_api_local_login(
    mut request: tiny_http::Request,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
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

fn finish_login(request: tiny_http::Request, auth: &crate::auth::Auth) -> RouteOutcome {
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
    request: tiny_http::Request,
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

fn redirect_to_login(request: tiny_http::Request) -> RouteOutcome {
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

fn forbidden_json(request: tiny_http::Request) -> RouteOutcome {
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
    mut request: tiny_http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if request.method().as_str() != "POST" {
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
    mut request: tiny_http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method().as_str() != "POST" {
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

fn handle_api_revoke_grant(
    mut request: tiny_http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method().as_str() != "POST" {
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
    mut request: tiny_http::Request,
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
    mut request: tiny_http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method().as_str() != "POST" {
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
    mut request: tiny_http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
) -> RouteOutcome {
    if request.method().as_str() != "POST" {
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
    mut request: tiny_http::Request,
    store: &Arc<Store>,
    user: Option<&crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if request.method().as_str() != "POST" {
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
    request: tiny_http::Request,
    _hub: &Hub,
    store: &Arc<Store>,
    user: Option<crate::store::UserRow>,
    auth: &crate::auth::Auth,
) -> RouteOutcome {
    if path == "/" {
        return buffered(
            crate::dashboard::render(store, user.as_ref(), auth),
            request,
        );
    }
    if path == "/links" {
        return render_link(request, store, user.as_ref());
    }
    if path == "/api/instances" {
        return handle_api_create_instance(request, store, user.as_ref(), auth);
    }
    if path == "/api/users" {
        return match request.method().as_str() {
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
        return match request.method().as_str() {
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
                        b"<html><body>admin only <a href=\"/\">back</a></body></html>".to_vec(),
                    ),
                    request,
                );
            }
            None => return redirect_to_login(request),
        }
    }
    if path == "/api/config/mint_tokens" {
        return match request.method().as_str() {
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
    let outcome = if path == "/instances" {
        json_list_instances_for(store, user.as_ref())
    } else if path == "/sessions" {
        json_list_sessions_for(store, user.as_ref())
    } else if let Some(rest) = path.strip_prefix("/sessions/") {
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
fn handle_push(store: &Arc<Store>, instance_id: i64, push: TunnelPush) {
    let TunnelPush::SessionIndex { sessions } = push;
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
    request: tiny_http::Request,
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

    let method = request.method().as_str().to_string();
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
    request: tiny_http::Request,
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

fn drive_tunnel(websocket: WebSocket<std::net::TcpStream>, hub: Arc<Hub>, store: Arc<Store>) {
    let mut websocket = websocket;
    let hello = match websocket.read() {
        Ok(WsMessage::Text(text)) => text,
        _ => return,
    };
    let Ok(parsed) = serde_json::from_str::<HelloFrame>(&hello) else {
        return;
    };
    let instance = match store.instance_by_registration_token(&parsed.registration_token) {
        Ok(instance) if instance.name == parsed.instance_name => instance,
        _ => {
            let _ = websocket.send(WsMessage::text("auth failed"));
            return;
        }
    };
    store.touch_instance(instance.id).ok();
    // Every tunnel gets a fresh control link; traffic on it slides the expiry
    // (see proxy_remote) so it lives as long as the tunnel does. The URL goes
    // back to the instance to display.
    let control_link = match mint_link(
        store.as_ref(),
        instance.id,
        None,
        Role::Controller,
        TUNNEL_LINK_TTL,
    ) {
        Ok(link) => link,
        Err(err) => {
            tracing::warn!(error = %err, instance_id = instance.id, "link mint failed");
            return;
        }
    };
    let link_hash = store::hash_token(&control_link);
    let _ = websocket.send(WsMessage::text(
        serde_json::json!({"link": control_link}).to_string(),
    ));
    let (cmd_tx, cmd_rx) = channel::<TunnelCommand>();
    let epoch = hub.attach(instance.id, cmd_tx, link_hash.clone());

    let write_socket = websocket.into_inner();
    let read_socket = match write_socket.try_clone() {
        Ok(read_socket) => read_socket,
        Err(_) => {
            hub.detach(instance.id, epoch);
            return;
        }
    };
    // Only the writer thread touches this handle; reader uses its own clone.
    let writer = Arc::new(Mutex::new(WebSocket::from_raw_socket(
        write_socket,
        tungstenite::protocol::Role::Server,
        None,
    )));
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

    let mut reader =
        WebSocket::from_raw_socket(read_socket, tungstenite::protocol::Role::Server, None);
    loop {
        let message = match reader.read() {
            Ok(message) => message,
            Err(_) => break,
        };
        match message {
            WsMessage::Text(text) => {
                if let Ok(push) = serde_json::from_str::<TunnelPush>(&text) {
                    handle_push(&store, instance.id, push);
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

    #[test]
    fn ws_addr_defaults_to_http_host_plus_one() {
        assert_eq!(resolve_ws_addr(None, "0.0.0.0:8688", 8688), "0.0.0.0:8689");
        assert_eq!(resolve_ws_addr(None, "127.0.0.1:0", 4321), "127.0.0.1:4322");
    }

    #[test]
    fn ws_addr_honors_host_only_or_full_bind() {
        // Host only -> reuse http port + 1 on that host.
        assert_eq!(
            resolve_ws_addr(Some("127.0.0.1"), "0.0.0.0:8688", 8688),
            "127.0.0.1:8689"
        );
        // Explicit port wins.
        assert_eq!(
            resolve_ws_addr(Some("127.0.0.1:9000"), "0.0.0.0:8688", 8688),
            "127.0.0.1:9000"
        );
    }
}
