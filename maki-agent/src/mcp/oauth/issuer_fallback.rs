//! A stopgap for #1341. Some auth servers have an issuer with a path, like
//! `https://host/api`, and publish their metadata only at
//! `https://host/api/.well-known/oauth-authorization-server`. No spec names that
//! URL, so rmcp 3.8 never asks for it and the login fails. When that happens, we
//! ask for it ourselves, once for each auth server the MCP server's protected
//! resource metadata names.
//!
//! rmcp keeps its SSRF checks private, so we use a simpler rule. Every request
//! stays on the MCP server's own origin, and the OAuth client never follows a
//! redirect off it. That way we only talk to a host the user already chose.
//!
//! Once rmcp asks for this URL itself (branch `fix/oauth-issuer-appended-discovery`),
//! its discovery succeeds and this never runs. Then delete this file and its one
//! call in `discover()`.

use reqwest::header::WWW_AUTHENTICATE;
use reqwest::{Client, Response, StatusCode};
use rmcp::transport::auth::{AuthorizationMetadata, WWWAuthenticateParams};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::info;
use url::Url;

use super::is_secure;

const METADATA_SUFFIX: &str = "/.well-known/oauth-authorization-server";
/// rmcp's cap on its own OAuth responses.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// RFC 9728 protected resource metadata. rmcp keeps its own copy private.
#[derive(Deserialize)]
struct ResourceMetadata {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
}

pub(super) async fn find(
    client: &Client,
    server_url: &str,
    www_authenticate: Option<&str>,
) -> Option<AuthorizationMetadata> {
    let server = Url::parse(server_url).ok()?;
    let advertised = match www_authenticate.and_then(|header| advertised_in(header, &server)) {
        Some(url) => url,
        None => probe(client, &server).await?,
    };
    let resource_metadata: ResourceMetadata = get_json(client, &server, advertised).await?;
    // RFC 9728 §3.3. Metadata naming some other resource could send this
    // server's login to an auth server that has nothing to do with it.
    if !names_server(&resource_metadata.resource, &server) {
        return None;
    }
    for issuer in &resource_metadata.authorization_servers {
        let Some(url) = appended_metadata_url(issuer) else {
            continue;
        };
        // `discover()` lets rmcp accept a missing `issuer`, but no spec names this
        // URL, so here the exact issuer is our only proof the document is real.
        if let Some(metadata) = get_json::<AuthorizationMetadata>(client, &server, url)
            .await
            .filter(|metadata| metadata.issuer.as_deref() == Some(issuer.as_str()))
        {
            info!(%issuer, "found OAuth metadata appended to the issuer path");
            return Some(metadata);
        }
    }
    None
}

/// rmcp does the same when it gets no challenge.
async fn probe(client: &Client, server: &Url) -> Option<Url> {
    let response = get(client, server, server.clone()).await?;
    if response.status() != StatusCode::UNAUTHORIZED {
        return None;
    }
    response
        .headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|header| advertised_in(header, server))
}

fn advertised_in(www_authenticate: &str, server: &Url) -> Option<Url> {
    WWWAuthenticateParams::parse(www_authenticate, server).resource_metadata_url
}

/// A copy of rmcp's private rule, so we accept exactly the metadata rmcp would.
/// The resource sits on this origin and its path is a prefix of the server's.
fn names_server(resource: &str, server: &Url) -> bool {
    Url::parse(resource).is_ok_and(|resource| {
        resource.fragment().is_none()
            && resource.origin() == server.origin()
            && server.path().starts_with(resource.path())
    })
}

/// A root issuer gets none, because there this is the plain RFC 8414 URL rmcp
/// already asked. RFC 8414 issuers carry no query or fragment.
fn appended_metadata_url(issuer: &str) -> Option<Url> {
    let mut url = Url::parse(issuer).ok()?;
    let issuer_path = url.path().trim_end_matches('/');
    if issuer_path.is_empty() || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let path = format!("{issuer_path}{METADATA_SUFFIX}");
    url.set_path(&path);
    Some(url)
}

async fn get_json<T: DeserializeOwned>(client: &Client, server: &Url, url: Url) -> Option<T> {
    let mut response = get(client, server, url).await?;
    if response.status() != StatusCode::OK {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

/// The OAuth client follows redirects only within one origin, so checking the
/// URL we ask is enough.
async fn get(client: &Client, server: &Url, url: Url) -> Option<Response> {
    if !may_fetch(server, &url) {
        return None;
    }
    client.get(url).send().await.ok()
}

fn may_fetch(server: &Url, url: &Url) -> bool {
    url.origin() == server.origin() && is_secure(url)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const SERVER: &str = "https://host.example.com/api/mcp";
    const ISSUER: &str = "https://host.example.com/api";
    const ISSUER_METADATA: &str =
        "https://host.example.com/api/.well-known/oauth-authorization-server";
    const LOOPBACK_SERVER: &str = "http://127.0.0.1:8080/api/mcp";
    const LOOPBACK_ISSUER_METADATA: &str =
        "http://127.0.0.1:8080/api/.well-known/oauth-authorization-server";
    const PLAIN_SERVER: &str = "http://host.example.com/api/mcp";

    #[test_case(SERVER,          ISSUER_METADATA,                 true  ; "same_origin")]
    #[test_case(SERVER,          "https://other.example.com/api", false ; "other_origin")]
    #[test_case(PLAIN_SERVER,    "http://host.example.com/api",   false ; "plain_http")]
    #[test_case(LOOPBACK_SERVER, LOOPBACK_ISSUER_METADATA,        true  ; "loopback_http")]
    fn requests_stay_on_a_secure_server_origin(server: &str, url: &str, allowed: bool) {
        let (server, url) = (Url::parse(server).unwrap(), Url::parse(url).unwrap());
        assert_eq!(may_fetch(&server, &url), allowed);
    }

    #[test_case(SERVER,                               true  ; "exact")]
    #[test_case("https://host.example.com/",          true  ; "origin")]
    #[test_case("https://host.example.com/api/mcp/",  false ; "longer_path")]
    #[test_case("https://host.example.com/other",     false ; "other_path")]
    #[test_case("https://other.example.com/api/mcp",  false ; "other_origin")]
    #[test_case("https://host.example.com/api/mcp#x", false ; "fragment")]
    fn resource_must_name_this_server(resource: &str, expected: bool) {
        assert_eq!(
            names_server(resource, &Url::parse(SERVER).unwrap()),
            expected
        );
    }

    #[test_case(ISSUER,                                  Some(ISSUER_METADATA) ; "path_issuer")]
    #[test_case("https://host.example.com/api/",         Some(ISSUER_METADATA) ; "trailing_slash")]
    #[test_case("https://host.example.com",              None                  ; "root_issuer")]
    #[test_case("https://host.example.com/api?tenant=1", None                  ; "query")]
    fn only_path_issuers_have_an_appended_url(issuer: &str, expected: Option<&str>) {
        assert_eq!(
            appended_metadata_url(issuer).as_ref().map(Url::as_str),
            expected
        );
    }
}
