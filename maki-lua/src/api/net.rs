use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use isahc::config::Configurable;
use isahc::{HttpClient, Request};
use mlua::{Lua, Result as LuaResult, Table, Value};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CF_MITIGATED: &str = "cf-mitigated";
const CF_CHALLENGE: &str = "challenge";
const FALLBACK_USER_AGENT: &str = "maki";

pub(crate) fn create_net_table(lua: &Lua) -> LuaResult<Table> {
    let net = lua.create_table()?;

    net.set(
        "request",
        lua.create_function(|lua, (url, opts): (String, Option<Table>)| {
            match do_request(lua, &url, opts.as_ref()) {
                Ok(tbl) => Ok((Value::Table(tbl), Value::Nil)),
                Err(e) => Ok((Value::Nil, Value::String(lua.create_string(&e)?))),
            }
        })?,
    )?;

    Ok(net)
}

fn build_request(url: &str, user_agent: &str, opts: Option<&Table>) -> Result<Request<()>, String> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(url)
        .header("User-Agent", user_agent);

    if let Some(headers) = opts.and_then(|o| o.get::<Table>("headers").ok()) {
        for pair in headers.pairs::<String, String>() {
            let (k, v) = pair.map_err(|e| format!("invalid header: {e}"))?;
            builder = builder.header(k.as_str(), v.as_str());
        }
    }

    builder
        .body(())
        .map_err(|e| format!("request build error: {e}"))
}

fn do_request(lua: &Lua, url: &str, opts: Option<&Table>) -> Result<Table, String> {
    let url = validate_and_upgrade_url(url)?;
    check_ssrf(&url)?;

    let max_bytes = opts
        .and_then(|o| o.get::<usize>("max_bytes").ok())
        .unwrap_or(DEFAULT_MAX_BYTES);

    let timeout = Duration::from_secs(
        opts.and_then(|o| o.get::<u64>("timeout").ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );

    let client = HttpClient::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("client error: {e}"))?;

    let response = client
        .send(build_request(&url, USER_AGENT, opts)?)
        .map_err(|e| format!("request failed: {e}"))?;

    let is_cf_challenge = response.status().as_u16() == 403
        && response
            .headers()
            .get(CF_MITIGATED)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains(CF_CHALLENGE));

    let mut response = if is_cf_challenge {
        client
            .send(build_request(&url, FALLBACK_USER_AGENT, opts)?)
            .map_err(|e| format!("request failed: {e}"))?
    } else {
        response
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
        && len > max_bytes
    {
        return Err(format!("response too large: {len} bytes"));
    }

    let mut bytes = Vec::new();
    response
        .body_mut()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read error: {e}"))?;

    if bytes.len() > max_bytes {
        return Err(format!("response too large: {} bytes", bytes.len()));
    }

    let body = String::from_utf8_lossy(&bytes).into_owned();

    let tbl = lua.create_table().map_err(|e| e.to_string())?;
    tbl.set("body", body).map_err(|e| e.to_string())?;
    tbl.set("status", status).map_err(|e| e.to_string())?;
    tbl.set("content_type", content_type)
        .map_err(|e| e.to_string())?;
    Ok(tbl)
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
}
