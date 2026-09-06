//! Browser authentication: OIDC login flow and cookie sessions.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::oidc::{self, OidcConfig};
use crate::store::{self, Store};

pub const COOKIE_NAME: &str = "maki_anchor_session";
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);
const LOCAL_FAIL_LIMIT: u32 = 5;
const LOCAL_LOCKOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("oidc: {0}")]
    Oidc(#[from] oidc::OidcError),
    #[error("store: {0}")]
    Store(#[from] store::StoreError),
    #[error("auth is not configured")]
    NotConfigured,
    #[error("too many failed logins, try again later")]
    RateLimited,
    #[error("setup already happened: users exist")]
    AlreadySetup,
}

/// One in-flight OIDC authorization: when it started plus the values the
/// callback must present back (nonce for the ID token, PKCE verifier for the
/// code exchange).
struct PendingLogin {
    at: i64,
    nonce: String,
    code_verifier: String,
}

/// Failed-login bookkeeping per (ip, username) so guessing one password
/// across accounts, or one account across the network, both stay slow.
struct Attempt {
    count: u32,
    until: std::time::Instant,
}

pub struct Auth {
    pub oidc: Option<OidcConfig>,
    pub store: Arc<Store>,
    pub allow_local: bool,
    pub mint_tokens: store::MintTokens,
    /// Honor `X-Forwarded-For`/`X-Forwarded-Proto` from the peer. Only safe
    /// when every connection genuinely comes through a reverse proxy that
    /// sets (and the network guarantees nothing else can forge) these
    /// headers — otherwise a direct client can hand itself an arbitrary
    /// origin and dodge the login lockout below, so this defaults to off.
    pub trust_proxy: bool,
    /// Login state -> pending record, so /callback can verify state and use
    /// the stored nonce/verifier. Bounded by pruning on insert.
    pending: Mutex<HashMap<String, PendingLogin>>,
    attempts: Mutex<HashMap<String, Attempt>>,
    rng_counter: AtomicU64,
}

const PENDING_TTL_SECS: i64 = 600;
const PENDING_CAP: usize = 100;

impl Auth {
    pub fn new(
        store: Arc<Store>,
        oidc: Option<OidcConfig>,
        allow_local: bool,
        mint_tokens: store::MintTokens,
        trust_proxy: bool,
    ) -> Self {
        Self {
            oidc,
            store,
            allow_local,
            mint_tokens,
            trust_proxy,
            pending: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            rng_counter: AtomicU64::new(0),
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

    /// Local passwords may be tried: asked for in config, or simply on file
    /// (first-run setup writes one even when nobody configured `allow_local`).
    pub fn local_login_allowed(&self) -> bool {
        self.allow_local || self.store.has_local_users().unwrap_or(false)
    }

    pub fn has_users(&self) -> bool {
        self.store.has_users().unwrap_or(true)
    }

    /// First-run account creation: the very first admin, on a store with no
    /// users, plus a live session cookie for immediate entry.
    pub fn setup_admin(
        &self,
        origin: &str,
        username: &str,
        password: &str,
    ) -> Result<String, AuthError> {
        let key = format!("{origin}|setup");
        if self.locked_out(&key) {
            return Err(AuthError::RateLimited);
        }
        match self.store.setup_first_admin(username, password) {
            Ok(Some(user)) => {
                self.attempts.lock().unwrap().remove(&key);
                let cookie = new_session_cookie();
                self.store
                    .create_oidc_session(&cookie, user.id, SESSION_TTL)
                    .map_err(AuthError::Store)?;
                Ok(cookie)
            }
            Ok(None) => Err(AuthError::AlreadySetup),
            Err(err) => {
                self.record_failure(&key);
                Err(err.into())
            }
        }
    }

    pub fn login_local(
        &self,
        origin: &str,
        username: &str,
        password: &str,
    ) -> Result<String, AuthError> {
        if !self.local_login_allowed() {
            return Err(AuthError::NotConfigured);
        }
        let key = format!("{origin}|{username}");
        if self.locked_out(&key) {
            return Err(AuthError::RateLimited);
        }
        match self.store.verify_local_user(username, password) {
            Ok(user) => {
                self.attempts.lock().unwrap().remove(&key);
                let cookie = new_session_cookie();
                self.store
                    .create_oidc_session(&cookie, user.id, SESSION_TTL)
                    .map_err(AuthError::Store)?;
                Ok(cookie)
            }
            Err(_) => {
                self.record_failure(&key);
                Err(AuthError::Oidc(oidc::OidcError::InvalidToken(
                    "invalid credentials".into(),
                )))
            }
        }
    }

    fn locked_out(&self, key: &str) -> bool {
        let attempts = self.attempts.lock().unwrap();
        attempts
            .get(key)
            .is_some_and(|a| a.count >= LOCAL_FAIL_LIMIT && a.until.elapsed() < LOCAL_LOCKOUT)
    }

    fn record_failure(&self, key: &str) {
        let mut attempts = self.attempts.lock().unwrap();
        // The map can only grow per distinct (ip, username); clear stale
        // entries on the way in so it cannot accumulate forever.
        attempts.retain(|_, a| a.count < LOCAL_FAIL_LIMIT || a.until.elapsed() < LOCAL_LOCKOUT);
        let attempt = attempts.entry(key.to_owned()).or_insert(Attempt {
            count: 0,
            until: std::time::Instant::now(),
        });
        attempt.count += 1;
        attempt.until = std::time::Instant::now();
    }

    pub fn enabled(&self) -> bool {
        self.oidc.is_some()
    }

    /// The URL to redirect the browser to, or None when OIDC is off.
    pub fn begin_login(&self) -> Result<String, AuthError> {
        let config = self.oidc.as_ref().ok_or(AuthError::NotConfigured)?;
        let discovery = oidc::discover(config)?;
        let state = self.new_random_hex(16);
        let nonce = self.new_random_hex(16);
        let code_verifier = self.new_random_hex(32);
        let code_challenge = oidc::pkce_challenge(&code_verifier);
        {
            let mut pending = self.pending.lock().unwrap();
            let now = store::now_unix();
            pending.retain(|_, p| now - p.at < PENDING_TTL_SECS);
            while pending.len() >= PENDING_CAP {
                let oldest = pending
                    .iter()
                    .min_by_key(|(_, p)| p.at)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    pending.remove(&oldest);
                } else {
                    break;
                }
            }
            pending.insert(
                state.clone(),
                PendingLogin {
                    at: now,
                    nonce: nonce.clone(),
                    code_verifier,
                },
            );
        }
        Ok(oidc::authorization_url(
            config,
            &discovery,
            &state,
            &nonce,
            &code_challenge,
        ))
    }

    fn new_random_hex(&self, bytes: usize) -> String {
        let mut buf = vec![0u8; bytes];
        getrandom::fill(&mut buf).expect("rng failed");
        // Mix in a counter so two same-instant draws cannot collide.
        let _ = self.rng_counter.fetch_add(1, Ordering::Relaxed);
        buf.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Finish the login: exchange the code, validate it against the pending
    /// record (state, nonce, PKCE), upsert the user, mint the cookie.
    /// Returns the cookie value on success.
    pub fn finish_login(&self, code: &str, state: &str) -> Result<String, AuthError> {
        let config = self.oidc.as_ref().ok_or(AuthError::NotConfigured)?;
        let claimed = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(state)
        };
        let Some(claimed) = claimed else {
            return Err(AuthError::Oidc(oidc::OidcError::InvalidToken(
                "unknown login state".to_owned(),
            )));
        };
        if store::now_unix() - claimed.at > PENDING_TTL_SECS {
            return Err(AuthError::Oidc(oidc::OidcError::InvalidToken(
                "login state expired".to_owned(),
            )));
        }
        let discovery = oidc::discover(config)?;
        let claims = oidc::exchange_code(
            config,
            &discovery,
            code,
            &claimed.code_verifier,
            &claimed.nonce,
        )?;
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
    pub fn clear_cookie(secure: bool) -> String {
        format!(
            "maki_anchor_session=deleted; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
            if secure { "; Secure" } else { "" }
        )
    }

    /// `secure` should be true whenever the browser reached this response
    /// over HTTPS (directly, or via a proxy hop `trust_proxy` allows this
    /// caller to believe) — a session cookie is exactly the kind of thing
    /// that must never ride cleartext.
    pub fn session_set_cookie(value: &str, secure: bool) -> String {
        format!(
            "{COOKIE_NAME}={value}; Path=/; Max-Age={MAX_AGE}; HttpOnly; SameSite=Lax{}",
            if secure { "; Secure" } else { "" }
        )
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_auth() -> Auth {
        let dir = tempfile::tempdir().unwrap();
        Auth::new(
            Store::open(&dir.path().join("db.sqlite")).unwrap(),
            None,
            true,
            store::MintTokens::Any,
            false,
        )
    }

    #[test]
    fn cookie_value_extraction() {
        let header = "other=1; maki_anchor_session=abc123; x=y";
        assert_eq!(extract_cookie_value(header).as_deref(), Some("abc123"));
        assert!(extract_cookie_value("nothing").is_none());
    }

    #[test]
    fn session_cookie_carries_max_age_and_httponly() {
        let cookie = Auth::session_set_cookie("abc", false);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains(&format!("Max-Age={MAX_AGE}")));
    }

    #[test]
    fn session_cookie_secure_flag_follows_the_caller() {
        assert!(!Auth::session_set_cookie("abc", false).contains("Secure"));
        assert!(Auth::session_set_cookie("abc", true).contains("; Secure"));
        assert!(!Auth::clear_cookie(false).contains("Secure"));
        assert!(Auth::clear_cookie(true).contains("; Secure"));
    }

    #[test]
    fn local_login_locks_out_after_repeated_failures() {
        let auth = test_auth();
        auth.store
            .create_local_user("target", "the-password", None, None, false)
            .unwrap();
        for _ in 0..LOCAL_FAIL_LIMIT {
            assert!(auth.login_local("10.0.0.1", "target", "guess").is_err());
        }
        let locked = auth
            .login_local("10.0.0.1", "target", "the-password")
            .expect_err("right password during lockout is still refused");
        assert!(
            matches!(locked, AuthError::RateLimited),
            "expected rate limit, got {locked:?}"
        );
        assert!(
            auth.login_local("10.0.0.2", "target", "the-password")
                .is_ok(),
            "a different origin is unaffected"
        );
    }
}
