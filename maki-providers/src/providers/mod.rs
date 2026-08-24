use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_lite::StreamExt;
use futures_lite::io::AsyncBufRead;
use isahc::config::Configurable;
use isahc::http::request::Builder;
use serde::Deserialize;
use tracing::debug;

use crate::AgentError;

pub(crate) mod anthropic;
pub(crate) mod aperture;
pub(crate) mod catalog;
pub(crate) mod commandcode;
pub(crate) mod copilot;
pub mod custom;
pub(crate) mod deepseek;
pub mod dynamic;
pub(crate) mod google;
pub(crate) mod llama_cpp;
pub(crate) mod local;
pub(crate) mod mistral;
pub(crate) mod ollama;
pub(crate) mod openai;
pub(crate) mod openai_compat;
pub mod opencode;
pub(crate) mod openrouter;
pub(crate) mod synthetic;
pub(crate) mod tensorx;
pub(crate) mod xai;
pub(crate) mod zai;

const LOW_SPEED_BYTES_PER_SEC: u32 = 1;

pub(crate) fn user_agent() -> &'static str {
    concat!(
        "maki/v",
        env!("CARGO_PKG_VERSION"),
        "-g",
        env!("GIT_SHORT_HASH")
    )
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub stream: Duration,
    pub low_speed: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            stream: Duration::from_secs(300),
            low_speed: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
pub struct ResolvedAuth {
    pub base_url: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl ResolvedAuth {
    pub fn bearer(api_key: &str) -> Self {
        Self {
            base_url: None,
            headers: vec![("authorization".into(), format!("Bearer {api_key}"))],
        }
    }

    /// Apply all auth headers to an HTTP request builder.
    pub fn configure_request(&self, builder: Builder) -> Builder {
        self.headers.iter().fold(builder, |b, (key, value)| {
            b.header(key.as_str(), value.as_str())
        })
    }
}

pub(crate) fn with_prefix<'a>(
    prefix: &Option<String>,
    system: &'a str,
    buf: &'a mut String,
) -> &'a str {
    match prefix {
        Some(p) => {
            *buf = format!("{p}\n\n{system}");
            buf
        }
        None => system,
    }
}

/// Loopback is the only address a browser callback may bind: a login
/// credential must never be reachable off this machine.
const CALLBACK_HOST: &str = "127.0.0.1";
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// A login callback carries a handful of short fields; anything larger is not
/// ours, and reading it would only let a local process grow our memory.
const MAX_CALLBACK_BYTES: usize = 10_000;

/// A one-shot loopback HTTP server for a browser login, shared by the
/// providers whose sign-in hands a credential back to a local port.
///
/// [`Self::wait`] polls for one request at a time and lets the caller decide
/// when the exchange is done, so a provider only writes the part that is
/// actually its own: whether the credential arrives as a POST body or as query
/// parameters, and what makes one valid.
///
/// stdin is polled alongside the socket throughout, because a browser cannot
/// always reach loopback and the user then has to paste instead.
pub(crate) struct LoopbackCallback {
    listener: TcpListener,
    what: &'static str,
    cors_origin: Option<&'static str>,
}

impl LoopbackCallback {
    /// Binds the first free port of `ports`, then any ephemeral one. Pass the
    /// ports an authorization server pre-registers, in preference order.
    ///
    /// `cors_origin` is the HTTPS page that will call back, and is only needed
    /// when it calls with `fetch` rather than by navigating here.
    pub(crate) fn bind(
        what: &'static str,
        ports: impl IntoIterator<Item = u16>,
        cors_origin: Option<&'static str>,
    ) -> Result<Self, AgentError> {
        let listener = ports
            .into_iter()
            .chain(std::iter::once(0))
            .find_map(|port| TcpListener::bind((CALLBACK_HOST, port)).ok())
            .ok_or_else(|| AgentError::Config {
                message: format!("could not bind a loopback port for the {what} callback"),
            })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| AgentError::Config {
                message: format!("{what} callback server: {e}"),
            })?;
        Ok(Self {
            listener,
            what,
            cors_origin,
        })
    }

    pub(crate) fn port(&self) -> Result<u16, AgentError> {
        Ok(self.listener.local_addr()?.port())
    }

    /// Polls until `timeout`, handing every request to `on_request` and every
    /// pasted stdin line to `on_paste`. Either returns `Ok(None)` to keep
    /// waiting, `Ok(Some(_))` to finish, or `Err` to abort the login.
    pub(crate) fn wait<T>(
        &self,
        timeout: Duration,
        timeout_message: &str,
        mut on_request: impl FnMut(&CallbackRequest, &mut Reply) -> Result<Option<T>, AgentError>,
        mut on_paste: impl FnMut(&str) -> Result<Option<T>, AgentError>,
    ) -> Result<T, AgentError> {
        let deadline = Instant::now() + timeout;
        let paste_rx = spawn_paste_reader();
        loop {
            if Instant::now() >= deadline {
                return Err(AgentError::Config {
                    message: timeout_message.into(),
                });
            }
            if let Ok(pasted) = paste_rx.try_recv()
                && let Some(done) = on_paste(&pasted)?
            {
                return Ok(done);
            }

            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    stream.set_read_timeout(Some(CALLBACK_READ_TIMEOUT)).ok();
                    let Some(request) = CallbackRequest::read(&mut stream) else {
                        continue;
                    };
                    let mut reply = Reply {
                        stream: &mut stream,
                        cors_origin: self.cors_origin,
                    };
                    if let Some(done) = on_request(&request, &mut reply)? {
                        return Ok(done);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(e) => {
                    return Err(AgentError::Config {
                        message: format!("{} callback server: {e}", self.what),
                    });
                }
            }
        }
    }
}

/// One request line and body from the callback socket.
pub(crate) struct CallbackRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) body: String,
}

impl CallbackRequest {
    /// Reads exactly the advertised `content-length`, so a POST body is not
    /// truncated by a short first read. `None` for anything unparseable.
    fn read(stream: &mut TcpStream) -> Option<Self> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        let header_end = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            if buf.len() > MAX_CALLBACK_BYTES {
                return None;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };

        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let want = content_length(&head).unwrap_or(0).min(MAX_CALLBACK_BYTES);
        let body_start = header_end + 4;
        while buf.len() < body_start + want {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }

        let mut parts = head.lines().next()?.split_whitespace();
        Some(Self {
            method: parts.next()?.to_string(),
            target: parts.next()?.to_string(),
            body: String::from_utf8_lossy(&buf[body_start..]).to_string(),
        })
    }

    /// The target's query string, empty when it has none.
    pub(crate) fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, q)| q)
    }
}

/// The response side of one callback request.
pub(crate) struct Reply<'a> {
    stream: &'a mut TcpStream,
    cors_origin: Option<&'static str>,
}

impl Reply<'_> {
    /// Best-effort: a browser that has already navigated away cannot be told
    /// anything, and that must not fail a login whose credential did arrive.
    pub(crate) fn send(&mut self, status: u16, content_type: &str, body: &str) {
        let _ = self.write(status, content_type, body);
    }

    fn write(&mut self, status: u16, content_type: &str, body: &str) -> io::Result<()> {
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "Bad Request",
        };
        write!(self.stream, "HTTP/1.1 {status} {reason}\r\n")?;
        if let Some(origin) = self.cors_origin {
            // An HTTPS page calling loopback is cross-origin, and Chrome gates
            // it behind a Private Network Access preflight on top of CORS.
            write!(
                self.stream,
                "access-control-allow-origin: {origin}\r\n\
                 access-control-allow-methods: POST, OPTIONS\r\n\
                 access-control-allow-headers: content-type\r\n\
                 access-control-allow-private-network: true\r\n"
            )?;
        }
        if !content_type.is_empty() {
            write!(self.stream, "content-type: {content_type}\r\n")?;
        }
        write!(
            self.stream,
            "content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Reads pasted lines off stdin for as long as anyone is listening, so a
/// browser that cannot reach loopback is not a dead end.
fn spawn_paste_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let pasted = line.trim().to_string();
                    if !pasted.is_empty() && tx.send(pasted).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// 256 CSPRNG bits, url-safe: the state and PKCE tokens a login mints.
pub(crate) fn random_token() -> Result<String, AgentError> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| AgentError::Config {
        message: format!("CSPRNG unavailable: {e}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub(crate) fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[derive(Deserialize)]
pub(crate) struct SseErrorPayload {
    pub error: SseErrorDetail,
}

#[derive(Deserialize)]
pub(crate) struct SseErrorDetail {
    #[serde(default)]
    pub r#type: String,
    pub message: String,
}

impl SseErrorPayload {
    pub fn into_agent_error(self) -> AgentError {
        let status = match self.error.r#type.as_str() {
            "overloaded_error" => 529,
            "api_error" | "server_error" => 500,
            "rate_limit_error" | "rate_limit_exceeded" | "tokens" => 429,
            "request_too_large" => 413,
            "not_found_error" => 404,
            "permission_error" => 403,
            "billing_error" | "insufficient_quota" => 402,
            "authentication_error" | "invalid_api_key" => 401,
            _ => 400,
        };
        AgentError::Api {
            status,
            message: self.error.message,
        }
    }
}

pub(crate) async fn next_sse_line<R: AsyncBufRead + Unpin>(
    lines: &mut futures_lite::io::Lines<R>,
    deadline: &mut Instant,
    stream_timeout: Duration,
) -> Result<Option<String>, AgentError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = futures_lite::future::or(
        async { lines.next().await.transpose().map_err(AgentError::from) },
        async {
            smol::Timer::after(remaining).await;
            Err(AgentError::Timeout {
                secs: stream_timeout.as_secs(),
            })
        },
    )
    .await;
    if let Ok(Some(_)) = &result {
        *deadline = Instant::now() + stream_timeout;
    }
    result
}

pub(crate) fn http_client(timeouts: Timeouts) -> isahc::HttpClient {
    isahc::HttpClient::builder()
        .connect_timeout(timeouts.connect)
        .low_speed_timeout(LOW_SPEED_BYTES_PER_SEC, timeouts.low_speed)
        .build()
        .expect("failed to build HTTP client")
}

#[derive(Clone, Debug)]
pub struct KeyPool {
    keys: Arc<Vec<String>>,
    index: Arc<AtomicUsize>,
}

impl KeyPool {
    pub fn from_env(env_var: &str) -> Result<Self, AgentError> {
        let raw = std::env::var(env_var).map_err(|_| AgentError::Config {
            message: format!("{env_var} not set"),
        })?;
        let keys: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if keys.is_empty() {
            return Err(AgentError::Config {
                message: format!("{env_var} is empty"),
            });
        }
        Ok(Self {
            keys: Arc::new(keys),
            index: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn resolve(slug: &str, env_var: &str) -> Result<Self, AgentError> {
        if let Ok(pool) = Self::from_env(env_var) {
            debug!(slug, keys = pool.len(), "resolved API key from env");
            return Ok(pool);
        }
        if let Some(key) = Self::key_from_file(slug) {
            debug!(slug, "resolved API key from saved credentials");
            return Ok(Self::from_keys(vec![key]));
        }
        if let Some(key) = Self::key_from_config(slug) {
            debug!(slug, "resolved API key from providers.toml");
            return Ok(Self::from_keys(vec![key]));
        }
        Err(AgentError::Config {
            message: format!(
                "{env_var} not set and no saved credentials for '{slug}' — run `maki auth login {slug}`"
            ),
        })
    }

    fn key_from_file(slug: &str) -> Option<String> {
        let dir = maki_storage::StateDir::resolve().ok()?;
        maki_storage::auth::load_provider_credentials(&dir, slug).map(|c| c.api_key)
    }

    fn key_from_config(slug: &str) -> Option<String> {
        maki_config::providers::ProvidersConfig::load()
            .get(slug)
            .and_then(|d| d.api_key.clone())
    }

    pub(crate) fn from_keys(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys),
            index: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn current(&self) -> &str {
        &self.keys[self.index.load(Ordering::Relaxed) % self.keys.len()]
    }

    pub fn rotate(&self) -> bool {
        if self.keys.len() <= 1 {
            return false;
        }
        self.index.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn rotate_auth(
        &self,
        auth: &Mutex<ResolvedAuth>,
        build: impl FnOnce(&str) -> ResolvedAuth,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        *auth.lock().unwrap() = build(self.current());
        true
    }

    pub fn rotate_headers(
        &self,
        auth: &Mutex<ResolvedAuth>,
        build: impl FnOnce(&str) -> Vec<(String, String)>,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        auth.lock().unwrap().headers = build(self.current());
        true
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::AsyncBufReadExt;
    use test_case::test_case;

    #[test_case("a b", "a%20b" ; "space")]
    #[test_case("a:b", "a%3Ab" ; "colon")]
    #[test_case("abc", "abc"   ; "passthrough")]
    fn urlenc_encodes(input: &str, expected: &str) {
        assert_eq!(urlenc(input), expected);
    }

    #[test_case("POST /callback HTTP/1.1\r\nContent-Length: 42", Some(42) ; "capitalized")]
    #[test_case("POST /callback HTTP/1.1\r\ncontent-length:  7 ", Some(7)  ; "lowercase_padded")]
    #[test_case("POST /callback HTTP/1.1",                        None     ; "absent")]
    fn content_length_is_case_insensitive(head: &str, expected: Option<usize>) {
        assert_eq!(content_length(head), expected);
    }

    #[test]
    fn callback_reads_a_whole_post_body_and_answers_with_cors() {
        let callback = LoopbackCallback::bind("test", [], Some("https://example.test")).unwrap();
        let port = callback.port().unwrap();

        // A body split across writes: the reader must follow content-length
        // rather than stopping at whatever the first read happened to return.
        let body = r#"{"apiKey":"secret","state":"s1"}"#;
        let sender = std::thread::spawn(move || {
            let mut stream = TcpStream::connect((CALLBACK_HOST, port)).unwrap();
            let (head, tail) = body.split_at(10);
            write!(
                stream,
                "POST /callback?x=1 HTTP/1.1\r\ncontent-length: {}\r\n\r\n{head}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            stream.write_all(tail.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });

        let got = callback
            .wait(
                Duration::from_secs(10),
                "timed out",
                |request, reply| {
                    reply.send(200, "application/json", r#"{"ok":true}"#);
                    Ok(Some((
                        request.method.clone(),
                        request.target.clone(),
                        request.query().to_string(),
                        request.body.clone(),
                    )))
                },
                |_| Ok(None),
            )
            .unwrap();

        assert_eq!(got.0, "POST");
        assert_eq!(got.1, "/callback?x=1");
        assert_eq!(got.2, "x=1");
        assert_eq!(got.3, body);

        let response = sender.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("access-control-allow-origin: https://example.test\r\n"));
        assert!(response.contains("access-control-allow-private-network: true\r\n"));
        assert!(response.ends_with("\r\n\r\n{\"ok\":true}"));
    }

    #[test]
    fn callback_without_a_cors_origin_sends_no_cors_headers() {
        let callback = LoopbackCallback::bind("test", [], None).unwrap();
        let port = callback.port().unwrap();

        let sender = std::thread::spawn(move || {
            let mut stream = TcpStream::connect((CALLBACK_HOST, port)).unwrap();
            write!(stream, "GET /callback?code=abc HTTP/1.1\r\n\r\n").unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });

        // The first request keeps waiting, so a stray probe cannot end a login.
        let mut seen = 0;
        let code = callback
            .wait(
                Duration::from_secs(10),
                "timed out",
                |request, reply| {
                    seen += 1;
                    reply.send(404, "text/plain", "no");
                    Ok(request
                        .query()
                        .strip_prefix("code=")
                        .map(std::string::ToString::to_string))
                },
                |_| Ok(None),
            )
            .unwrap();

        assert_eq!(code, "abc");
        assert_eq!(seen, 1);
        let response = sender.join().unwrap();
        assert!(!response.contains("access-control-allow-origin"));
    }

    #[test]
    fn random_tokens_differ_and_are_url_safe() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    struct NeverReader;

    impl futures_lite::io::AsyncRead for NeverReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
    }

    impl futures_lite::io::AsyncBufRead for NeverReader {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            std::task::Poll::Pending
        }

        fn consume(self: std::pin::Pin<&mut Self>, _amt: usize) {}
    }

    #[test]
    fn next_sse_line_expired_deadline_returns_timeout() {
        smol::block_on(async {
            let mut lines = NeverReader.lines();
            let mut past = Instant::now() - Duration::from_secs(1);
            let stream_timeout = Duration::from_secs(300);
            let err = next_sse_line(&mut lines, &mut past, stream_timeout)
                .await
                .unwrap_err();
            assert!(matches!(err, AgentError::Timeout { .. }));
        })
    }

    #[test]
    fn key_pool_single_key_current() {
        let pool = KeyPool::from_keys(vec!["sk-1".into()]);
        assert_eq!(pool.current(), "sk-1");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn key_pool_single_key_rotate_returns_false() {
        let pool = KeyPool::from_keys(vec!["sk-1".into()]);
        assert!(!pool.rotate());
        assert_eq!(pool.current(), "sk-1");
    }

    #[test]
    fn key_pool_multi_key_rotates() {
        let pool = KeyPool::from_keys(vec!["sk-1".into(), "sk-2".into(), "sk-3".into()]);
        assert_eq!(pool.current(), "sk-1");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-2");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-3");
    }

    #[test]
    fn key_pool_wraps_around() {
        let pool = KeyPool::from_keys(vec!["a".into(), "b".into()]);
        pool.rotate();
        pool.rotate();
        assert_eq!(pool.current(), "a");
    }

    #[test]
    fn resolve_from_env() {
        let env_var = format!("MAKI_TEST_KEY_{}", fastrand::u32(..));
        unsafe { std::env::set_var(&env_var, "from-env") };
        let pool = KeyPool::resolve("test_slug", &env_var).unwrap();
        unsafe { std::env::remove_var(&env_var) };
        assert_eq!(pool.current(), "from-env");
    }

    #[test]
    fn resolve_env_supports_comma_separated() {
        let env_var = format!("MAKI_TEST_MULTI_{}", fastrand::u32(..));
        unsafe { std::env::set_var(&env_var, "sk-1, sk-2, sk-3") };
        let pool = KeyPool::resolve("test_slug", &env_var).unwrap();
        unsafe { std::env::remove_var(&env_var) };
        assert_eq!(pool.current(), "sk-1");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-2");
    }

    #[test]
    fn resolve_returns_error_when_nothing_found() {
        let slug = format!("test_resolve_none_{}", fastrand::u32(..));
        let env_var = format!("MAKI_TEST_KEY_NONE_{}", fastrand::u32(..));
        let result = KeyPool::resolve(&slug, &env_var);
        assert!(result.is_err());
        let msg = format!("{result:?}");
        assert!(msg.contains(&env_var) || msg.contains(&slug));
    }
}
