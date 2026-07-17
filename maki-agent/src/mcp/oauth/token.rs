use std::io::Read;

use isahc::HttpClient;
use isahc::http::Request;
use maki_storage::auth::{OAuthTokens, now_millis};

use super::OAuthError;

#[allow(clippy::too_many_arguments)]
pub async fn exchange_code(
    client: &HttpClient,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    client_id: &str,
    client_secret: Option<&str>,
    resource: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
        ("client_id", client_id),
        ("resource", resource),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    token_request(client, token_endpoint, &params).await
}

pub async fn refresh_token(
    client: &HttpClient,
    token_endpoint: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
    resource: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("resource", resource),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let mut tokens = token_request(client, token_endpoint, &params).await?;
    if tokens.refresh.is_empty() {
        tokens.refresh = refresh_token.to_string();
    }
    Ok(tokens)
}

async fn token_request(
    client: &HttpClient,
    token_endpoint: &str,
    params: &[(&str, &str)],
) -> Result<OAuthTokens, OAuthError> {
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let req = Request::post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.into_bytes())
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
        let mut body_str = String::new();
        let _ = response.body_mut().read_to_string(&mut body_str);
        return Err(OAuthError::ServerRejected {
            status: response.status().as_u16(),
            body: body_str,
        });
    }

    let mut body_str = String::new();
    response
        .body_mut()
        .read_to_string(&mut body_str)
        .map_err(|e| OAuthError::Network(e.to_string()))?;

    parse_token_response(&body_str)
}

fn parse_token_response(body: &str) -> Result<OAuthTokens, OAuthError> {
    let resp: serde_json::Value =
        serde_json::from_str(body).map_err(|e| OAuthError::InvalidResponse(e.to_string()))?;

    let access = resp["access_token"]
        .as_str()
        .ok_or_else(|| OAuthError::InvalidResponse("missing access_token".into()))?
        .to_string();
    let refresh = resp["refresh_token"].as_str().unwrap_or("").to_string();
    let expires_in = resp["expires_in"].as_u64().unwrap_or(3600);
    let expires = now_millis() + expires_in * 1000;

    Ok(OAuthTokens {
        access,
        refresh,
        expires,
        account_id: None,
    })
}

use std::fmt::Write;

pub(super) fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                let _ = write!(result, "%{b:02X}");
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("foo=bar&baz"), "foo%3Dbar%26baz");
        assert_eq!(url_encode("abc-def_ghi.jkl~mno"), "abc-def_ghi.jkl~mno");
    }

    #[test_case(r#"{"access_token":"a1","refresh_token":"r1","expires_in":60}"#, "a1", "r1" ; "rotated_refresh")]
    #[test_case(r#"{"access_token":"a1","expires_in":60}"#, "a1", "" ; "missing_refresh_is_empty")]
    fn parse_token_response_fields(body: &str, access: &str, refresh: &str) {
        let tokens = parse_token_response(body).unwrap();
        assert_eq!(tokens.access, access);
        assert_eq!(tokens.refresh, refresh);
    }

    #[test]
    fn parse_token_response_missing_access_is_error() {
        assert!(parse_token_response(r#"{"refresh_token":"r1"}"#).is_err());
    }
}
