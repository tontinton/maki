use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{DataDir, StorageError, atomic_write_permissions};

const AUTH_DIR: &str = "auth";
const AUTH_FILE_MODE: u32 = 0o600;
const REFRESH_BUFFER_SECS: u64 = 60;

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

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn provider_path(dir: &DataDir, provider: &str) -> std::path::PathBuf {
    dir.path().join(AUTH_DIR).join(format!("{provider}.json"))
}

pub fn load_tokens(dir: &DataDir, provider: &str) -> Option<OAuthTokens> {
    let path = provider_path(dir, provider);
    fs::read_to_string(&path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
}

pub fn save_tokens(
    dir: &DataDir,
    provider: &str,
    tokens: &OAuthTokens,
) -> Result<(), StorageError> {
    let auth_dir = dir.path().join(AUTH_DIR);
    fs::create_dir_all(&auth_dir)?;
    let path = provider_path(dir, provider);
    let json = serde_json::to_string_pretty(tokens)?;
    atomic_write_permissions(&path, json.as_bytes(), AUTH_FILE_MODE)?;
    debug!(path = %path.display(), provider, "OAuth tokens saved");
    Ok(())
}

pub fn delete_tokens(dir: &DataDir, provider: &str) -> Result<bool, StorageError> {
    let path = provider_path(dir, provider);
    if path.exists() {
        fs::remove_file(&path)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use test_case::test_case;

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
        let dir = DataDir::from_path(tmp.path().to_path_buf());
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
            let metadata = fs::metadata(provider_path(&dir, "anthropic")).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, AUTH_FILE_MODE);
        }

        assert!(delete_tokens(&dir, "anthropic").unwrap());
        assert!(load_tokens(&dir, "anthropic").is_none());
        assert!(!delete_tokens(&dir, "anthropic").unwrap());
    }

    #[test]
    fn separate_providers() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf());
        let t1 = OAuthTokens {
            access: "a1".into(),
            refresh: "r1".into(),
            expires: 1,
            account_id: None,
        };
        let t2 = OAuthTokens {
            access: "a2".into(),
            refresh: "r2".into(),
            expires: 2,
            account_id: Some("acct_123".into()),
        };
        save_tokens(&dir, "anthropic", &t1).unwrap();
        save_tokens(&dir, "openai", &t2).unwrap();

        assert_eq!(load_tokens(&dir, "anthropic").unwrap().access, "a1");
        let openai = load_tokens(&dir, "openai").unwrap();
        assert_eq!(openai.access, "a2");
        assert_eq!(openai.account_id.as_deref(), Some("acct_123"));
    }
}
