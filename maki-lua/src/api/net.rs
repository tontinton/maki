use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use futures_lite::io::AsyncReadExt;
use isahc::config::{Configurable, RedirectPolicy, ResolveMap, VersionNegotiation};
use isahc::http::HeaderMap;
use isahc::{AsyncBody, HttpClient, Request, Response};
use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::Timeouts;
use mlua::{Lua, Result as LuaResult, Table};
use regex::bytes::Regex;
use smol::{Timer, unblock};
use thiserror::Error;
use url::Url;

use crate::api::util::pair::{Pair, try_pair};

use crate::plugin_permissions::{NetEgress, PluginPermissions};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_RETRIES: u32 = 3;
/// What a provider hook's GET retries: the codec's own side requests never do.
const PROVIDER_GET_RETRIES: u32 = 0;
const GET: &str = "GET";
const NO_ATTEMPT: &str = "no request was attempted";
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
const MAX_POOLED_CLIENTS: usize = 8;
/// A pooled client holds a curl thread and open sockets, so an endpoint nobody
/// reads any more has to give them back.
const CLIENT_IDLE_TTL: Duration = Duration::from_secs(120);
const INVALID_LINE_MATCH: &str = "invalid line_match";
/// Methods whose requests carry no body at all when the caller gave none.
const BODYLESS_METHODS: &[&str] = &["GET", "HEAD"];
const HEADER_VALUE_SEPARATOR: &str = ", ";
const ALLOWLIST_HINT: &str = "add it to `net.allowed_private_hosts` in your init.lua to allow it";
const UNDECLARED_HOST_HINT: &str = "add it to `net_hosts` under `[permissions]` in plugin.toml";
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
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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
    /// The caller's own total bound, when it stated one.
    timeout: Option<Duration>,
    max_bytes: usize,
    retries: u32,
    /// Keep only response lines this matches. `None` disables the filter.
    line_match: Option<Regex>,
    route: Route,
    /// Carried rather than passed, so every hop is vetted against the same
    /// reach the first one was.
    egress: NetEgress,
}

/// Which rules a hop goes out under, settled by [`vet`] again for every hop.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Route {
    /// The origin of a provider this plugin registered, as the user or maki
    /// chose it. Reached on the terms the codec's own requests there run
    /// under: no guard, maki's user agent, and connect and stall bounds
    /// instead of a total one.
    Provider,
    /// Anywhere else, behind the guard. `None` when the guard reached its
    /// verdict without DNS, so there is no address to pin.
    Guarded(Option<DnsPin>),
}

impl Route {
    /// The total bound a caller that stated none gets. A provider's origin has
    /// none, like the codec's requests: its stall bound catches a dead server.
    fn default_timeout(&self) -> Option<Duration> {
        match self {
            Self::Provider => None,
            Self::Guarded(_) => Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        }
    }

    fn user_agents(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Provider => (maki_providers::user_agent(), None),
            Self::Guarded(_) => (USER_AGENT, Some(FALLBACK_USER_AGENT)),
        }
    }
}

pub(crate) struct ResponseData {
    pub(crate) body: String,
    pub(crate) status: u16,
    content_type: String,
    pub(crate) headers: Vec<(String, String)>,
}

/// Why a request came back without a response, split the way a provider
/// classifies it: a transport failure reads as the codec's own would.
#[derive(Debug, Error)]
pub(crate) enum NetError {
    /// Refused by maki before or between hops, or a response it will not read.
    #[error("{0}")]
    Refused(String),
    #[error("request failed: {0}")]
    Transport(isahc::Error),
    #[error("read error: {0}")]
    Read(io::Error),
}

impl From<String> for NetError {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}

impl ResponseData {
    fn into_table(self, lua: &Lua) -> LuaResult<Table> {
        let tbl = lua.create_table()?;
        tbl.set("body", self.body)?;
        tbl.set("status", self.status)?;
        tbl.set("content_type", self.content_type)?;
        tbl.set("headers", lua.create_table_from(self.headers)?)?;
        Ok(tbl)
    }
}

/// `http` already keeps names in lowercase. A header sent more than once is
/// joined into one value, as RFC 9110 allows, which mangles `set-cookie`
/// because its values hold commas. Bytes that are not UTF-8 become U+FFFD
/// rather than dropping the header, so a lookup does not quietly miss it.
fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .keys()
        .map(|name| {
            let value = headers
                .get_all(name)
                .iter()
                .map(|value| String::from_utf8_lossy(value.as_bytes()))
                .collect::<Vec<_>>()
                .join(HEADER_VALUE_SEPARATOR);
            (name.as_str().to_owned(), value)
        })
        .collect()
}

/// Make an HTTP request and return the response body. Plain `http://`
/// URLs are automatically upgraded to `https://`. Requests to private
/// or metadata IP addresses are blocked for safety, unless the host is
/// listed in `net.allowed_private_hosts`.
///
/// A request to the origin of a provider this plugin registered, as the
/// user (`<SLUG>_BASE_URL`, `providers.toml`) or maki (a built-in's
/// default) chose it, goes out the way the provider's chat requests do:
/// no address check or upgrade, maki's user agent, and connect and stall
/// timeouts instead of a total one.
///
/// {opts} fields:
///   `method` (string) HTTP verb (default `"GET"`).
///   `headers` (table) Header name/value pairs.
///   `body` (string) Request body.
///   `timeout` (integer) Total timeout in seconds, max 120 (default 30,
///     none on a provider's origin).
///   `max_bytes` (integer) Max response size in bytes (default 5 MB).
///   `retry` (integer) Retries on 5xx errors (default 3).
///   `line_match` (string) Regex. Keep only the response lines it
///   matches. Filtering happens after the body is read, so `max_bytes`
///   still caps the transfer.
///
/// The response table has `body` (string), `status` (integer),
/// `content_type` (string) and `headers` (table). `headers` comes from the
/// final response after redirects and is keyed by lowercase name, as in
/// `res.headers["retry-after"]`. A header sent more than once has its values
/// joined with `, `, which mangles `set-cookie`. Bytes that are not UTF-8
/// become U+FFFD. The table can go straight to `maki.provider.http_error`.
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
async fn request(
    lua: Lua,
    #[ctx] egress: NetEgress,
    url: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let params = try_pair!(extract_request_params(&url, egress, opts.as_ref()).await);
    let resp = try_pair!(do_request(params).await);
    Ok((Some(resp.into_table(&lua)?), None))
}

lua_table! {
    /// HTTP client for fetching web content. All traffic goes over HTTPS
    /// (plain HTTP is upgraded). Private and metadata IP addresses are
    /// blocked to prevent SSRF, including after a redirect. Hosts listed in
    /// the `net.allowed_private_hosts` config option are exempt, and so is a
    /// provider plugin's own origin (see `maki.net.request`).
    /// Failed requests (5xx) are retried automatically.
    ///
    /// Requests reuse a pool of clients, so calls to the same host share one
    /// keep-alive connection rather than pay a fresh handshake each time.
    ///
    /// ```lua
    /// local res, err = maki.net.request("https://example.com")
    /// if res then print(res.body) end
    /// ```
    "maki.net" => pub(crate) fn create_net_table(perms: &PluginPermissions, egress: NetEgress), DOCS [
        request(perms, egress),
    ]
}

/// The plugin's own reach, checked once the SSRF guard has settled what the
/// URL really points at. See [`NetEgress`] for what a plugin may reach and
/// why the manifest is not the whole of it.
fn check_declared_host(url: &str, egress: &NetEgress) -> Result<(), String> {
    let (host, _) = extract_host_port(url).ok_or("cannot extract host from URL")?;
    if egress.allows(host) {
        return Ok(());
    }
    Err(format!(
        "blocked: {host} is not a host this plugin declared ({UNDECLARED_HOST_HINT})"
    ))
}

/// Every URL this client is about to open goes through here: scheme, SSRF and
/// the plugin's declared hosts, in that order, because the last one wants the
/// address the guard settled on. One door, so a redirect cannot reach what the
/// URL the caller wrote could not.
///
/// The origin of a provider the plugin registered skips all three, when the
/// user or maki chose it: the codec already goes there unguarded.
async fn vet(
    url: &str,
    allowed: &HostAllowlist,
    egress: &NetEgress,
) -> Result<(String, Route), String> {
    if let Ok(parsed) = Url::parse(url)
        && egress.vouches(&parsed)
    {
        return Ok((parsed.into(), Route::Provider));
    }
    let url = validate_and_upgrade_url(url, allowed)?;
    let pin = check_ssrf(&url, allowed).await?;
    check_declared_host(&url, egress)?;
    Ok((url, Route::Guarded(pin)))
}

async fn extract_request_params(
    url: &str,
    egress: NetEgress,
    opts: Option<&Table>,
) -> Result<RequestParams, String> {
    let allowed = ALLOWED_PRIVATE_HOSTS.load_full();
    let (url, route) = vet(url, &allowed, &egress).await?;

    let method = opts
        .and_then(|o| o.get::<String>("method").ok())
        .unwrap_or_else(|| GET.to_owned());

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

    let timeout = opts
        .and_then(|o| o.get::<u64>("timeout").ok())
        .map(|secs| Duration::from_secs(secs.min(MAX_TIMEOUT_SECS)));

    let max_bytes = opts
        .and_then(|o| o.get::<usize>("max_bytes").ok())
        .unwrap_or(DEFAULT_MAX_BYTES);

    let retries = opts
        .and_then(|o| o.get::<u32>("retry").ok())
        .unwrap_or(MAX_RETRIES);

    let line_match = opts
        .and_then(|o| o.get::<String>("line_match").ok())
        .map(|pattern| Regex::new(&pattern).map_err(|e| format!("{INVALID_LINE_MATCH}: {e}")))
        .transpose()?;

    Ok(RequestParams {
        url,
        method,
        headers,
        body,
        timeout,
        max_bytes,
        retries,
        line_match,
        route,
        egress,
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

    // A zero-length body is not the same as no body: isahc announces the known
    // length, which turns a GET into an upload carrying `content-length: 0`.
    // None of maki's other clients send that, and a plugin standing in for one
    // of them has to look the same on the wire. Every other method keeps its
    // `content-length: 0`, which servers answer a POST without with a 411.
    let bodyless = BODYLESS_METHODS
        .iter()
        .any(|bodyless| method.eq_ignore_ascii_case(bodyless));
    let body = if bodyless && body.is_empty() {
        AsyncBody::empty()
    } else {
        AsyncBody::from(body)
    };
    builder
        .body(body)
        .map_err(|e| format!("request build error: {e}"))
}

async fn send_with_retries(
    client: &HttpClient,
    params: &RequestParams,
) -> Result<Response<AsyncBody>, NetError> {
    let is_get = params.method.eq_ignore_ascii_case(GET);
    let (user_agent, fallback_user_agent) = params.route.user_agents();
    let mut last_err = None;

    'retry: {
        for attempt in 0..=params.retries {
            let req = build_request(
                &params.url,
                user_agent,
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

                    if is_cf_challenge
                        && is_get
                        && let Some(fallback_user_agent) = fallback_user_agent
                    {
                        let req = build_request(
                            &params.url,
                            fallback_user_agent,
                            &params.method,
                            &params.headers,
                            params.body.clone(),
                        )?;
                        match client.send_async(req).await {
                            Ok(resp) => break 'retry Ok(resp),
                            Err(e) => last_err = Some(e),
                        }
                    } else if status >= 500 && attempt < params.retries {
                        continue;
                    } else {
                        break 'retry Ok(resp);
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.map_or_else(|| NO_ATTEMPT.to_owned().into(), NetError::Transport))
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
fn build_client(key: &ClientKey) -> Result<HttpClient, String> {
    let mut builder = HttpClient::builder()
        // Redirects are followed by hand, so every hop goes through the SSRF
        // check. Left to curl, a URL that passed the check could still bounce
        // us into 169.254.169.254.
        .redirect_policy(RedirectPolicy::None)
        // The workspace enables curl's http2 feature for OTLP over gRPC. This
        // client fetches arbitrary user URLs, so keep it on HTTP/1.1 rather
        // than change how every one of them is negotiated.
        .version_negotiation(VersionNegotiation::http11());

    if let Some(timeout) = key.timeout {
        builder = builder.timeout(timeout);
    }
    match &key.route {
        Route::Provider => builder = Timeouts::default().bound(builder),
        // Connect to the address the guard vetted instead of asking DNS again
        // and trusting whatever the second answer says.
        Route::Guarded(Some(pin)) => {
            builder = builder.dns_resolve(ResolveMap::new().add(&pin.host, pin.port, pin.addr));
        }
        Route::Guarded(None) => {}
    }

    builder.build().map_err(|e| format!("client error: {e}"))
}

/// Everything a client is built from, and so what a request can reuse one by.
/// [`build_client`] reads the key rather than the request, so a new client
/// option cannot be left out of it. If it could, a provider origin's stall
/// bounds or a pinned hop's resolve map would leak to a plain request that
/// happens to share its timeout.
#[derive(Clone, Eq, Hash, PartialEq)]
struct ClientKey {
    timeout: Option<Duration>,
    route: Route,
}

impl ClientKey {
    fn of(params: &RequestParams) -> Self {
        Self {
            timeout: params.timeout.or(params.route.default_timeout()),
            route: params.route.clone(),
        }
    }
}

type PooledClient = (Arc<HttpClient>, Instant);
type ClientPool = HashMap<ClientKey, PooledClient>;

/// The curl thread and the keep-alive connection cache live inside the
/// client, so rebuilding one per call pays a thread and a handshake every
/// time. A loop fetching the same endpoint hits the same key on every call.
/// A request holds an `Arc`, so freeing a slot only closes sockets once the
/// requests using that client are done with it.
static CLIENT_POOL: LazyLock<Mutex<ClientPool>> = LazyLock::new(Mutex::default);

fn lock_pool() -> MutexGuard<'static, ClientPool> {
    CLIENT_POOL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn evict_idle(pool: &mut ClientPool, now: Instant) {
    pool.retain(|_, (_, seen)| now.duration_since(*seen) < CLIENT_IDLE_TTL);
}

/// Frees a slot for a newcomer, always: clients nobody has used for
/// `CLIENT_IDLE_TTL` first, then the least recently used one. Without the
/// idle sweep a host that was fetched once keeps a thread and a socket for
/// the rest of the session.
fn make_room(pool: &mut ClientPool, now: Instant) {
    evict_idle(pool, now);
    while pool.len() >= MAX_POOLED_CLIENTS {
        let Some(lru) = pool
            .iter()
            .min_by_key(|(_, (_, seen))| *seen)
            .map(|(lru, _)| lru.clone())
        else {
            break;
        };
        pool.remove(&lru);
    }
}

/// Hands out a client for `params`, reusing the pooled one when the key matches.
fn pooled_client(params: &RequestParams) -> Result<Arc<HttpClient>, String> {
    let key = ClientKey::of(params);
    let now = Instant::now();
    {
        let mut pool = lock_pool();
        evict_idle(&mut pool, now);
        if let Some((client, seen)) = pool.get_mut(&key) {
            *seen = now;
            return Ok(Arc::clone(client));
        }
    }
    // Built outside the lock: `build_client` starts a curl thread, and this
    // runs on the Lua thread's executor. Two callers racing the same cold key
    // each pay for a client, and the loser is dropped when its request ends.
    let client = Arc::new(build_client(&key)?);
    let now = Instant::now();
    let mut pool = lock_pool();
    make_room(&mut pool, now);
    pool.insert(key, (Arc::clone(&client), now));
    Ok(client)
}

/// Gives back every pooled client, called when the Lua thread stops so that no
/// socket outlives the session that opened it.
pub fn clear_client_pool() {
    lock_pool().clear();
}

/// Keeps only the lines `pattern` matches, so a caller never carries the lines
/// it discards into the Lua VM. The line terminator is not part of the match,
/// so `$` anchors at the end of the line.
fn keep_lines_matching(body: &[u8], pattern: &Regex) -> Vec<u8> {
    let mut kept = Vec::new();
    for line in body.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if pattern.is_match(content) {
            kept.extend_from_slice(line);
        }
    }
    kept
}

/// The GET behind a provider hook's `ctx.get_json`: vetted, pooled and routed
/// exactly as `maki.net.request` would send it.
pub(crate) async fn provider_get(
    url: &str,
    headers: Vec<(String, String)>,
    egress: NetEgress,
) -> Result<ResponseData, NetError> {
    let allowed = ALLOWED_PRIVATE_HOSTS.load_full();
    let (url, route) = vet(url, &allowed, &egress).await?;
    do_request(RequestParams {
        url,
        method: GET.to_owned(),
        headers,
        body: Vec::new(),
        timeout: None,
        max_bytes: DEFAULT_MAX_BYTES,
        retries: PROVIDER_GET_RETRIES,
        line_match: None,
        route,
        egress,
    })
    .await
}

async fn do_request(mut params: RequestParams) -> Result<ResponseData, NetError> {
    let allowed = ALLOWED_PRIVATE_HOSTS.load_full();
    let client = pooled_client(&params)?;
    let mut response = send_with_retries(&client, &params).await?;

    for _ in 0..MAX_REDIRECTS {
        let Some(location) = redirect_location(&response) else {
            break;
        };
        params
            .follow_redirect(response.status().as_u16(), &location, &allowed)
            .await?;
        let client = pooled_client(&params)?;
        response = send_with_retries(&client, &params).await?;
    }
    if redirect_location(&response).is_some() {
        return Err(format!("gave up after {MAX_REDIRECTS} redirects").into());
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
        return Err(format!("response too large: {len} bytes").into());
    }

    let mut bytes = Vec::new();
    response
        .body_mut()
        .take((params.max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(NetError::Read)?;

    if bytes.len() > params.max_bytes {
        return Err(format!("response too large: {} bytes", bytes.len()).into());
    }

    let bytes = match &params.line_match {
        Some(pattern) => keep_lines_matching(&bytes, pattern),
        None => bytes,
    };
    let body = String::from_utf8_lossy(&bytes).into_owned();
    Ok(ResponseData {
        body,
        status,
        content_type,
        headers: collect_headers(response.headers()),
    })
}

impl RequestParams {
    /// Points the request at a redirect target after putting it through the
    /// same scheme, SSRF and declared-host rules as the URL the caller asked
    /// for.
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
        let (target, route) = vet(target.as_str(), allowed, &self.egress).await?;

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
        self.route = route;
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
    use isahc::http::{HeaderName, HeaderValue};
    use std::collections::HashMap;
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
    const PUBLIC_HOST: &str = "8.8.8.8";
    const OTHER_PUBLIC_HOST: &str = "1.1.1.1";
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
    const KEEP_PATTERN: &str = "^vllm:generation_tokens_total";
    const RETRY_AFTER_HEADER: &str = "Retry-After";
    const RETRY_AFTER_KEY: &str = "retry-after";
    const RETRY_AFTER_SECS: &str = "30";
    const VARY_HEADER: &str = "vary";
    const VARY_FIRST: &str = "Accept";
    const VARY_SECOND: &str = "Origin";
    const VARY_JOINED: &str = "Accept, Origin";
    const LATIN1_HEADER: &str = "x-name";
    const LATIN1_BYTES: &[u8] = b"caf\xe9";
    const LATIN1_LOSSY: &str = "caf\u{FFFD}";
    const JSON_CONTENT_TYPE: &str = "application/json";
    const TOO_MANY_REQUESTS: u16 = 429;

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
        smol::block_on(extract_request_params(url, NetEgress::default(), opts))
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
            timeout: None,
            max_bytes: DEFAULT_MAX_BYTES,
            retries: 0,
            line_match: None,
            route: Route::Guarded(None),
            egress: NetEgress::default(),
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
        params.route = Route::Guarded(Some(DnsPin {
            host: LOCALHOST_ENTRY.to_string(),
            port: ALLOWED_PORT,
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }));
        redirect(
            &mut params,
            302,
            LOOPBACK_PORT_URL,
            &allowlist(&[LOOPBACK_PORT_ENTRY]),
        )
        .unwrap();
        assert!(
            matches!(params.route, Route::Guarded(None)),
            "{:?}",
            params.route
        );
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

    /// isahc sends no body, and so no `content-length`, only for an empty
    /// body. A GET must look like every other client's; a POST without the
    /// header gets a 411 from servers that insist on it.
    #[test_case("GET", true ; "get_sends_no_body")]
    #[test_case("head", true ; "head_sends_no_body")]
    #[test_case("POST", false ; "post_keeps_its_zero_length")]
    #[test_case("DELETE", false ; "delete_keeps_its_zero_length")]
    fn an_empty_body_is_dropped_only_where_it_means_nothing(method: &str, dropped: bool) {
        let req = build_request("https://example.com", "agent", method, &[], vec![]).unwrap();
        assert_eq!(req.body().is_empty(), dropped);
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
        let net =
            create_net_table(&lua, &PluginPermissions::trusted(), NetEgress::default()).unwrap();
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

    fn declared(hosts: Option<&[&str]>) -> NetEgress {
        NetEgress::new(hosts.map(|hosts| hosts.iter().map(|host| (*host).to_owned()).collect()))
    }

    /// The gate sits in `extract_request_params`, so these go through it
    /// rather than through the matcher alone. Addresses, so nothing resolves.
    #[test_case(None, true ; "no_declared_list_reaches_any_host")]
    #[test_case(Some(&[PUBLIC_HOST]), true ; "declared_host_is_reachable")]
    #[test_case(Some(&[OTHER_PUBLIC_HOST]), false ; "undeclared_host_is_denied")]
    fn declared_net_hosts_gate_requests(hosts: Option<&[&str]>, allowed: bool) {
        let result = smol::block_on(extract_request_params(PUBLIC_URL, declared(hosts), None));
        assert_eq!(result.is_ok(), allowed, "{hosts:?}");
    }

    /// The defect the shared `vet` exists to prevent: a declared host that
    /// answers with a `Location` elsewhere must not carry the plugin past its
    /// own list, however ordinary that hop looks to the SSRF guard.
    #[test_case(Some(&[PUBLIC_HOST]), false ; "a_hop_off_the_list_is_refused")]
    #[test_case(Some(&[PUBLIC_HOST, OTHER_PUBLIC_HOST]), true ; "a_declared_hop_is_followed")]
    fn a_redirect_is_vetted_against_the_declared_hosts(hosts: Option<&[&str]>, allowed: bool) {
        const MOVED: u16 = 302;
        let mut params =
            smol::block_on(extract_request_params(PUBLIC_URL, declared(hosts), None)).unwrap();
        let allowlist = ALLOWED_PRIVATE_HOSTS.load_full();

        let hop = smol::block_on(params.follow_redirect(MOVED, OTHER_PUBLIC_URL, &allowlist));

        assert_eq!(hop.is_ok(), allowed, "{hosts:?}");
    }

    #[test]
    fn extract_params_defaults_no_opts() {
        let params = request_params(PUBLIC_URL, None).unwrap();
        assert_eq!(params.url, PUBLIC_URL);
        assert_eq!(params.method, "GET");
        assert!(params.headers.is_empty());
        assert!(params.body.is_empty());
        assert_eq!(
            params.timeout.or(params.route.default_timeout()),
            Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        );
        assert_eq!(params.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(params.retries, MAX_RETRIES);
    }

    #[test]
    fn extract_params_timeout_clamped_to_max() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("timeout", MAX_TIMEOUT_SECS + 100).unwrap();
        let params = request_params(PUBLIC_URL, Some(&opts)).unwrap();
        assert_eq!(params.timeout, Some(Duration::from_secs(MAX_TIMEOUT_SECS)));
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

    #[test_case(&[(RETRY_AFTER_HEADER, RETRY_AFTER_SECS.as_bytes())], &[(RETRY_AFTER_KEY, RETRY_AFTER_SECS)] ; "name_is_lowercased")]
    #[test_case(&[(VARY_HEADER, VARY_FIRST.as_bytes()), (VARY_HEADER, VARY_SECOND.as_bytes())], &[(VARY_HEADER, VARY_JOINED)] ; "repeated_header_is_joined")]
    #[test_case(&[(LATIN1_HEADER, LATIN1_BYTES)], &[(LATIN1_HEADER, LATIN1_LOSSY)] ; "non_utf8_value_is_replaced_not_dropped")]
    fn collect_headers_cases(sent: &[(&str, &[u8])], expected: &[(&str, &str)]) {
        let mut headers = HeaderMap::new();
        for (name, value) in sent {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_bytes(value).unwrap(),
            );
        }
        let collected: HashMap<String, String> = collect_headers(&headers).into_iter().collect();
        let expected: HashMap<String, String> = expected
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn response_table_exposes_headers_by_lowercase_name() {
        let lua = Lua::new();
        let response = ResponseData {
            body: PAYLOAD.to_owned(),
            status: TOO_MANY_REQUESTS,
            content_type: JSON_CONTENT_TYPE.to_owned(),
            headers: vec![(RETRY_AFTER_KEY.to_owned(), RETRY_AFTER_SECS.to_owned())],
        };
        lua.globals()
            .set("res", response.into_table(&lua).unwrap())
            .unwrap();
        let (status, body, content_type, retry_after): (u16, String, String, String) = lua
            .load(format!(
                r#"return res.status, res.body, res.content_type, res.headers["{RETRY_AFTER_KEY}"]"#
            ))
            .eval()
            .unwrap();
        assert_eq!(status, TOO_MANY_REQUESTS);
        assert_eq!(body, PAYLOAD);
        assert_eq!(content_type, JSON_CONTENT_TYPE);
        assert_eq!(retry_after, RETRY_AFTER_SECS);
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

    fn line_match_opts(lua: &Lua, pattern: &str) -> Table {
        let opts = lua.create_table().unwrap();
        opts.set("line_match", pattern).unwrap();
        opts
    }

    #[test]
    fn extract_params_line_match_default_none() {
        assert!(
            request_params(PUBLIC_URL, None)
                .unwrap()
                .line_match
                .is_none()
        );
    }

    #[test]
    fn extract_params_line_match_compiled() {
        let lua = Lua::new();
        let opts = line_match_opts(&lua, KEEP_PATTERN);
        let params = request_params(PUBLIC_URL, Some(&opts)).unwrap();
        assert_eq!(params.line_match.unwrap().as_str(), KEEP_PATTERN);
    }

    #[test_case("(" ; "unclosed_group")]
    #[test_case("[a-" ; "unclosed_class")]
    fn extract_params_line_match_invalid_errors(pattern: &str) {
        let lua = Lua::new();
        let opts = line_match_opts(&lua, pattern);
        let Err(err) = request_params(PUBLIC_URL, Some(&opts)) else {
            panic!("invalid line_match accepted");
        };
        assert!(err.starts_with(INVALID_LINE_MATCH), "{err}");
    }

    #[test_case(KEEP_PATTERN, "vllm:a 1\nvllm:generation_tokens_total{e=\"0\"} 2.0\nvllm:b 3\n", "vllm:generation_tokens_total{e=\"0\"} 2.0\n" ; "lf_terminated")]
    #[test_case(KEEP_PATTERN, "vllm:a 1\r\nvllm:generation_tokens_total 2.0\r\n", "vllm:generation_tokens_total 2.0\r\n" ; "crlf_terminated")]
    #[test_case(KEEP_PATTERN, "vllm:generation_tokens_total 2.0", "vllm:generation_tokens_total 2.0" ; "no_trailing_newline")]
    #[test_case("generation", "vllm:a 1\nvllm:generation_tokens_total 2.0\n", "vllm:generation_tokens_total 2.0\n" ; "unanchored")]
    #[test_case(r" 2\.0$", "vllm:a 2.0 1\r\nvllm:b 2.0\r\n", "vllm:b 2.0\r\n" ; "end_anchor_skips_line_terminator")]
    fn keep_lines_matching_keeps_only_matching_lines(pattern: &str, body: &str, expected: &str) {
        let kept = keep_lines_matching(body.as_bytes(), &Regex::new(pattern).unwrap());
        assert_eq!(String::from_utf8(kept).unwrap(), expected);
    }

    /// The pool is process-wide, so the tests that touch it run one at a time.
    static POOL_TESTS: Mutex<()> = Mutex::new(());

    fn pool_tests_serialized() -> MutexGuard<'static, ()> {
        POOL_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    const FIRST_POOL_TIMEOUT_SECS: u64 = 400;
    /// Long enough that the sweep sees it as idle, short enough for a test.
    const OLDER_THAN_IDLE_TTL: Duration = Duration::new(CLIENT_IDLE_TTL.as_secs() + 1, 0);
    /// A monotonic clock starts at boot, so `Instant::now()` cannot always
    /// afford an age. Every test reads the clock this far ahead.
    const CLOCK_HEADROOM_SECS: u64 = 3600;

    fn clock() -> Instant {
        Instant::now() + Duration::from_secs(CLOCK_HEADROOM_SECS)
    }

    fn client_key(timeout_secs: u64) -> ClientKey {
        ClientKey {
            timeout: Some(Duration::from_secs(timeout_secs)),
            route: Route::Guarded(None),
        }
    }

    fn a_client_seen_at(seen: Instant) -> PooledClient {
        (Arc::new(HttpClient::new().unwrap()), seen)
    }

    fn ago(now: Instant, since: Duration) -> Instant {
        now - since
    }

    /// A pool at the cap. One client was last used `oldest_ago` before `now`,
    /// the rest just now.
    fn a_full_pool(now: Instant, oldest_ago: Duration) -> ClientPool {
        let mut pool: ClientPool = (1..MAX_POOLED_CLIENTS as u64)
            .map(|n| {
                (
                    client_key(FIRST_POOL_TIMEOUT_SECS + n),
                    a_client_seen_at(now),
                )
            })
            .collect();
        pool.insert(
            client_key(FIRST_POOL_TIMEOUT_SECS),
            a_client_seen_at(ago(now, oldest_ago)),
        );
        pool
    }

    #[test]
    fn make_room_always_frees_a_slot_for_the_newcomer() {
        let now = clock();
        // Every client the same age, so nothing but the count decides.
        let mut pool = a_full_pool(now, Duration::ZERO);
        make_room(&mut pool, now);
        assert!(
            pool.len() < MAX_POOLED_CLIENTS,
            "an insert on a full pool would go over the cap"
        );
    }

    #[test]
    fn make_room_drops_the_least_recently_used_client() {
        let now = clock();
        let mut pool = a_full_pool(now, Duration::from_secs(60));
        let oldest = client_key(FIRST_POOL_TIMEOUT_SECS);
        make_room(&mut pool, now);
        assert!(!pool.contains_key(&oldest), "the oldest client stayed");
        assert_eq!(pool.len(), MAX_POOLED_CLIENTS - 1);
    }

    #[test]
    fn make_room_hands_back_idle_clients_even_when_the_pool_is_not_full() {
        let now = clock();
        let idle = client_key(FIRST_POOL_TIMEOUT_SECS);
        let fresh = client_key(FIRST_POOL_TIMEOUT_SECS + 1);
        let mut pool = ClientPool::new();
        pool.insert(
            idle.clone(),
            a_client_seen_at(ago(now, OLDER_THAN_IDLE_TTL)),
        );
        pool.insert(fresh.clone(), a_client_seen_at(now));
        make_room(&mut pool, now);
        assert!(
            !pool.contains_key(&idle),
            "an idle client holds a thread and a socket for nothing"
        );
        assert!(pool.contains_key(&fresh));
    }

    fn params_with_timeout(secs: u64) -> RequestParams {
        RequestParams {
            timeout: Some(Duration::from_secs(secs)),
            ..redirect_params(PUBLIC_URL)
        }
    }

    #[test]
    fn the_pool_never_grows_past_the_cap() {
        let _guard = pool_tests_serialized();
        clear_client_pool();
        for secs in 101..101 + 3 * MAX_POOLED_CLIENTS as u64 {
            pooled_client(&params_with_timeout(secs)).unwrap();
        }
        assert!(
            lock_pool().len() <= MAX_POOLED_CLIENTS,
            "pool grew past the cap: {}",
            lock_pool().len()
        );
    }

    #[test]
    fn clearing_the_pool_hands_every_client_back() {
        let _guard = pool_tests_serialized();
        clear_client_pool();
        let params = params_with_timeout(DEFAULT_TIMEOUT_SECS);
        let client = pooled_client(&params).unwrap();
        assert_eq!(lock_pool().len(), 1);
        clear_client_pool();
        assert!(lock_pool().is_empty());
        assert!(
            !Arc::ptr_eq(&client, &pooled_client(&params).unwrap()),
            "a cleared client came back out of the pool"
        );
    }

    #[test]
    fn pooled_client_is_reused_when_the_pool_is_full() {
        let _guard = pool_tests_serialized();
        clear_client_pool();
        let params: Vec<_> = (0..MAX_POOLED_CLIENTS as u64)
            .map(|n| params_with_timeout(FIRST_POOL_TIMEOUT_SECS + n))
            .collect();
        let clients: Vec<_> = params.iter().map(|p| pooled_client(p).unwrap()).collect();
        for (params, client) in params.iter().zip(&clients) {
            assert!(
                Arc::ptr_eq(client, &pooled_client(params).unwrap()),
                "a hit on a full pool was evicted"
            );
        }
    }

    #[test]
    fn pooled_client_is_reused_for_the_same_key() {
        let _guard = pool_tests_serialized();
        clear_client_pool();
        let params = redirect_params(PUBLIC_URL);
        let first = pooled_client(&params).unwrap();
        let second = pooled_client(&params).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeat call built a new client"
        );
    }

    fn a_pinned_route() -> Route {
        Route::Guarded(Some(DnsPin {
            host: "example.com".to_string(),
            port: HTTPS_PORT,
            addr: IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
        }))
    }

    /// The other request states the default timeout out loud, so the route is
    /// the only thing that tells the two apart.
    #[test_case(a_pinned_route() ; "a_pinned_hop")]
    #[test_case(Route::Provider ; "a_provider_origin")]
    fn pooled_client_is_keyed_on_the_route(route: Route) {
        let _guard = pool_tests_serialized();
        clear_client_pool();
        let plain = redirect_params(PUBLIC_URL);
        let other = RequestParams {
            route,
            ..params_with_timeout(DEFAULT_TIMEOUT_SECS)
        };
        assert!(!Arc::ptr_eq(
            &pooled_client(&plain).unwrap(),
            &pooled_client(&other).unwrap()
        ));
    }
}
