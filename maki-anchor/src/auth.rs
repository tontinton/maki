//! Browser authentication: OIDC login flow and cookie sessions.

use std::sync::Arc;

use crate::oidc::{self, OidcConfig};
use crate::store::{self, Store};

pub const COOKIE_NAME: &str = "maki_anchor_session";
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("oidc: {0}")]
    Oidc(#[from] oidc::OidcError),
    #[error("store: {0}")]
    Store(#[from] store::StoreError),
    #[error("auth is not configured")]
    NotConfigured,
}

pub struct Auth {
    pub oidc: Option<OidcConfig>,
    pub store: Arc<Store>,
    pub allow_local: bool,
    pub mint_tokens: store::MintTokens,
    /// Login state -> timestamp, so /callback can verify the state parameter.
    /// Bounded by pruning on insert.
    pending: std::sync::Mutex<std::collections::HashMap<String, i64>>,
}

const PENDING_TTL_SECS: i64 = 600;
const PENDING_CAP: usize = 100;

impl Auth {
    pub fn new(
        store: Arc<Store>,
        oidc: Option<OidcConfig>,
        allow_local: bool,
        mint_tokens: store::MintTokens,
    ) -> Self {
        Self {
            oidc,
            store,
            allow_local,
            mint_tokens,
            pending: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn effective_mint_tokens(&self) -> store::MintTokens {
        if let Ok(Some(v)) = self.store.get_setting("mint_tokens")
            && let Some(m) = store::MintTokens::parse(&v)
        {
            return m;
        }
        self.mint_tokens
    }

    pub fn can_mint_tokens(&self, user: Option<&store::UserRow>) -> bool {
        match self.effective_mint_tokens() {
            store::MintTokens::Any => true,
            store::MintTokens::User => user.is_some(),
            store::MintTokens::Admin => user.is_some_and(|u| u.is_admin),
        }
    }

    pub fn login_local(&self, username: &str, password: &str) -> Result<String, AuthError> {
        if !self.allow_local {
            return Err(AuthError::NotConfigured);
        }
        let user = self
            .store
            .verify_local_user(username, password)
            .map_err(|_| {
                AuthError::Oidc(oidc::OidcError::InvalidToken("invalid credentials".into()))
            })?;
        let cookie = new_session_cookie();
        self.store
            .create_oidc_session(&cookie, user.id, SESSION_TTL)
            .map_err(AuthError::Store)?;
        Ok(cookie)
    }

    pub fn enabled(&self) -> bool {
        self.oidc.is_some()
    }

    /// The URL to redirect the browser to, or None when OIDC is off.
    pub fn begin_login(&self) -> Result<String, AuthError> {
        let config = self.oidc.as_ref().ok_or(AuthError::NotConfigured)?;
        let discovery = oidc::discover(config)?;
        let state = new_state();
        {
            let mut pending = self.pending.lock().unwrap();
            let now = store::now_unix();
            pending.retain(|_, at| now - *at < PENDING_TTL_SECS);
            while pending.len() >= PENDING_CAP {
                let oldest = pending
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    pending.remove(&oldest);
                }
            }
            pending.insert(state.clone(), now);
        }
        Ok(oidc::authorization_url(config, &discovery, &state))
    }

    /// Finish the login: exchange the code, upsert the user, mint the cookie.
    /// Returns the cookie value on success.
    pub fn finish_login(&self, code: &str, state: &str) -> Result<String, AuthError> {
        let config = self.oidc.as_ref().ok_or(AuthError::NotConfigured)?;
        let claimed = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(state)
        };
        let at = claimed.ok_or_else(|| {
            AuthError::Oidc(oidc::OidcError::InvalidToken(
                "unknown login state".to_owned(),
            ))
        })?;
        if store::now_unix() - at > PENDING_TTL_SECS {
            return Err(AuthError::Oidc(oidc::OidcError::InvalidToken(
                "login state expired".to_owned(),
            )));
        }
        let discovery = oidc::discover(config)?;
        let claims = oidc::exchange_code(config, &discovery, code, state)?;
        let user =
            self.store
                .upsert_user(&claims.sub, claims.email.as_deref(), claims.name.as_deref())?;
        let cookie = new_session_cookie();
        self.store
            .create_oidc_session(&cookie, user.id, SESSION_TTL)?;
        Ok(cookie)
    }

    /// The logged-in user for a request cookie header, if any.
    pub fn user_from_cookie(&self, cookie_header: Option<&str>) -> Option<store::UserRow> {
        let value = cookie_header.and_then(extract_cookie_value)?;
        self.store.user_by_cookie(&value).ok()
    }

    pub fn logout(&self, cookie_header: Option<&str>) {
        if let Some(value) = cookie_header.and_then(extract_cookie_value) {
            self.store.delete_oidc_session(&value).ok();
        }
    }

    /// A Set-Cookie header value that clears the session.
    pub fn clear_cookie() -> &'static str {
        "maki_anchor_session=deleted; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
    }

    pub fn session_set_cookie(value: &str) -> String {
        format!("{COOKIE_NAME}={value}; Path=/; Max-Age={MAX_AGE}; HttpOnly; SameSite=Lax")
    }
}

const MAX_AGE: i64 = 7 * 24 * 60 * 60;

fn extract_cookie_value(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let part = part.trim();
        let (name, value) = part.split_once('=')?;
        (name == COOKIE_NAME).then(|| value.to_owned())
    })
}

fn new_session_cookie() -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("rng failed");
    hex(&bytes)
}

fn new_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("rng failed");
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_value_extraction() {
        let header = "other=1; maki_anchor_session=abc123; x=y";
        assert_eq!(extract_cookie_value(header).as_deref(), Some("abc123"));
        assert!(extract_cookie_value("nothing").is_none());
    }
}
