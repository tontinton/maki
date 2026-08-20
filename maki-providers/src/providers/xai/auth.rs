use std::io::{self, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::str;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use isahc::ReadResponseExt;
use isahc::config::{Configurable, RedirectPolicy};
use maki_storage::StateDir;
use maki_storage::auth::{OAuthTokens, delete_tokens, load_tokens, now_millis, save_tokens};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, error, warn};

use crate::AgentError;
use crate::providers::{KeyPool, ResolvedAuth, urlenc};

use super::catalog;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_TIMEOUT: Duration = Duration::from_secs(300);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const DEFAULT_EXPIRES_SECS: u64 = 3600;
const GROK_CLI_DEFAULT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
const MS_THRESHOLD: f64 = 10_000_000_000.0;

pub(crate) const PROVIDER: &str = "xai";
pub(crate) const API_KEY_ENV: &str = "XAI_API_KEY";
pub(crate) const TOKEN_AUTH: &str = "xai-grok-cli";
pub(crate) const AUTHENTICATE_RESPONSE: &str = "authenticate-response";
pub(crate) const CLIENT_IDENTIFIER: &str = "maki";
pub(crate) const CLI_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const ISSUER: &str = "https://auth.x.ai";
const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REDIRECT_HOST: &str = "127.0.0.1";
const REDIRECT_PORT: u16 = 56121;
const REDIRECT_PATH: &str = "/callback";
const GROK_AUTH_REL: &str = ".grok/auth.json";
const GROK_SCOPE_PREFIX: &str = "https://auth.x.ai::";
const GROK_LEGACY_SCOPE: &str = "https://accounts.x.ai/sign-in";
const DEVICE_DEFAULT_INTERVAL_SECS: u64 = 5;
const DEVICE_MIN_INTERVAL_SECS: u64 = 1;
const DEVICE_SLOW_DOWN_SECS: u64 = 5;
const MAX_USER_CODE_LEN: usize = 128;
const MAX_DEVICE_CODE_LEN: usize = 4096;
const MAX_VERIFICATION_URI_LEN: usize = 2048;
const MAX_DEVICE_EXPIRY_SECS: u64 = 24 * 60 * 60;

const NOT_AUTHENTICATED: &str = "not authenticated, run `maki auth login xai` or set XAI_API_KEY";
const DEVICE_TIMEOUT: &str = "xAI device authorization timed out";
const DEVICE_DENIED: &str = "xAI device authorization was denied";
const DEVICE_EXPIRED: &str = "xAI device authorization expired; run `maki auth login xai` again";
const CALLBACK_TIMEOUT_MSG: &str = "timed out waiting for xAI OAuth callback";
const STATE_MISMATCH: &str = "xAI authorization failed: state mismatch";
const RAW_CODE_MSG: &str = "raw xAI authorization codes are not accepted; paste the complete redirect URL containing both code and state";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct DeviceTokenError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

fn http_client(timeout: Duration) -> Result<isahc::HttpClient, AgentError> {
    isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .redirect_policy(RedirectPolicy::None)
        .build()
        .map_err(|e| AgentError::Config {
            message: format!("http client: {e}"),
        })
}

fn oauth_form_headers() -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/x-www-form-urlencoded".into()),
        ("accept", "application/json".into()),
        ("user-agent", crate::providers::user_agent().into()),
        ("x-grok-client-version", env!("CARGO_PKG_VERSION").into()),
        ("x-grok-client-surface", "cli".into()),
    ]
}

fn post_form(url: &str, body: &str, timeout: Duration) -> Result<(u16, String), AgentError> {
    if url != TOKEN_URL && url != DEVICE_URL {
        return Err(AgentError::Config {
            message: "refusing to send xAI credentials to an untrusted endpoint".into(),
        });
    }
    let client = http_client(timeout)?;
    let mut builder = isahc::Request::builder().method("POST").uri(url);
    for (key, value) in oauth_form_headers() {
        builder = builder.header(key, value);
    }
    let request = builder.body(body.as_bytes().to_vec())?;
    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("xAI OAuth request: {e}"),
    })?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    Ok((status, text))
}

fn into_oauth_tokens(
    resp: TokenResponse,
    fallback_refresh: Option<String>,
) -> Result<OAuthTokens, AgentError> {
    let refresh = resp
        .refresh_token
        .filter(|s| !s.is_empty())
        .or(fallback_refresh)
        .ok_or_else(|| AgentError::Config {
            message: "xAI token response did not include a refresh token".into(),
        })?;
    let expires = now_millis() + resp.expires_in.unwrap_or(DEFAULT_EXPIRES_SECS) * 1000;
    Ok(OAuthTokens {
        access: resp.access_token,
        refresh,
        expires,
        account_id: None,
    })
}

pub(crate) fn refresh_tokens(tokens: &OAuthTokens) -> Result<OAuthTokens, AgentError> {
    if tokens.refresh.is_empty() {
        return Err(AgentError::Config {
            message: "xAI credentials are expired and do not include a refresh token".into(),
        });
    }
    debug!(expired = tokens.is_expired(), "refreshing xAI OAuth tokens");

    let form_body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlenc(&tokens.refresh),
        urlenc(CLIENT_ID),
    );
    let (status, body_text) = post_form(TOKEN_URL, &form_body, TOKEN_EXCHANGE_TIMEOUT)?;
    if status != 200 {
        return Err(AgentError::Config {
            message: format!("xAI token refresh failed ({status}): {body_text}"),
        });
    }
    let token_resp: TokenResponse = serde_json::from_str(&body_text)?;
    into_oauth_tokens(token_resp, Some(tokens.refresh.clone()))
}

fn oauth_headers(access: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), format!("Bearer {access}")),
        ("x-xai-token-auth".into(), TOKEN_AUTH.into()),
        (
            "x-authenticateresponse".into(),
            AUTHENTICATE_RESPONSE.into(),
        ),
        ("x-grok-client-identifier".into(), CLIENT_IDENTIFIER.into()),
        (
            "x-grok-client-version".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
        ("x-grok-client-mode".into(), client_mode().into()),
    ]
}

fn client_mode() -> &'static str {
    if io::IsTerminal::is_terminal(&io::stdin()) && io::IsTerminal::is_terminal(&io::stdout()) {
        "interactive"
    } else {
        "headless"
    }
}

pub(crate) fn build_oauth_resolved(tokens: &OAuthTokens) -> ResolvedAuth {
    ResolvedAuth {
        base_url: Some(CLI_BASE_URL.into()),
        headers: oauth_headers(&tokens.access),
    }
}

pub(crate) fn is_oauth(dir: &StateDir) -> bool {
    load_tokens(dir, PROVIDER).is_some()
}

pub fn resolve(dir: &StateDir) -> Result<ResolvedAuth, AgentError> {
    if let Some(tokens) = load_tokens(dir, PROVIDER) {
        if !tokens.is_expired() {
            debug!("using xAI OAuth authentication");
            return Ok(build_oauth_resolved(&tokens));
        }
        match refresh_tokens(&tokens) {
            Ok(fresh) => {
                save_tokens(dir, PROVIDER, &fresh)?;
                debug!("using xAI OAuth authentication (refreshed)");
                return Ok(build_oauth_resolved(&fresh));
            }
            Err(e) => {
                warn!(error = %e, "xAI OAuth refresh failed, clearing stale tokens");
                delete_tokens(dir, PROVIDER).ok();
                catalog::invalidate();
            }
        }
    }

    if let Ok(pool) = KeyPool::resolve(PROVIDER, API_KEY_ENV) {
        debug!("using xAI API key authentication");
        return Ok(ResolvedAuth::bearer(pool.current()));
    }

    Err(AgentError::Config {
        message: NOT_AUTHENTICATED.into(),
    })
}

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    if let Some(existing) = grok_cli_credentials() {
        println!("Found official Grok CLI credentials in ~/.grok/auth.json.");
        let answer = prompt("Use them instead of a new xAI OAuth login? [Y/n] ")?;
        if answer.is_empty() || answer.to_ascii_lowercase().starts_with('y') {
            match ensure_fresh(existing) {
                Ok(tokens) => return finish_login(dir, tokens),
                Err(e) => {
                    warn!(error = %e, "existing Grok CLI credentials could not be refreshed");
                    println!(
                        "Existing credentials could not be refreshed. Starting a new login..."
                    );
                }
            }
        }
    }

    let method = select_login_method()?;
    let tokens = match method {
        LoginMethod::Device => device_login()?,
        LoginMethod::Browser => browser_login()?,
    };
    finish_login(dir, tokens)
}

pub fn logout(dir: &StateDir) -> Result<(), AgentError> {
    catalog::invalidate();
    if delete_tokens(dir, PROVIDER)? {
        println!("Logged out of xAI.");
    } else if maki_storage::auth::delete_provider_credentials(dir, PROVIDER)? {
        println!("Removed saved xAI API key.");
    } else {
        println!("Not currently logged in to xAI.");
    }
    Ok(())
}

fn finish_login(dir: &StateDir, tokens: OAuthTokens) -> Result<(), AgentError> {
    save_tokens(dir, PROVIDER, &tokens)?;
    println!("Authenticated successfully.");
    match catalog::refresh(&tokens.access) {
        Ok(models) => {
            println!("Loaded {} models from your xAI catalog.", models.len());
        }
        Err(e) => {
            warn!(error = %e, "xAI catalog refresh failed after login");
            println!(
                "Login succeeded, but the model catalog could not be refreshed; using the curated fallback."
            );
        }
    }
    Ok(())
}

fn ensure_fresh(tokens: OAuthTokens) -> Result<OAuthTokens, AgentError> {
    if !tokens.is_expired() {
        return Ok(tokens);
    }
    refresh_tokens(&tokens)
}

enum LoginMethod {
    Browser,
    Device,
}

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
        || !io::IsTerminal::is_terminal(&io::stdin())
}

fn select_login_method() -> Result<LoginMethod, AgentError> {
    let default_device = prefer_device();
    println!("xAI login method:");
    if default_device {
        println!("  1. Browser login");
        println!("  2. Device code login (recommended for this session)");
    } else {
        println!("  1. Browser login (default)");
        println!("  2. Device code login (remote/headless)");
    }
    let answer = prompt("Select [1-2]: ")?;
    match answer.as_str() {
        "" if default_device => Ok(LoginMethod::Device),
        "" | "1" | "browser" => Ok(LoginMethod::Browser),
        "2" | "device" => Ok(LoginMethod::Device),
        _ => Err(AgentError::Config {
            message: "invalid xAI login method".into(),
        }),
    }
}

fn prompt(message: &str) -> Result<String, AgentError> {
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

fn device_login() -> Result<OAuthTokens, AgentError> {
    let device = request_device_code()?;
    println!(
        "Open this URL in your browser:\n\n  {}\n",
        device.verification_uri
    );
    println!("Enter code: {}\n", device.user_code);
    println!("Waiting for authorization...");
    poll_device_token(&device).map_err(|e| {
        error!(error = %e, "xAI device authorization failed");
        e
    })
}

fn request_device_code() -> Result<DeviceCodeResponse, AgentError> {
    let form_body = format!(
        "client_id={}&scope={}&referrer={}",
        urlenc(CLIENT_ID),
        urlenc(SCOPE),
        urlenc(CLIENT_IDENTIFIER),
    );
    let (status, body_text) = post_form(DEVICE_URL, &form_body, TOKEN_EXCHANGE_TIMEOUT)?;
    if status == 404 {
        return Err(AgentError::Config {
            message: "xAI device authorization is not available; choose browser login".into(),
        });
    }
    if status != 200 {
        return Err(AgentError::Config {
            message: format!("xAI device authorization request failed ({status}): {body_text}"),
        });
    }
    let device: DeviceCodeResponse = serde_json::from_str(&body_text)?;
    validate_device_challenge(&device)?;
    Ok(device)
}

fn validate_device_challenge(device: &DeviceCodeResponse) -> Result<(), AgentError> {
    if device.device_code.is_empty() || device.device_code.len() > MAX_DEVICE_CODE_LEN {
        return Err(AgentError::Config {
            message: "xAI device authorization response had an invalid schema".into(),
        });
    }
    if device.user_code.is_empty()
        || device.user_code.len() > MAX_USER_CODE_LEN
        || !device
            .user_code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AgentError::Config {
            message: "xAI device authorization response had an invalid schema".into(),
        });
    }
    if !valid_verification_uri(&device.verification_uri, &device.device_code) {
        return Err(AgentError::Config {
            message: "xAI device authorization response had an invalid schema".into(),
        });
    }
    Ok(())
}

pub(crate) fn valid_verification_uri(uri: &str, device_code: &str) -> bool {
    if uri.is_empty() || uri.len() > MAX_VERIFICATION_URI_LEN {
        return false;
    }
    let Ok(parsed) = url_origin(uri) else {
        return false;
    };
    if parsed != ISSUER && parsed != "https://accounts.x.ai" {
        return false;
    }
    !uri.contains(device_code)
}

fn url_origin(uri: &str) -> Result<String, ()> {
    let rest = uri.strip_prefix("https://").ok_or(())?;
    let host = rest.split(['/', '?', '#']).next().ok_or(())?;
    if host.is_empty() || host.contains('@') {
        return Err(());
    }
    Ok(format!("https://{host}"))
}

fn poll_device_token(device: &DeviceCodeResponse) -> Result<OAuthTokens, AgentError> {
    let mut interval = Duration::from_secs(
        device
            .interval
            .unwrap_or(DEVICE_DEFAULT_INTERVAL_SECS)
            .max(DEVICE_MIN_INTERVAL_SECS),
    );
    let timeout = Duration::from_secs(
        device
            .expires_in
            .unwrap_or(POLL_TIMEOUT.as_secs())
            .min(MAX_DEVICE_EXPIRY_SECS)
            .min(POLL_TIMEOUT.as_secs()),
    );
    let deadline = Instant::now() + timeout;
    let form_body = format!(
        "grant_type={}&device_code={}&client_id={}",
        urlenc(DEVICE_GRANT),
        urlenc(&device.device_code),
        urlenc(CLIENT_ID),
    );

    loop {
        if Instant::now() >= deadline {
            return Err(AgentError::Config {
                message: DEVICE_EXPIRED.into(),
            });
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));

        let (status, body_text) = post_form(TOKEN_URL, &form_body, TOKEN_EXCHANGE_TIMEOUT)?;
        if status == 200 {
            let token_resp: TokenResponse = serde_json::from_str(&body_text)?;
            return into_oauth_tokens(token_resp, None);
        }

        let parsed: DeviceTokenError =
            serde_json::from_str(&body_text).unwrap_or(DeviceTokenError {
                error: None,
                interval: None,
            });
        match parsed.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = Duration::from_secs(
                    parsed
                        .interval
                        .unwrap_or(interval.as_secs() + DEVICE_SLOW_DOWN_SECS)
                        .max(interval.as_secs() + DEVICE_SLOW_DOWN_SECS),
                );
            }
            Some("access_denied" | "authorization_denied") => {
                return Err(AgentError::Config {
                    message: DEVICE_DENIED.into(),
                });
            }
            Some("expired_token") => {
                return Err(AgentError::Config {
                    message: DEVICE_EXPIRED.into(),
                });
            }
            Some(_) if status == 400 || status == 401 || status == 403 => {
                return Err(AgentError::Config {
                    message: format!("xAI device authorization failed ({status}): {body_text}"),
                });
            }
            _ if status == 408 || status == 429 || status >= 500 => {}
            _ => {
                return Err(AgentError::Config {
                    message: DEVICE_TIMEOUT.into(),
                });
            }
        }
    }
}

fn browser_login() -> Result<OAuthTokens, AgentError> {
    let (verifier, challenge) = pkce_pair()?;
    let state = random_token()?;
    let nonce = random_token()?;
    let listener = bind_callback()?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://{REDIRECT_HOST}:{port}{REDIRECT_PATH}");

    let authorize_url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&nonce={}",
        urlenc(CLIENT_ID),
        urlenc(&redirect_uri),
        urlenc(SCOPE),
        urlenc(&challenge),
        urlenc(&state),
        urlenc(&nonce),
    );

    println!("Open this URL in your browser:\n\n  {authorize_url}\n");
    if let Err(e) = open::that(&authorize_url) {
        warn!(error = %e, "failed to open browser");
    }
    println!("Waiting for xAI OAuth callback on {redirect_uri}...");
    println!("If the redirect cannot reach this process, paste the complete redirect URL below.");

    let callback = wait_for_callback(listener, &state)?;
    if let Some(error) = callback.error {
        return Err(AgentError::Config {
            message: format!("xAI authorization failed: {error}"),
        });
    }
    let code = callback.code.ok_or_else(|| AgentError::Config {
        message: "xAI authorization failed: no authorization code returned".into(),
    })?;

    let form_body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlenc(&code),
        urlenc(&redirect_uri),
        urlenc(CLIENT_ID),
        urlenc(&verifier),
    );
    let (status, body_text) = post_form(TOKEN_URL, &form_body, TOKEN_EXCHANGE_TIMEOUT)?;
    if status != 200 {
        return Err(AgentError::Config {
            message: format!("xAI token exchange failed ({status}): {body_text}"),
        });
    }
    let token_resp: TokenResponse = serde_json::from_str(&body_text)?;
    into_oauth_tokens(token_resp, None)
}

#[derive(Debug)]
struct CallbackResult {
    code: Option<String>,
    error: Option<String>,
}

fn bind_callback() -> Result<TcpListener, AgentError> {
    TcpListener::bind((REDIRECT_HOST, REDIRECT_PORT))
        .or_else(|_| TcpListener::bind((REDIRECT_HOST, 0)))
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            Ok(listener)
        })
        .map_err(|e| AgentError::Config {
            message: format!("xAI OAuth callback server: {e}"),
        })
}

fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<CallbackResult, AgentError> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let paste_rx = spawn_paste_reader();
    loop {
        if Instant::now() >= deadline {
            return Err(AgentError::Config {
                message: CALLBACK_TIMEOUT_MSG.into(),
            });
        }
        if let Ok(pasted) = paste_rx.try_recv() {
            return parse_callback_input(&pasted, expected_state);
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 4096];
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let Some(target) = request.split_whitespace().nth(1) else {
                    continue;
                };
                if !target.starts_with(REDIRECT_PATH) {
                    let _ = write_http(&mut stream, 404, "text/plain; charset=utf-8", "Not found");
                    continue;
                }
                match parse_callback_target(target, expected_state) {
                    Ok(result) => {
                        let html = if result.error.is_some() {
                            "<html><body><h1>xAI authorization failed.</h1>You can close this tab.</body></html>"
                        } else {
                            "<html><body><h1>xAI authorization received.</h1>You can close this tab.</body></html>"
                        };
                        let _ = write_http(&mut stream, 200, "text/html; charset=utf-8", html);
                        return Ok(result);
                    }
                    Err(_) => {
                        let _ = write_http(
                            &mut stream,
                            400,
                            "text/html; charset=utf-8",
                            "<html><body><h1>xAI authorization state mismatch.</h1>Please return to maki and try again.</body></html>",
                        );
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                return Err(AgentError::Config {
                    message: format!("xAI OAuth callback: {e}"),
                });
            }
        }
    }
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

fn write_http(
    stream: &mut impl Write,
    status: u16,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        if status == 200 {
            "OK"
        } else if status == 404 {
            "Not Found"
        } else {
            "Bad Request"
        },
        body.len(),
    )
}

fn parse_callback_target(target: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    parse_callback_query(query, expected_state)
}

fn parse_callback_input(input: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let value = input.trim();
    if value.is_empty() {
        return Err(AgentError::Config {
            message: "empty xAI OAuth callback".into(),
        });
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && value.len() >= 20
    {
        return Err(AgentError::Config {
            message: RAW_CODE_MSG.into(),
        });
    }
    let query = if let Some(idx) = value.find('?') {
        &value[idx + 1..]
    } else if value.contains('=') {
        value
    } else {
        return Err(AgentError::Config {
            message: "ignored pasted xAI OAuth input because it was not a complete redirect URL"
                .into(),
        });
    };
    parse_callback_query(query, expected_state)
}

fn parse_callback_query(query: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded = percent_decode(value);
        match key {
            "code" => code = Some(decoded),
            "state" => state = Some(decoded),
            "error" => error = Some(decoded),
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

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3).and_then(|b| str::from_utf8(b).ok())
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            out.push(value);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pkce_pair() -> Result<(String, String), AgentError> {
    let verifier = random_token()?;
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    Ok((verifier, challenge))
}

fn random_token() -> Result<String, AgentError> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| AgentError::Config {
        message: format!("CSPRNG unavailable: {e}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub(crate) fn grok_cli_credentials() -> Option<OAuthTokens> {
    let path = grok_auth_path()?;
    let data: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    parse_grok_auth(&data)
}

fn grok_auth_path() -> Option<PathBuf> {
    Some(maki_storage::paths::home()?.join(GROK_AUTH_REL))
}

pub(crate) fn parse_grok_auth(data: &serde_json::Value) -> Option<OAuthTokens> {
    let scoped_key = format!("{GROK_SCOPE_PREFIX}{CLIENT_ID}");
    if let Some(oidc) = data.get(&scoped_key).and_then(|v| v.as_object())
        && let Some(tokens) = tokens_from_grok_object(oidc)
    {
        return Some(tokens);
    }
    if let Some(legacy) = data.get(GROK_LEGACY_SCOPE).and_then(|v| v.as_object())
        && let Some(access) = first_string_field(legacy, &["key", "access_token", "token"])
    {
        return Some(OAuthTokens {
            access,
            refresh: String::new(),
            expires: now_millis() + GROK_CLI_DEFAULT_TTL_MS,
            account_id: None,
        });
    }
    if let Some(access) = data
        .get("access_token")
        .or_else(|| data.get("token"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(OAuthTokens {
            access: access.to_string(),
            refresh: data
                .get("refresh_token")
                .or_else(|| data.get("refresh"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            expires: parse_expiry(data.get("expires_at").or_else(|| data.get("expires")))
                .unwrap_or_else(|| now_millis() + GROK_CLI_DEFAULT_TTL_MS),
            account_id: None,
        });
    }
    None
}

fn tokens_from_grok_object(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<OAuthTokens> {
    let access = first_string_field(obj, &["key", "access_token", "token"])?;
    Some(OAuthTokens {
        access,
        refresh: first_string_field(obj, &["refresh_token", "refresh"]).unwrap_or_default(),
        expires: parse_expiry(obj.get("expires_at"))
            .unwrap_or_else(|| now_millis() + GROK_CLI_DEFAULT_TTL_MS),
        account_id: None,
    })
}

fn first_string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_expiry(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    if let Some(n) = value.as_f64() {
        return Some(normalize_epoch_ms(n));
    }
    let s = value.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<f64>() {
        return Some(normalize_epoch_ms(n));
    }
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(|ts| u64::try_from(ts.as_millisecond()).unwrap_or(0))
}

fn normalize_epoch_ms(value: f64) -> u64 {
    if value > MS_THRESHOLD {
        value as u64
    } else {
        (value * 1000.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn parse_grok_oidc_scope() {
        let data = serde_json::json!({
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                "key": "access-1",
                "refresh_token": "refresh-1",
                "expires_at": 9_999_999_999_000u64
            }
        });
        let tokens = parse_grok_auth(&data).unwrap();
        assert_eq!(tokens.access, "access-1");
        assert_eq!(tokens.refresh, "refresh-1");
        assert_eq!(tokens.expires, 9_999_999_999_000);
    }

    #[test]
    fn parse_grok_legacy_scope() {
        let data = serde_json::json!({
            "https://accounts.x.ai/sign-in": { "key": "legacy-access" }
        });
        let tokens = parse_grok_auth(&data).unwrap();
        assert_eq!(tokens.access, "legacy-access");
        assert!(tokens.refresh.is_empty());
    }

    #[test]
    fn parse_grok_top_level_tokens() {
        let data = serde_json::json!({
            "access_token": "top-access",
            "refresh_token": "top-refresh",
            "expires": "2000000000"
        });
        let tokens = parse_grok_auth(&data).unwrap();
        assert_eq!(tokens.access, "top-access");
        assert_eq!(tokens.refresh, "top-refresh");
        assert_eq!(tokens.expires, 2_000_000_000_000);
    }

    #[test_case("https://auth.x.ai/device", "secret-code", true)]
    #[test_case("https://accounts.x.ai/device", "secret-code", true)]
    #[test_case("http://auth.x.ai/device", "secret-code", false)]
    #[test_case("https://evil.example/device", "secret-code", false)]
    #[test_case("https://auth.x.ai/device?code=secret-code", "secret-code", false)]
    fn verification_uri_validation(uri: &str, secret: &str, expected: bool) {
        assert_eq!(valid_verification_uri(uri, secret), expected);
    }

    #[test]
    fn callback_query_requires_matching_state() {
        let err = parse_callback_query("code=abc&state=other", "expected").unwrap_err();
        assert_eq!(err.to_string(), STATE_MISMATCH);
    }

    #[test]
    fn callback_query_accepts_code_and_state() {
        let result = parse_callback_query("code=abc%2Fdef&state=expected", "expected").unwrap();
        assert_eq!(result.code.as_deref(), Some("abc/def"));
        assert!(result.error.is_none());
    }

    #[test_case("a%20b", "a b" ; "decodes_percent_escape")]
    #[test_case("%41", "A" ; "decodes_escape_at_end")]
    #[test_case("%\u{20ac}", "%\u{20ac}" ; "keeps_multibyte_after_percent")]
    #[test_case("a+b", "a b" ; "decodes_plus_as_space")]
    fn percent_decode_handles_edge_cases(input: &str, expected: &str) {
        assert_eq!(percent_decode(input), expected);
    }

    #[test]
    fn raw_authorization_codes_are_rejected() {
        let err = parse_callback_input("Abcdefghijklmnopqrstuvwxyz0123", "state").unwrap_err();
        assert_eq!(err.to_string(), RAW_CODE_MSG);
    }

    #[test]
    fn into_oauth_tokens_requires_refresh() {
        let err = into_oauth_tokens(
            TokenResponse {
                access_token: "a".into(),
                refresh_token: None,
                expires_in: Some(60),
            },
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refresh token"));
    }
}
