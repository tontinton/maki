//! The one rule for every header maki sends: each name goes out once.
//!
//! RFC 9110 (section 5.3) only lets a field repeat when its definition is a
//! list, and leaves two copies of any other field to the receiver's guess:
//! the first, the last, or a 400. So headers are kept in a [`HeaderMap`] and
//! layered by name, never appended. Where maki, a declaration, the
//! credentials and the user all name one header, the most specific of them
//! wins: maki's own defaults, then a declaration's static `headers`, then the
//! credentials, then `[<slug>.headers]` from `providers.toml`.

use std::str::from_utf8;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("'{0}' is not a valid header name")]
    Name(String),
    #[error("header '{0}' has a control character in its value")]
    Value(String),
    #[error("header '{0}' is given twice (header names are case-insensitive)")]
    Duplicate(String),
}

pub fn header_name(name: &str) -> Result<HeaderName, HeaderError> {
    HeaderName::from_bytes(name.as_bytes()).map_err(|_| HeaderError::Name(name.to_owned()))
}

pub fn header_value(name: &str, value: &str) -> Result<HeaderValue, HeaderError> {
    HeaderValue::from_str(value).map_err(|_| HeaderError::Value(name.to_owned()))
}

/// A header table as someone wrote it. `Authorization` and `authorization`
/// in one table are refused rather than letting the table's iteration order
/// pick a winner.
pub fn parse_headers<N: AsRef<str>, V: AsRef<str>>(
    pairs: impl IntoIterator<Item = (N, V)>,
) -> Result<HeaderMap, HeaderError> {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let (name, value) = (name.as_ref(), value.as_ref());
        if headers
            .insert(header_name(name)?, header_value(name, value)?)
            .is_some()
        {
            return Err(HeaderError::Duplicate(name.to_owned()));
        }
    }
    Ok(headers)
}

/// Lays `layer` over `headers`: every name in `layer` replaces whatever
/// `headers` held under it. Spelled out because `HeaderMap`'s own `extend`
/// appends when handed pairs.
pub fn overlay(headers: &mut HeaderMap, layer: &HeaderMap) {
    for (name, value) in layer {
        headers.insert(name, value.clone());
    }
}

/// The names and values as text. [`header_value`] only admits text, so no
/// entry it parsed is lost.
pub fn header_pairs(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), from_utf8(value.as_bytes()).ok()?)))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const AGENT_HEADER: &str = "user-agent";
    const MAKI_AGENT: &str = "maki";
    const GATEWAY_AGENT: &str = "gateway";
    const ACCEPT_HEADER: &str = "accept";
    const JSON: &str = "application/json";

    #[test_case(&[("Authorization", "a"), ("authorization", "b")], HeaderError::Duplicate("authorization".into()) ; "case_variants")]
    #[test_case(&[("bad header", "a")], HeaderError::Name("bad header".into()) ; "space_in_the_name")]
    #[test_case(&[("x-title", "line\nbreak")], HeaderError::Value("x-title".into()) ; "control_character")]
    fn parse_headers_refuses(pairs: &[(&str, &str)], expected: HeaderError) {
        assert_eq!(parse_headers(pairs.iter().copied()).unwrap_err(), expected);
    }

    #[test]
    fn overlay_replaces_by_name_and_keeps_the_rest() {
        let mut headers =
            parse_headers([(AGENT_HEADER, MAKI_AGENT), (ACCEPT_HEADER, JSON)]).unwrap();
        let layer = parse_headers([("User-Agent", GATEWAY_AGENT)]).unwrap();

        overlay(&mut headers, &layer);

        let agents: Vec<_> = headers.get_all(AGENT_HEADER).iter().collect();
        assert_eq!(agents, [GATEWAY_AGENT]);
        assert_eq!(headers[ACCEPT_HEADER], JSON);
    }
}
