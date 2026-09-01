use std::io::{self, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use std::{env, thread};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::AgentError;

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(2);
const READ_CHUNK: usize = 1024;
const MAX_REQUEST_SIZE: usize = 8192;
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";
const TOKEN_BYTES: usize = 32;
const MIN_RAW_CODE_LEN: usize = 20;

const SUCCESS_HTML: &str =
    "<html><body><h1>Authentication successful</h1><p>You can close this tab.</p></body></html>";
const FAILURE_HTML: &str =
    "<html><body><h1>Authentication failed</h1><p>Return to Maki and try again.</p></body></html>";
const NOT_FOUND_HTML: &str = "<html><body><h1>Not found</h1></body></html>";

const STATE_MISMATCH: &str = "OAuth authorization failed: state mismatch";
const CALLBACK_TIMEOUT_MSG: &str = "timed out waiting for the OAuth callback";
const RAW_CODE_MSG: &str = "raw authorization codes are not accepted; paste the complete redirect URL containing both code and state";
const EMPTY_PASTE_MSG: &str = "empty OAuth callback";
const INCOMPLETE_PASTE_MSG: &str =
    "ignored pasted OAuth input because it was not a complete redirect URL";
const INVALID_METHOD_MSG: &str = "invalid login method";

#[derive(Clone, Copy)]
enum Status {
    Ok,
    BadRequest,
    Forbidden,
    NotFound,
}

impl Status {
    fn line(self) -> &'static str {
        match self {
            Self::Ok => "200 OK",
            Self::BadRequest => "400 Bad Request",
            Self::Forbidden => "403 Forbidden",
            Self::NotFound => "404 Not Found",
        }
    }
}

#[derive(Debug)]
pub(crate) struct CallbackResult {
    pub code: Option<String>,
    pub error: Option<String>,
}

/// Loopback redirect server for the OAuth authorization code flow.
pub(crate) struct Loopback {
    /// Preferred port. Providers that register a fixed redirect URI must get this one.
    pub port: u16,
    /// Retry on an ephemeral port when `port` is taken. Only safe when the provider
    /// accepts any loopback port in the redirect URI.
    pub fallback_to_ephemeral: bool,
    pub path: &'static str,
    pub timeout: Duration,
    /// Also accept the redirect URL pasted on stdin, for when the browser runs elsewhere.
    pub paste_fallback: bool,
}

pub(crate) struct Server {
    listeners: Vec<TcpListener>,
    port: u16,
    config: Loopback,
}

impl Loopback {
    pub(crate) fn bind(self) -> Result<Server, AgentError> {
        let bound = bind_localhost(self.port).or_else(|e| {
            if self.fallback_to_ephemeral {
                bind_localhost(0)
            } else {
                Err(e)
            }
        });
        let (listeners, port) = bound.map_err(|e| AgentError::Config {
            message: format!(
                "OAuth callback could not listen on port {} ({e}); another login may already be running, otherwise pick device login",
                self.port
            ),
        })?;
        Ok(Server {
            listeners,
            port,
            config: self,
        })
    }
}

impl Server {
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn wait(&self, expected_state: &str) -> Result<CallbackResult, AgentError> {
        let deadline = Instant::now() + self.config.timeout;
        let pastes = self.config.paste_fallback.then(spawn_paste_reader);

        while Instant::now() < deadline {
            if let Some(rx) = &pastes
                && let Ok(pasted) = rx.try_recv()
            {
                return parse_callback_input(&pasted, expected_state);
            }

            let mut accepted = false;
            for listener in &self.listeners {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted = true;
                        if let Some(result) = self.serve(stream, expected_state) {
                            return Ok(result);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        return Err(AgentError::Config {
                            message: format!("OAuth callback: {e}"),
                        });
                    }
                }
            }
            if !accepted {
                thread::sleep(ACCEPT_POLL);
            }
        }

        Err(AgentError::Config {
            message: CALLBACK_TIMEOUT_MSG.into(),
        })
    }

    /// `None` keeps waiting: a stale tab or a stray request must not end the login.
    fn serve(&self, mut stream: TcpStream, expected_state: &str) -> Option<CallbackResult> {
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).ok();
        let head = read_request_head(&mut stream).ok()?;
        let head = String::from_utf8_lossy(&head);
        let target = head.split_whitespace().nth(1)?;

        if request_path(target) != self.config.path {
            let _ = write_http(&mut stream, Status::NotFound, NOT_FOUND_HTML);
            return None;
        }

        match parse_callback_target(target, expected_state) {
            Ok(result) => {
                let complete = result.error.is_none() && result.code.is_some();
                let status = if complete {
                    Status::Ok
                } else {
                    Status::BadRequest
                };
                let body = if complete { SUCCESS_HTML } else { FAILURE_HTML };
                let _ = write_http(&mut stream, status, body);
                Some(result)
            }
            Err(_) => {
                let _ = write_http(&mut stream, Status::Forbidden, FAILURE_HTML);
                None
            }
        }
    }
}

/// Browsers resolve `localhost` to either stack, so both have to answer.
fn bind_localhost(port: u16) -> io::Result<(Vec<TcpListener>, u16)> {
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let bound = v4.local_addr()?.port();
    let mut listeners = vec![v4];

    match TcpListener::bind((Ipv6Addr::LOCALHOST, bound)) {
        Ok(v6) => listeners.push(v6),
        // Another process owns the v6 half of the port we were told to use, so a browser
        // resolving `localhost` to ::1 would hand our code to them and we would hang.
        Err(e) if e.kind() == io::ErrorKind::AddrInUse && port != 0 => return Err(e),
        // ipv6 can be disabled outright, then v4 is all there is.
        Err(_) => {}
    }

    for listener in &listeners {
        listener.set_nonblocking(true)?;
    }
    Ok((listeners, bound))
}

fn read_request_head(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut request = Vec::with_capacity(READ_CHUNK);
    let mut buffer = [0u8; READ_CHUNK];
    while request.len() < MAX_REQUEST_SIZE {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..size]);
        if request
            .windows(HEADER_TERMINATOR.len())
            .any(|bytes| bytes == HEADER_TERMINATOR)
        {
            break;
        }
    }
    Ok(request)
}

fn write_http(stream: &mut impl Write, status: Status, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        status.line(),
        body.len(),
    )
}

fn request_path(target: &str) -> &str {
    target.split_once('?').map_or(target, |(path, _)| path)
}

fn spawn_paste_reader() -> Receiver<String> {
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

fn parse_callback_target(target: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    parse_callback_query(
        target.split_once('?').map_or("", |(_, query)| query),
        expected_state,
    )
}

fn parse_callback_input(input: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let value = input.trim();
    if value.is_empty() {
        return Err(AgentError::Config {
            message: EMPTY_PASTE_MSG.into(),
        });
    }
    if value.len() >= MIN_RAW_CODE_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AgentError::Config {
            message: RAW_CODE_MSG.into(),
        });
    }
    let query = match value.split_once('?') {
        Some((_, query)) => query,
        None if value.contains('=') => value,
        None => {
            return Err(AgentError::Config {
                message: INCOMPLETE_PASTE_MSG.into(),
            });
        }
    };
    parse_callback_query(query, expected_state)
}

fn parse_callback_query(query: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error_description" => error = Some(value.into_owned()),
            "error" if error.is_none() => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(AgentError::Config {
            message: STATE_MISMATCH.into(),
        });
    }
    Ok(CallbackResult { code, error })
}

pub(crate) fn pkce_pair() -> Result<(String, String), AgentError> {
    let verifier = random_token()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

pub(crate) fn random_token() -> Result<String, AgentError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| AgentError::Config {
        message: format!("CSPRNG unavailable: {e}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) enum LoginMethod {
    Browser,
    Device,
}

/// Browser login needs a browser on this machine and a reachable loopback port.
fn prefer_device() -> bool {
    env::var_os("SSH_CONNECTION").is_some()
        || env::var_os("SSH_CLIENT").is_some()
        || env::var_os("SSH_TTY").is_some()
        || env::var_os("WSL_DISTRO_NAME").is_some()
        || env::var_os("WSL_INTEROP").is_some()
        || env::var_os("container").is_some()
        || env::var_os("KUBERNETES_SERVICE_HOST").is_some()
        || env::var_os("CODESPACES").is_some()
        || env::var_os("REMOTE_CONTAINERS").is_some()
        || env::var_os("DEVCONTAINER").is_some()
        || !io::stdin().is_terminal()
}

pub(crate) fn select_login_method(provider: &str) -> Result<LoginMethod, AgentError> {
    let default_device = prefer_device();
    println!("{provider} login method:");
    if default_device {
        println!("  1. Browser login");
        println!("  2. Device code login (recommended for this session)");
    } else {
        println!("  1. Browser login (default)");
        println!("  2. Device code login (remote/headless)");
    }
    match prompt("Select [1-2]: ")?.as_str() {
        "" if default_device => Ok(LoginMethod::Device),
        "" | "1" | "browser" => Ok(LoginMethod::Browser),
        "2" | "device" => Ok(LoginMethod::Device),
        _ => Err(AgentError::Config {
            message: INVALID_METHOD_MSG.into(),
        }),
    }
}

pub(crate) fn prompt(message: &str) -> Result<String, AgentError> {
    print!("{message}");
    io::stdout().flush().map_err(|e| AgentError::Config {
        message: format!("prompt: {e}"),
    })?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| AgentError::Config {
            message: format!("prompt: {e}"),
        })?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
    use std::thread;

    use test_case::test_case;

    use super::{
        CallbackResult, Duration, Loopback, RAW_CODE_MSG, STATE_MISMATCH, bind_localhost,
        parse_callback_input, parse_callback_query, parse_callback_target, pkce_pair, request_path,
    };

    // RFC 7636 Appendix B: https://www.rfc-editor.org/rfc/rfc7636.html#appendix-B
    const RFC7636_CHALLENGE_LEN: usize = 43;
    const EXPECTED_STATE: &str = "expected";

    fn parse(target: &str) -> CallbackResult {
        parse_callback_target(target, EXPECTED_STATE).unwrap()
    }

    #[test]
    fn callback_requires_matching_state() {
        let error = parse_callback_target("/callback?code=abc&state=other", EXPECTED_STATE)
            .unwrap_err()
            .to_string();
        assert_eq!(error, STATE_MISMATCH);
    }

    #[test]
    fn callback_without_state_is_rejected() {
        let error = parse_callback_target("/callback?code=abc", EXPECTED_STATE)
            .unwrap_err()
            .to_string();
        assert_eq!(error, STATE_MISMATCH);
    }

    #[test]
    fn callback_decodes_code() {
        let result = parse("/callback?code=abc%2Fdef&state=expected");
        assert_eq!(result.code.as_deref(), Some("abc/def"));
        assert!(result.error.is_none());
    }

    #[test]
    fn error_description_wins_over_error() {
        let result =
            parse("/callback?error=access_denied&error_description=Login+cancelled&state=expected");
        assert_eq!(result.error.as_deref(), Some("Login cancelled"));

        let reordered =
            parse("/callback?error_description=Login+cancelled&error=access_denied&state=expected");
        assert_eq!(reordered.error.as_deref(), Some("Login cancelled"));
    }

    #[test]
    fn raw_authorization_codes_are_rejected() {
        let error = parse_callback_input("Abcdefghijklmnopqrstuvwxyz0123", EXPECTED_STATE)
            .unwrap_err()
            .to_string();
        assert_eq!(error, RAW_CODE_MSG);
    }

    #[test]
    fn pasted_redirect_url_is_accepted() {
        let result = parse_callback_input(
            "http://127.0.0.1:56121/callback?code=abc&state=expected",
            EXPECTED_STATE,
        )
        .unwrap();
        assert_eq!(result.code.as_deref(), Some("abc"));
    }

    #[test_case("/callback", "/callback" ; "bare_path")]
    #[test_case("/callback?code=abc", "/callback" ; "strips_query")]
    #[test_case("/favicon.ico", "/favicon.ico" ; "other_path")]
    fn request_path_strips_query(target: &str, expected: &str) {
        assert_eq!(request_path(target), expected);
    }

    #[test]
    fn pkce_challenge_is_url_safe_base64_sha256() {
        let (verifier, challenge) = pkce_pair().unwrap();
        assert_ne!(verifier, challenge);
        assert_eq!(challenge.len(), RFC7636_CHALLENGE_LEN);
        assert!(
            challenge
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn state_mismatch_is_checked_before_query_contents() {
        assert!(parse_callback_query("", EXPECTED_STATE).is_err());
    }

    const TEST_PATH: &str = "/callback";
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);
    const TEST_CODE: &str = "authorization-code";

    /// `None` when the host has no listener, which is a valid setup for ipv6.
    fn callback_over(host: IpAddr) -> Option<CallbackResult> {
        let server = Loopback {
            port: 0,
            fallback_to_ephemeral: false,
            path: TEST_PATH,
            timeout: TEST_TIMEOUT,
            paste_fallback: false,
        }
        .bind()
        .unwrap();
        let port = server.port();
        server
            .listeners
            .iter()
            .find(|l| l.local_addr().is_ok_and(|addr| addr.ip() == host))?;

        let redirect = thread::spawn(move || {
            let mut stream = TcpStream::connect((host, port))?;
            write!(
                stream,
                "GET {TEST_PATH}?code={TEST_CODE}&state={EXPECTED_STATE} HTTP/1.1\r\nhost: localhost\r\n\r\n"
            )?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            std::io::Result::Ok(response)
        });

        let result = server.wait(EXPECTED_STATE).unwrap();
        assert!(
            redirect
                .join()
                .unwrap()
                .unwrap()
                .starts_with("HTTP/1.1 200")
        );
        Some(result)
    }

    #[test]
    fn serves_the_callback_over_ipv4() {
        let result = callback_over(Ipv4Addr::LOCALHOST.into()).unwrap();
        assert_eq!(result.code.as_deref(), Some(TEST_CODE));
    }

    /// `localhost` resolves to `::1` first on some systems, so the v6 socket has to answer too.
    #[test]
    fn serves_the_callback_over_ipv6() {
        if let Some(result) = callback_over(Ipv6Addr::LOCALHOST.into()) {
            assert_eq!(result.code.as_deref(), Some(TEST_CODE));
        }
    }

    #[test]
    fn ephemeral_bind_reports_the_real_port() {
        let (listeners, port) = bind_localhost(0).unwrap();
        assert_ne!(port, 0);
        for listener in &listeners {
            assert_eq!(listener.local_addr().unwrap().port(), port);
        }
    }

    #[test]
    fn a_taken_ipv6_half_fails_instead_of_hanging() {
        let Ok(squatter) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            return;
        };
        let port = squatter.local_addr().unwrap().port();
        assert!(bind_localhost(port).is_err());
    }
}
