pub mod callback;
pub mod manual;
mod store;

use std::path::Path;
use std::time::Duration;

use maki_storage::StateDir;
use maki_storage::auth::{McpAuthData, load_mcp_auth, save_mcp_auth};
use reqwest::Client;
use rmcp::transport::auth::{AuthorizationMetadata, OAuthClientConfig};
use rmcp::transport::{AuthError, AuthorizationManager};
use tokio::time::timeout;
use tracing::{info, warn};
use url::{Host, Url};

use self::callback::{CallbackResult, CallbackServer};
use self::store::AuthStore;
use super::config::{CallbackScheme, OauthClientConfig};
use super::error::McpError;
use super::transport::{CLIENT_NAME, http_client};

const AUTH_TIMEOUT: Duration = Duration::from_secs(600);
/// Per OAuth request while logging in.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Per OAuth request while refreshing behind an MCP request.
pub(super) const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_PARAM: &str = "state";
const RESOURCE_PARAM: &str = "resource";
const HTTPS: &str = "https";
const HTTP: &str = "http";
const LOCALHOST: &str = "localhost";

#[derive(Debug, thiserror::Error)]
enum OAuthError {
    #[error(transparent)]
    Auth(#[from] AuthError),
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
    ca_file: Option<&Path>,
) -> Result<(), McpError> {
    async {
        let client = http_client(ca_file, Some(LOGIN_TIMEOUT)).map_err(OAuthError::Other)?;

        // We only get here after the server turned the token down, or when the
        // user asks to log in, so a token that hasn't expired yet proves
        // nothing. A stored login that can't be used at all, say because its
        // auth server moved, goes straight to a new login.
        let stored = stored_manager(storage, server_name, server_url, client.clone())
            .await
            .unwrap_or_else(|e| {
                warn!(server = server_name, error = %e, "stored login unusable, starting full flow");
                None
            });
        if let Some(manager) = stored {
            match manager.refresh_token().await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    warn!(server = server_name, error = %e, "token refresh failed, starting full flow");
                }
            }
        }

        login(
            server_name,
            server_url,
            www_authenticate,
            storage,
            interaction,
            static_client.unwrap_or_default(),
            client,
        )
        .await
    }
    .await
    .map_err(|e| McpError::OAuthFailed {
        server: server_name.into(),
        reason: e.to_string(),
    })
}

/// A manager that refreshes the tokens stored for this server in place, or
/// `None` when there are none to refresh. It names the resource the login
/// named, which is the server URL for logins from before it was kept.
pub(super) async fn stored_manager(
    storage: &StateDir,
    server_name: &str,
    server_url: &str,
    client: Client,
) -> Result<Option<AuthorizationManager>, AuthError> {
    let Some(mut record) =
        load_mcp_auth(storage, server_name, server_url).filter(|r| r.tokens.is_some())
    else {
        return Ok(None);
    };
    let pinned = record
        .auth_server
        .clone()
        .and_then(|value| serde_json::from_value::<AuthorizationMetadata>(value).ok());
    let metadata = match pinned {
        Some(metadata) => metadata,
        None => pin_auth_server(storage, server_name, &mut record, client.clone()).await?,
    };
    let resource = record.resource.as_deref().unwrap_or(&record.server_url);
    let mut manager = AuthorizationManager::new(resource).await?;
    manager.with_client(client)?;
    manager.set_metadata(metadata);
    manager.configure_client(client_config(&record))?;
    manager.set_credential_store(AuthStore::new(storage.clone(), server_name, record));
    Ok(Some(manager))
}

/// Records from before `auth_server` pinned only the token endpoint, so fresh
/// discovery must agree with it before the refresh token goes anywhere. The
/// result is saved right away, so each record is discovered once.
async fn pin_auth_server(
    storage: &StateDir,
    server_name: &str,
    record: &mut McpAuthData,
    client: Client,
) -> Result<AuthorizationMetadata, AuthError> {
    let (_, metadata) = discover(&record.server_url, None, client).await?;
    check_pin(record, &metadata)?;
    record.auth_server = serde_json::to_value(&metadata).ok();
    if let Err(e) = save_mcp_auth(storage, server_name, record) {
        warn!(server = server_name, error = %e, "failed to save the MCP OAuth auth server");
    }
    Ok(metadata)
}

/// The returned manager keeps the RFC 8707 resource the server named, if any.
async fn discover(
    server_url: &str,
    www_authenticate: Option<&str>,
    client: Client,
) -> Result<(AuthorizationManager, AuthorizationMetadata), AuthError> {
    let mut manager = AuthorizationManager::new(server_url).await?;
    manager.with_client(client)?;
    // maki never checked the metadata's `issuer`, and some auth servers leave it out.
    manager.set_allow_missing_issuer(true);
    let metadata = manager
        .resolve_metadata_from_challenge(www_authenticate)
        .await?
        .metadata;
    require_https(&metadata)?;
    Ok((manager, metadata))
}

/// rmcp only refuses resource metadata that downgrades from https, so the
/// endpoints it found are held to HTTPS here, loopback aside.
fn require_https(metadata: &AuthorizationMetadata) -> Result<(), AuthError> {
    [
        Some(&metadata.authorization_endpoint),
        Some(&metadata.token_endpoint),
        metadata.registration_endpoint.as_ref(),
    ]
    .into_iter()
    .flatten()
    .try_for_each(|endpoint| {
        let url = Url::parse(endpoint)?;
        let loopback = match url.host() {
            Some(Host::Domain(domain)) => domain == LOCALHOST,
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        match url.scheme() {
            HTTPS => Ok(()),
            HTTP if loopback => Ok(()),
            _ => Err(AuthError::MetadataError(format!(
                "OAuth endpoint must use HTTPS: {endpoint}"
            ))),
        }
    })
}

fn check_pin(record: &McpAuthData, metadata: &AuthorizationMetadata) -> Result<(), AuthError> {
    match &record.token_endpoint {
        Some(pinned) if *pinned != metadata.token_endpoint => {
            Err(AuthError::MetadataError(format!(
                "token endpoint moved from {pinned} to {}",
                metadata.token_endpoint
            )))
        }
        _ => Ok(()),
    }
}

fn client_config(record: &McpAuthData) -> OAuthClientConfig {
    let redirect_uri = record.redirect_uri.as_deref().unwrap_or(&record.server_url);
    let mut config = OAuthClientConfig::new(&record.client_id, redirect_uri);
    config.client_secret = record.client_secret.clone();
    config
}

async fn login(
    server_name: &str,
    server_url: &str,
    www_authenticate: Option<&str>,
    storage: &StateDir,
    interaction: Interaction,
    static_client: OauthClientConfig,
    client: Client,
) -> Result<(), OAuthError> {
    let (mut manager, metadata) = discover(server_url, www_authenticate, client).await?;
    manager.set_metadata(metadata.clone());

    let OauthClientConfig {
        client_id,
        client_secret,
        callback_port,
        callback_path,
        callback_hostname,
        callback_scheme,
    } = static_client;
    let callback = CallbackServer::bind(
        callback_port,
        callback_path.as_deref(),
        callback_hostname.as_deref(),
        callback_scheme,
    )
    .await
    .map_err(OAuthError::Other)?;
    let redirect_uri = callback.redirect_uri();
    let scopes = manager.select_initial_scopes(&[]).await;
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();

    let reusable = load_mcp_auth(storage, server_name, server_url)
        .filter(|r| can_reuse_client(r, &redirect_uri));
    let (client_id, client_secret, client_secret_expires_at) = match (client_id, reusable) {
        (Some(client_id), _) => (client_id, client_secret, None),
        (None, Some(existing)) => (
            existing.client_id,
            existing.client_secret,
            existing.client_secret_expires_at,
        ),
        (None, None) => {
            let registered = manager
                .register_client(CLIENT_NAME, &redirect_uri, &scopes)
                .await?;
            (registered.client_id, registered.client_secret, None)
        }
    };
    let mut record = McpAuthData {
        server_url: server_url.to_owned(),
        tokens: None,
        client_id,
        client_secret,
        client_secret_expires_at,
        redirect_uri: Some(redirect_uri.clone()),
        token_endpoint: Some(metadata.token_endpoint.clone()),
        auth_server: serde_json::to_value(&metadata).ok(),
        resource: None,
    };
    manager.configure_client(client_config(&record))?;

    let auth_url = manager.get_authorization_url(&scopes).await?;
    let state = query_param(&auth_url, STATE_PARAM)
        .ok_or_else(|| OAuthError::Other("authorization URL carries no state".into()))?;
    // rmcp keeps the resource it discovered to itself, so it's read off the URL.
    record.resource = query_param(&auth_url, RESOURCE_PARAM).filter(|r| !r.is_empty());
    manager.set_credential_store(AuthStore::new(storage.clone(), server_name, record));

    info!(server = server_name, endpoint = %metadata.authorization_endpoint, "starting OAuth authorization");
    let result = wait_for_code(
        server_name,
        &auth_url,
        callback,
        &state,
        interaction,
        callback_scheme,
    )
    .await
    .map_err(OAuthError::Other)?;

    manager
        .exchange_code_for_token_with_issuer(&result.code, &state, result.issuer.as_deref())
        .await?;
    info!(server = server_name, "OAuth authentication complete");
    Ok(())
}

async fn wait_for_code(
    server_name: &str,
    auth_url: &str,
    callback: CallbackServer,
    state: &str,
    interaction: Interaction,
    callback_scheme: CallbackScheme,
) -> Result<CallbackResult, String> {
    let redirect_uri = callback.redirect_uri();
    let wait_callback = callback.wait_for_callback(state);
    match interaction {
        Interaction::Cli => {
            eprintln!("\nOpen this URL in your browser:\n\n  {auth_url}\n");

            if is_headless() {
                info!(
                    server = server_name,
                    "no display detected, skipping browser open"
                );
            } else if let Err(e) = open::that(auth_url) {
                warn!(server = server_name, error = %e, "failed to open browser");
            }

            eprintln!("Waiting for callback on {redirect_uri}...");
            if callback_scheme == CallbackScheme::Https {
                eprintln!(
                    "Your browser will warn about Maki's self-signed certificate for this login; proceed past it."
                );
            }
            eprintln!("If this machine has no browser, log in on another device and paste");
            eprintln!("the full redirect URL ({redirect_uri}?...) here:");

            let callback_or_paste = async {
                tokio::select! {
                    callback = wait_callback => callback,
                    pasted = manual::wait_for_paste(state) => pasted,
                }
            };
            timeout(AUTH_TIMEOUT, callback_or_paste)
                .await
                .unwrap_or_else(|_| Err(auth_timed_out()))
        }
        Interaction::Background => {
            let cause = if is_headless() {
                Some("no display to open a browser".to_string())
            } else {
                open::that(auth_url)
                    .err()
                    .map(|e| format!("failed to open browser: {e}"))
            };
            match cause {
                Some(cause) => Err(format!("{cause}; run 'maki mcp auth {server_name}'")),
                None => timeout(AUTH_TIMEOUT, wait_callback)
                    .await
                    .unwrap_or_else(|_| Err(auth_timed_out())),
            }
        }
    }
}

fn auth_timed_out() -> String {
    format!(
        "OAuth flow timed out after {} minutes",
        AUTH_TIMEOUT.as_secs() / 60
    )
}

fn is_headless() -> bool {
    cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

/// rmcp's registration drops `client_secret_expires_at`, so a secret without a
/// known expiry may already be dead, and the login registers a new client.
fn can_reuse_client(record: &McpAuthData, redirect_uri: &str) -> bool {
    record.redirect_uri.as_deref() == Some(redirect_uri)
        && (record.client_secret.is_none() || record.client_secret_expires_at.is_some())
}

fn query_param(url: &str, key: &str) -> Option<String> {
    Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PINNED: &str = "https://auth.example.com/token";
    const MOVED: &str = "https://evil.example.com/token";
    const REDIRECT: &str = "http://localhost:3118/callback";
    const OTHER_REDIRECT: &str = "http://localhost:4000/callback";
    const SECRET: &str = "secret";

    fn record(token_endpoint: Option<&str>) -> McpAuthData {
        McpAuthData {
            server_url: "https://mcp.example.com/mcp".into(),
            tokens: None,
            client_id: "cid".into(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: None,
            token_endpoint: token_endpoint.map(Into::into),
            auth_server: None,
            resource: None,
        }
    }

    #[test_case(Some(PINNED), PINNED, true  ; "same_endpoint")]
    #[test_case(Some(PINNED), MOVED,  false ; "moved_endpoint")]
    #[test_case(None,         MOVED,  true  ; "never_pinned")]
    fn discovery_must_match_the_pinned_token_endpoint(
        pinned: Option<&str>,
        discovered: &str,
        allowed: bool,
    ) {
        let mut metadata = AuthorizationMetadata::default();
        metadata.token_endpoint = discovered.into();
        assert_eq!(check_pin(&record(pinned), &metadata).is_ok(), allowed);
    }

    #[test_case("https://auth.example.com/token",       true  ; "https")]
    #[test_case("http://127.0.0.1:8080/token",          true  ; "loopback_ipv4")]
    #[test_case("http://[::1]:8080/token",              true  ; "loopback_ipv6")]
    #[test_case("http://localhost:8080/token",          true  ; "localhost")]
    #[test_case("http://auth.example.com/token",        false ; "plain_http")]
    #[test_case("http://localhost.evil.example/token",  false ; "localhost_prefix")]
    #[test_case("ftp://auth.example.com/token",         false ; "other_scheme")]
    fn discovered_endpoints_must_use_https(token_endpoint: &str, allowed: bool) {
        let mut metadata = AuthorizationMetadata::default();
        metadata.authorization_endpoint = PINNED.into();
        metadata.token_endpoint = token_endpoint.into();
        assert_eq!(require_https(&metadata).is_ok(), allowed);
    }

    #[test_case(None,         None,           REDIRECT,       true  ; "public_client")]
    #[test_case(Some(SECRET), Some(u64::MAX), REDIRECT,       true  ; "secret_with_known_expiry")]
    #[test_case(Some(SECRET), None,           REDIRECT,       false ; "secret_without_known_expiry")]
    #[test_case(None,         None,           OTHER_REDIRECT, false ; "other_redirect_uri")]
    fn login_reuses_a_stored_client(
        secret: Option<&str>,
        secret_expires_at: Option<u64>,
        redirect_uri: &str,
        reused: bool,
    ) {
        let stored = McpAuthData {
            client_secret: secret.map(Into::into),
            client_secret_expires_at: secret_expires_at,
            redirect_uri: Some(REDIRECT.into()),
            ..record(None)
        };
        assert_eq!(can_reuse_client(&stored, redirect_uri), reused);
    }

    #[test_case("https://a.example/authorize?client_id=x&state=abc", Some("abc") ; "present")]
    #[test_case("https://a.example/authorize?client_id=x",           None        ; "absent")]
    #[test_case("not a url",                                           None        ; "invalid")]
    fn reads_the_state_off_the_authorization_url(url: &str, expected: Option<&str>) {
        assert_eq!(query_param(url, STATE_PARAM).as_deref(), expected);
    }
}
