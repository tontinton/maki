//! A recorded loopback server: the one copy every replay suite in the
//! workspace serves its transcripts from. Also the one way a test registers a
//! declared provider, see [`register_bundled`], and hides this machine's own
//! provider setup, see [`isolated_home`].
//!
//! Synchronous and dependency-light on purpose. It is the thing a provider is
//! compared *against*, so it shares nothing with the async stack under test:
//! a blocking `TcpListener`, a fixed script, and the request bytes kept
//! verbatim. Verbatim matters, because a recorder that parsed the body first
//! would quietly repair whatever the client got wrong, and each suite is left
//! to decide for itself what counts as a difference.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tempfile::TempDir;

use crate::providers::plugin::{self, DeclAuthority, ProviderDecl, ProviderHooks, Registration};

const LOOPBACK: &str = "127.0.0.1:0";
const REASON_PHRASE: &str = "Recorded";
const CONTENT_LENGTH: &str = "content-length";
const AUTHORIZATION: &str = "authorization";
const BIND_FAILED: &str = "cannot bind loopback";
const IO_FAILED: &str = "the recorded connection broke";
const QUERY_START: char = '?';
const BAD_DECLARATION: &str = "the test declaration must register";
const TEMPDIR_FAILED: &str = "cannot create the isolated home";
const BASE_URL_SUFFIX: &str = "_BASE_URL";
const HOME_ENV: &str = "HOME";
// Logs sit beside the state base, so keep its parent inside the guard.
const XDG_DIRS: &[(&str, &str)] = &[
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
];
const MIXED_SCRIPT: &str =
    "a script is either all routed with `Canned::at` or all sequential, never a mix";

/// The answer to a routed request no entry is left for. It is written and
/// recorded rather than dropped, so a stray request shows up in the
/// observation instead of hanging the client.
const NO_ROUTE: Canned = Canned::json(
    404,
    r#"{"error":{"message":"no canned answer for this path"}}"#,
);

pub const SSE_HEADERS: &[(&str, &str)] = &[("content-type", "text/event-stream")];
pub const JSON_HEADERS: &[(&str, &str)] = &[("content-type", "application/json")];

/// One recorded response, replayed in script order unless it is routed.
pub struct Canned {
    pub status: u16,
    pub headers: &'static [(&'static str, &'static str)],
    pub body: &'static str,
    /// `Some` serves this answer to the first request for this path, whatever
    /// order it arrives in. Set through [`Canned::at`].
    pub path: Option<&'static str>,
}

impl Canned {
    pub const fn sse(body: &'static str) -> Self {
        Self {
            status: 200,
            headers: SSE_HEADERS,
            body,
            path: None,
        }
    }

    pub const fn json(status: u16, body: &'static str) -> Self {
        Self {
            status,
            headers: JSON_HEADERS,
            body,
            path: None,
        }
    }

    /// `answer`, served to the request for `path` instead of in script order.
    /// For a provider that fires requests concurrently, where arrival order is
    /// a race and a sequential script would hand each one the other's answer.
    ///
    /// `path` is matched against the request path with its query string cut,
    /// so a query carrying today's date still routes. Two entries for one path
    /// are served in script order.
    pub const fn at(path: &'static str, answer: Canned) -> Self {
        Self {
            path: Some(path),
            ..answer
        }
    }
}

/// Whether `script` routes by path. [`serve`] has already rejected a mix, so
/// the first entry speaks for all of them.
pub fn is_routed(script: &[Canned]) -> bool {
    script.first().is_some_and(|canned| canned.path.is_some())
}

/// What the client actually put on the wire, before anything parsed it.
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Lowercased names, in name order, so a comparison reads the same on
    /// every run whatever order the client emitted them in.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn authorization(&self) -> &str {
        self.headers.get(AUTHORIZATION).map_or("", String::as_str)
    }

    /// The body as the provider meant it, for assertions about content rather
    /// than about bytes.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// Every request the server has answered, in order. Each entry lands before
/// its response is written, so a request the client has an answer to is
/// already here.
pub type Requests = Arc<Mutex<Vec<Recorded>>>;

/// Serves `script` on loopback, one connection per entry: in order, or by
/// path when every entry is routed. Panics on a script that mixes the two,
/// since which entry a request drew would then depend on the race routing
/// exists to remove.
///
/// The log is shared rather than joined and the server thread is detached: a
/// script is an upper bound on what a run sends, and a caller replaying one
/// script through two providers has to be able to read what the shorter of
/// them sent without parking forever on an `accept` that will never return.
pub fn serve(script: &'static [Canned]) -> (String, Requests) {
    let routed = is_routed(script);
    assert!(
        script.iter().all(|canned| canned.path.is_some() == routed),
        "{MIXED_SCRIPT}"
    );
    let listener = TcpListener::bind(LOOPBACK).expect(BIND_FAILED);
    let base_url = format!("http://{}/v1", listener.local_addr().expect(BIND_FAILED));
    let requests = Requests::default();
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        let mut served = vec![false; script.len()];
        for next in 0..script.len() {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let request = read_request(&stream);
            let answer = if routed {
                route(script, &mut served, &request.path)
            } else {
                &script[next]
            };
            log.lock().unwrap().push(request);
            write_canned(&stream, answer);
        }
    });
    (base_url, requests)
}

fn route<'s>(script: &'s [Canned], served: &mut [bool], path: &str) -> &'s Canned {
    let path = path.split(QUERY_START).next().unwrap_or_default();
    let Some(index) =
        (0..script.len()).find(|&index| !served[index] && script[index].path == Some(path))
    else {
        return &NO_ROUTE;
    };
    served[index] = true;
    &script[index]
}

fn read_request(stream: &TcpStream) -> Recorded {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect(IO_FAILED);
    let mut start = request_line.split_whitespace();
    let method = start.next().unwrap_or_default().to_owned();
    let path = start.next().unwrap_or_default().to_owned();

    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect(IO_FAILED);
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect(IO_FAILED);
    Recorded {
        method,
        path,
        headers,
        body,
    }
}

fn write_canned(mut stream: &TcpStream, canned: &Canned) {
    let mut response = format!(
        "HTTP/1.1 {} {REASON_PHRASE}\r\n{CONTENT_LENGTH}: {}\r\nconnection: close\r\n",
        canned.status,
        canned.body.len()
    );
    for (name, value) in canned.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(canned.body);
    stream.write_all(response.as_bytes()).expect(IO_FAILED);
    stream.flush().expect(IO_FAILED);
}

/// Registers `authored` like a bundled plugin load would, with `host` as its
/// only net host. Each call is a whole load, so it replaces whatever the last
/// call registered.
pub fn register_bundled(authored: Value, host: &str) {
    let mut decl: ProviderDecl = serde_json::from_value(authored).expect(BAD_DECLARATION);
    decl.net_hosts = vec![host.to_owned()];
    plugin::begin_load();
    let registration = Registration {
        decl,
        hooks: ProviderHooks::default(),
    };
    plugin::register(registration, DeclAuthority::Bundled, &[]).expect(BAD_DECLARATION);
    plugin::commit_load();
}

/// One SSE frame, so a parser test can spell its events as `json!` instead of
/// an escaped string.
pub fn sse_event(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// What the transport tests in `http` and `error` fail against: a port that
/// refuses, a port that cannot complete a TLS handshake, a name that cannot
/// resolve, and a secret that must never reach a message.
#[cfg(test)]
pub(crate) mod transport {
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    pub(crate) const LOOPBACK_HOST: &str = "127.0.0.1";
    /// Reserved by RFC 6761, so every resolver answers NXDOMAIN for it. The
    /// trailing dot keeps it absolute: a search-domain resolver would
    /// otherwise rewrite the bare name to something that can resolve.
    pub(crate) const UNRESOLVABLE_HOST: &str = "maki.invalid.";
    pub(crate) const QUERY_SECRET: &str = "sk-not-for-logs";
    pub(crate) const URL_MARKER: &str = "for url";
    const PLAIN_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\r\n";

    fn loopback_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind((LOOPBACK_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// A loopback port nothing listens on any more.
    pub(crate) fn closed_port() -> u16 {
        loopback_listener().1
    }

    /// A loopback port where plain HTTP answers, so a TLS handshake to it
    /// cannot complete.
    pub(crate) fn plain_http_port() -> u16 {
        let (listener, port) = loopback_listener();
        thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _ = conn.write_all(PLAIN_HTTP_RESPONSE);
        });
        port
    }
}

/// Points every per-user directory at a fresh temp dir and clears every
/// `<SLUG>_BASE_URL` override, so a test reads none of this machine's provider
/// config or credentials and writes its own. Call it first, since the base
/// dirs are resolved once per process, and keep the guard for the whole test.
pub fn isolated_home() -> TempDir {
    let home = TempDir::new().expect(TEMPDIR_FAILED);
    let overrides: Vec<_> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key.to_str()
                .is_some_and(|key| key.ends_with(BASE_URL_SUFFIX))
        })
        .collect();
    // SAFETY: Nextest runs each test in its own process. Callers set up the
    // environment before starting work that can read it.
    unsafe {
        for key in overrides {
            std::env::remove_var(key);
        }
        std::env::set_var(HOME_ENV, home.path());
        for (var, dir) in XDG_DIRS {
            std::env::set_var(var, home.path().join(dir));
        }
    }
    home
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::panic::catch_unwind;
    use std::path::PathBuf;

    use maki_storage::paths;
    use test_case::test_case;

    use super::*;

    const A_PATH: &str = "/v1/a";
    const B_PATH: &str = "/v1/b";
    const A_BODY: &str = r#"{"route":"a"}"#;
    const B_BODY: &str = r#"{"route":"b"}"#;
    const DIRECTORY_ESCAPED: &str = "a user directory escaped the isolated home";
    const ROUTED: &[Canned] = &[
        Canned::at(B_PATH, Canned::json(200, B_BODY)),
        Canned::at(A_PATH, Canned::json(200, A_BODY)),
    ];
    const MIXED: &[Canned] = &[
        Canned::at(A_PATH, Canned::json(200, A_BODY)),
        Canned::json(200, B_BODY),
    ];

    #[test_case(paths::config_dir ; "config")]
    #[test_case(paths::xdg_config_dir ; "xdg_config")]
    #[test_case(paths::data_dir ; "data")]
    #[test_case(paths::state_dir ; "state")]
    #[test_case(paths::logs_dir ; "logs")]
    #[test_case(paths::cache_dir ; "cache")]
    fn isolated_home_contains_user_directories(resolve: fn() -> io::Result<PathBuf>) {
        let home = isolated_home();
        let directory = resolve().unwrap();
        assert!(
            directory.starts_with(home.path()),
            "{DIRECTORY_ESCAPED}: {}",
            directory.display()
        );
    }

    /// One raw request on its own connection, answered by the status line and
    /// the body.
    fn fetch(base_url: &str, path: &str) -> (String, String) {
        let authority = base_url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap();
        let mut stream = TcpStream::connect(authority).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nhost: {authority}\r\n{CONTENT_LENGTH}: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.lines().next().unwrap().to_owned();
        (status, body.to_owned())
    }

    #[test_case(A_PATH, A_BODY ; "plain_path")]
    #[test_case("/v1/a?start_date=2026-09-23", A_BODY ; "query_is_cut")]
    fn routed_script_answers_by_path_not_order(path: &str, expected: &str) {
        let (base_url, requests) = serve(ROUTED);
        assert_eq!(fetch(&base_url, path).1, expected);
        assert_eq!(fetch(&base_url, B_PATH).1, B_BODY);
        assert_eq!(requests.lock().unwrap()[0].path, path);
    }

    #[test]
    fn unrouted_path_is_answered_and_recorded() {
        const STRAY: &str = "/v1/stray";
        let (base_url, requests) = serve(ROUTED);
        let (status, body) = fetch(&base_url, STRAY);
        assert!(status.contains(&NO_ROUTE.status.to_string()));
        assert_eq!(body, NO_ROUTE.body);
        assert_eq!(requests.lock().unwrap()[0].path, STRAY);
    }

    #[test]
    fn mixed_script_is_rejected() {
        let payload = catch_unwind(|| serve(MIXED)).err().unwrap();
        assert_eq!(payload.downcast_ref::<String>().unwrap(), MIXED_SCRIPT);
    }
}
