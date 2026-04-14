use std::time::Duration;

use isahc::config::Configurable;
use isahc::{AsyncReadResponseExt, ReadResponseExt, Request};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/tontinton/maki/releases/latest";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] isahc::Error),
    #[error("failed to build request: {0}")]
    Request(#[from] isahc::http::Error),
    #[error("failed to read response: {0}")]
    Io(#[from] std::io::Error),
    #[error("server returned HTTP {0}")]
    Status(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<(u32, u32, u32)> {
        let mut it = s.split('.');
        Some((
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        ))
    };
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

fn client() -> Result<isahc::HttpClient, VersionError> {
    Ok(isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

fn request() -> Result<isahc::Request<()>, VersionError> {
    Ok(Request::get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "maki")
        .body(())?)
}

fn parse_tag(bytes: &[u8]) -> Result<String, VersionError> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or(VersionError::InvalidResponse("missing tag_name"))?;
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

pub fn fetch_latest() -> Result<String, VersionError> {
    let mut resp = client()?.send(request()?)?;
    if !resp.status().is_success() {
        return Err(VersionError::Status(resp.status().as_u16()));
    }
    parse_tag(&resp.bytes()?)
}

pub async fn fetch_latest_async() -> Result<String, VersionError> {
    let mut resp = client()?.send_async(request()?).await?;
    if !resp.status().is_success() {
        return Err(VersionError::Status(resp.status().as_u16()));
    }
    parse_tag(&resp.bytes().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("0.2.0", "0.1.0", true  ; "minor_bump")]
    #[test_case("1.0.0", "0.9.9", true  ; "major_bump")]
    #[test_case("0.1.1", "0.1.0", true  ; "patch_bump")]
    #[test_case("0.1.0", "0.1.0", false ; "equal")]
    #[test_case("0.0.9", "0.1.0", false ; "older")]
    #[test_case("abc",   "0.1.0", false ; "garbage_latest")]
    #[test_case("1.0.0-rc1", "0.9.0", false ; "prerelease_ignored")]
    fn is_newer_cases(latest: &str, current: &str, expected: bool) {
        assert_eq!(is_newer(latest, current), expected);
    }
}
