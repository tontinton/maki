use std::io::Read;

use isahc::HttpClient;
use isahc::http::Request;
use serde::Deserialize;

use super::OAuthError;

const MAX_RESPONSE_BODY: usize = 1_048_576;

#[derive(Debug)]
pub struct WwwAuthenticateInfo {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResourceMetadata {
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    pub resource: String,
    pub scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct AuthServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
}

pub fn parse_www_authenticate(header: &str) -> Option<WwwAuthenticateInfo> {
    if !header.contains("Bearer") {
        return None;
    }
    Some(WwwAuthenticateInfo {
        resource_metadata: extract_param(header, "resource_metadata"),
        scope: extract_param(header, "scope"),
    })
}

fn extract_param(header: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=\"");
    let start = header.find(&prefix)?;
    let rest = &header[start + prefix.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

struct UrlParts<'a> {
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
}

fn parse_url(url: &str) -> UrlParts<'_> {
    let base = url.trim_end_matches('/');
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let after_scheme = &base[scheme_end..];
    let (authority, path) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, ""),
    };
    UrlParts {
        scheme: &base[..scheme_end],
        authority,
        path,
    }
}

fn origin(url: &str) -> String {
    let parts = parse_url(url);
    format!("{}{}", parts.scheme, parts.authority)
}

fn well_known_url(base_url: &str, well_known: &str) -> String {
    let parts = parse_url(base_url);
    if parts.path.is_empty() || parts.path == "/" {
        format!(
            "{}{}/.well-known/{well_known}",
            parts.scheme, parts.authority
        )
    } else {
        format!(
            "{}{}/.well-known/{well_known}{}",
            parts.scheme, parts.authority, parts.path
        )
    }
}

pub(super) fn server_origin(server_url: &str) -> String {
    origin(server_url)
}

fn validate_endpoint_url(url: &str) -> Result<(), OAuthError> {
    if url.starts_with("https://")
        || url.starts_with("http://127.0.0.1")
        || url.starts_with("http://localhost")
    {
        Ok(())
    } else {
        Err(OAuthError::Other(format!(
            "endpoint URL must use HTTPS: {url}"
        )))
    }
}

pub fn validate_auth_server(meta: &AuthServerMetadata) -> Result<(), OAuthError> {
    validate_endpoint_url(&meta.authorization_endpoint)?;
    validate_endpoint_url(&meta.token_endpoint)?;
    if let Some(ref ep) = meta.registration_endpoint {
        validate_endpoint_url(ep)?;
    }
    Ok(())
}

fn resource_metadata_urls(server_url: &str) -> Vec<String> {
    let path_specific = well_known_url(server_url, "oauth-protected-resource");
    let root = format!(
        "{}/.well-known/oauth-protected-resource",
        origin(server_url)
    );
    if path_specific == root {
        vec![path_specific]
    } else {
        vec![path_specific, root]
    }
}

fn normalize_resource(url: &str) -> String {
    let parts = parse_url(url);
    format!(
        "{}{}{}",
        parts.scheme.to_ascii_lowercase(),
        parts.authority.to_ascii_lowercase(),
        parts.path
    )
}

fn validate_resource_metadata(
    meta: ResourceMetadata,
    server_url: &str,
) -> Result<ResourceMetadata, OAuthError> {
    let resource = normalize_resource(&meta.resource);
    if resource == normalize_resource(server_url)
        || resource == normalize_resource(&origin(server_url))
    {
        Ok(meta)
    } else {
        Err(OAuthError::Other(format!(
            "resource metadata resource does not match server URL: {}",
            meta.resource
        )))
    }
}

pub async fn discover_resource_metadata(
    client: &HttpClient,
    server_url: &str,
    www_auth: Option<&WwwAuthenticateInfo>,
) -> Result<ResourceMetadata, OAuthError> {
    if let Some(info) = www_auth
        && let Some(ref url) = info.resource_metadata
        && origin(url) == origin(server_url)
        && let Ok(meta) = fetch_json::<ResourceMetadata>(client, url).await
        && let Ok(meta) = validate_resource_metadata(meta, server_url)
    {
        return Ok(meta);
    }

    let mut last_err = OAuthError::Other("no candidates".into());
    for url in resource_metadata_urls(server_url) {
        match fetch_json::<ResourceMetadata>(client, &url).await {
            Ok(meta) => match validate_resource_metadata(meta, server_url) {
                Ok(meta) => return Ok(meta),
                Err(e) => last_err = e,
            },
            Err(e) => last_err = e,
        }
    }
    Err(OAuthError::Other(format!(
        "resource metadata discovery failed: {last_err}"
    )))
}

pub async fn discover_auth_server(
    client: &HttpClient,
    issuer_url: &str,
) -> Result<AuthServerMetadata, OAuthError> {
    let parts = parse_url(issuer_url);
    let has_path = !parts.path.is_empty() && parts.path != "/";

    let well_known_names = ["oauth-authorization-server", "openid-configuration"];
    let mut candidates: Vec<String> = well_known_names
        .iter()
        .map(|name| well_known_url(issuer_url, name))
        .collect();

    if has_path {
        for name in &well_known_names {
            candidates.push(format!(
                "{}{}/.well-known/{name}",
                parts.scheme, parts.authority
            ));
        }
    }

    let mut last_err = OAuthError::Other("no candidates".into());
    for url in &candidates {
        match fetch_json::<AuthServerMetadata>(client, url).await {
            Ok(meta) => {
                validate_auth_server(&meta)?;
                return Ok(meta);
            }
            Err(e) => last_err = e,
        }
    }
    Err(OAuthError::Other(format!(
        "auth server discovery failed: {last_err}"
    )))
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &HttpClient,
    url: &str,
) -> Result<T, OAuthError> {
    let req = Request::get(url)
        .header("Accept", "application/json")
        .body(())
        .map_err(|e| OAuthError::Other(e.to_string()))?;

    let mut response = smol::unblock({
        let client = client.clone();
        move || {
            client
                .send(req)
                .map_err(|e| OAuthError::Network(e.to_string()))
        }
    })
    .await?;

    if !response.status().is_success() {
        let mut body = String::new();
        let _ = response.body_mut().read_to_string(&mut body);
        return Err(OAuthError::ServerRejected {
            status: response.status().as_u16(),
            body,
        });
    }

    let mut body = String::new();
    response
        .body_mut()
        .take(MAX_RESPONSE_BODY as u64)
        .read_to_string(&mut body)
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| OAuthError::InvalidResponse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(
        r#"Bearer realm="example", resource_metadata="https://rs.example.com/.well-known/oauth-protected-resource", scope="read write""#,
        Some("https://rs.example.com/.well-known/oauth-protected-resource"),
        Some("read write")
        ; "full_header"
    )]
    #[test_case(
        "Bearer realm=\"example\"",
        None,
        None
        ; "bearer_no_resource_metadata"
    )]
    #[test_case(
        "Basic realm=\"example\"",
        None,
        None
        ; "non_bearer_returns_none"
    )]
    fn parse_www_auth(header: &str, expected_url: Option<&str>, expected_scope: Option<&str>) {
        let result = parse_www_authenticate(header);
        match (result, expected_url, expected_scope) {
            (None, None, None) => {}
            (Some(info), url, scope) => {
                assert_eq!(info.resource_metadata.as_deref(), url);
                assert_eq!(info.scope.as_deref(), scope);
            }
            (None, _, _) => panic!("expected Some, got None"),
        }
    }

    #[test_case("https://example.com",          "https://example.com" ; "bare_origin")]
    #[test_case("https://example.com/",         "https://example.com" ; "trailing_slash")]
    #[test_case("https://example.com/api/v1",   "https://example.com" ; "with_path")]
    fn server_origin_extracts_origin(url: &str, expected: &str) {
        assert_eq!(server_origin(url), expected);
    }

    #[test_case(
        "https://example.com", "oauth-protected-resource",
        "https://example.com/.well-known/oauth-protected-resource"
        ; "no_path"
    )]
    #[test_case(
        "https://example.com/api/v1", "oauth-authorization-server",
        "https://example.com/.well-known/oauth-authorization-server/api/v1"
        ; "with_path"
    )]
    fn well_known_url_construction(base: &str, name: &str, expected: &str) {
        assert_eq!(well_known_url(base, name), expected);
    }

    #[test_case(
        "https://example.com/mcp",
        &["https://example.com/.well-known/oauth-protected-resource/mcp", "https://example.com/.well-known/oauth-protected-resource"]
        ; "path_specific_then_root"
    )]
    #[test_case(
        "https://example.com",
        &["https://example.com/.well-known/oauth-protected-resource"]
        ; "root_only"
    )]
    fn resource_metadata_url_candidates(server_url: &str, expected: &[&str]) {
        assert_eq!(resource_metadata_urls(server_url), expected);
    }

    #[test_case("https://example.com/mcp", "https://example.com/mcp", true ; "matching_resource")]
    #[test_case("https://example.com/mcp/", "https://example.com/mcp", true ; "resource_trailing_slash")]
    #[test_case("https://example.com/mcp", "https://example.com/mcp/", true ; "server_url_trailing_slash")]
    #[test_case("HTTPS://EXAMPLE.com/mcp", "https://example.com/mcp", true ; "scheme_host_case_insensitive")]
    #[test_case("https://example.com", "https://example.com/mcp", true ; "origin_resource_from_root_metadata")]
    #[test_case("https://example.com/other", "https://example.com/mcp", false ; "different_resource")]
    #[test_case("https://example.com/MCP", "https://example.com/mcp", false ; "path_case_sensitive")]
    fn resource_metadata_validation(resource: &str, server_url: &str, should_pass: bool) {
        let meta = ResourceMetadata {
            authorization_servers: Vec::new(),
            resource: resource.into(),
            scopes_supported: None,
        };
        assert_eq!(
            validate_resource_metadata(meta, server_url).is_ok(),
            should_pass
        );
    }

    #[test_case("https://example.com/token",   true  ; "https_valid")]
    #[test_case("http://127.0.0.1:8080/token", true  ; "localhost_valid")]
    #[test_case("http://example.com/token",    false ; "http_rejected")]
    #[test_case("ftp://example.com/token",     false ; "ftp_rejected")]
    fn endpoint_url_validation(url: &str, should_pass: bool) {
        assert_eq!(validate_endpoint_url(url).is_ok(), should_pass);
    }
}
