use std::io;
use std::process::Command;
use std::time::Duration;

use async_compat::Compat;
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, RequestBuilder};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/tontinton/maki/releases/latest";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "maki";

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned HTTP {0}")]
    Status(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("curl failed: {0}")]
    Curl(#[from] io::Error),
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

/// Redirects stay off, as with isahc. reqwest runs on tokio, so a request
/// built from this has to be awaited through [`Compat`].
pub fn client() -> ClientBuilder {
    Client::builder()
        .redirect(Policy::none())
        .user_agent(USER_AGENT)
}

fn version_client() -> Result<Client, VersionError> {
    Ok(client()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

fn request(client: &Client) -> RequestBuilder {
    client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
}

fn parse_tag(bytes: &[u8]) -> Result<String, VersionError> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or(VersionError::InvalidResponse("missing tag_name"))?;
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

/// Fallback for TLS-inspecting proxies (e.g. Cloudflare WARP): system `curl`
/// trusts the OS certificate store, which our statically linked OpenSSL
/// cannot read. Callers keep their original error when this also fails,
/// since the TLS error is the useful one to show.
pub fn curl_fetch(url: &str) -> io::Result<Vec<u8>> {
    let max_time = CONNECT_TIMEOUT + REQUEST_TIMEOUT;
    let out = Command::new("curl")
        .args(["-fsSL", "-A", "maki", "--max-time"])
        .arg(max_time.as_secs().to_string())
        .arg(url)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

pub async fn fetch_latest() -> Result<String, VersionError> {
    Compat::new(async {
        let resp = request(&version_client()?).send().await?;
        if !resp.status().is_success() {
            return Err(VersionError::Status(resp.status().as_u16()));
        }
        parse_tag(&resp.bytes().await?)
    })
    .await
}

/// Blocking, for callers that already fell back to [`curl_fetch`].
pub fn curl_fetch_latest() -> Result<String, VersionError> {
    parse_tag(&curl_fetch(RELEASES_URL)?)
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
