use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};
use std::{env, thread};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use isahc::ReadResponseExt;
use isahc::config::{Configurable, VersionNegotiation};
use maki_storage::StateDir;
use maki_storage::auth::{OAuthTokens, delete_tokens, load_tokens, now_millis, save_tokens};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, error, warn};

use crate::AgentError;
use crate::providers::{ResolvedAuth, refreshed_tokens, urlenc};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const PROVIDER: &str = "openai";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const REDIRECT_HOST: &str = "127.0.0.1";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const REDIRECT_PORT: u16 = 1455;
const REDIRECT_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_TIMEOUT: Duration = Duration::from_secs(300);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const MAX_REQUEST_SIZE: usize = 8192;
const CALLBACK_TIMEOUT_MSG: &str = "timed out waiting for OpenAI OAuth callback";
const STATE_MISMATCH: &str = "OpenAI authorization failed: state mismatch";
const SUCCESS_HTML: &str =
    "<html><body><h1>Authentication successful</h1><p>You can close this tab.</p></body></html>";
const ERROR_HTML: &str =
    "<html><body><h1>Authentication failed</h1><p>Return to Maki and try again.</p></body></html>";

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug)]
struct CallbackResult {
    code: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    expires_in: Option<u64>,
}

fn http_client(timeout: Duration) -> Result<isahc::HttpClient, AgentError> {
    isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        // curl carries http2 for OTLP.
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .map_err(|e| AgentError::Config {
            message: format!("http client: {e}"),
        })
}

fn extract_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    claims
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|org| org.get("id"))
                .and_then(|v| v.as_str())
        })
        .map(String::from)
}

fn extract_account_id_from_tokens(resp: &TokenResponse) -> Option<String> {
    if let Some(id_token) = &resp.id_token
        && let Some(id) = extract_account_id(id_token)
    {
        return Some(id);
    }
    extract_account_id(&resp.access_token)
}

fn request_device_code() -> Result<DeviceCodeResponse, AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let body = serde_json::json!({"client_id": CLIENT_ID});
    let json_body = serde_json::to_vec(&body)?;

    let request = isahc::Request::builder()
        .method("POST")
        .uri(DEVICE_CODE_URL)
        .header("content-type", "application/json")
        .body(json_body)?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("device code request: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!("device code request failed: {body_text}"),
        });
    }

    let body_text = resp.text()?;
    serde_json::from_str(&body_text).map_err(Into::into)
}

fn poll_device_token(device: &DeviceCodeResponse) -> Result<DeviceTokenResponse, AgentError> {
    let client = http_client(POLL_TIMEOUT)?;
    let interval_secs = device.interval.parse::<u64>().unwrap_or(5).max(1);
    let poll_interval = Duration::from_secs(interval_secs) + POLL_SAFETY_MARGIN;
    let deadline = std::time::Instant::now() + POLL_TIMEOUT;

    let body = serde_json::json!({
        "device_auth_id": device.device_auth_id,
        "user_code": device.user_code,
    });
    let json_body = serde_json::to_vec(&body)?;

    loop {
        if std::time::Instant::now() > deadline {
            return Err(AgentError::Config {
                message: "device authorization timed out".into(),
            });
        }

        thread::sleep(poll_interval);

        let request = isahc::Request::builder()
            .method("POST")
            .uri(DEVICE_TOKEN_URL)
            .header("content-type", "application/json")
            .body(json_body.clone())?;

        let mut resp = client.send(request).map_err(|e| AgentError::Config {
            message: format!("device token poll: {e}"),
        })?;

        if resp.status().as_u16() == 200 {
            let body_text = resp.text()?;
            return serde_json::from_str(&body_text).map_err(Into::into);
        }

        let status = resp.status().as_u16();
        if status != 403 && status != 404 {
            let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
            return Err(AgentError::Config {
                message: format!("device token poll failed ({status}): {body_text}"),
            });
        }
    }
}

fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;

    let form_body = format!(
        "grant_type=authorization_code\
         &code={}\
         &redirect_uri={}\
         &client_id={}\
         &code_verifier={}",
        urlenc(code),
        urlenc(redirect_uri),
        urlenc(CLIENT_ID),
        urlenc(verifier),
    );

    let request = isahc::Request::builder()
        .method("POST")
        .uri(OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body.into_bytes())?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("token exchange: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!("token exchange failed: {body_text}"),
        });
    }

    let body_text = resp.text()?;
    serde_json::from_str(&body_text).map_err(Into::into)
}

fn authorization_url(challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=maki",
        urlenc(CLIENT_ID),
        urlenc(REDIRECT_URI),
        urlenc(SCOPE),
        urlenc(challenge),
        urlenc(state),
    )
}

fn pkce_pair() -> Result<(String, String), AgentError> {
    let verifier = random_token()?;
    Ok((verifier.clone(), pkce_challenge(&verifier)))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_token() -> Result<String, AgentError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AgentError::Config {
        message: format!("CSPRNG unavailable: {e}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn bind_callback() -> Result<TcpListener, AgentError> {
    let listener =
        TcpListener::bind((REDIRECT_HOST, REDIRECT_PORT)).map_err(|e| AgentError::Config {
            message: format!("OpenAI OAuth callback could not bind localhost:{REDIRECT_PORT}: {e}"),
        })?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<CallbackResult, AgentError> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(AgentError::Config {
                message: CALLBACK_TIMEOUT_MSG.into(),
            });
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let Ok(request) = read_http_headers(&mut stream) else {
                    continue;
                };
                let request = String::from_utf8_lossy(&request);
                let Some(target) = request.split_whitespace().nth(1) else {
                    continue;
                };
                if target.split_once('?').map_or(target, |(path, _)| path) != REDIRECT_PATH {
                    let _ = write_http(&mut stream, 404, "Not found");
                    continue;
                }

                match parse_callback_target(target, expected_state) {
                    Ok(result) => {
                        let success = result.error.is_none() && result.code.is_some();
                        let body = if success { SUCCESS_HTML } else { ERROR_HTML };
                        let status = if success { 200 } else { 400 };
                        let _ = write_http(&mut stream, status, body);
                        return Ok(result);
                    }
                    Err(_) => {
                        let _ = write_http(&mut stream, 403, ERROR_HTML);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) => return Err(e.into()),
        }
    }
}

fn read_http_headers(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    while request.len() < MAX_REQUEST_SIZE {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..size]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
    }
    Ok(request)
}

fn parse_callback_target(target: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
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

fn write_http(stream: &mut impl Write, status: u16, body: &str) -> io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    )
}

fn into_oauth_tokens(resp: TokenResponse) -> OAuthTokens {
    let account_id = extract_account_id_from_tokens(&resp);
    let expires = now_millis() + resp.expires_in.unwrap_or(3600) * 1000;
    OAuthTokens {
        access: resp.access_token,
        refresh: resp.refresh_token,
        expires,
        account_id,
    }
}

pub(crate) fn refresh_tokens(tokens: &OAuthTokens) -> Result<OAuthTokens, AgentError> {
    let expired = tokens.is_expired();
    debug!(expired, "refreshing OpenAI OAuth tokens");

    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let form_body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlenc(&tokens.refresh),
        urlenc(CLIENT_ID),
    );

    let request = isahc::Request::builder()
        .method("POST")
        .uri(OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body.into_bytes())?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("OpenAI token refresh: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!("OpenAI token refresh failed: {body_text}"),
        });
    }

    let body_text = resp.text()?;
    let token_resp: TokenResponse = serde_json::from_str(&body_text)?;
    Ok(into_oauth_tokens(token_resp))
}

pub(crate) const CODING_PLAN_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

pub(crate) fn build_oauth_resolved(tokens: &OAuthTokens) -> Result<ResolvedAuth, AgentError> {
    ResolvedAuth::bearer(PROVIDER, &tokens.access)
}

pub(crate) fn build_coding_plan_resolved(tokens: &OAuthTokens) -> Result<ResolvedAuth, AgentError> {
    let mut headers = vec![("authorization".into(), format!("Bearer {}", tokens.access))];
    if let Some(account_id) = &tokens.account_id {
        headers.push(("chatgpt-account-id".into(), account_id.clone()));
    }
    Ok(ResolvedAuth::new(PROVIDER, headers)?.with_base_url(Some(CODING_PLAN_BASE_URL.into())))
}

pub(crate) fn is_oauth(dir: &StateDir) -> bool {
    load_tokens(dir, PROVIDER).is_some()
}

pub fn resolve(dir: &StateDir) -> Result<ResolvedAuth, AgentError> {
    if let Some(tokens) = load_tokens(dir, PROVIDER) {
        if !tokens.is_expired() {
            debug!("using OpenAI OAuth authentication");
            return build_oauth_resolved(&tokens);
        }
        match refreshed_tokens(dir, PROVIDER, refresh_tokens) {
            Ok(fresh) => {
                debug!("using OpenAI OAuth authentication (refreshed)");
                return build_oauth_resolved(&fresh);
            }
            Err(e) => {
                warn!(error = %e, "OpenAI OAuth refresh failed, clearing stale tokens");
                delete_tokens(dir, PROVIDER).ok();
            }
        }
    }

    if let Ok(key) = env::var("OPENAI_API_KEY") {
        debug!("using OpenAI API key authentication");
        return ResolvedAuth::bearer(PROVIDER, &key);
    }

    if let Some(creds) = maki_storage::auth::load_provider_credentials(dir, PROVIDER) {
        debug!("using OpenAI saved API key");
        return ResolvedAuth::bearer(PROVIDER, &creds.api_key);
    }

    Err(AgentError::Config {
        message: "not authenticated, run `maki auth login openai` or set OPENAI_API_KEY".into(),
    })
}

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    let token_response = match select_login_method()? {
        LoginMethod::Browser => browser_login()?,
        LoginMethod::Device => device_login()?,
    };
    let tokens = into_oauth_tokens(token_response);
    save_tokens(dir, PROVIDER, &tokens)?;
    println!("Authenticated successfully.");
    Ok(())
}

enum LoginMethod {
    Browser,
    Device,
}

fn select_login_method() -> Result<LoginMethod, AgentError> {
    println!("OpenAI login method:");
    println!("  1. Browser login");
    println!("  2. Device code login (default)");
    print!("Select [1-2]: ");
    io::stdout().flush().map_err(|e| AgentError::Config {
        message: format!("prompt: {e}"),
    })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| AgentError::Config {
            message: format!("prompt: {e}"),
        })?;
    match answer.trim() {
        "1" | "browser" => Ok(LoginMethod::Browser),
        "" | "2" | "device" => Ok(LoginMethod::Device),
        _ => Err(AgentError::Config {
            message: "invalid OpenAI login method".into(),
        }),
    }
}

fn device_login() -> Result<TokenResponse, AgentError> {
    let device = request_device_code()?;
    println!("Open this URL in your browser:\n\n  {DEVICE_AUTH_URL}\n");
    println!("Enter code: {}\n", device.user_code);
    println!("Waiting for authorization...");
    let device_token = poll_device_token(&device).map_err(|e| {
        error!(error = %e, "OpenAI device authorization failed");
        e
    })?;
    exchange_authorization_code(
        &device_token.authorization_code,
        &device_token.code_verifier,
        DEVICE_REDIRECT_URI,
    )
}

fn browser_login() -> Result<TokenResponse, AgentError> {
    let (verifier, challenge) = pkce_pair()?;
    let state = random_token()?;
    let listener = bind_callback()?;
    let authorize_url = authorization_url(&challenge, &state);

    println!("Open this URL in your browser:\n\n  {authorize_url}\n");
    if let Err(e) = open::that(&authorize_url) {
        warn!(error = %e, "failed to open browser");
    }
    println!("Waiting for OpenAI OAuth callback on {REDIRECT_URI}...");

    let callback = wait_for_callback(listener, &state).map_err(|e| {
        error!(error = %e, "OpenAI browser authorization failed");
        e
    })?;
    if let Some(error) = callback.error {
        return Err(AgentError::Config {
            message: format!("OpenAI authorization failed: {error}"),
        });
    }
    let code = callback.code.ok_or_else(|| AgentError::Config {
        message: "OpenAI authorization failed: no authorization code returned".into(),
    })?;

    exchange_authorization_code(&code, &verifier, REDIRECT_URI).map_err(|e| {
        error!(error = %e, "OpenAI token exchange failed");
        e
    })
}

pub fn logout(dir: &StateDir) -> Result<(), AgentError> {
    if delete_tokens(dir, PROVIDER)? {
        println!("Logged out of OpenAI.");
    } else {
        println!("Not currently logged in to OpenAI.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7636 Appendix B: https://www.rfc-editor.org/rfc/rfc7636.html#appendix-B
    const RFC7636_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC7636_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn extract_account_id_from_jwt() {
        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({"chatgpt_account_id": "acct_123"})
                .to_string()
                .as_bytes(),
        );
        let token = format!("{header}.{payload}.sig");
        assert_eq!(extract_account_id(&token).as_deref(), Some("acct_123"));

        assert_eq!(extract_account_id("not.a.jwt"), None);
        assert_eq!(extract_account_id("invalid"), None);
    }

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        assert_eq!(pkce_challenge(RFC7636_VERIFIER), RFC7636_CHALLENGE);
    }

    #[test]
    fn callback_requires_matching_state() {
        let error = parse_callback_target(
            "/auth/callback?code=authorization-code&state=wrong",
            "expected",
        )
        .unwrap_err();
        assert_eq!(error.to_string(), STATE_MISMATCH);
    }

    #[test]
    fn callback_decodes_code_and_error_description() {
        let success = parse_callback_target(
            "/auth/callback?code=authorization%2Fcode&state=expected",
            "expected",
        )
        .unwrap();
        assert_eq!(success.code.as_deref(), Some("authorization/code"));
        assert!(success.error.is_none());

        let failure = parse_callback_target(
            "/auth/callback?error=access_denied&error_description=Login+cancelled&state=expected",
            "expected",
        )
        .unwrap();
        assert_eq!(failure.error.as_deref(), Some("Login cancelled"));
    }

    #[test]
    fn authorization_uses_loopback_pkce_flow() {
        let url = authorization_url("challenge", "state");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("code_challenge=challenge&code_challenge_method=S256"));
        assert!(url.contains("state=state"));
        assert!(!url.contains("device"));
    }
}
