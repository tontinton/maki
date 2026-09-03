//! Minimal OIDC relying-party flow on `ureq`: discovery, authorization
//! redirect, code exchange, JWKS-backed ID token validation. No cookie-jar
//! magic; the anchor owns session cookies itself.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::Deserialize;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const CLOCK_SKEW_SECS: i64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("discovery failed at {issuer}: {message}")]
    Discovery { issuer: String, message: String },
    #[error("token exchange failed: {0}")]
    Exchange(String),
    #[error("id token invalid: {0}")]
    InvalidToken(String),
    #[error("id token signature invalid")]
    BadSignature,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http: {0}")]
    Http(String),
}

/// The subset of discovery the anchor uses.
#[derive(Debug, Deserialize)]
pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub issuer: String,
}

#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// The anchor's own `https://host` origin; callback is `{origin}/callback`.
    pub origin: String,
}

#[derive(Debug, Clone)]
pub struct Claims {
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// Discovery document fetch with a fresh HTTP client per call (rare).
pub fn discover(config: &OidcConfig) -> Result<Discovery, OidcError> {
    let well_known = format!(
        "{}/.well-known/openid-configuration",
        config.issuer.trim_end_matches('/')
    );
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let body = agent
        .get(&well_known)
        .call()
        .map_err(|e| OidcError::Discovery {
            issuer: config.issuer.clone(),
            message: e.to_string(),
        })?
        .into_string()
        .map_err(|e| OidcError::Discovery {
            issuer: config.issuer.clone(),
            message: e.to_string(),
        })?;
    let mut discovery: Discovery = serde_json::from_str(&body)?;
    if discovery.issuer.is_empty() {
        discovery.issuer = config.issuer.clone();
    }
    Ok(discovery)
}

/// Build the browser redirect URL for the authorization code flow.
pub fn authorization_url(config: &OidcConfig, discovery: &Discovery, state: &str) -> String {
    let mut url =
        ::url::Url::parse(&discovery.authorization_endpoint).expect("issuer endpoint is a url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair(
            "redirect_uri",
            &format!("{}/callback", config.origin.trim_end_matches('/')),
        )
        .append_pair("scope", "openid profile email")
        .append_pair("state", state);
    url.to_string()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// Exchange the authorization code for an ID token and validate it.
pub fn exchange_code(
    config: &OidcConfig,
    discovery: &Discovery,
    code: &str,
    state: &str,
) -> Result<Claims, OidcError> {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let response = agent
        .post(&discovery.token_endpoint)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form_urlencoded(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            (
                "redirect_uri",
                &format!("{}/callback", config.origin.trim_end_matches('/')),
            ),
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("state", state),
        ]))
        .map_err(|e| OidcError::Exchange(e.to_string()))?;
    let body = response
        .into_string()
        .map_err(|e| OidcError::Exchange(e.to_string()))?;
    let token: TokenResponse = serde_json::from_str(&body)?;
    validate_id_token(config, discovery, &token.id_token)
}

fn form_urlencoded(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&encode_component(key));
        out.push('=');
        out.push_str(&encode_component(value));
    }
    out
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn validate_id_token(
    config: &OidcConfig,
    discovery: &Discovery,
    token: &str,
) -> Result<Claims, OidcError> {
    let (header_b64, payload_b64, signature_b64) = split_jwt(token)?;
    let header: JwtHeader = serde_json::from_slice(&b64_decode(&header_b64)?)?;
    if header.alg != "RS256" {
        return Err(OidcError::InvalidToken(format!(
            "unsupported alg {:?}",
            header.alg
        )));
    }
    let claims: serde_json::Value = serde_json::from_slice(&b64_decode(&payload_b64)?)?;

    // Audience and issuer are checked locally; expiry with skew tolerance.
    if claims["iss"].as_str() != Some(discovery.issuer.as_str()) {
        return Err(OidcError::InvalidToken(format!(
            "issuer mismatch: {:?} != {:?}",
            claims["iss"].as_str(),
            discovery.issuer
        )));
    }
    if claims["aud"].as_str() != Some(config.client_id.as_str()) {
        let audiences: Option<Vec<&str>> = claims["aud"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect());
        let ok = audiences.is_some_and(|a| a.contains(&config.client_id.as_str()));
        if !ok {
            return Err(OidcError::InvalidToken("audience mismatch".to_owned()));
        }
    }
    let exp = claims["exp"]
        .as_i64()
        .ok_or_else(|| OidcError::InvalidToken("missing exp".to_owned()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if exp + CLOCK_SKEW_SECS < now {
        return Err(OidcError::InvalidToken("token expired".to_owned()));
    }

    let jwks = fetch_jwks(discovery)?;
    let key = jwks
        .keys
        .iter()
        .find(|k| k.kid.as_deref() == header.kid.as_deref() && k.kty == "RSA")
        .ok_or(OidcError::BadSignature)?;
    let signature = b64_decode(signature_b64.trim())?;
    verify_rs256(key, &signed_bytes(&header_b64, &payload_b64), &signature)?;

    Ok(Claims {
        sub: claims["sub"]
            .as_str()
            .ok_or_else(|| OidcError::InvalidToken("missing sub".to_owned()))?
            .to_owned(),
        email: claims["email"].as_str().map(str::to_owned),
        name: claims["name"].as_str().map(str::to_owned),
    })
}

fn signed_bytes(header_b64: &str, payload_b64: &str) -> Vec<u8> {
    format!("{header_b64}.{payload_b64}").into_bytes()
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    n: String,
    e: String,
}

fn split_jwt(token: &str) -> Result<(String, String, String), OidcError> {
    let mut parts = token.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => Ok((h.to_owned(), p.to_owned(), s.to_owned())),
        _ => Err(OidcError::InvalidToken("not a JWT".to_owned())),
    }
}

fn b64_decode(data: &str) -> Result<Vec<u8>, OidcError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .map_err(|e| OidcError::InvalidToken(format!("base64: {e}")))
}

fn fetch_jwks(discovery: &Discovery) -> Result<Jwks, OidcError> {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let body = agent
        .get(&discovery.jwks_uri)
        .call()
        .map_err(|e| OidcError::Http(e.to_string()))?
        .into_string()
        .map_err(|e| OidcError::Http(e.to_string()))?;
    Ok(serde_json::from_str(&body)?)
}

/// RS256 over the signed input using the JWKS RSA public key: SHA-256 +
/// PKCS#1 v1.5 over num-bigint modular exponentiation. No openssl build dep.
fn verify_rs256(key: &Jwk, message: &[u8], signature: &[u8]) -> Result<(), OidcError> {
    if key.kty != "RSA" {
        return Err(OidcError::BadSignature);
    }
    let modulus = rs_base64_to_biguint(&key.n)?;
    let exponent = rs_base64_to_biguint(&key.e)?;
    let sig_int = num_bigint::BigUint::from_bytes_be(signature);
    if sig_int >= modulus {
        return Err(OidcError::BadSignature);
    }
    let recovered = sig_int.modpow(&exponent, &modulus);
    let padded = recovered.to_bytes_be();

    // DigestInfo for SHA-256 (RFC 8017 section 9.2 note 1).
    const DIGEST_INFO: &[u8] = &[
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(message);
    let modulus_len = modulus.to_bytes_be().len();
    let mut expected = vec![0u8; modulus_len - 1];
    // EM = 0x01 || PS(0xff...) || 0x00 || DigestInfo || digest, per RFC 8017;
    // the leading 0x00 is the byte cut off by the length above.
    expected[0] = 0x01;
    let tail_len = 1 + DIGEST_INFO.len() + digest.len();
    if tail_len > modulus_len - 1 {
        return Err(OidcError::BadSignature);
    }
    let ps_len = modulus_len - 1 - tail_len - 1;
    for byte in &mut expected[1..1 + ps_len] {
        *byte = 0xFF;
    }
    let mut cursor = 1 + ps_len;
    expected[cursor] = 0x00;
    cursor += 1;
    expected[cursor..cursor + DIGEST_INFO.len()].copy_from_slice(DIGEST_INFO);
    cursor += DIGEST_INFO.len();
    expected[cursor..].copy_from_slice(&digest);

    if padded == expected {
        Ok(())
    } else {
        Err(OidcError::BadSignature)
    }
}

fn rs_base64_to_biguint(value: &str) -> Result<num_bigint::BigUint, OidcError> {
    Ok(num_bigint::BigUint::from_bytes_be(&b64_decode(value)?))
}
