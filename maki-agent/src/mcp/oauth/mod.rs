pub mod callback;
pub mod discovery;
pub mod pkce;
pub mod registration;
pub mod token;

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

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use isahc::HttpClient;
use isahc::config::{Configurable, RedirectPolicy};
use maki_storage::StateDir;
use maki_storage::auth::{McpAuthData, load_mcp_auth, save_mcp_auth};
use tracing::{info, warn};

use self::callback::CallbackServer;
use self::discovery::parse_www_authenticate;
use super::error::McpError;

pub async fn authenticate(
    server_name: &str,
    server_url: &str,
    www_authenticate: Option<&str>,
    storage: &StateDir,
) -> Result<McpAuthData, McpError> {
    let wrap = |e: OAuthError| McpError::OAuthFailed {
        server: server_name.into(),
        reason: e.to_string(),
    };
    let client = build_http_client().map_err(|e| wrap(OAuthError::Other(e.to_string())))?;

    if let Some(existing) = load_mcp_auth(storage, server_name, server_url)
        && let Some(ref tokens) = existing.tokens
    {
        if !tokens.is_expired() {
            return Ok(existing);
        }
        if !tokens.refresh.is_empty() {
            let auth_server = discover_auth_server_for(&client, server_url, None)
                .await
                .map_err(&wrap)?;
            match token::refresh_token(
                &client,
                &auth_server.token_endpoint,
                &tokens.refresh,
                &existing.client_id,
                existing.client_secret.as_deref(),
                server_url,
            )
            .await
            {
                Ok(new_tokens) => {
                    let data = McpAuthData {
                        tokens: Some(new_tokens),
                        ..existing
                    };
                    save_mcp_auth(storage, server_name, &data)
                        .map_err(|e| wrap(OAuthError::Other(e.to_string())))?;
                    return Ok(data);
                }
                Err(e) => {
                    warn!(server = server_name, error = %e, "token refresh failed, starting full flow");
                }
            }
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

    let callback = CallbackServer::bind()
        .await
        .map_err(|e| wrap(OAuthError::Other(e)))?;
    let redirect_uri = callback.redirect_uri();

    let reg = if let Some(existing) = load_mcp_auth(storage, server_name, server_url)
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
    );

    info!(server = server_name, endpoint = %auth_server.authorization_endpoint, "opening browser for OAuth");
    if let Err(e) = open::that(&auth_url) {
        warn!(server = server_name, error = %e, "failed to open browser - manually visit the auth URL in logs");
    }

    let result = callback
        .wait_for_callback(&state)
        .await
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
    };

    save_mcp_auth(storage, server_name, &data)
        .map_err(|e| wrap(OAuthError::Other(e.to_string())))?;
    info!(server = server_name, "OAuth authentication complete");
    Ok(data)
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

fn build_http_client() -> Result<HttpClient, isahc::Error> {
    HttpClient::builder()
        .redirect_policy(RedirectPolicy::Limit(super::http::MAX_REDIRECTS))
        .timeout(std::time::Duration::from_secs(30))
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
) -> String {
    let mut url = format!(
        "{authorization_endpoint}?response_type=code&client_id={}&redirect_uri={}&state={state}&code_challenge={code_challenge}&code_challenge_method=S256&resource={}",
        token::url_encode(client_id),
        token::url_encode(redirect_uri),
        token::url_encode(resource),
    );
    if let Some(s) = scope {
        url.push_str("&scope=");
        url.push_str(&token::url_encode(s));
    }
    url
}
