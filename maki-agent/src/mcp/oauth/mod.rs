pub mod callback;
pub mod discovery;
pub mod manual;
pub mod pkce;
pub mod registration;
pub mod token;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_lite::future;
use isahc::HttpClient;
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use maki_storage::StateDir;
use maki_storage::auth::{McpAuthData, load_mcp_auth, save_mcp_auth};
use tracing::{info, warn};
use url::Url;

use self::callback::{CallbackResult, CallbackServer};
use self::discovery::parse_www_authenticate;
use super::config::OauthClientConfig;
use super::error::McpError;

const AUTH_TIMEOUT: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// In-band refresh blocks requests waiting on the transport's auth lock, so it
/// gets a much tighter budget than the interactive flow.
const SILENT_REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("network error: {0}")]
    Network(String),
    #[error("server rejected request: HTTP {status} {body}")]
    ServerRejected { status: u16, body: String },
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Copy)]
pub enum Interaction {
    Cli,
    Background,
}

pub async fn authenticate(
    server_name: &str,
    server_url: &str,
    www_authenticate: Option<&str>,
    storage: &StateDir,
    interaction: Interaction,
    static_client: Option<OauthClientConfig>,
) -> Result<McpAuthData, McpError> {
    let wrap = |e: OAuthError| McpError::OAuthFailed {
        server: server_name.into(),
        reason: e.to_string(),
    };
    let client =
        build_http_client(HTTP_TIMEOUT).map_err(|e| wrap(OAuthError::Other(e.to_string())))?;

    if let Some(existing) = load_mcp_auth(storage, server_name, server_url)
        && let Some(ref tokens) = existing.tokens
        && !tokens.is_expired()
    {
        return Ok(existing);
    }

    match silent_refresh(storage, server_name, server_url).await {
        Ok(Some(data)) => return Ok(data),
        Ok(None) => {}
        Err(e) => {
            warn!(server = server_name, error = %e, "token refresh failed, starting full flow");
        }
    }

    let www_auth = www_authenticate.and_then(parse_www_authenticate);

    let resource_meta =
        discovery::discover_resource_metadata(&client, server_url, www_auth.as_ref())
            .await
            .map_err(&wrap)?;

    let auth_server_url = resource_meta
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| discovery::server_origin(server_url));

    let auth_server = discovery::discover_auth_server(&client, &auth_server_url)
        .await
        .map_err(&wrap)?;

    if !auth_server.code_challenge_methods_supported.is_empty()
        && !auth_server
            .code_challenge_methods_supported
            .iter()
            .any(|m| m == "S256")
    {
        return Err(wrap(OAuthError::Other(
            "server does not support S256 PKCE".into(),
        )));
    }

    let callback = CallbackServer::bind(
        static_client.as_ref().and_then(|c| c.callback_port),
        static_client
            .as_ref()
            .and_then(|c| c.callback_path.as_deref()),
        static_client
            .as_ref()
            .and_then(|c| c.callback_hostname.as_deref()),
    )
    .await
    .map_err(|e| wrap(OAuthError::Other(e)))?;
    let redirect_uri = callback.redirect_uri();

    let reg = if let Some(c) = static_client {
        registration::ClientRegistration {
            client_id: c.client_id,
            client_secret: c.client_secret,
            client_secret_expires_at: None,
        }
    } else if let Some(existing) = load_mcp_auth(storage, server_name, server_url)
        && existing.redirect_uri.as_deref() == Some(&redirect_uri)
    {
        registration::ClientRegistration {
            client_id: existing.client_id,
            client_secret: existing.client_secret,
            client_secret_expires_at: existing.client_secret_expires_at,
        }
    } else if let Some(endpoint) = &auth_server.registration_endpoint {
        registration::register_client(&client, endpoint, &redirect_uri)
            .await
            .map_err(&wrap)?
    } else {
        return Err(wrap(OAuthError::Other(
            "no stored client and server has no registration endpoint".into(),
        )));
    };

    let pkce = pkce::generate().map_err(&wrap)?;

    let mut state_buf = [0u8; 16];
    getrandom::fill(&mut state_buf)
        .map_err(|e| wrap(OAuthError::Other(format!("CSPRNG unavailable: {e}"))))?;
    let state = URL_SAFE_NO_PAD.encode(state_buf);

    let scope = www_auth
        .as_ref()
        .and_then(|w| w.scope.clone())
        .or_else(|| resource_meta.scopes_supported.as_ref().map(|s| s.join(" ")));

    let auth_url = build_authorization_url(
        &auth_server.authorization_endpoint,
        &reg.client_id,
        &redirect_uri,
        &state,
        &pkce.challenge,
        scope.as_deref(),
        server_url,
    )
    .map_err(&wrap)?;

    info!(server = server_name, endpoint = %auth_server.authorization_endpoint, "starting OAuth authorization");
    let result = match interaction {
        Interaction::Cli => {
            eprintln!("\nOpen this URL in your browser:\n\n  {auth_url}\n");

            if is_headless() {
                info!(
                    server = server_name,
                    "no display detected, skipping browser open"
                );
            } else if let Err(e) = open::that(&auth_url) {
                warn!(server = server_name, error = %e, "failed to open browser");
            }

            eprintln!("Waiting for callback on {redirect_uri}...");
            eprintln!("If this machine has no browser, log in on another device and paste");
            eprintln!("the full redirect URL ({redirect_uri}?...) here:");

            let callback_or_paste = future::race(
                callback.wait_for_callback(&state),
                manual::wait_for_paste(&state),
            );

            future::race(callback_or_paste, auth_timeout()).await
        }
        Interaction::Background => {
            let cause = if is_headless() {
                Some("no display to open a browser".to_string())
            } else {
                open::that(&auth_url)
                    .err()
                    .map(|e| format!("failed to open browser: {e}"))
            };
            match cause {
                Some(cause) => Err(format!("{cause}; run 'maki mcp auth {server_name}'")),
                None => future::race(callback.wait_for_callback(&state), auth_timeout()).await,
            }
        }
    }
    .map_err(|e| wrap(OAuthError::Other(e)))?;

    let tokens = token::exchange_code(
        &client,
        &auth_server.token_endpoint,
        &result.code,
        &redirect_uri,
        &pkce.verifier,
        &reg.client_id,
        reg.client_secret.as_deref(),
        server_url,
    )
    .await
    .map_err(&wrap)?;

    let data = McpAuthData {
        server_url: server_url.to_string(),
        tokens: Some(tokens),
        client_id: reg.client_id,
        client_secret: reg.client_secret,
        client_secret_expires_at: reg.client_secret_expires_at,
        redirect_uri: Some(redirect_uri),
        token_endpoint: Some(auth_server.token_endpoint.clone()),
    };

    save_mcp_auth(storage, server_name, &data)
        .map_err(|e| wrap(OAuthError::Other(e.to_string())))?;
    info!(server = server_name, "OAuth authentication complete");
    Ok(data)
}

/// Refresh stored tokens without any user interaction. `Ok(None)` means an
/// interactive flow is required (no stored auth or no refresh token).
pub async fn silent_refresh(
    storage: &StateDir,
    server_name: &str,
    server_url: &str,
) -> Result<Option<McpAuthData>, OAuthError> {
    let Some(existing) = load_mcp_auth(storage, server_name, server_url) else {
        return Ok(None);
    };

    let Some(ref tokens) = existing.tokens else {
        return Ok(None);
    };

    if tokens.refresh.is_empty() {
        return Ok(None);
    }

    let client = build_http_client(SILENT_REFRESH_HTTP_TIMEOUT)
        .map_err(|e| OAuthError::Other(e.to_string()))?;

    // Trust the endpoint pinned at interactive auth over fresh discovery: a
    // later-compromised server must not redirect the refresh token (and any
    // static client secret) elsewhere. Pre-pin records fall back to discovery.
    let token_endpoint = match existing.token_endpoint.clone() {
        Some(pinned) => pinned,
        None => {
            discover_auth_server_for(&client, server_url, None)
                .await?
                .token_endpoint
        }
    };

    let new_tokens = token::refresh_token(
        &client,
        &token_endpoint,
        &tokens.refresh,
        &existing.client_id,
        existing.client_secret.as_deref(),
        server_url,
    )
    .await?;

    let data = McpAuthData {
        tokens: Some(new_tokens),
        ..existing
    };

    save_mcp_auth(storage, server_name, &data).map_err(|e| OAuthError::Other(e.to_string()))?;
    info!(server = server_name, "MCP OAuth tokens refreshed");

    Ok(Some(data))
}

async fn discover_auth_server_for(
    client: &HttpClient,
    server_url: &str,
    www_auth: Option<&discovery::WwwAuthenticateInfo>,
) -> Result<discovery::AuthServerMetadata, OAuthError> {
    let resource_meta = discovery::discover_resource_metadata(client, server_url, www_auth).await?;
    let auth_server_url = resource_meta
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| discovery::server_origin(server_url));
    discovery::discover_auth_server(client, &auth_server_url).await
}

async fn auth_timeout() -> Result<CallbackResult, String> {
    smol::Timer::after(AUTH_TIMEOUT).await;
    Err(format!(
        "OAuth flow timed out after {} minutes",
        AUTH_TIMEOUT.as_secs() / 60
    ))
}

fn is_headless() -> bool {
    cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

fn build_http_client(timeout: Duration) -> Result<HttpClient, isahc::Error> {
    HttpClient::builder()
        .redirect_policy(RedirectPolicy::Limit(super::http::MAX_REDIRECTS))
        // Same pin as mcp::http, so oauth and data traffic match.
        .version_negotiation(VersionNegotiation::http11())
        .timeout(timeout)
        .build()
}

fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    scope: Option<&str>,
    resource: &str,
) -> Result<String, OAuthError> {
    let parsed = Url::parse(authorization_endpoint).map_err(|e| {
        OAuthError::InvalidResponse(format!(
            "invalid authorization endpoint {authorization_endpoint}: {e}"
        ))
    })?;
    let sep = if parsed.query().is_some() { '&' } else { '?' };
    let mut url = format!(
        "{authorization_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&state={state}&code_challenge={code_challenge}&code_challenge_method=S256&resource={}",
        token::url_encode(client_id),
        token::url_encode(redirect_uri),
        token::url_encode(resource),
    );
    if let Some(s) = scope {
        url.push_str("&scope=");
        url.push_str(&token::url_encode(s));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{OAuthError, build_authorization_url};
    use test_case::test_case;
    use url::Url;

    const CLIENT_ID: &str = "client-id";
    const REDIRECT_URI: &str = "http://127.0.0.1:8080/callback";
    const STATE: &str = "state-value";
    const CHALLENGE: &str = "challenge-value";

    const SCOPE: &str = "offline#access";
    const RESOURCE: &str = "https://example.com/resource a b";

    fn query_pairs(url: &str) -> Vec<(String, String)> {
        let parsed = Url::parse(url).expect("built authorization URL should parse");
        parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn has(pairs: &[(String, String)], key: &str, value: &str) -> bool {
        pairs.iter().any(|(k, v)| k == key && v == value)
    }

    #[test_case("https://example.com/authorize", None; "plain_endpoint")]
    #[test_case("https://mcp-slack.example.com/dcr/authorize?provider=slack", Some("slack"); "slack_provider")]
    fn builds_authorization_url(endpoint: &str, provider: Option<&str>) {
        let url = build_authorization_url(
            endpoint,
            CLIENT_ID,
            REDIRECT_URI,
            STATE,
            CHALLENGE,
            Some(SCOPE),
            RESOURCE,
        )
        .expect("endpoint should parse");

        assert!(url.contains("resource=https%3A%2F%2Fexample.com%2Fresource%20a%20b"));
        assert!(url.contains("scope=offline%23access"));

        let pairs = query_pairs(&url);
        assert!(has(&pairs, "response_type", "code"));
        assert!(has(&pairs, "client_id", CLIENT_ID));
        assert!(has(&pairs, "resource", RESOURCE));
        assert!(has(&pairs, "scope", SCOPE));
        if let Some(expected) = provider {
            assert!(has(&pairs, "provider", expected));
        }
    }

    #[test_case("not a url"; "invalid_endpoint")]
    fn invalid_endpoint_returns_error(endpoint: &str) {
        assert!(matches!(
            build_authorization_url(
                endpoint,
                CLIENT_ID,
                REDIRECT_URI,
                STATE,
                CHALLENGE,
                None,
                RESOURCE
            ),
            Err(OAuthError::InvalidResponse(_))
        ));
    }
}
