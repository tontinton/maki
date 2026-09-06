use std::io;
use std::process::Command;
use std::time::Duration;

use isahc::config::{Configurable, VersionNegotiation};
use isahc::{AsyncReadResponseExt, ReadResponseExt, Request};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// This fork carries the remote-control/anchor features upstream lacks, so
/// the updater tracks the fork by default. `MAKI_INSTALL_REPO` redirects it
/// (e.g. to `tontinton/maki`), the same knob the installer exposes.
pub const DEFAULT_REPO: &str = "wmantly/maki";
pub const REPO_ENV: &str = "MAKI_INSTALL_REPO";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn update_repo() -> String {
    std::env::var(REPO_ENV)
        .ok()
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_owned())
}

/// The fork releases raw binaries under the same names as upstream, so
/// fetching `install.sh` from the fork's own tree means the update installs a
/// binary that understands the anchor fields it writes.
pub fn install_script_url() -> String {
    format!(
        "https://raw.githubusercontent.com/{}/main/install.sh",
        update_repo()
    )
}

fn releases_url() -> String {
    format!(
        "https://api.github.com/repos/{}/releases/latest",
        update_repo()
    )
}

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

/// True when `latest` is a newer version than `current` under semver,
/// including the fork's `-rc.N` pre-release line: `0.5.0-rc.14` is newer than
/// `0.5.0-rc.13`, and any release outranks its own pre-releases. Malformed
/// input is not newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (Version::parse(latest), Version::parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Version {
    core: [u32; 3],
    /// Dot-separated pre-release identifiers after the first `-`; empty for a
    /// release. Semver: equal cores, a release outranks any of its
    /// pre-releases, so the empty `pre` is the greatest, not the least.
    pre: Vec<PreId>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match self.core.cmp(&other.core) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Equal,
            (true, false) => Greater,
            (false, true) => Less,
            (false, false) => self.pre.cmp(&other.pre),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PreId {
    Num(u32),
    Text(String),
}

// semver: numeric identifiers rank below alphanumeric ones, then compare as
// numbers / as strings; a shorter prefix of an equal run is lesser.
impl Ord for PreId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self, other) {
            (PreId::Num(a), PreId::Num(b)) => a.cmp(b),
            (PreId::Text(a), PreId::Text(b)) => a.cmp(b),
            (PreId::Num(_), PreId::Text(_)) => Less,
            (PreId::Text(_), PreId::Num(_)) => Greater,
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Version {
    fn parse(s: &str) -> Option<Version> {
        let s = s.trim().strip_prefix('v').unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.');
        let mut read = || nums.next()?.parse::<u32>().ok();
        let core = [read()?, read()?, read()?];
        if nums.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| match id.parse::<u32>() {
                    Ok(n) => PreId::Num(n),
                    Err(_) => PreId::Text(id.to_owned()),
                })
                .collect(),
        };
        Some(Version { core, pre })
    }
}

fn client() -> Result<isahc::HttpClient, VersionError> {
    Ok(isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        // curl carries http2 for OTLP.
        .version_negotiation(VersionNegotiation::http11())
        .build()?)
}

fn request() -> Result<isahc::Request<()>, VersionError> {
    let url = releases_url();
    Ok(Request::get(url.as_str())
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

pub fn fetch_latest() -> Result<String, VersionError> {
    let bytes = fetch_bytes().or_else(|e| curl_fetch(&releases_url()).map_err(|_| e))?;
    parse_tag(&bytes)
}

fn fetch_bytes() -> Result<Vec<u8>, VersionError> {
    let mut resp = client()?.send(request()?)?;
    if !resp.status().is_success() {
        return Err(VersionError::Status(resp.status().as_u16()));
    }
    Ok(resp.bytes()?)
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
    #[test_case("0.5.0-rc.14", "0.5.0-rc.13", true  ; "rc_bump")]
    #[test_case("0.5.0-rc.13", "0.5.0-rc.14", false ; "rc_older")]
    #[test_case("0.5.0-rc.13", "0.5.0-rc.13", false ; "rc_equal")]
    #[test_case("0.5.0",       "0.5.0-rc.1", true  ; "release_beats_its_prereleases")]
    #[test_case("0.5.0-rc.1",  "0.5.0",      false ; "prerelease_loses_to_release")]
    #[test_case("1.0.0-rc.1",  "0.9.0",      true  ; "prerelease_of_a_higher_core")]
    #[test_case("0.5.0-rc.2",  "0.5.0-rc.10", false ; "rc_number_compares_numerically")]
    #[test_case("0.5.0-rc.1.1", "0.5.0-rc.1", true ; "longer_prerelease_wins_after_prefix")]
    #[test_case("0.5", "0.5.0", false ; "missing_core_segment_is_not_newer")]
    fn is_newer_cases(latest: &str, current: &str, expected: bool) {
        assert_eq!(is_newer(latest, current), expected);
    }
}
