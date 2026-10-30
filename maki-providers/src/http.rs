//! The transport rules the provider client and maki.net share: which proxy a
//! URL goes through and how names are looked up. How a failed request is read
//! is shared by the MCP transport too.

use std::env;
use std::error::Error;
use std::io;
use std::iter;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;

use maki_config::split_host_port;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Error as ReqwestError, Proxy};
use smol::{Timer, unblock};
use thiserror::Error as ThisError;

pub const HTTP_SCHEME: &str = "http://";
pub const HTTPS_SCHEME: &str = "https://";
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;
pub const DNS_ATTEMPTS: u32 = 3;
const NO_PROXY_ALL: &str = "*";
const NO_PROXY_SEPARATOR: char = ',';
const NO_PROXY_BLANKS: [char; 2] = [' ', '\t'];
const DNS_RETRY_DELAY: Duration = Duration::from_millis(150);
pub(crate) const NO_ADDRESSES: &str = "the name has no addresses";
/// How a connect fails when nothing answers, besides a name that does not
/// resolve.
const UNANSWERED: [io::ErrorKind; 3] = [
    io::ErrorKind::ConnectionRefused,
    io::ErrorKind::HostUnreachable,
    io::ErrorKind::NetworkUnreachable,
];

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

/// curl's `NO_PROXY` rule for a host name: an entry matches the name itself
/// and every subdomain, with leading and trailing dots ignored.
fn no_proxy_matches(no_proxy: &str, host: &str) -> bool {
    if no_proxy == NO_PROXY_ALL {
        return true;
    }
    let host = host.trim_end_matches('.').as_bytes();
    for segment in no_proxy.split(NO_PROXY_SEPARATOR) {
        let segment = segment.trim_matches(NO_PROXY_BLANKS);
        let entry = segment
            .split_once(NO_PROXY_BLANKS)
            .map_or(segment, |(entry, _)| entry);
        if domain_matches(entry.trim_matches('.').as_bytes(), host) {
            return true;
        }
        // curl stops reading the list at a blank inside an entry.
        if entry.len() < segment.len() {
            return false;
        }
    }
    false
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

/// getaddrinfo blocks, so it runs on the blocking pool rather than on the
/// executor thread every other future shares. A resolver saying "try again"
/// (a cold cache, a link that just came back) has not answered yet, so a few
/// retries go out before the lookup counts as a failure.
pub async fn resolve(host: &str, port: u16, attempts: u32) -> io::Result<Vec<SocketAddr>> {
    let mut attempt = 1;
    loop {
        let target = (host.to_string(), port);
        match unblock(move || target.to_socket_addrs()).await {
            Ok(addrs) => return Ok(addrs.collect()),
            Err(e) if attempt >= attempts => return Err(e),
            Err(e) => {
                tracing::debug!(host, port, attempt, error = %e, "name lookup failed, retrying")
            }
        }
        attempt += 1;
        Timer::after(DNS_RETRY_DELAY).await;
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

/// Nothing answered: no socket listening, no route to it, or a name that does
/// not resolve. A connect timeout stays out, as it does for the provider's own
/// requests, since it reads the same as a stall.
pub fn is_connect_failure(error: &ReqwestError) -> bool {
    error.is_connect()
        && !error.is_timeout()
        && causes(error).any(|e| {
            e.is::<Unresolved>()
                || e.downcast_ref::<io::Error>()
                    .is_some_and(|e| UNANSWERED.contains(&e.kind()))
        })
}

fn causes<'a>(error: &'a (dyn Error + 'static)) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    iter::successors(Some(error), |&e| e.source())
}

#[derive(Debug, ThisError)]
#[error("cannot resolve {host}")]
pub(crate) struct Unresolved {
    pub(crate) host: String,
    pub(crate) source: io::Error,
}

/// The client's own name lookups. They run here rather than in reqwest so a
/// name that does not resolve, or resolves to nothing, fails as
/// [`Unresolved`], which is how [`is_connect_failure`] knows it.
pub struct Lookup;

impl Resolve for Lookup {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            match resolve(&host, 0, DNS_ATTEMPTS).await {
                Ok(addrs) if !addrs.is_empty() => Ok(Box::new(addrs.into_iter()) as Addrs),
                Ok(_) => Err(Unresolved {
                    host,
                    source: io::Error::other(NO_ADDRESSES),
                }
                .into()),
                Err(source) => Err(Unresolved { host, source }.into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_compat::Compat;
    use reqwest::Client;
    use std::sync::Arc;
    use test_case::test_case;

    use crate::test_support::transport::{
        LOOPBACK_HOST, QUERY_SECRET, UNRESOLVABLE_HOST, URL_MARKER, closed_port, plain_http_port,
    };

    const PROXY_URL: &str = "http://127.0.0.1:3128";
    const SOCKS_PROXY_URL: &str = "socks5://127.0.0.1:1080";
    /// An address rather than a name, so no test needs a DNS answer.
    const PUBLIC_URL: &str = "https://8.8.8.8/";
    const PUBLIC_HTTP_URL: &str = "http://8.8.8.8/";

    /// reqwest arms its timeouts as `send` is called, so the call has to
    /// happen inside the future `Compat` runs, never before. The shell's
    /// proxy is left out, so the request goes where the test points it.
    fn failed_request(url: &str) -> ReqwestError {
        let client = Client::builder()
            .no_proxy()
            .dns_resolver(Arc::new(Lookup))
            .build()
            .unwrap();
        smol::block_on(Compat::new(async {
            client.get(url).send().await.map(|r| r.status())
        }))
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

    #[test_case(closed_port_url, true ; "a_closed_port_draws_on_the_connect_budget")]
    #[test_case(unresolvable_url, true ; "an_unresolvable_name_draws_on_the_connect_budget")]
    #[test_case(plain_tcp_behind_https_url, false ; "a_failed_handshake_retries_as_transient")]
    fn transport_failures_classify_like_a_providers(url: fn() -> String, expected: bool) {
        let error = failed_request(&url());
        assert_eq!(
            is_connect_failure(&error),
            expected,
            "{}",
            error_chain(&error)
        );
    }

    #[test]
    fn error_chain_leaves_out_the_url() {
        let error = failed_request(&format!("{}?key={QUERY_SECRET}", closed_port_url()));
        let chain = error_chain(&error);
        assert!(!chain.contains(QUERY_SECRET), "{chain}");
        assert!(!chain.contains(URL_MARKER), "{chain}");
    }
}
