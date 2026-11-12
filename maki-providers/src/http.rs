//! The proxy rules the provider client and maki.net share, and the name lookup
//! maki.net vets addresses with. How a failed request is read is shared by the
//! MCP transport too.

use std::env;
use std::error::Error;
use std::io;
use std::iter;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::LazyLock;
use std::time::Duration;

use maki_config::split_host_port;
use reqwest::{Error as ReqwestError, Proxy};
use tokio::net::lookup_host;
use tokio::time::sleep;

pub const HTTP_SCHEME: &str = "http://";
pub const HTTPS_SCHEME: &str = "https://";
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;
pub const DNS_ATTEMPTS: u32 = 3;
const NO_PROXY_ALL: &str = "*";
const NO_PROXY_SEPARATOR: char = ',';
const NO_PROXY_BLANKS: [char; 2] = [' ', '\t'];
const CIDR_SEPARATOR: char = '/';
const DNS_RETRY_DELAY: Duration = Duration::from_millis(150);

static PROXY_ENV: LazyLock<ProxyEnv> =
    LazyLock::new(|| ProxyEnv::from_env(|name| env::var(name).ok()));

/// The proxy settings read from the environment once, for the process.
pub fn proxy_env() -> &'static ProxyEnv {
    &PROXY_ENV
}

/// Which proxy a request goes through, and so whether the proxy resolves the
/// host name instead of us. Sandboxes like nono block DNS and only let traffic
/// out through their proxy, so there the proxy has to resolve names for us.
///
/// The guard and the client have to agree exactly. If we think the proxy
/// resolves a name but the client resolves it, the client connects to an
/// address the guard never checked. So the client is handed its proxy from
/// [`Self::proxy_for`] too, instead of reading the environment by its own rules.
#[derive(Debug, Default)]
pub struct ProxyEnv {
    http: Option<String>,
    https: Option<String>,
    no_proxy: String,
}

impl ProxyEnv {
    /// Same rules as curl. An empty variable counts as unset. Plain http only
    /// reads lowercase `http_proxy`, because under CGI a request's `Proxy:`
    /// header turns into `HTTP_PROXY` and would let a client pick our proxy.
    /// A proxy reqwest cannot parse counts as unset too, since the client goes
    /// direct past it.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let var = |name: &str| var(name).filter(|value| !value.is_empty());
        let proxy = |name: &str| var(name).filter(|value| Proxy::all(value.as_str()).is_ok());
        let all = || proxy("all_proxy").or_else(|| proxy("ALL_PROXY"));
        Self {
            http: proxy("http_proxy").or_else(all),
            https: proxy("https_proxy")
                .or_else(|| proxy("HTTPS_PROXY"))
                .or_else(all),
            no_proxy: var("no_proxy")
                .or_else(|| var("NO_PROXY"))
                .unwrap_or_default(),
        }
    }

    pub fn proxy_for(&self, url: &str) -> Option<&str> {
        let (host, _) = extract_host_port(url)?;
        let proxy = if url.starts_with(HTTPS_SCHEME) {
            &self.https
        } else {
            &self.http
        };
        proxy
            .as_deref()
            .filter(|_| !no_proxy_matches(&self.no_proxy, host))
    }
}

/// curl's `NO_PROXY` rule for a host: an entry matches the name itself and
/// every subdomain, with leading and trailing dots ignored, and an address
/// matches an entry in CIDR notation that covers it.
fn no_proxy_matches(no_proxy: &str, host: &str) -> bool {
    if no_proxy == NO_PROXY_ALL {
        return true;
    }
    let host = host.trim_end_matches('.');
    let address = host.parse::<IpAddr>().ok();
    for segment in no_proxy.split(NO_PROXY_SEPARATOR) {
        let segment = segment.trim_matches(NO_PROXY_BLANKS);
        let entry = segment
            .split_once(NO_PROXY_BLANKS)
            .map_or(segment, |(entry, _)| entry);
        if domain_matches(entry.trim_matches('.').as_bytes(), host.as_bytes())
            || address.is_some_and(|address| cidr_matches(entry, address))
        {
            return true;
        }
        // curl stops reading the list at a blank inside an entry.
        if entry.len() < segment.len() {
            return false;
        }
    }
    false
}

/// `10.0.0.0/8` or `fe80::/10`: the address shares the network's leading
/// bits. An entry of the other address family never matches.
fn cidr_matches(entry: &str, address: IpAddr) -> bool {
    let Some((network, bits)) = entry.split_once(CIDR_SEPARATOR) else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return false;
    };
    let (network, address, width) = match (network.parse::<IpAddr>(), address) {
        (Ok(IpAddr::V4(network)), IpAddr::V4(address)) => (
            u128::from(u32::from(network)),
            u128::from(u32::from(address)),
            Ipv4Addr::BITS,
        ),
        (Ok(IpAddr::V6(network)), IpAddr::V6(address)) => {
            (u128::from(network), u128::from(address), Ipv6Addr::BITS)
        }
        _ => return false,
    };
    bits <= width && (network ^ address).checked_shr(width - bits).unwrap_or(0) == 0
}

fn domain_matches(entry: &[u8], host: &[u8]) -> bool {
    !entry.is_empty()
        && host.len().checked_sub(entry.len()).is_some_and(|split| {
            let (head, tail) = host.split_at(split);
            tail.eq_ignore_ascii_case(entry) && (head.is_empty() || head.ends_with(b"."))
        })
}

/// Host and port of an `http(s)` URL. Any userinfo is dropped, so
/// `https://example.com@127.0.0.1/` is seen for the loopback address it is.
pub fn extract_host_port(url: &str) -> Option<(&str, u16)> {
    let (rest, default_port) = url
        .strip_prefix(HTTPS_SCHEME)
        .map(|rest| (rest, HTTPS_PORT))
        .or_else(|| url.strip_prefix(HTTP_SCHEME).map(|rest| (rest, HTTP_PORT)))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = split_host_port(authority)?;
    Some((host, port.unwrap_or(default_port)))
}

/// A resolver saying "try again" (a cold cache, a link that just came back)
/// has not answered yet, so a few retries go out before the lookup counts as
/// a failure.
pub async fn resolve(host: &str, port: u16, attempts: u32) -> io::Result<Vec<SocketAddr>> {
    let mut attempt = 1;
    loop {
        match lookup_host((host, port)).await {
            Ok(addrs) => return Ok(addrs.collect()),
            Err(e) if attempt >= attempts => return Err(e),
            Err(e) => {
                tracing::debug!(host, port, attempt, error = %e, "name lookup failed, retrying")
            }
        }
        attempt += 1;
        sleep(DNS_RETRY_DELAY).await;
    }
}

/// reqwest's own message is only the outermost layer, "error sending request",
/// so the causes beneath it are spelled out. Each layer loses the ` for url
/// (..)` reqwest appends, since a URL can carry a key in the path or query.
pub fn error_chain(error: &(dyn Error + 'static)) -> String {
    causes(error)
        .map(
            |e| match e.downcast_ref::<ReqwestError>().and_then(ReqwestError::url) {
                Some(url) => e.to_string().replace(&format!(" for url ({url})"), ""),
                None => e.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join(": ")
}

/// The connection was never made: no socket listening, no route to it, a name
/// that does not resolve, a handshake that failed, or a proxy that refused the
/// tunnel.
///
/// A timeout stays out: both the connect timeout and the read timeout answer
/// `is_timeout()`, and an expired connect and a stall on a stream that was
/// already flowing want opposite budgets, which the error alone cannot say.
/// So a timeout retries as transient, unbounded, and a blackholed SYN keeps
/// trying where a refused port gives up in seconds. Splitting the two means
/// tracking whether any byte ever arrived.
pub fn is_connect_failure(error: &ReqwestError) -> bool {
    error.is_connect() && !error.is_timeout()
}

fn causes<'a>(error: &'a (dyn Error + 'static)) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    iter::successors(Some(error), |&e| e.source())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;
    use test_case::test_case;

    use crate::test_support::transport::{
        LOOPBACK_HOST, QUERY_SECRET, UNRESOLVABLE_HOST, URL_MARKER, closed_port, hangup_port,
        plain_http_port,
    };

    const PROXY_URL: &str = "http://127.0.0.1:3128";
    const SOCKS_PROXY_URL: &str = "socks5://127.0.0.1:1080";
    /// An address rather than a name, so no test needs a DNS answer.
    const PUBLIC_URL: &str = "https://8.8.8.8/";
    const PUBLIC_HTTP_URL: &str = "http://8.8.8.8/";

    /// The shell's proxy is left out, so the request goes where the test points it.
    async fn failed_request(url: &str) -> ReqwestError {
        let client = Client::builder().no_proxy().build().unwrap();
        client
            .get(url)
            .send()
            .await
            .map(|r| r.status())
            .unwrap_err()
    }

    #[test_case("HTTPS_PROXY", PROXY_URL, false, true ; "uppercase_https_proxy")]
    #[test_case("ALL_PROXY", PROXY_URL, true, true ; "all_proxy_covers_both")]
    #[test_case("http_proxy", PROXY_URL, true, false ; "http_proxy_leaves_https_direct")]
    #[test_case("HTTP_PROXY", PROXY_URL, false, false ; "uppercase_http_proxy_ignored_like_curl")]
    #[test_case("HTTPS_PROXY", "", false, false ; "empty_counts_as_unset")]
    #[test_case("ALL_PROXY", SOCKS_PROXY_URL, true, true ; "a_socks_proxy_parses")]
    #[test_case("ALL_PROXY", "127.0.0.1:3128", true, true ; "no_scheme_is_http")]
    #[test_case("ALL_PROXY", "http://bad host:3128", false, false ; "unparseable_counts_as_unset")]
    fn proxy_for_reads_what_curl_reads(var: &str, value: &str, http: bool, https: bool) {
        let proxy_env = ProxyEnv::from_env(|name| (name == var).then(|| value.to_string()));
        let proxied = (
            proxy_env.proxy_for(PUBLIC_HTTP_URL).is_some(),
            proxy_env.proxy_for(PUBLIC_URL).is_some(),
        );
        assert_eq!(proxied, (http, https));
    }

    #[test]
    fn a_scheme_proxy_shadows_all_proxy() {
        let proxy_env = ProxyEnv::from_env(|name| match name {
            "https_proxy" => Some(SOCKS_PROXY_URL.to_string()),
            "ALL_PROXY" => Some(PROXY_URL.to_string()),
            _ => None,
        });
        assert_eq!(proxy_env.proxy_for(PUBLIC_URL), Some(SOCKS_PROXY_URL));
    }

    #[test_case("*", "example.com", true ; "wildcard")]
    #[test_case("example.com", "example.com", true ; "exact")]
    #[test_case("example.com", "API.Example.com", true ; "subdomain_any_case")]
    #[test_case(".example.com", "example.com", true ; "leading_dot")]
    #[test_case("example.com.", "example.com.", true ; "trailing_dots")]
    #[test_case("localhost, example.com", "example.com", true ; "comma_and_space_list")]
    #[test_case("localhost,\texample.com", "example.com", true ; "tab_after_comma")]
    #[test_case("localhost example.com", "example.com", false ; "blank_ends_the_list")]
    #[test_case("example.com", "badexample.com", false ; "suffix_without_dot")]
    #[test_case("api.example.com", "example.com", false ; "longer_entry")]
    #[test_case("10.0.0.0/8", "10.1.2.20", true ; "ipv4_cidr")]
    #[test_case("10.0.0.0/8", "11.1.2.20", false ; "outside_the_ipv4_cidr")]
    #[test_case("10.0.0.0/0", "192.168.1.1", true ; "a_zero_prefix_covers_every_address")]
    #[test_case("10.0.0.0/33", "10.1.2.20", false ; "a_prefix_past_the_width")]
    #[test_case("fe80::/10", "fe80::1", true ; "ipv6_cidr")]
    #[test_case("10.0.0.0/8", "fe80::1", false ; "a_cidr_of_the_other_family")]
    #[test_case("10.0.0.0/8", "10.example.com", false ; "a_cidr_needs_an_address")]
    fn no_proxy_matches_cases(no_proxy: &str, host: &str, expected: bool) {
        assert_eq!(no_proxy_matches(no_proxy, host), expected);
    }

    #[test_case("https://example.com", Some(("example.com", HTTPS_PORT)) ; "simple_domain")]
    #[test_case("http://example.com", Some(("example.com", HTTP_PORT)) ; "http_default_port")]
    #[test_case("https://example.com:8080/path", Some(("example.com", 8080)) ; "domain_with_port")]
    #[test_case("https://[::1]/path", Some(("::1", HTTPS_PORT)) ; "bracketed_ipv6")]
    #[test_case("https://[::1]:8080/path", Some(("::1", 8080)) ; "bracketed_ipv6_with_port")]
    #[test_case("https://192.168.1.1:443", Some(("192.168.1.1", HTTPS_PORT)) ; "ipv4_with_port")]
    #[test_case("https://user:pw@10.0.0.1/", Some(("10.0.0.1", HTTPS_PORT)) ; "userinfo_stripped")]
    #[test_case("https://example.com?a=/b", Some(("example.com", HTTPS_PORT)) ; "query_before_path")]
    #[test_case("not-a-url", None ; "no_scheme")]
    fn extract_host_port_cases(url: &str, expected: Option<(&str, u16)>) {
        assert_eq!(extract_host_port(url), expected);
    }

    fn closed_port_url() -> String {
        format!("http://{LOOPBACK_HOST}:{}/", closed_port())
    }

    fn unresolvable_url() -> String {
        format!("https://{UNRESOLVABLE_HOST}/")
    }

    fn plain_tcp_behind_https_url() -> String {
        format!("https://{LOOPBACK_HOST}:{}/", plain_http_port())
    }

    fn hangup_url() -> String {
        format!("http://{LOOPBACK_HOST}:{}/", hangup_port())
    }

    #[test_case(closed_port_url, true ; "a_closed_port")]
    #[test_case(unresolvable_url, true ; "an_unresolvable_name")]
    #[test_case(plain_tcp_behind_https_url, true ; "a_failed_handshake")]
    #[test_case(hangup_url, false ; "a_hangup_after_connecting")]
    #[tokio::test]
    async fn only_a_connection_never_made_is_a_connect_failure(
        url: fn() -> String,
        expected: bool,
    ) {
        let error = failed_request(&url()).await;
        assert_eq!(
            is_connect_failure(&error),
            expected,
            "{}",
            error_chain(&error)
        );
    }

    #[tokio::test]
    async fn error_chain_leaves_out_the_url() {
        let error = failed_request(&format!("{}?key={QUERY_SECRET}", closed_port_url())).await;
        let chain = error_chain(&error);
        assert!(!chain.contains(QUERY_SECRET), "{chain}");
        assert!(!chain.contains(URL_MARKER), "{chain}");
    }
}
