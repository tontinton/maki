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

use std::io;
use std::time::Duration;

use maki_storage::StateDir;
use maki_storage::auth::{ProviderCredentials, save_provider_credentials};
use serde::Deserialize;
use tracing::warn;

use crate::AgentError;

use super::super::{CallbackRequest, LoopbackCallback, Reply, random_token, urlenc};
use super::PLAN_SLUG;

const STUDIO_BASE_URL: &str = "https://commandcode.ai";
/// The port the studio's allow-list expects, with the same small search range
/// the reference client uses before falling back to an ephemeral port.
const CALLBACK_PORT: u16 = 5959;
const CALLBACK_PORT_RANGE: u16 = 10;
const CALLBACK_PATH: &str = "/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
/// The studio's keys are long opaque tokens; nothing near this short is one.
const MIN_API_KEY_LEN: usize = 32;

const OK: &str = r#"{"success":true}"#;
const NOT_OK: &str = r#"{"success":false}"#;
const JSON: &str = "application/json";

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    let ports = (0..CALLBACK_PORT_RANGE).map(|offset| CALLBACK_PORT + offset);
    // The studio is an HTTPS page posting to this loopback server with `fetch`,
    // so the CORS and Private Network Access headers are not optional here.
    let key = match LoopbackCallback::bind("Command Code", ports, Some(STUDIO_BASE_URL)) {
        Ok(callback) => browser_login(&callback)?,
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

fn browser_login(callback: &LoopbackCallback) -> Result<String, AgentError> {
    let state = random_token()?;
    let callback_url = format!("http://localhost:{}{CALLBACK_PATH}", callback.port()?);
    let auth_url = format!(
        "{STUDIO_BASE_URL}/studio/auth/cli?callback={}&state={}",
        urlenc(&callback_url),
        urlenc(&state),
    );

    println!("Open this URL in your browser:\n\n  {auth_url}\n");
    if let Err(e) = open::that(&auth_url) {
        warn!(error = %e, "failed to open browser");
    }
    println!("Waiting for Command Code to send the key back to {callback_url}...");
    println!("If it cannot reach this process, paste your API key below instead.");

    callback.wait(
        CALLBACK_TIMEOUT,
        "Command Code browser login timed out",
        |request, reply| handle_request(request, reply, &state),
        |pasted| {
            // Anything typed at this prompt that is not key-shaped is a stray
            // keypress, not a paste: saving it would report success and only
            // fail on the next request.
            let key = sanitize_api_key(pasted);
            if looks_like_api_key(&key) {
                return Ok(Some(key));
            }
            println!("That does not look like a Command Code API key, still waiting...");
            Ok(None)
        },
    )
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

/// `Ok(None)` means the exchange is not finished: a preflight, a stray probe,
/// or a body that did not carry a usable key.
fn handle_request(
    request: &CallbackRequest,
    reply: &mut Reply,
    expected_state: &str,
) -> Result<Option<String>, AgentError> {
    // The studio is an HTTPS page posting to a loopback HTTP server, so Chrome
    // sends a Private Network Access preflight that has to be answered.
    if request.method == "OPTIONS" {
        reply.send(204, "", "");
        return Ok(None);
    }
    if !request.target.starts_with(CALLBACK_PATH) {
        reply.send(404, JSON, NOT_OK);
        return Ok(None);
    }
    if request.method != "POST" {
        reply.send(405, JSON, NOT_OK);
        return Ok(None);
    }

    let Some(callback) = Callback::parse(&request.body) else {
        reply.send(400, JSON, NOT_OK);
        return Ok(None);
    };

    if let Some(error) = callback.error {
        reply.send(200, JSON, OK);
        let detail = callback.error_description.unwrap_or(error);
        return Err(AgentError::Config {
            message: format!("Command Code authorization failed: {detail}"),
        });
    }

    // Reject a key that arrives without the state we minted: a page we did not
    // open must not be able to plant a credential on this machine.
    if callback.state.as_deref() != Some(expected_state) {
        reply.send(400, JSON, NOT_OK);
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
        reply.send(400, JSON, NOT_OK);
        return Ok(None);
    }
    reply.send(200, JSON, OK);
    Ok(Some(key))
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
    if !looks_like_api_key(&key) {
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

/// Shape only, and deliberately loose: this rejects a typo, not a forgery. The
/// key that arrives over the state-checked callback is not put through it, so
/// a change to the studio's key format cannot break the browser flow.
fn looks_like_api_key(key: &str) -> bool {
    key.len() >= MIN_API_KEY_LEN
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
    fn only_key_shaped_input_is_accepted_from_a_paste() {
        // A real studio key: long, opaque, url-safe.
        assert!(looks_like_api_key(
            "user_2abcDEF456ghiJKL789mnoPQR012stuVWX345yz"
        ));
        // A stray keypress or a fat-fingered line, which is what the waiting
        // prompt actually collects when the browser flow is still running.
        assert!(!looks_like_api_key(""));
        assert!(!looks_like_api_key("y"));
        assert!(!looks_like_api_key("yes please"));
        // Long enough, but a URL is not a key.
        assert!(!looks_like_api_key(
            "https://commandcode.ai/studio/keys/mine"
        ));
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
}
