//! Browser login for Command Code.
//!
//! Despite being presented as OAuth by the Command Code CLI, this is not a
//! token exchange: the studio mints a long-lived API key and hands it back, so
//! there is no authorization code, no refresh, and nothing to expire. maki
//! therefore stores the result as an ordinary provider credential rather than
//! wrapping it in an OAuth envelope with a fabricated expiry.
//!
//! The handshake: bind a one-shot loopback server, open
//! `/studio/auth/cli?callback=…&state=…`, and wait for the studio to POST
//! `{apiKey, state, …}` back. A browser that cannot reach loopback (the studio
//! then offers "copy your API key" instead) falls through to a paste prompt.

use std::io::{self, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use maki_storage::StateDir;
use maki_storage::auth::{ProviderCredentials, save_provider_credentials};
use serde::Deserialize;
use tracing::warn;

use crate::AgentError;

use super::PLAN_SLUG;

const STUDIO_BASE_URL: &str = "https://commandcode.ai";
const CALLBACK_HOST: &str = "127.0.0.1";
/// The port the studio's allow-list expects, with the same small search range
/// the reference client uses before falling back to an ephemeral port.
const CALLBACK_PORT: u16 = 5959;
const CALLBACK_PORT_RANGE: u16 = 10;
const CALLBACK_PATH: &str = "/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// The studio posts a handful of short fields; anything larger is not ours.
const MAX_BODY: usize = 10_000;

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    let key = match bind_callback() {
        Ok(listener) => browser_login(listener)?,
        Err(e) => {
            warn!(error = %e, "could not start the Command Code callback server");
            prompt_api_key("Could not start browser auth. Paste your Command Code API key:")?
        }
    };

    save_provider_credentials(
        dir,
        PLAN_SLUG,
        &ProviderCredentials {
            api_key: key,
            host: None,
        },
    )
    .map_err(|e| AgentError::Config {
        message: format!("save Command Code credentials: {e}"),
    })?;

    println!("\n  \x1b[32m✓\x1b[0m Command Code authenticated");
    Ok(())
}

fn browser_login(listener: TcpListener) -> Result<String, AgentError> {
    let state = random_token()?;
    let port = listener.local_addr()?.port();
    let callback_url = format!("http://localhost:{port}{CALLBACK_PATH}");
    let auth_url = format!(
        "{STUDIO_BASE_URL}/studio/auth/cli?callback={}&state={}",
        super::super::urlenc(&callback_url),
        super::super::urlenc(&state),
    );

    println!("Open this URL in your browser:\n\n  {auth_url}\n");
    if let Err(e) = open::that(&auth_url) {
        warn!(error = %e, "failed to open browser");
    }
    println!("Waiting for Command Code to send the key back to {callback_url}...");
    println!("If it cannot reach this process, paste your API key below instead.");

    wait_for_callback(listener, &state)
}

fn bind_callback() -> Result<TcpListener, AgentError> {
    (0..CALLBACK_PORT_RANGE)
        .map(|offset| CALLBACK_PORT + offset)
        .chain(std::iter::once(0))
        .find_map(|port| TcpListener::bind((CALLBACK_HOST, port)).ok())
        .ok_or_else(|| AgentError::Config {
            message: "could not bind a loopback port for the Command Code callback".into(),
        })
        .and_then(|listener| {
            listener
                .set_nonblocking(true)
                .map_err(|e| AgentError::Config {
                    message: format!("Command Code callback server: {e}"),
                })?;
            Ok(listener)
        })
}

/// The studio mixes casings on the wire — `apiKey` but `error_description` —
/// so each field is renamed explicitly rather than with a blanket rule.
#[derive(Deserialize)]
struct Callback {
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

impl Callback {
    fn parse(body: &str) -> Option<Self> {
        serde_json::from_str(body).ok()
    }
}

fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String, AgentError> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let paste_rx = spawn_paste_reader();
    loop {
        if Instant::now() >= deadline {
            return Err(AgentError::Config {
                message: "Command Code browser login timed out".into(),
            });
        }
        if let Ok(pasted) = paste_rx.try_recv() {
            let key = sanitize_api_key(&pasted);
            if !key.is_empty() {
                return Ok(key);
            }
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                match handle_request(&mut stream, expected_state) {
                    Ok(Some(key)) => return Ok(key),
                    Ok(None) => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) => {
                return Err(AgentError::Config {
                    message: format!("Command Code callback server: {e}"),
                });
            }
        }
    }
}

/// `Ok(None)` means the exchange is not finished: a preflight, a stray probe,
/// or a body that did not carry a usable key.
fn handle_request(
    stream: &mut std::net::TcpStream,
    expected_state: &str,
) -> Result<Option<String>, AgentError> {
    let Some((head, body)) = read_request(stream) else {
        return Ok(None);
    };
    let mut parts = head.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };

    // The studio is an HTTPS page posting to a loopback HTTP server, so Chrome
    // sends a Private Network Access preflight that has to be answered.
    if method == "OPTIONS" {
        let _ = write_http(stream, 204, "", "");
        return Ok(None);
    }
    if !target.starts_with(CALLBACK_PATH) {
        let _ = write_http(stream, 404, "application/json", r#"{"success":false}"#);
        return Ok(None);
    }
    if method != "POST" {
        let _ = write_http(stream, 405, "application/json", r#"{"success":false}"#);
        return Ok(None);
    }

    let Some(callback) = Callback::parse(&body) else {
        let _ = write_http(stream, 400, "application/json", r#"{"success":false}"#);
        return Ok(None);
    };

    if let Some(error) = callback.error {
        let _ = write_http(stream, 200, "application/json", r#"{"success":true}"#);
        let detail = callback.error_description.unwrap_or(error);
        return Err(AgentError::Config {
            message: format!("Command Code authorization failed: {detail}"),
        });
    }

    // Reject a key that arrives without the state we minted: a page we did not
    // open must not be able to plant a credential on this machine.
    if callback.state.as_deref() != Some(expected_state) {
        let _ = write_http(stream, 400, "application/json", r#"{"success":false}"#);
        return Err(AgentError::Config {
            message: "Command Code state token mismatch, the login was not the one maki started"
                .into(),
        });
    }

    let key = callback
        .api_key
        .map(|k| sanitize_api_key(&k))
        .unwrap_or_default();
    if key.is_empty() {
        let _ = write_http(stream, 400, "application/json", r#"{"success":false}"#);
        return Ok(None);
    }
    let _ = write_http(stream, 200, "application/json", r#"{"success":true}"#);
    Ok(Some(key))
}

/// Returns the request line and the body, reading exactly the advertised
/// `content-length` so a POST is not truncated by a short first read.
fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        if let Some(at) = find_header_end(&buf) {
            break at;
        }
        if buf.len() > MAX_BODY {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let want = content_length(&head).unwrap_or(0).min(MAX_BODY);
    let body_start = header_end + 4;
    while buf.len() < body_start + want {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = String::from_utf8_lossy(&buf[body_start..]).to_string();
    let request_line = head.lines().next().unwrap_or("").to_string();
    Some((request_line, body))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.trim().parse().ok())
}

fn write_http(
    stream: &mut impl Write,
    status: u16,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Request",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\n")?;
    // The studio page is a different origin than the loopback callback, and
    // Chrome additionally gates HTTPS-to-loopback behind Private Network Access.
    write!(
        stream,
        "access-control-allow-origin: {STUDIO_BASE_URL}\r\n\
         access-control-allow-methods: POST, OPTIONS\r\n\
         access-control-allow-headers: content-type\r\n\
         access-control-allow-private-network: true\r\n"
    )?;
    if !content_type.is_empty() {
        write!(stream, "content-type: {content_type}\r\n")?;
    }
    write!(
        stream,
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

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

fn prompt_api_key(message: &str) -> Result<String, AgentError> {
    println!("{message}");
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| AgentError::Config {
            message: format!("read API key: {e}"),
        })?;
    let key = sanitize_api_key(&line);
    if key.is_empty() {
        return Err(AgentError::Config {
            message: "no Command Code API key provided".into(),
        });
    }
    Ok(key)
}

/// Terminals wrap a paste in bracketed-paste markers and users bring along
/// stray control characters; a key that carries either fails auth confusingly.
fn sanitize_api_key(input: &str) -> String {
    input
        .replace("\u{1b}[200~", "")
        .replace("\u{1b}[201~", "")
        .replace("[200~", "")
        .replace("[201~", "")
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .collect::<String>()
        .trim()
        .to_string()
}

fn random_token() -> Result<String, AgentError> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| AgentError::Config {
        message: format!("CSPRNG unavailable: {e}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_bracketed_paste_and_control_bytes() {
        assert_eq!(
            sanitize_api_key("\u{1b}[200~cc-abc123\u{1b}[201~\n"),
            "cc-abc123"
        );
        assert_eq!(sanitize_api_key("[200~cc-abc123[201~"), "cc-abc123");
        assert_eq!(sanitize_api_key("  cc-key  "), "cc-key");
        assert_eq!(sanitize_api_key("\n\t "), "");
    }

    #[test]
    fn content_length_is_case_insensitive() {
        assert_eq!(
            content_length("POST /callback HTTP/1.1\r\nContent-Length: 42"),
            Some(42)
        );
        assert_eq!(
            content_length("POST /callback HTTP/1.1\r\ncontent-length:  7 "),
            Some(7)
        );
        assert_eq!(content_length("POST /callback HTTP/1.1"), None);
    }

    #[test]
    fn callback_parses_the_studio_camel_case_payload() {
        let c = Callback::parse(
            r#"{"apiKey":"cc-1","state":"s1","userId":"u","userName":"n","keyName":"k"}"#,
        )
        .unwrap();
        assert_eq!(c.api_key.as_deref(), Some("cc-1"));
        assert_eq!(c.state.as_deref(), Some("s1"));
        assert!(c.error.is_none());

        let denied =
            Callback::parse(r#"{"error":"access_denied","error_description":"user said no"}"#)
                .unwrap();
        assert_eq!(denied.error.as_deref(), Some("access_denied"));
        assert_eq!(denied.error_description.as_deref(), Some("user said no"));

        assert!(Callback::parse("not json").is_none());
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
}
