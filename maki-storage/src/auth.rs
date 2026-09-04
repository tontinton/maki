use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{StateDir, StorageError, atomic_write_permissions};

const AUTH_DIR: &str = "auth";
const AUTH_FILE_MODE: u32 = 0o600;
const REFRESH_BUFFER_SECS: u64 = 60;
const LOCK_SUFFIX: &str = ".lock";
const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl OAuthTokens {
    pub fn is_expired(&self) -> bool {
        now_millis() + REFRESH_BUFFER_SECS * 1000 >= self.expires
    }

    pub fn is_hard_expired(&self) -> bool {
        now_millis() >= self.expires
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct McpAuthData {
    pub server_url: String,
    pub tokens: Option<OAuthTokens>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_secret_expires_at: Option<u64>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Token endpoint pinned at interactive auth. Silent refresh reuses it
    /// instead of trusting fresh discovery, so a later-compromised server
    /// cannot redirect the refresh token and client secret elsewhere.
    #[serde(default)]
    pub token_endpoint: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderCredentials {
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl ProviderCredentials {
    pub fn masked_api_key(&self) -> String {
        if self.api_key.len() > 8 {
            format!(
                "{}...{}",
                &self.api_key[..4],
                &self.api_key[self.api_key.len() - 4..]
            )
        } else {
            "****".to_string()
        }
    }
}

/// Refresh tokens rotate and a rotated one is single use, so two processes
/// refreshing at once leave the loser replaying a spent token, which providers
/// answer by revoking the whole family instead of failing the one call. Waiting
/// forever would be worse than the race though, since this runs on the ui
/// thread too, so a lock that does not arrive in time is given up and the
/// caller carries on as before.
pub fn lock_exclusive(path: &Path) -> Option<File> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            fs::create_dir_all(path.parent()?).ok()?;
            OpenOptions::new()
                .write(true)
                .truncate(false)
                .create(true)
                .open(path)
                .ok()?
        }
    };
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => sleep(LOCK_POLL),
            Err(_) => return None,
        }
    }
}

/// Every write replaces the token file, so the lock sits beside it under a name
/// no loader reads.
pub fn lock_tokens(dir: &StateDir, provider: &str) -> Option<File> {
    lock_exclusive(&auth_path(dir, &format!("{provider}{LOCK_SUFFIX}")))
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn auth_path(dir: &StateDir, filename: &str) -> PathBuf {
    dir.path().join(AUTH_DIR).join(format!("{filename}.json"))
}

fn load_auth<T: DeserializeOwned>(path: &Path) -> Option<T> {
    fs::read_to_string(path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
}

fn save_auth(path: &Path, data: &impl Serialize) -> Result<(), StorageError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(data)?;
    atomic_write_permissions(path, json.as_bytes(), AUTH_FILE_MODE)?;
    debug!(path = %path.display(), "auth data saved");
    Ok(())
}

fn delete_auth(path: &Path) -> Result<bool, StorageError> {
    if path.exists() {
        fs::remove_file(path)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn load_tokens(dir: &StateDir, provider: &str) -> Option<OAuthTokens> {
    load_auth(&auth_path(dir, provider))
}

pub fn save_tokens(
    dir: &StateDir,
    provider: &str,
    tokens: &OAuthTokens,
) -> Result<(), StorageError> {
    save_auth(&auth_path(dir, provider), tokens)
}

pub fn delete_tokens(dir: &StateDir, provider: &str) -> Result<bool, StorageError> {
    delete_auth(&auth_path(dir, provider))
}

pub fn load_mcp_auth(dir: &StateDir, server_name: &str, expected_url: &str) -> Option<McpAuthData> {
    let data: McpAuthData = load_auth(&auth_path(dir, &format!("mcp-{server_name}")))?;
    if data.server_url != expected_url {
        return None;
    }
    if let Some(expires_at) = data.client_secret_expires_at
        && now_millis() / 1000 >= expires_at
    {
        return None;
    }
    Some(data)
}

pub fn save_mcp_auth(
    dir: &StateDir,
    server_name: &str,
    data: &McpAuthData,
) -> Result<(), StorageError> {
    save_auth(&auth_path(dir, &format!("mcp-{server_name}")), data)
}

pub fn delete_mcp_auth(dir: &StateDir, server_name: &str) -> Result<bool, StorageError> {
    delete_auth(&auth_path(dir, &format!("mcp-{server_name}")))
}

pub fn load_provider_credentials(dir: &StateDir, slug: &str) -> Option<ProviderCredentials> {
    load_auth(&auth_path(dir, slug))
}

pub fn save_provider_credentials(
    dir: &StateDir,
    slug: &str,
    creds: &ProviderCredentials,
) -> Result<(), StorageError> {
    save_auth(&auth_path(dir, slug), creds)
}

pub fn delete_provider_credentials(dir: &StateDir, slug: &str) -> Result<bool, StorageError> {
    delete_auth(&auth_path(dir, slug))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use test_case::test_case;

    const TEST_URL: &str = "https://mcp.example.com";

    fn test_mcp_data() -> McpAuthData {
        McpAuthData {
            server_url: TEST_URL.into(),
            tokens: None,
            client_id: "client-123".into(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: None,
            token_endpoint: None,
        }
    }

    #[test_case(0,                              true  ; "epoch_is_expired")]
    #[test_case(now_millis() + 3_600_000,       false ; "future_is_valid")]
    fn token_expiry(expires: u64, expected: bool) {
        let tokens = OAuthTokens {
            access: "a".into(),
            refresh: "r".into(),
            expires,
            account_id: None,
        };
        assert_eq!(tokens.is_expired(), expected);
    }

    #[test]
    fn save_load_delete_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let tokens = OAuthTokens {
            access: "access_tok".into(),
            refresh: "refresh_tok".into(),
            expires: 9999999999,
            account_id: None,
        };
        save_tokens(&dir, "anthropic", &tokens).unwrap();

        let loaded = load_tokens(&dir, "anthropic").unwrap();
        assert_eq!(loaded.access, "access_tok");
        assert_eq!(loaded.refresh, "refresh_tok");
        assert_eq!(loaded.expires, 9999999999);

        #[cfg(unix)]
        {
            let metadata = fs::metadata(auth_path(&dir, "anthropic")).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, AUTH_FILE_MODE);
        }

        assert!(delete_tokens(&dir, "anthropic").unwrap());
        assert!(load_tokens(&dir, "anthropic").is_none());
        assert!(!delete_tokens(&dir, "anthropic").unwrap());
    }

    #[test]
    fn mcp_auth_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let data = McpAuthData {
            tokens: Some(OAuthTokens {
                access: "acc".into(),
                refresh: "ref".into(),
                expires: 9999999999,
                account_id: None,
            }),
            ..test_mcp_data()
        };
        save_mcp_auth(&dir, "srv", &data).unwrap();
        let loaded = load_mcp_auth(&dir, "srv", TEST_URL).unwrap();
        assert_eq!(loaded.client_id, "client-123");
        assert_eq!(loaded.tokens.unwrap().access, "acc");
    }

    #[test_case(
        test_mcp_data(),
        "https://other.example.com"
        ; "url_mismatch"
    )]
    #[test_case(
        McpAuthData {
            client_secret: Some("s".into()),
            client_secret_expires_at: Some(1),
            ..test_mcp_data()
        },
        TEST_URL
        ; "expired_client_secret"
    )]
    fn mcp_auth_load_returns_none(data: McpAuthData, lookup_url: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        save_mcp_auth(&dir, "srv", &data).unwrap();
        assert!(load_mcp_auth(&dir, "srv", lookup_url).is_none());
    }
}
