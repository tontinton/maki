use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_lite::io::AsyncReadExt;
use isahc::config::{Configurable, RedirectPolicy, ResolveMap, VersionNegotiation};
use isahc::{AsyncBody, HttpClient, Request, Response};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table};
use smol::{Timer, unblock};
use url::Url;

use crate::api::util::pair::{Pair, try_pair};

use crate::plugin_permissions::PluginPermissions;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_RETRIES: u32 = 3;
const MAX_REDIRECTS: u32 = 10;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CF_MITIGATED: &str = "cf-mitigated";
const CF_CHALLENGE: &str = "challenge";
const FALLBACK_USER_AGENT: &str = "maki";
const HTTP_SCHEME: &str = "http://";
const HTTPS_SCHEME: &str = "https://";
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;
const DNS_ATTEMPTS: u32 = 3;
const DNS_RETRY_DELAY: Duration = Duration::from_millis(150);
const ALLOWLIST_HINT: &str = "add it to `net.allowed_private_hosts` in your init.lua to allow it";
/// Reserved IPv4 ranges the standard library has no predicate for. Carrier
/// grade NAT is the one that bites: Alibaba Cloud parks its instance metadata
/// service on it at 100.100.100.200. Then protocol assignments, benchmarking,
/// and everything from 240.0.0.0 up, which takes in the broadcast address.
const RESERVED_V4_NETS: [(Ipv4Addr, u8); 4] = [
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];
/// Credentials handed to one authority are not for whoever it redirects us to.
/// The same set isahc scrubbed before redirects were followed by hand.
const CROSS_AUTHORITY_HEADERS: [&str; 5] = [
    "authorization",
    "cookie",
    "cookie2",
    "proxy-authorization",
    "www-authenticate",
];

/// Hosts the user marked as safe to reach on a private address, from
/// `net.allowed_private_hosts`. Process-wide because the guard sits far below
/// the config: every `maki.net` call, in any plugin, on any Lua thread, reads
/// the same list, and `/reload` swaps it.
static ALLOWED_PRIVATE_HOSTS: LazyLock<ArcSwap<HostAllowlist>> = LazyLock::new(ArcSwap::default);

/// Applies `net.allowed_private_hosts`. Entries that parse as neither a host,
/// a `host:port`, nor a CIDR range are dropped with a warning.
pub fn set_allowed_private_hosts(entries: &[String]) {
    ALLOWED_PRIVATE_HOSTS.store(Arc::new(HostAllowlist::parse(entries)));
}

/// Split by how far a rule may be trusted: a name only ever answers for the
/// host written in the URL, a range answers for resolved addresses too.
#[derive(Debug, Default)]
struct HostAllowlist {
    /// Name and the port it is pinned to, `None` for any port.
    names: Vec<(String, Option<u16>)>,
    /// Network address, prefix length, and port as above.
    nets: Vec<(IpAddr, u8, Option<u16>)>,
}

impl HostAllowlist {
    fn parse(entries: &[String]) -> Self {
        let mut list = Self::default();
        for entry in entries {
            if list.add(entry.trim()).is_none() {
                tracing::warn!(
                    entry,
                    "ignoring unparseable net.allowed_private_hosts entry"
                );
            }
        }
        list
    }

    fn add(&mut self, entry: &str) -> Option<()> {
        if let Some((addr, prefix)) = entry.split_once('/') {
            let addr: IpAddr = addr.parse().ok()?;
            let prefix = prefix.parse().ok().filter(|p| *p <= address_bits(addr))?;
            self.nets.push((addr, prefix, None));
            return Some(());
        }
        let (host, port) = split_host_port(entry);
        if host.is_empty() {
            return None;
        }
        match host.parse::<IpAddr>() {
            Ok(addr) => self.nets.push((addr, address_bits(addr), port)),
            Err(_) => self.names.push((host.to_string(), port)),
        }
        Some(())
    }

    /// Answers for the host in the URL, before any DNS lookup: a name the user
    /// wrote is trusted whatever it resolves to.
    fn allows_host(&self, host: &str, port: u16) -> bool {
        self.names
            .iter()
            .any(|(name, allowed)| port_matches(*allowed, port) && name.eq_ignore_ascii_case(host))
            || host.parse().is_ok_and(|ip| self.allows_ip(ip, port))
    }

    /// Once DNS has spoken only the ranges count, so a name the user did not
    /// write cannot borrow another name's exemption, though it is let through
    /// when it resolves into a range the user opened.
    fn allows_ip(&self, ip: IpAddr, port: u16) -> bool {
        self.nets.iter().any(|(net, prefix, allowed)| {
            port_matches(*allowed, port) && ip_in_net(ip, *net, *prefix)
        })
    }
}

fn port_matches(allowed: Option<u16>, port: u16) -> bool {
    allowed.is_none_or(|allowed| allowed == port)
}

/// Width of the address, which is also the prefix of a rule naming one host.
fn address_bits(ip: IpAddr) -> u8 {
    if ip.is_ipv4() { 32 } else { 128 }
}

/// An IPv4 range also covers the `::ffff:` spelling of an address in it, while
/// an IPv6 range never covers IPv4.
fn ip_in_net(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            leading_bits_match(ip.to_bits().into(), net.to_bits().into(), prefix, u32::BITS)
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            leading_bits_match(ip.to_bits(), net.to_bits(), prefix, u128::BITS)
        }
        (IpAddr::V6(ip), IpAddr::V4(_)) => ip
            .to_ipv4_mapped()
            .is_some_and(|ip| ip_in_net(ip.into(), net, prefix)),
        (IpAddr::V4(_), IpAddr::V6(_)) => false,
    }
}

/// `add` refuses a prefix longer than the address, so the shift stays in
/// range, and a `/0` gets its own answer because shifting by the full width
/// would panic.
fn leading_bits_match(ip: u128, net: u128, prefix: u8, width: u32) -> bool {
    prefix == 0 || (ip ^ net) >> (width - u32::from(prefix)) == 0
}

/// Splits an authority into host and port, leaving a bare IPv6 literal like
/// `::1` (more than one colon, no brackets) whole.
fn split_host_port(authority: &str) -> (&str, Option<u16>) {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').unwrap_or((rest, ""));
        return (host, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            port.parse().map_or((authority, None), |p| (host, Some(p)))
        }
        _ => (authority, None),
    }
}

/// The address the SSRF guard actually vetted for the host in the URL.
///
/// Without it the name is looked up twice, once by the guard and once by curl
/// at connect time, and a record with a zero TTL can answer public to the
/// first and 169.254.169.254 to the second.
#[derive(Debug)]
struct DnsPin {
    host: String,
    port: u16,
    addr: IpAddr,
}

struct RequestParams {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    max_bytes: usize,
    retries: u32,
    /// `None` when the guard reached its verdict without DNS.
    pin: Option<DnsPin>,
}

struct ResponseData {
    body: String,
    status: u16,
    content_type: String,
}

/// Make an HTTP request and return the response body. Plain `http://`
/// URLs are automatically upgraded to `https://`. Requests to private
/// or metadata IP addresses are blocked for safety, unless the host is
/// listed in `net.allowed_private_hosts`.
///
/// {opts} fields:
///   `method` (string) HTTP verb (default `"GET"`).
///   `headers` (table) Header name/value pairs.
///   `body` (string) Request body.
///   `timeout` (integer) Timeout in seconds, max 120 (default 30).
///   `max_bytes` (integer) Max response size in bytes (default 5 MB).
///   `retry` (integer) Retries on 5xx errors (default 3).
///
/// The response table has three fields: `body` (string), `status`
/// (integer), and `content_type` (string).
///
/// @param url string URL starting with `http://` or `https://`.
/// @param opts table? Request options (see above).
/// @return (table?, string?) Response table, or nil plus an error string.
/// @example
/// local res, err = maki.net.request("https://httpbin.org/get")
/// if err then
///   print("failed: " .. err)
/// else
///   print(res.status, res.body)
/// end
#[lua_fn(guard = Net)]
async fn request(lua: Lua, url: String, opts: Option<Table>) -> LuaResult<Pair<Table>> {
    let params = try_pair!(extract_request_params(&url, opts.as_ref()).await);
    let resp = try_pair!(do_request(params).await);
    let tbl = lua.create_table()?;
    tbl.set("body", resp.body)?;
    tbl.set("status", resp.status)?;
    tbl.set("content_type", resp.content_type)?;
    Ok((Some(tbl), None))
}

lua_table! {
    /// HTTP client for fetching web content. All traffic goes over HTTPS
    /// (plain HTTP is upgraded). Private and metadata IP addresses are
    /// blocked to prevent SSRF, including after a redirect. Hosts listed in
    /// the `net.allowed_private_hosts` config option are exempt.
    /// Failed requests (5xx) are retried automatically.
    ///
    /// ```lua
    /// local res, err = maki.net.request("https://example.com")
    /// if res then print(res.body) end
    /// ```
    "maki.net" => pub(crate) fn create_net_table(perms: &PluginPermissions), DOCS [
        request(perms),
    ]
}

async fn extract_request_params(url: &str, opts: Option<&Table>) -> Result<RequestParams, String> {
    let allowed = ALLOWED_PRIVATE_HOSTS.load_full();
    let url = validate_and_upgrade_url(url, &allowed)?;
    let pin = check_ssrf(&url, &allowed).await?;

    let method = opts
        .and_then(|o| o.get::<String>("method").ok())
        .unwrap_or_else(|| "GET".to_string());

    let headers = if let Some(tbl) = opts.and_then(|o| o.get::<Table>("headers").ok()) {
        let mut h = Vec::new();
        for pair in tbl.pairs::<String, String>() {
            let (k, v) = pair.map_err(|e| format!("invalid header: {e}"))?;
            h.push((k, v));
        }
        h
    } else {
        Vec::new()
    };

    let body = opts
        .and_then(|o| o.get::<String>("body").ok())
        .map(|s| s.into_bytes())
        .unwrap_or_default();

    let timeout = Duration::from_secs(
        opts.and_then(|o| o.get::<u64>("timeout").ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );

    let max_bytes = opts
        .and_then(|o| o.get::<usize>("max_bytes").ok())
        .unwrap_or(DEFAULT_MAX_BYTES);

    let retries = opts
        .and_then(|o| o.get::<u32>("retry").ok())
        .unwrap_or(MAX_RETRIES);

    Ok(RequestParams {
        url,
        method,
        headers,
        body,
        timeout,
        max_bytes,
        retries,
        pin,
    })
}

fn build_request(
    url: &str,
    user_agent: &str,
    method: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<Request<AsyncBody>, String> {
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .header("User-Agent", user_agent);

    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }

    builder
        .body(AsyncBody::from(body))
        .map_err(|e| format!("request build error: {e}"))
}

async fn send_with_retries(
    client: &HttpClient,
    params: &RequestParams,
) -> Result<Response<AsyncBody>, String> {
    let is_get = params.method.eq_ignore_ascii_case("GET");
    let mut last_err = String::new();

    'retry: {
        for attempt in 0..=params.retries {
            let req = build_request(
                &params.url,
                USER_AGENT,
                &params.method,
                &params.headers,
                params.body.clone(),
            )?;
            match client.send_async(req).await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let is_cf_challenge = status == 403
                        && resp
                            .headers()
                            .get(CF_MITIGATED)
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v.contains(CF_CHALLENGE));

                    if is_cf_challenge && is_get {
                        let req = build_request(
                            &params.url,
                            FALLBACK_USER_AGENT,
                            &params.method,
                            &params.headers,
                            params.body.clone(),
                        )?;
                        match client.send_async(req).await {
                            Ok(resp) => break 'retry Ok(resp),
                            Err(e) => last_err = format!("request failed: {e}"),
                        }
                    } else if status >= 500 && attempt < params.retries {
                        last_err = format!("HTTP {status}");
                        continue;
                    } else {
                        break 'retry Ok(resp);
                    }
                }
                Err(e) => last_err = format!("request failed: {e}"),
            }
        }
        Err(last_err)
    }
}

fn redirect_location(response: &Response<AsyncBody>) -> Option<String> {
    if !response.status().is_redirection() {
        return None;
    }
    let location = response.headers().get("location")?;
    if let Ok(location) = location.to_str() {
        return Some(location.to_string());
    }
    // Misconfigured servers put raw bytes in `Location` and browsers recover
    // from it, so encode them rather than drop the hop and return an empty body
    // as if the redirect had never been sent.
    let mut encoded = String::new();
    for &byte in location.as_bytes() {
        match byte {
            0x21..=0x7E => encoded.push(char::from(byte)),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(encoded)
}

/// One client per hop, because a DNS override is a property of the curl handle
/// isahc builds the client around and cannot be attached to a single request.
/// The pin changes with every redirect, so the client has to as well.
fn build_client(params: &RequestParams) -> Result<HttpClient, String> {
    let mut builder = HttpClient::builder()
        .timeout(params.timeout)
        // Redirects are followed by hand, so every hop goes through the SSRF
        // check. Left to curl, a URL that passed the check could still bounce
        // us into 169.254.169.254.
        .redirect_policy(RedirectPolicy::None)
        // The workspace enables curl's http2 feature for OTLP over gRPC. This
        // client fetches arbitrary user URLs, so keep it on HTTP/1.1 rather
        // than change how every one of them is negotiated.
        .version_negotiation(VersionNegotiation::http11());

    // Connect to the address the guard vetted instead of asking DNS again and
    // trusting whatever the second answer says.
    if let Some(pin) = &params.pin {
        builder = builder.dns_resolve(ResolveMap::new().add(&pin.host, pin.port, pin.addr));
    }

    builder.build().map_err(|e| format!("client error: {e}"))
}

async fn do_request(mut params: RequestParams) -> Result<ResponseData, String> {
    let allowed = ALLOWED_PRIVATE_HOSTS.load_full();
    let mut response = send_with_retries(&build_client(&params)?, &params).await?;

    for _ in 0..MAX_REDIRECTS {
        let Some(location) = redirect_location(&response) else {
            break;
        };
        params
            .follow_redirect(response.status().as_u16(), &location, &allowed)
            .await?;
        response = send_with_retries(&build_client(&params)?, &params).await?;
    }
    if redirect_location(&response).is_some() {
        return Err(format!("gave up after {MAX_REDIRECTS} redirects"));
    }

    let status = response.status().as_u16();

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if let Some(len) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && len > params.max_bytes
    {
        return Err(format!("response too large: {len} bytes"));
    }

    let mut bytes = Vec::new();
    response
        .body_mut()
        .take((params.max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("read error: {e}"))?;

    if bytes.len() > params.max_bytes {
        return Err(format!("response too large: {} bytes", bytes.len()));
    }

    let body = String::from_utf8_lossy(&bytes).into_owned();
    Ok(ResponseData {
        body,
        status,
        content_type,
    })
}

impl RequestParams {
    /// Points the request at a redirect target after putting it through the
    /// same scheme and SSRF rules as the URL the caller asked for.
    async fn follow_redirect(
        &mut self,
        status: u16,
        location: &str,
        allowed: &HostAllowlist,
    ) -> Result<(), String> {
        let base = Url::parse(&self.url).map_err(|e| format!("invalid URL {}: {e}", self.url))?;
        let target = base
            .join(location)
            .map_err(|e| format!("invalid redirect to {location}: {e}"))?;
        let target = validate_and_upgrade_url(target.as_str(), allowed)?;
        let pin = check_ssrf(&target, allowed).await?;

        let landed =
            Url::parse(&target).map_err(|e| format!("invalid redirect to {location}: {e}"))?;
        if authority(&base) != authority(&landed) {
            self.headers.retain(|(name, _)| {
                !CROSS_AUTHORITY_HEADERS
                    .iter()
                    .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
            });
        }

        // 301, 302 and 303 turn anything that is not a read into a `GET`, the
        // way browsers and curl do it. 307 and 308 keep method and body.
        let is_read =
            self.method.eq_ignore_ascii_case("GET") || self.method.eq_ignore_ascii_case("HEAD");
        if matches!(status, 301..=303) && !is_read {
            self.method = "GET".to_string();
            self.body.clear();
        }
        self.url = target;
        self.pin = pin;
        Ok(())
    }
}

/// What decides whether a redirect stays within the same authority, the same
/// triple isahc compared before dropping credentials.
fn authority(url: &Url) -> (&str, Option<&str>, Option<u16>) {
    (url.scheme(), url.host_str(), url.port_or_known_default())
}

/// Host and port of an `http(s)` URL. Any userinfo is dropped, so
/// `https://example.com@127.0.0.1/` is seen for the loopback address it is.
fn extract_host_port(url: &str) -> Option<(&str, u16)> {
    let (rest, default_port) = url
        .strip_prefix(HTTPS_SCHEME)
        .map(|rest| (rest, HTTPS_PORT))
        .or_else(|| url.strip_prefix(HTTP_SCHEME).map(|rest| (rest, HTTP_PORT)))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if authority.is_empty() {
        return None;
    }
    let (host, port) = split_host_port(authority);
    Some((host, port.unwrap_or(default_port)))
}

/// getaddrinfo blocks, so it runs on the blocking pool rather than on the
/// executor thread every other plugin future shares. A resolver saying "try
/// again" (a cold cache, a link that just came back) has not answered yet, so a
/// couple of retries go out before the lookup counts as a failure.
async fn resolve(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let mut attempt = 1;
    loop {
        let target = (host.to_string(), port);
        match unblock(move || target.to_socket_addrs()).await {
            Ok(addrs) => return Ok(addrs.collect()),
            Err(e) if attempt == DNS_ATTEMPTS => return Err(e),
            Err(e) => {
                tracing::debug!(host, port, attempt, error = %e, "name lookup failed, retrying")
            }
        }
        attempt += 1;
        Timer::after(DNS_RETRY_DELAY).await;
    }
}

/// Runs the guard and, when it had to resolve a name to reach its verdict,
/// hands back the address the request must then be pinned to.
async fn check_ssrf(url: &str, allowed: &HostAllowlist) -> Result<Option<DnsPin>, String> {
    let (host, port) = extract_host_port(url).ok_or("cannot extract host from URL")?;
    // A name on the allowlist is trusted whatever it resolves to, and a URL
    // carrying a literal address leaves curl nothing to resolve. Neither
    // reached a verdict through DNS, so neither has anything to pin.
    if allowed.allows_host(host, port) {
        return Ok(None);
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(format!(
                "blocked: {ip} is a private/metadata address ({ALLOWLIST_HINT})"
            ));
        }
        return Ok(None);
    }

    // A host we cannot resolve is a host we cannot vouch for, and that covers
    // being offline: the answer the guard would have judged never arrives. The
    // failure is the network's and not a verdict, so it is not worded as one.
    let addrs = resolve(host, port)
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?;
    let mut vetted = None;
    for sa in addrs {
        if is_private_ip(&sa.ip()) && !allowed.allows_ip(sa.ip(), port) {
            return Err(format!(
                "blocked: {host} resolves to private address {} ({ALLOWLIST_HINT})",
                sa.ip()
            ));
        }
        // curl's resolve list holds one address per host and port, so the first
        // the resolver offered wins. That is the one curl would have tried
        // first anyway, the order arriving already sorted.
        vetted.get_or_insert(sa.ip());
    }

    let addr = vetted
        .ok_or_else(|| format!("blocked: {host} resolves to no addresses ({ALLOWLIST_HINT})"))?;
    Ok(Some(DnsPin {
        host: host.to_string(),
        port,
        addr,
    }))
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || is_reserved_v4(*v4)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(&IpAddr::V4(v4));
            }
            if let Some(v4) = v6.to_ipv4() {
                return is_private_ip(&IpAddr::V4(v4));
            }
            let bytes = v6.octets();
            // fe80::/10 link-local and fec0::/10 site-local. Site-local was
            // deprecated rather than withdrawn, and stacks still route it.
            if bytes[0] == 0xfe && matches!(bytes[1] & 0xc0, 0x80 | 0xc0) {
                return true;
            }
            if bytes[0] & 0xfe == 0xfc {
                return true;
            }
            false
        }
    }
}

fn is_reserved_v4(v4: Ipv4Addr) -> bool {
    RESERVED_V4_NETS
        .iter()
        .any(|(net, prefix)| ip_in_net(v4.into(), (*net).into(), *prefix))
}

/// An allowlisted host keeps plain `http://`, because the local services
/// people put on that list rarely have a certificate.
///
/// Normalising through the WHATWG parser first is what makes the guard read the
/// host curl will dial: a trailing dot, an empty port and a percent encoded
/// zone id all survive a hand split of the authority but not this.
fn validate_and_upgrade_url(url: &str, allowed: &HostAllowlist) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|e| format!("invalid URL {url}: {e}"))?;
    let url = parsed.as_str();
    if let Some(rest) = url.strip_prefix(HTTP_SCHEME) {
        if extract_host_port(url).is_some_and(|(host, port)| allowed.allows_host(host, port)) {
            return Ok(url.to_string());
        }
        return Ok(format!("{HTTPS_SCHEME}{rest}"));
    }
    if url.starts_with(HTTPS_SCHEME) {
        return Ok(url.to_string());
    }
    Err(format!(
        "URL must start with http:// or https://, got: {url}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_permissions::PluginPermissions;
    use std::net::Ipv6Addr;
    use test_case::test_case;

    const SEARX_HOST: &str = "searx.lan";
    const SEARX_URL: &str = "http://searx.lan:8888/search";
    const LOOPBACK_PORT_ENTRY: &str = "127.0.0.1:8888";
    const LOOPBACK_PORT_URL: &str = "http://127.0.0.1:8888/search";
    const LOOPBACK_URL: &str = "https://127.0.0.1";
    const LOCALHOST_ENTRY: &str = "localhost";
    const LOCALHOST_URL: &str = "http://localhost:8888";
    const PRIVATE_CIDR_ENTRY: &str = "10.0.0.0/8";
    const IN_CIDR_URL: &str = "https://10.1.2.3";
    const OUT_OF_CIDR_URL: &str = "https://192.168.1.1";
    const METADATA_URL: &str = "http://169.254.169.254/latest/meta-data";
    const ALIYUN_METADATA_URL: &str = "http://100.100.100.200/latest/meta-data";
    const LOOPBACK_CIDR_ENTRY: &str = "127.0.0.0/8";
    const IPV6_LOOPBACK_ENTRY: &str = "::1/128";
    const ALLOWED_PORT: u16 = 8888;
    /// An address rather than a name, so no test needs a DNS answer.
    const PUBLIC_URL: &str = "https://8.8.8.8/";
    const PUBLIC_HTTP_URL: &str = "http://8.8.8.8/";
    const OTHER_PUBLIC_URL: &str = "https://1.1.1.1/";
    const PUBLIC_URL_OTHER_PORT: &str = "https://8.8.8.8:8443/";
    const BLOCKED_PREFIX: &str = "blocked:";
    /// Reserved by RFC 6761, so every resolver answers NXDOMAIN for it.
    const UNRESOLVABLE_HOST: &str = "maki.invalid";
    const PAYLOAD: &str = "payload";
    const AUTH_HEADER: &str = "Authorization";
    const AUTH_VALUE: &str = "Bearer tok";
    const ACCEPT_HEADER: &str = "Accept";
    const ACCEPT_VALUE: &str = "text/html";

    fn allowlist(entries: &[&str]) -> HostAllowlist {
        HostAllowlist::parse(&entries.iter().map(|e| (*e).to_string()).collect::<Vec<_>>())
    }

    /// The guard went async when the name lookup moved off the executor thread.
    /// Driving it to completion here keeps the tests below about the verdict.
    fn ssrf(url: &str, allowed: &HostAllowlist) -> Result<Option<DnsPin>, String> {
        smol::block_on(check_ssrf(url, allowed))
    }

    fn redirect(
        params: &mut RequestParams,
        status: u16,
        location: &str,
        allowed: &HostAllowlist,
    ) -> Result<(), String> {
        smol::block_on(params.follow_redirect(status, location, allowed))
    }

    fn request_params(url: &str, opts: Option<&Table>) -> Result<RequestParams, String> {
        smol::block_on(extract_request_params(url, opts))
    }

    #[test_case(&[], "https://example.com/", "https://example.com/" ; "https_passthrough")]
    #[test_case(&[], "http://example.com", "https://example.com/" ; "http_upgraded_to_https")]
    #[test_case(&[SEARX_HOST], SEARX_URL, SEARX_URL ; "allowlisted_host_keeps_plain_http")]
    fn validate_and_upgrade_url_valid(entries: &[&str], input: &str, expected: &str) {
        assert_eq!(
            validate_and_upgrade_url(input, &allowlist(entries)).unwrap(),
            expected
        );
    }

    #[test_case("ftp://example.com" ; "unsupported_scheme")]
    #[test_case("example.com" ; "bare_domain")]
    fn validate_and_upgrade_url_invalid(input: &str) {
        assert!(validate_and_upgrade_url(input, &HostAllowlist::default()).is_err());
    }

    #[test_case(&[], PUBLIC_URL, true ; "public_ip_allowed")]
    #[test_case(&[], LOOPBACK_URL, false ; "loopback_blocked")]
    #[test_case(&[], "https://192.168.1.1", false ; "private_blocked")]
    #[test_case(&[], "https://10.0.0.1", false ; "rfc1918_10_blocked")]
    #[test_case(&[], "https://172.16.0.1", false ; "rfc1918_172_blocked")]
    #[test_case(&[], "https://169.254.169.254", false ; "aws_metadata_blocked")]
    #[test_case(&[], ALIYUN_METADATA_URL, false ; "aliyun_metadata_blocked")]
    #[test_case(&[], "https://[::1]", false ; "ipv6_loopback_blocked")]
    #[test_case(&[], "https://[::ffff:127.0.0.1]", false ; "ipv4_mapped_loopback_blocked")]
    #[test_case(&[], "https://0.0.0.0", false ; "unspecified_blocked")]
    #[test_case(&[], "https://[::ffff:169.254.169.254]", false ; "ipv4_mapped_metadata_blocked")]
    #[test_case(&[], "https://example.com@127.0.0.1/", false ; "userinfo_hiding_loopback_blocked")]
    #[test_case(&[], LOCALHOST_URL, false ; "name_resolving_to_loopback_blocked")]
    #[test_case(&[LOOPBACK_PORT_ENTRY], LOOPBACK_PORT_URL, true ; "ip_with_port_allowed")]
    #[test_case(&[LOOPBACK_PORT_ENTRY], LOOPBACK_URL, false ; "same_ip_other_port_still_blocked")]
    #[test_case(&[LOCALHOST_ENTRY], LOCALHOST_URL, true ; "name_allowed_whatever_it_resolves_to")]
    #[test_case(&[LOCALHOST_ENTRY], LOOPBACK_PORT_URL, false ; "other_private_host_still_blocked")]
    #[test_case(&[PRIVATE_CIDR_ENTRY], IN_CIDR_URL, true ; "cidr_range_allowed")]
    #[test_case(&[PRIVATE_CIDR_ENTRY], OUT_OF_CIDR_URL, false ; "outside_cidr_still_blocked")]
    #[test_case(&[PRIVATE_CIDR_ENTRY], METADATA_URL, false ; "metadata_never_allowed_by_a_range")]
    fn check_ssrf_cases(entries: &[&str], url: &str, allowed: bool) {
        let result = ssrf(url, &allowlist(entries));
        assert_eq!(
            result.is_ok(),
            allowed,
            "{url} with {entries:?}: {result:?}"
        );
    }

    /// Spellings glibc rejects but curl normalises, so the guard has to read
    /// them the way the WHATWG parser does or never see the real host.
    #[test_case("https://192.168.1.1./" ; "trailing_dot_on_a_private_address")]
    #[test_case("https://127.0.0.1.:11434/" ; "trailing_dot_with_a_port")]
    #[test_case("https://127.0.0.1:/" ; "empty_port")]
    #[test_case("https://[fe80::1%25eth0]/" ; "percent_encoded_zone_id")]
    fn normalised_bypass_is_refused(url: &str) {
        let allowed = HostAllowlist::default();
        let result = validate_and_upgrade_url(url, &allowed).and_then(|u| ssrf(&u, &allowed));
        assert!(result.is_err(), "{url}");
    }

    #[test_case(LOOPBACK_URL ; "private_address")]
    fn blocked_message_points_at_the_config_option(url: &str) {
        let err = ssrf(url, &HostAllowlist::default()).unwrap_err();
        assert!(err.starts_with(BLOCKED_PREFIX), "{err}");
        assert!(err.contains(ALLOWLIST_HINT), "{err}");
    }

    /// A resolver with no answer has reached no verdict, so pointing at the
    /// allowlist would send the caller after the wrong problem.
    #[test]
    fn an_unresolvable_host_reads_as_a_network_failure() {
        let url = format!("https://{UNRESOLVABLE_HOST}/");
        let err = ssrf(&url, &HostAllowlist::default()).expect_err(UNRESOLVABLE_HOST);
        assert!(!err.starts_with(BLOCKED_PREFIX), "{err}");
        assert!(err.contains(UNRESOLVABLE_HOST), "{err}");
    }

    fn redirect_params(url: &str) -> RequestParams {
        RequestParams {
            url: url.to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_bytes: DEFAULT_MAX_BYTES,
            retries: 0,
            pin: None,
        }
    }

    /// The whole point of the pin: curl is handed the address the guard read,
    /// so a second lookup cannot answer with a different one.
    #[test]
    fn a_resolved_host_is_pinned_to_the_address_the_guard_vetted() {
        let allowed = allowlist(&[LOOPBACK_CIDR_ENTRY, IPV6_LOOPBACK_ENTRY]);
        let pin = ssrf(LOCALHOST_URL, &allowed)
            .unwrap()
            .expect("a resolved host must be pinned");
        assert_eq!(pin.host, LOCALHOST_ENTRY);
        assert_eq!(pin.port, ALLOWED_PORT);
        assert!(is_private_ip(&pin.addr), "{pin:?}");
    }

    #[test_case(PUBLIC_URL ; "literal_address_needs_no_lookup")]
    #[test_case(LOCALHOST_URL ; "allowlisted_name_is_trusted_however_it_resolves")]
    fn hosts_the_guard_never_resolved_are_not_pinned(url: &str) {
        let pin = ssrf(url, &allowlist(&[LOCALHOST_ENTRY])).unwrap();
        assert!(pin.is_none(), "{pin:?}");
    }

    /// A hop is only followed once it has been vetted, so the pin has to move
    /// with the URL: the address vetted for the previous host must not decide
    /// where the next one connects.
    #[test]
    fn a_followed_redirect_replaces_the_pin() {
        let mut params = redirect_params(LOOPBACK_PORT_URL);
        params.pin = Some(DnsPin {
            host: LOCALHOST_ENTRY.to_string(),
            port: ALLOWED_PORT,
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        });
        redirect(
            &mut params,
            302,
            LOOPBACK_PORT_URL,
            &allowlist(&[LOOPBACK_PORT_ENTRY]),
        )
        .unwrap();
        assert!(params.pin.is_none(), "{:?}", params.pin);
    }

    #[test]
    fn redirect_hop_into_a_private_address_is_refused() {
        let mut params = redirect_params(LOOPBACK_PORT_URL);
        let err = redirect(
            &mut params,
            302,
            METADATA_URL,
            &allowlist(&[LOOPBACK_PORT_ENTRY]),
        )
        .unwrap_err();
        assert!(err.starts_with(BLOCKED_PREFIX), "{err}");
        assert_eq!(
            params.url, LOOPBACK_PORT_URL,
            "refused hop moved the request"
        );
    }

    #[test]
    fn relative_redirect_on_an_allowlisted_host_is_followed() {
        let mut params = redirect_params(LOOPBACK_PORT_URL);
        redirect(
            &mut params,
            302,
            "/results",
            &allowlist(&[LOOPBACK_PORT_ENTRY]),
        )
        .unwrap();
        assert_eq!(params.url, "http://127.0.0.1:8888/results");
    }

    #[test_case(PUBLIC_URL, true ; "same_authority_keeps_credentials")]
    #[test_case(OTHER_PUBLIC_URL, false ; "other_host_drops_credentials")]
    #[test_case(PUBLIC_HTTP_URL, true ; "same_host_upgraded_back_to_https_keeps_credentials")]
    #[test_case(PUBLIC_URL_OTHER_PORT, false ; "other_port_drops_credentials")]
    fn redirect_scrubs_credentials_across_authorities(location: &str, kept: bool) {
        let mut params = redirect_params(PUBLIC_URL);
        params.headers = vec![
            (AUTH_HEADER.to_string(), AUTH_VALUE.to_string()),
            (ACCEPT_HEADER.to_string(), ACCEPT_VALUE.to_string()),
        ];
        redirect(&mut params, 302, location, &HostAllowlist::default()).unwrap();
        assert_eq!(
            params.headers.iter().any(|(k, _)| k == AUTH_HEADER),
            kept,
            "{location}: {:?}",
            params.headers
        );
        assert!(params.headers.iter().any(|(k, _)| k == ACCEPT_HEADER));
    }

    #[test_case(303, "GET", "" ; "303_rewrites_a_post_into_a_get")]
    #[test_case(308, "POST", PAYLOAD ; "308_keeps_method_and_body")]
    fn redirect_rewrites_the_method_per_status(status: u16, method: &str, body: &str) {
        let mut params = redirect_params(PUBLIC_URL);
        params.method = "POST".to_string();
        params.body = PAYLOAD.into();
        redirect(&mut params, status, PUBLIC_URL, &HostAllowlist::default()).unwrap();
        assert_eq!(params.method, method);
        assert_eq!(params.body, body.as_bytes());
    }

    #[test_case(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), true ; "v4_unspecified")]
    #[test_case(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)), true ; "v4_rfc1918_class_b")]
    #[test_case(IpAddr::V4(Ipv4Addr::new(172, 31, 255, 255)), true ; "v4_rfc1918_class_b_upper")]
    #[test_case(IpAddr::V4(Ipv4Addr::new(172, 32, 0, 1)), false ; "v4_172_32_is_public")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001)), true ; "ipv4_mapped_private")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808)), false ; "ipv4_mapped_public")]
    #[test_case(IpAddr::V6(Ipv6Addr::UNSPECIFIED), true ; "v6_unspecified")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)), true ; "v6_link_local")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)), true ; "v6_unique_local_fc")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)), true ; "v6_unique_local_fd")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 1)), true ; "v6_site_local")]
    #[test_case(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)), false ; "v6_global_unicast")]
    fn is_private_ip_cases(ip: IpAddr, expected: bool) {
        assert_eq!(is_private_ip(&ip), expected);
    }

    /// Ranges the standard library has no predicate for. Each is checked in
    /// both spellings, because `::ffff:100.100.100.200` reaches the same host
    /// as `100.100.100.200`.
    #[test_case(Ipv4Addr::new(100, 100, 100, 200) ; "aliyun_metadata_in_cgnat")]
    #[test_case(Ipv4Addr::new(100, 64, 0, 0) ; "cgnat_first")]
    #[test_case(Ipv4Addr::new(100, 127, 255, 255) ; "cgnat_last")]
    #[test_case(Ipv4Addr::new(192, 0, 0, 1) ; "ietf_protocol_assignments")]
    #[test_case(Ipv4Addr::new(198, 18, 0, 1) ; "benchmarking_first")]
    #[test_case(Ipv4Addr::new(198, 19, 255, 255) ; "benchmarking_last")]
    #[test_case(Ipv4Addr::new(240, 0, 0, 1) ; "reserved_class_e")]
    #[test_case(Ipv4Addr::BROADCAST ; "broadcast")]
    fn reserved_v4_is_private_in_both_spellings(v4: Ipv4Addr) {
        assert!(is_private_ip(&IpAddr::V4(v4)));
        assert!(is_private_ip(&IpAddr::V6(v4.to_ipv6_mapped())));
    }

    /// The address just past each range, so a prefix that is one bit too wide
    /// does not pass unnoticed.
    #[test_case(Ipv4Addr::new(100, 63, 255, 255) ; "below_cgnat")]
    #[test_case(Ipv4Addr::new(100, 128, 0, 0) ; "above_cgnat")]
    #[test_case(Ipv4Addr::new(192, 0, 1, 1) ; "above_ietf_protocol_assignments")]
    #[test_case(Ipv4Addr::new(198, 20, 0, 0) ; "above_benchmarking")]
    #[test_case(Ipv4Addr::new(198, 17, 255, 255) ; "below_benchmarking")]
    fn addresses_beside_the_reserved_ranges_stay_public(v4: Ipv4Addr) {
        assert!(!is_private_ip(&IpAddr::V4(v4)));
        assert!(!is_private_ip(&IpAddr::V6(v4.to_ipv6_mapped())));
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

    #[test_case("10.0.0.0/33" ; "prefix_too_long")]
    #[test_case("10.0.0.0/x" ; "prefix_not_a_number")]
    #[test_case("" ; "empty_entry")]
    fn unparseable_allowlist_entries_are_dropped(entry: &str) {
        let list = allowlist(&[entry]);
        assert!(list.names.is_empty() && list.nets.is_empty(), "{list:?}");
    }

    #[test]
    fn build_request_get_no_opts() {
        let req = build_request("https://example.com", "agent", "GET", &[], vec![]).unwrap();
        assert_eq!(req.method(), "GET");
        assert_eq!(req.body().len(), Some(0));
        assert_eq!(req.headers()["User-Agent"], "agent");
    }

    #[test]
    fn build_request_post_with_body_and_headers() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let req = build_request(
            "https://example.com",
            "agent",
            "POST",
            &headers,
            b"hello world".to_vec(),
        )
        .unwrap();
        assert_eq!(req.method(), "POST");
        assert_eq!(req.body().len(), Some(b"hello world".len() as u64));
        assert_eq!(req.headers()["Content-Type"], "application/json");
    }

    #[test]
    fn build_request_multiple_headers() {
        let headers = vec![
            ("Accept".to_string(), "text/html".to_string()),
            ("X-Custom".to_string(), "foo".to_string()),
        ];
        let req = build_request("https://example.com", "agent", "GET", &headers, vec![]).unwrap();
        assert_eq!(req.headers()["Accept"], "text/html");
        assert_eq!(req.headers()["X-Custom"], "foo");
    }

    #[test]
    fn build_request_invalid_uri_errors() {
        let result = build_request("not a valid uri \x00", "agent", "GET", &[], vec![]);
        assert!(result.is_err());
    }

    #[test_case(r#"net.request("https://127.0.0.1")"# ; "ssrf_blocked")]
    #[test_case(r#"net.request("ftp://x")"# ; "invalid_url")]
    fn lua_request_error_returns_nil_and_message(expr: &str) {
        let lua = Lua::new();
        let net = create_net_table(&lua, &PluginPermissions::trusted()).unwrap();
        lua.globals().set("net", net).unwrap();
        let (is_nil, has_err): (bool, bool) = lua
            .load(format!(
                "local r, err = {expr}; return r == nil, err ~= nil"
            ))
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn extract_params_defaults_no_opts() {
        let params = request_params(PUBLIC_URL, None).unwrap();
        assert_eq!(params.url, PUBLIC_URL);
        assert_eq!(params.method, "GET");
        assert!(params.headers.is_empty());
        assert!(params.body.is_empty());
        assert_eq!(params.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(params.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(params.retries, MAX_RETRIES);
    }

    #[test]
    fn extract_params_timeout_clamped_to_max() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("timeout", MAX_TIMEOUT_SECS + 100).unwrap();
        let params = request_params(PUBLIC_URL, Some(&opts)).unwrap();
        assert_eq!(params.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn extract_params_post_with_body() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("method", "POST").unwrap();
        opts.set("body", r#"{"key":"val"}"#).unwrap();
        let params = request_params(PUBLIC_URL, Some(&opts)).unwrap();
        assert_eq!(params.method, "POST");
        assert_eq!(params.body, br#"{"key":"val"}"#);
    }

    #[test]
    fn extract_params_http_upgraded_to_https() {
        let params = request_params(PUBLIC_HTTP_URL, None).unwrap();
        assert_eq!(params.url, PUBLIC_URL);
    }

    #[test]
    fn extract_params_headers_collected() {
        let lua = Lua::new();
        let headers = lua.create_table().unwrap();
        headers.set(AUTH_HEADER, AUTH_VALUE).unwrap();
        headers.set(ACCEPT_HEADER, ACCEPT_VALUE).unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("headers", headers).unwrap();
        let params = request_params(PUBLIC_URL, Some(&opts)).unwrap();
        assert_eq!(params.headers.len(), 2);
        assert!(
            params
                .headers
                .iter()
                .any(|(k, v)| k == AUTH_HEADER && v == AUTH_VALUE)
        );
        assert!(
            params
                .headers
                .iter()
                .any(|(k, v)| k == ACCEPT_HEADER && v == ACCEPT_VALUE)
        );
    }
}
