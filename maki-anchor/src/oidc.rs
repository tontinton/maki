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

/// Build the browser redirect URL for the authorization code flow (with
/// PKCE; the verifier lives anchor-side against the state).
pub fn authorization_url(
    config: &OidcConfig,
    discovery: &Discovery,
    state: &str,
    code_challenge: &str,
) -> String {
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
        .append_pair("state", state)
        .append_pair("nonce", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    url.to_string()
}

/// S256 code challenge for a verifier, per RFC 7636.
pub fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
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
    code_verifier: &str,
    expected_nonce: &str,
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
            ("code_verifier", code_verifier),
        ]))
        .map_err(|e| OidcError::Exchange(e.to_string()))?;
    let body = response
        .into_string()
        .map_err(|e| OidcError::Exchange(e.to_string()))?;
    let token: TokenResponse = serde_json::from_str(&body)?;
    validate_id_token(config, discovery, &token.id_token, expected_nonce)
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
    expected_nonce: &str,
) -> Result<Claims, OidcError> {
    let (header_b64, payload_b64, signature_b64) = split_jwt(token)?;
    let header: JwtHeader = serde_json::from_slice(&b64_decode(&header_b64)?)?;
    if header.alg != "RS256" {
        return Err(OidcError::InvalidToken(format!(
            "unsupported alg {:?}",
            header.alg
        )));
    }
    // A missing kid would let an unsigned-kid token match any kid-less key;
    // real providers always label theirs.
    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| OidcError::InvalidToken("jwt header missing kid".to_owned()))?;
    let claims: serde_json::Value = serde_json::from_slice(&b64_decode(&payload_b64)?)?;

    // Audience, issuer and nonce are checked locally; expiry and not-before
    // with skew tolerance.
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
    if claims["nonce"].as_str() != Some(expected_nonce) {
        return Err(OidcError::InvalidToken("nonce mismatch".to_owned()));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let exp = claims["exp"]
        .as_i64()
        .ok_or_else(|| OidcError::InvalidToken("missing exp".to_owned()))?;
    if exp + CLOCK_SKEW_SECS < now {
        return Err(OidcError::InvalidToken("token expired".to_owned()));
    }
    if let Some(nbf) = claims["nbf"].as_i64()
        && nbf > now + CLOCK_SKEW_SECS
    {
        return Err(OidcError::InvalidToken("token not yet valid".to_owned()));
    }

    let jwks = fetch_jwks(discovery)?;
    let key = jwks
        .keys
        .iter()
        .find(|k| k.kid.as_deref() == Some(kid) && k.kty == "RSA")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OidcConfig {
        OidcConfig {
            issuer: "https://auth.example.com".into(),
            client_id: "maki-anchor".into(),
            client_secret: "s".into(),
            origin: "https://maki.example.com".into(),
        }
    }

    fn discovery() -> Discovery {
        Discovery {
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            jwks_uri: "https://auth.example.com/jwks".into(),
            issuer: "https://auth.example.com".into(),
        }
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    /// Signature garbage is fine: every case here fails on a claim check that
    /// runs before `fetch_jwks`, so the network is never touched.
    fn token(header: &str, claims: serde_json::Value) -> String {
        format!(
            "{}.{}.{}",
            b64(header),
            b64(&claims.to_string()),
            b64("sig")
        )
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({
            "iss": "https://auth.example.com",
            "aud": "maki-anchor",
            "sub": "user-1",
            "nonce": "n0nce",
            "exp": now() + 3600,
        })
    }

    fn validate(header: &str, claims: serde_json::Value, nonce: &str) -> String {
        validate_id_token(&config(), &discovery(), &token(header, claims), nonce)
            .expect_err("expected rejection")
            .to_string()
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn rejects_alg_none() {
        let msg = validate(r#"{"alg":"none","kid":"k"}"#, claims(), "n0nce");
        assert!(msg.contains("unsupported alg"), "{msg}");
    }

    #[test]
    fn rejects_missing_kid() {
        let msg = validate(r#"{"alg":"RS256"}"#, claims(), "n0nce");
        assert!(msg.contains("missing kid"), "{msg}");
    }

    #[test]
    fn rejects_issuer_and_audience_mismatch() {
        let mut bad = claims();
        bad["iss"] = serde_json::json!("https://evil");
        assert!(validate(r#"{"alg":"RS256","kid":"k"}"#, bad, "n0nce").contains("issuer mismatch"));

        let mut bad = claims();
        bad["aud"] = serde_json::json!("someone-else");
        assert!(
            validate(r#"{"alg":"RS256","kid":"k"}"#, bad, "n0nce").contains("audience mismatch")
        );
    }

    #[test]
    fn rejects_nonce_mismatch() {
        assert!(
            validate(r#"{"alg":"RS256","kid":"k"}"#, claims(), "stolen").contains("nonce mismatch")
        );
    }

    #[test]
    fn rejects_expired_and_not_yet_valid() {
        let mut bad = claims();
        bad["exp"] = serde_json::json!(now() - 3600);
        assert!(validate(r#"{"alg":"RS256","kid":"k"}"#, bad, "n0nce").contains("expired"));

        let mut bad = claims();
        bad["nbf"] = serde_json::json!(now() + 3600);
        assert!(validate(r#"{"alg":"RS256","kid":"k"}"#, bad, "n0nce").contains("not yet valid"));
    }

    #[test]
    fn array_audience_passes_the_gate() {
        // A valid array aud containing the client id must clear the audience
        // check; the next rejection is the deliberately-wrong nonce, proving
        // we got past audience (and never reach the JWKS fetch).
        let mut c = claims();
        c["aud"] = serde_json::json!(["other", "maki-anchor"]);
        let msg = validate(r#"{"alg":"RS256","kid":"k"}"#, c, "wrong-n0nce");
        assert!(
            msg.contains("nonce mismatch"),
            "audience should have passed: {msg}"
        );
    }

    #[test]
    fn rejects_malformed_jwt() {
        let msg = validate_id_token(&config(), &discovery(), "a.b", "n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("not a JWT"), "{msg}");
    }
}
