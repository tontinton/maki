use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use maki_storage::StateDir;
use maki_storage::auth::{
    McpAuthData, OAuthTokens, delete_mcp_auth, load_mcp_auth, now_millis, save_mcp_auth,
};
use oauth2::basic::BasicTokenType;
use oauth2::{AccessToken, RefreshToken, TokenResponse};
use rmcp::transport::auth::{CredentialRefreshGuard, OAuthTokenResponse, VendorExtraTokenFields};
use rmcp::transport::{AuthError, CredentialStore, StoredCredentials};

const MILLIS_PER_SEC: u64 = 1000;
/// What maki assumed before rmcp when a token response has no `expires_in`.
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// rmcp's credentials kept in maki's own `mcp-<server>` auth file, so stored
/// logins survive the switch and `maki mcp logout` still finds them.
///
/// rmcp loads them before every request, so they are served from memory. A
/// refresh reads the file again first, since another maki may have rotated
/// the refresh token in the meantime.
pub(super) struct AuthStore {
    storage: StateDir,
    server_name: String,
    record: Mutex<McpAuthData>,
}

impl AuthStore {
    pub(super) fn new(storage: StateDir, server_name: &str, record: McpAuthData) -> Self {
        Self {
            storage,
            server_name: server_name.to_owned(),
            record: Mutex::new(record),
        }
    }

    fn record(&self) -> MutexGuard<'_, McpAuthData> {
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl CredentialStore for AuthStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let record = self.record();
        Ok(record.tokens.clone().map(|tokens| {
            StoredCredentials::new(
                record.client_id.clone(),
                Some(token_response(tokens)),
                Vec::new(),
                Some(now_millis() / MILLIS_PER_SEC),
            )
        }))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let mut record = self.record();
        record.client_id = credentials.client_id;
        record.tokens = credentials.token_response.as_ref().map(stored_tokens);
        save_mcp_auth(&self.storage, &self.server_name, &record)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.record().tokens = None;
        delete_mcp_auth(&self.storage, &self.server_name)
            .map(drop)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let mut record = self.record();
        match load_mcp_auth(&self.storage, &self.server_name, &record.server_url) {
            Some(on_disk) => *record = on_disk,
            None => record.tokens = None,
        }
        Ok(None)
    }
}

/// Without a refresh token there is nothing to renew early, so the token goes
/// out until the server turns it down, as it did before rmcp. `expires_in`
/// counts from now, matching the `token_received_at` that [`AuthStore::load`]
/// reports.
fn token_response(tokens: OAuthTokens) -> OAuthTokenResponse {
    let mut response = OAuthTokenResponse::new(
        AccessToken::new(tokens.access),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    if !tokens.refresh.is_empty() {
        response.set_refresh_token(Some(RefreshToken::new(tokens.refresh)));
        response.set_expires_in(Some(&Duration::from_millis(
            tokens.expires.saturating_sub(now_millis()),
        )));
    }
    response
}

fn stored_tokens(response: &OAuthTokenResponse) -> OAuthTokens {
    let lifetime = response.expires_in().unwrap_or(DEFAULT_TOKEN_LIFETIME);
    OAuthTokens {
        access: response.access_token().secret().clone(),
        refresh: response
            .refresh_token()
            .map(|r| r.secret().clone())
            .unwrap_or_default(),
        expires: now_millis() + lifetime.as_millis() as u64,
        account_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SERVER: &str = "srv";
    const URL: &str = "https://mcp.example.com/mcp";
    const ACCESS: &str = "access-1";
    const REFRESH: &str = "refresh-1";
    const ROTATED: &str = "refresh-2";
    const HOUR_MS: u64 = 3_600_000;

    fn record(tokens: Option<OAuthTokens>) -> McpAuthData {
        McpAuthData {
            server_url: URL.into(),
            tokens,
            client_id: "cid".into(),
            client_secret: Some("secret".into()),
            client_secret_expires_at: None,
            redirect_uri: Some("http://127.0.0.1:1/callback".into()),
            token_endpoint: Some("https://auth.example.com/token".into()),
            auth_server: None,
            resource: None,
        }
    }

    fn tokens(refresh: &str, expires: u64) -> OAuthTokens {
        OAuthTokens {
            access: ACCESS.into(),
            refresh: refresh.into(),
            expires,
            account_id: None,
        }
    }

    fn store(dir: &tempfile::TempDir, tokens: OAuthTokens) -> (StateDir, AuthStore) {
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let store = AuthStore::new(storage.clone(), SERVER, record(Some(tokens)));
        (storage, store)
    }

    #[tokio::test]
    async fn round_trips_tokens_through_the_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let expires = now_millis() + HOUR_MS;
        let (storage, store) = store(&dir, tokens(REFRESH, expires));

        let loaded = store.load().await.unwrap().unwrap();
        store.save(loaded).await.unwrap();

        let saved = load_mcp_auth(&storage, SERVER, URL).unwrap();
        let saved_tokens = saved.tokens.unwrap();
        assert_eq!(saved_tokens.access, ACCESS);
        assert_eq!(saved_tokens.refresh, REFRESH);
        assert!(saved_tokens.expires.abs_diff(expires) < MILLIS_PER_SEC * 2);
        assert_eq!(saved.client_secret.as_deref(), Some("secret"));
    }

    #[test_case(REFRESH, true  ; "with_refresh")]
    #[test_case("",      false ; "without_refresh")]
    #[tokio::test]
    async fn expiry_is_only_reported_with_a_refresh_token(refresh: &str, reported: bool) {
        let dir = tempfile::tempdir().unwrap();
        let (_, store) = store(&dir, tokens(refresh, now_millis()));
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(
            loaded.token_response.unwrap().expires_in().is_some(),
            reported
        );
    }

    #[tokio::test]
    async fn refresh_picks_up_a_token_another_process_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, store) = store(&dir, tokens(REFRESH, now_millis()));
        save_mcp_auth(
            &storage,
            SERVER,
            &record(Some(tokens(ROTATED, now_millis()))),
        )
        .unwrap();

        let loaded = async {
            let _guard = store.acquire_refresh_guard().await.unwrap();
            store.load().await.unwrap().unwrap()
        }
        .await;

        let response = loaded.token_response.unwrap();
        assert_eq!(response.refresh_token().unwrap().secret(), ROTATED);
    }

    #[test]
    fn missing_expiry_falls_back_to_the_default_lifetime() {
        let response = OAuthTokenResponse::new(
            AccessToken::new(ACCESS.into()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );
        let expected = now_millis() + DEFAULT_TOKEN_LIFETIME.as_millis() as u64;
        assert!(stored_tokens(&response).expires.abs_diff(expected) < MILLIS_PER_SEC * 2);
    }
}
