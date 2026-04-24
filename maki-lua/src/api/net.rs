use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use futures_lite::io::AsyncReadExt;
use isahc::config::{Configurable, RedirectPolicy};
use isahc::{AsyncBody, HttpClient, Request};
use mlua::{Lua, Result as LuaResult, Table, Value};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_RETRIES: u32 = 3;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CF_MITIGATED: &str = "cf-mitigated";
const CF_CHALLENGE: &str = "challenge";
const FALLBACK_USER_AGENT: &str = "maki";

struct RequestParams {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    max_bytes: usize,
    retries: u32,
}

struct ResponseData {
    body: String,
    status: u16,
    content_type: String,
}

pub(crate) fn create_net_table(lua: &Lua) -> LuaResult<Table> {
    let net = lua.create_table()?;

    // Supports both Neovim's current vim.net.request(url, opts, on_response) signature
    // and the proposed multi-method API: vim.net.request(url, opts) with opts.method.
    // See: https://github.com/neovim/neovim/issues/38946
    net.set(
        "request",
        lua.create_async_function(|lua, (url, opts): (String, Option<Table>)| async move {
            let params = match extract_request_params(&url, opts.as_ref()) {
                Ok(p) => p,
                Err(e) => return Ok((Value::Nil, Value::String(lua.create_string(&e)?))),
            };
            match do_request(params).await {
                Ok(resp) => {
                    let tbl = lua.create_table()?;
                    tbl.set("body", resp.body)?;
                    tbl.set("status", resp.status)?;
                    tbl.set("content_type", resp.content_type)?;
                    Ok((Value::Table(tbl), Value::Nil))
                }
                Err(e) => Ok((Value::Nil, Value::String(lua.create_string(&e)?))),
            }
        })?,
    )?;

    Ok(net)
}

fn extract_request_params(url: &str, opts: Option<&Table>) -> Result<RequestParams, String> {
    let url = validate_and_upgrade_url(url)?;
    check_ssrf(&url)?;

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

async fn do_request(params: RequestParams) -> Result<ResponseData, String> {
    let client = HttpClient::builder()
        .timeout(params.timeout)
        .redirect_policy(RedirectPolicy::Follow)
        .build()
        .map_err(|e| format!("client error: {e}"))?;

    let is_get = params.method.eq_ignore_ascii_case("GET");
    let mut last_err = String::new();

    let mut response = 'retry: {
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
                            Ok(resp) => break 'retry resp,
                            Err(e) => last_err = format!("request failed: {e}"),
                        }
                    } else if status >= 500 && attempt < params.retries {
                        last_err = format!("HTTP {status}");
                        continue;
                    } else {
                        break 'retry resp;
                    }
                }
                Err(e) => last_err = format!("request failed: {e}"),
            }
        }
        return Err(last_err);
    };

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

fn extract_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_port = rest.split('/').next()?;
    if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next()
    } else {
        host_port.split(':').next()
    }
}

fn check_ssrf(url: &str) -> Result<(), String> {
    let host = extract_host(url).ok_or("cannot extract host from URL")?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(format!("blocked: {ip} is a private/metadata address"));
        }
        return Ok(());
    }

    let addr = format!("{host}:443");
    if let Ok(addrs) = addr.to_socket_addrs() {
        for sa in addrs {
            if is_private_ip(&sa.ip()) {
                return Err(format!(
                    "blocked: {host} resolves to private address {}",
                    sa.ip()
                ));
            }
        }
    }
    Ok(())
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
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
            if bytes[0] == 0xfe && (bytes[1] & 0xc0) == 0x80 {
                return true;
            }
            if bytes[0] & 0xfe == 0xfc {
                return true;
            }
            false
        }
    }
}

fn validate_and_upgrade_url(url: &str) -> Result<String, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        return Ok(format!("https://{rest}"));
    }
    if url.starts_with("https://") {
        return Ok(url.to_string());
    }
    Err(format!(
        "URL must start with http:// or https://, got: {url}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use test_case::test_case;

    #[test_case("https://example.com", "https://example.com" ; "https_passthrough")]
    #[test_case("http://example.com", "https://example.com" ; "http_upgraded_to_https")]
    fn validate_and_upgrade_url_valid(input: &str, expected: &str) {
        assert_eq!(validate_and_upgrade_url(input).unwrap(), expected);
    }

    #[test_case("ftp://example.com" ; "unsupported_scheme")]
    #[test_case("example.com" ; "bare_domain")]
    fn validate_and_upgrade_url_invalid(input: &str) {
        assert!(validate_and_upgrade_url(input).is_err());
    }

    #[test_case("https://8.8.8.8", Ok(()) ; "public_ip_allowed")]
    #[test_case("https://127.0.0.1", Err(()) ; "loopback_blocked")]
    #[test_case("https://192.168.1.1", Err(()) ; "private_blocked")]
    #[test_case("https://10.0.0.1", Err(()) ; "rfc1918_10_blocked")]
    #[test_case("https://172.16.0.1", Err(()) ; "rfc1918_172_blocked")]
    #[test_case("https://169.254.169.254", Err(()) ; "aws_metadata_blocked")]
    #[test_case("https://[::1]", Err(()) ; "ipv6_loopback_blocked")]
    #[test_case("https://[::ffff:127.0.0.1]", Err(()) ; "ipv4_mapped_loopback_blocked")]
    #[test_case("https://0.0.0.0", Err(()) ; "unspecified_blocked")]
    #[test_case("https://[::ffff:169.254.169.254]", Err(()) ; "ipv4_mapped_metadata_blocked")]
    fn check_ssrf_cases(url: &str, expected: Result<(), ()>) {
        match expected {
            Ok(()) => assert!(check_ssrf(url).is_ok(), "{url} should be allowed"),
            Err(()) => assert!(check_ssrf(url).is_err(), "{url} should be blocked"),
        }
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
    #[test_case(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)), false ; "v6_global_unicast")]
    fn is_private_ip_cases(ip: IpAddr, expected: bool) {
        assert_eq!(is_private_ip(&ip), expected);
    }

    #[test_case("https://example.com", Some("example.com") ; "simple_domain")]
    #[test_case("https://example.com:8080/path", Some("example.com") ; "domain_with_port")]
    #[test_case("https://[::1]/path", Some("::1") ; "bracketed_ipv6")]
    #[test_case("https://[::1]:8080/path", Some("::1") ; "bracketed_ipv6_with_port")]
    #[test_case("https://192.168.1.1:443", Some("192.168.1.1") ; "ipv4_with_port")]
    #[test_case("not-a-url", None ; "no_scheme")]
    fn extract_host_cases(url: &str, expected: Option<&str>) {
        assert_eq!(extract_host(url), expected);
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
        let net = create_net_table(&lua).unwrap();
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
        let params = extract_request_params("https://example.com", None).unwrap();
        assert_eq!(params.url, "https://example.com");
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
        let params = extract_request_params("https://example.com", Some(&opts)).unwrap();
        assert_eq!(params.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn extract_params_post_with_body() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("method", "POST").unwrap();
        opts.set("body", r#"{"key":"val"}"#).unwrap();
        let params = extract_request_params("https://example.com", Some(&opts)).unwrap();
        assert_eq!(params.method, "POST");
        assert_eq!(params.body, br#"{"key":"val"}"#);
    }

    #[test]
    fn extract_params_http_upgraded_to_https() {
        let params = extract_request_params("http://example.com", None).unwrap();
        assert_eq!(params.url, "https://example.com");
    }

    #[test]
    fn extract_params_headers_collected() {
        let lua = Lua::new();
        let headers = lua.create_table().unwrap();
        headers.set("Authorization", "Bearer tok").unwrap();
        headers.set("Accept", "text/html").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("headers", headers).unwrap();
        let params = extract_request_params("https://example.com", Some(&opts)).unwrap();
        assert_eq!(params.headers.len(), 2);
        assert!(
            params
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer tok")
        );
        assert!(
            params
                .headers
                .iter()
                .any(|(k, v)| k == "Accept" && v == "text/html")
        );
    }
}
