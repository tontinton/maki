use tokio::io::{AsyncBufReadExt, BufReader, stdin};

use super::callback::{CallbackResult, parse_query};

const PASTE_HINT: &str = "Paste the full redirect URL (or its query string with code= and state=):";
const MISSING_PARAMS: &str = "missing code or state parameter";
const STATE_MISMATCH: &str = "state mismatch (wrong or stale login attempt)";

pub(crate) async fn wait_for_paste(expected_state: &str) -> Result<CallbackResult, String> {
    // If the callback server wins the race, the blocking stdin read is dropped
    // mid-line and may swallow one buffered line. Fine here: this only runs in
    // the CLI flow, which exits right after authentication completes.
    let mut lines = BufReader::new(stdin()).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("stdin read failed: {e}"))?
    {
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        match parse_pasted(line, expected_state)? {
            Outcome::Code(result) => return Ok(result),
            Outcome::Retry(msg) => eprintln!("{msg}\n{PASTE_HINT}"),
        }
    }

    Err("stdin closed before a redirect URL was pasted".into())
}

#[derive(Debug)]
enum Outcome {
    Code(CallbackResult),
    Retry(&'static str),
}

fn parse_pasted(input: &str, expected_state: &str) -> Result<Outcome, String> {
    let params = parse_query(extract_query(input));

    if let Some((_, error)) = params.iter().find(|(k, _)| k == "error") {
        let desc = params
            .iter()
            .find(|(k, _)| k == "error_description")
            .map(|(_, v)| v.as_str())
            .unwrap_or(error);
        return Err(format!("OAuth error: {desc}"));
    }

    let state = params
        .iter()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.as_str());

    let code = params
        .iter()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.clone());

    let (Some(state), Some(code)) = (state, code) else {
        return Ok(Outcome::Retry(MISSING_PARAMS));
    };

    if state != expected_state {
        return Ok(Outcome::Retry(STATE_MISMATCH));
    }

    let issuer = params
        .iter()
        .find(|(k, _)| k == "iss")
        .map(|(_, v)| v.clone());

    Ok(Outcome::Code(CallbackResult { code, issuer }))
}

fn extract_query(input: &str) -> &str {
    let no_fragment = input.split('#').next().unwrap_or(input);
    match no_fragment.split_once('?') {
        Some((_, query)) => query,
        None => no_fragment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const STATE: &str = "expected-state";

    #[test_case("http://127.0.0.1:19876/mcp/oauth/callback?code=abc&state=expected-state", "abc", None ; "full_redirect_url")]
    #[test_case("code=abc&state=expected-state", "abc", None ; "bare_query_string")]
    #[test_case("code=a%2Bb%20c&state=expected-state", "a+b c", None ; "url_encoded_code")]
    #[test_case("http://127.0.0.1:19876/mcp/oauth/callback?state=expected-state&code=xyz#frag", "xyz", None ; "fragment_stripped")]
    #[test_case("code=abc&state=expected-state&iss=https%3A%2F%2Fauth.example.com", "abc", Some("https://auth.example.com") ; "with_issuer")]
    fn accepts_code(input: &str, expected_code: &str, expected_issuer: Option<&str>) {
        match parse_pasted(input, STATE).unwrap() {
            Outcome::Code(result) => {
                assert_eq!(result.code, expected_code);
                assert_eq!(result.issuer.as_deref(), expected_issuer);
            }
            Outcome::Retry(msg) => panic!("expected code, got retry: {msg}"),
        }
    }

    #[test_case("state=expected-state" ; "missing_code")]
    #[test_case("code=abc" ; "missing_state")]
    #[test_case("not a url at all" ; "garbage_input")]
    fn retries_on_missing_params(input: &str) {
        match parse_pasted(input, STATE).unwrap() {
            Outcome::Retry(msg) => assert_eq!(msg, MISSING_PARAMS),
            Outcome::Code(_) => panic!("expected retry"),
        }
    }

    #[test]
    fn retries_on_state_mismatch() {
        match parse_pasted("code=abc&state=wrong", STATE).unwrap() {
            Outcome::Retry(msg) => assert_eq!(msg, STATE_MISMATCH),
            Outcome::Code(_) => panic!("expected retry"),
        }
    }

    #[test]
    fn errors_on_oauth_error_param() {
        let err =
            parse_pasted("error=access_denied&error_description=User%20denied", STATE).unwrap_err();
        assert_eq!(err, "OAuth error: User denied");
    }
}
