//! Provider error types with retry semantics.
//! Retryable: 429, 5xx, IO, HTTP transport. Non-retryable: other 4xx, JSON parse, config,
//! channel closed, user cancel. `retry_kind()` says which budget a retry draws
//! from, so a connection that was never made can stop while a connection that
//! dropped is waited out. `user_message()` returns human-readable text for each
//! variant.

use std::{io, time::Duration};

use isahc::{AsyncReadResponseExt, error::ErrorKind as HttpErrorKind};

use crate::{
    providers::opencode::{self, NonLoginError},
    retry::RetryKind,
};

/// Request fields that cap the *output*. A 400 naming one of them is about the
/// cap we sent, never about the prompt being too big.
const OUTPUT_CAP_FIELDS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];
/// Markers of the OpenAI/vLLM shape, which quotes the two halves separately:
/// `you requested 10000 tokens (6000 in the messages, 4000 in the completion)`.
const MESSAGES_HALF: &str = " in the messages";
const COMPLETION_HALF: &str = " in the completion";
const OPENAI_LIMIT: &str = "maximum context length is ";
const CONNECT_FAILED_MESSAGE: &str =
    "could not connect, check the server is running and the base URL is correct";
const NETWORK_ERROR_MESSAGE: &str = "connection error, check your network";

/// Why a provider refused a request for size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// The transcript alone does not fit. Dropping context is the only way
    /// through.
    Prompt,
    /// The transcript plus the output budget does not fit, though the
    /// transcript by itself may well. Asking for less output costs nothing, so
    /// this must never be answered by summarizing the transcript away.
    ///
    /// The fields are whatever the server quoted. A server that counts the
    /// halves out loud also hands over an exact measurement of a prompt maki
    /// could only estimate.
    Budget {
        prompt: Option<u32>,
        limit: Option<u32>,
    },
}

fn trailing_number(text: &str) -> Option<u32> {
    let text = text.trim_end();
    let start = text.len() - text.bytes().rev().take_while(u8::is_ascii_digit).count();
    text[start..].parse().ok()
}

fn leading_number(text: &str) -> Option<u32> {
    let text = text.trim_start();
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    text[..end].parse().ok()
}

/// Digits just before `marker`, e.g. the `6000` of `6000 in the messages`.
fn number_before(text: &str, marker: &str) -> Option<u32> {
    trailing_number(text.split_once(marker)?.0)
}

/// Digits just after `marker`, e.g. the `8192` of
/// `maximum context length is 8192 tokens`.
fn number_after(text: &str, marker: &str) -> Option<u32> {
    leading_number(text.split_once(marker)?.1)
}

/// Anthropic reports the condition as arithmetic:
/// ``input length and `max_tokens` exceed context limit: 199773 + 8192 > 200000``.
/// All three numbers have to parse, or any prose with a `+` and a `>` would
/// match.
fn sum_over_limit(text: &str) -> Option<Overflow> {
    let (head, limit) = text.split_once(" > ")?;
    let (prompt, output) = head.rsplit_once(" + ")?;
    leading_number(output)?;
    Some(Overflow::Budget {
        prompt: Some(trailing_number(prompt)?),
        limit: Some(leading_number(limit)?),
    })
}

fn budget_overflow(m: &str) -> Option<Overflow> {
    sum_over_limit(m).or_else(|| {
        // A zero-token completion half means the prompt alone is the problem,
        // and no budget is small enough to fix that.
        (number_before(m, COMPLETION_HALF)? > 0).then(|| Overflow::Budget {
            prompt: number_before(m, MESSAGES_HALF),
            limit: number_after(m, OPENAI_LIMIT),
        })
    })
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("API error ({status}): {message}")]
    Api {
        status: u16,
        message: String,
        /// What the server's `Retry-After` header asked for, when it sent one.
        retry_after: Option<Duration>,
    },
    #[error("{message}")]
    Config { message: String },
    #[error("tool error in {tool}: {message}")]
    Tool { tool: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] isahc::Error),
    #[error("http request: {0}")]
    HttpRequest(#[from] isahc::http::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("channel send failed")]
    Channel,
    #[error("cancelled")]
    Cancelled,
    #[error("stream timed out after {secs}s of inactivity")]
    Timeout { secs: u64 },
    #[error("compaction returned no summary")]
    EmptySummary,
}

impl AgentError {
    /// An API error with no `Retry-After` behind it. Everything that is not a
    /// response we read the headers of lands here, SSE error frames included.
    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Which budget trying again would draw from, or `None` when trying again
    /// is pointless. The kinds are priced differently, so the retry loop needs
    /// more than a yes or no.
    pub fn retry_kind(&self) -> Option<RetryKind> {
        if self.is_context_overflow() || self.is_quota_exhausted() {
            return None;
        }
        match self {
            Self::Api { status: 429, .. } => Some(RetryKind::RateLimit),
            Self::Api { status, .. } if *status >= 500 => Some(RetryKind::Transient),
            Self::Http(e) if is_connect_failure(e) => Some(RetryKind::Connect),
            Self::Io(e) if e.kind() == io::ErrorKind::ConnectionRefused => Some(RetryKind::Connect),
            Self::Io(_) | Self::Http(_) => Some(RetryKind::Transient),
            Self::Timeout { .. } => Some(RetryKind::Timeout),
            Self::Api { .. }
            | Self::Config { .. }
            | Self::Tool { .. }
            | Self::Channel
            | Self::Json(_)
            | Self::Cancelled
            | Self::EmptySummary
            | Self::HttpRequest(_) => None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retry_kind().is_some()
    }

    /// Refused for size, either cause.
    pub fn is_context_overflow(&self) -> bool {
        self.overflow().is_some()
    }

    /// Why a provider refused the request for size, and what it said about the
    /// numbers. The two causes have opposite remedies: one is fixed by asking
    /// for less output, the other only by dropping context.
    ///
    /// Provider error formats:
    /// - Anthropic:  413 "prompt is too long"  <https://docs.anthropic.com/en/docs/errors>
    /// - Anthropic:  400 "input length and `max_tokens` exceed context limit: A + B > C"
    /// - OpenAI:     400 "maximum context length is X tokens"  <https://platform.openai.com/docs/guides/error-codes>
    /// - vLLM:       400 "... you requested N tokens (A in the messages, B in the completion)"
    /// - Gemini:     400 "input token count exceeds" / "too many tokens"  <https://ai.google.dev/gemini-api/docs/troubleshooting>
    /// - Ollama:     400 "context length exceeded"  <https://docs.ollama.com/api/errors>
    /// - llama.cpp:  400 "exceeds the available context size"  <https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-context.cpp>
    /// - Bedrock:    400 ValidationException "Input is too long for requested model"  <https://repost.aws/knowledge-center/bedrock-validation-exception-errors>
    /// - DeepSeek:   400 "maximum context length is X tokens"  <https://api-docs.deepseek.com/quick_start/pricing>
    /// - Mistral:    400 "too large for model with X maximum context length"  <https://docs.mistral.ai/resources/known-limitations>
    /// - OpenRouter: 400 "endpoint's maximum context length is X tokens"  <https://openrouter.ai/docs/api/reference/errors-and-debugging.mdx>
    /// - Synthetic:  400 pass-through from upstream models (OpenAI-compatible)  <https://synthetic.new>
    pub fn overflow(&self) -> Option<Overflow> {
        let Self::Api {
            status, message, ..
        } = self
        else {
            return None;
        };
        if !matches!(status, 400 | 413) {
            return None;
        }
        let m = message.to_lowercase();
        // Before the output-cap guard below: servers that enforce
        // `prompt + max_tokens <= window` name `max_tokens` while reporting a
        // budget overflow, and reading that as "our cap was malformed" turns a
        // one-field fix into an unrecoverable error.
        if let Some(budget) = budget_overflow(&m) {
            return Some(budget);
        }
        if *status == 413 {
            return Some(Overflow::Prompt);
        }
        // `Invalid 'max_tokens': integer above maximum value` reads as "token"
        // plus "maximum" and would sail through the sniff below. The caller
        // answers a prompt overflow by summarizing the session away, and no
        // amount of that fixes a cap we guessed too high. Our own 100k default
        // for unknown OpenAI-kind models is how you hit this.
        if OUTPUT_CAP_FIELDS.iter().any(|field| m.contains(field)) {
            return None;
        }
        let is_scope = m.contains("context")
            || m.contains("token")
            || m.contains("prompt")
            || m.contains("input");
        let is_overflow = m.contains("exceeds")
            || m.contains("exceeded")
            || m.contains("too long")
            || m.contains("too many")
            || m.contains("maximum");
        (is_scope && is_overflow).then_some(Overflow::Prompt)
    }

    pub fn is_auth_error(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. }) && self.non_login_error().is_none()
    }

    /// OpenCode serves billing and plan failures on the statuses we otherwise
    /// read as a stale token, and only that provider knows its error types.
    fn non_login_error(&self) -> Option<NonLoginError> {
        let Self::Api {
            status, message, ..
        } = self
        else {
            return None;
        };
        opencode::non_login_error(*status, message)
    }

    /// A plan quota that resets on a weekly or monthly boundary: retrying just
    /// reprints the same message until the user cancels.
    fn is_quota_exhausted(&self) -> bool {
        self.non_login_error().is_some_and(|error| error.is_quota)
    }

    pub fn should_rotate_key(&self) -> bool {
        // A plan quota is per-account, so the user's other keys are just as spent.
        !self.is_quota_exhausted()
            && matches!(self, Self::Api { status, .. } if *status == 429 || *status == 401 || *status == 403)
    }

    pub fn user_message(&self) -> String {
        if let Some(error) = self.non_login_error() {
            return error.message;
        }
        match self {
            Self::Config { message } => message.clone(),
            Self::Api { status: 429, .. } => "rate limited, try again in a moment".into(),
            Self::Api { status: 529, .. } => "provider is overloaded, try again later".into(),
            Self::Api { status, .. } if *status >= 500 => format!("server error ({status})"),
            Self::Api { status: 401, .. } => {
                "authentication failed, run `maki auth login` or check your API key".into()
            }
            Self::Api {
                status, message, ..
            } => format!("API error ({status}): {message}"),
            Self::Tool { tool, message } => format!("{tool}: {message}"),
            Self::Io(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                CONNECT_FAILED_MESSAGE.into()
            }
            Self::Io(e) => format!("I/O error: {e}"),
            Self::Http(e) if is_connect_failure(e) => CONNECT_FAILED_MESSAGE.into(),
            Self::Http(_) => NETWORK_ERROR_MESSAGE.into(),
            Self::Timeout { .. } => "stream timed out, retrying".into(),
            Self::HttpRequest(e) => format!("request error: {e}"),
            Self::Json(_) => "received an invalid response from the API".into(),
            Self::Channel => "internal error, try again".into(),
            Self::Cancelled => "cancelled".into(),
            Self::EmptySummary => "compaction returned no summary, history kept as is".into(),
        }
    }

    pub async fn from_response(mut response: isahc::Response<isahc::AsyncBody>) -> Self {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let message = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read error body".into());
        Self::Api {
            status,
            message,
            retry_after,
        }
    }

    /// How long the server asked us to wait, when it bothered to say. Always a
    /// positive duration: only [`Self::from_response`] ever reads headers, and
    /// an error built any other way answers `None` and the caller falls back on
    /// its own backoff.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub fn retry_message(&self) -> String {
        if let Some(error) = self.non_login_error() {
            return error.message;
        }
        match self {
            Self::Api { status: 429, .. } => "Rate limited".into(),
            Self::Api { status: 529, .. } => "Provider is overloaded".into(),
            Self::Api { status, .. } if *status >= 500 => format!("Server error ({status})"),
            Self::Io(_) | Self::Http(_) => "Connection error".into(),
            Self::Timeout { .. } => "Stream timed out".into(),
            _ => self.to_string(),
        }
    }
}

impl<T> From<flume::SendError<T>> for AgentError {
    fn from(_: flume::SendError<T>) -> Self {
        Self::Channel
    }
}

impl From<maki_storage::StorageError> for AgentError {
    fn from(e: maki_storage::StorageError) -> Self {
        match e {
            maki_storage::StorageError::Io(io) => Self::Io(io),
            maki_storage::StorageError::Json(j) => Self::Json(j),
            other => Self::api(0, other.to_string()),
        }
    }
}

/// Nothing answered at the other end, either because no socket is listening or
/// because the host name does not resolve. isahc folds a refused connection and
/// a dead provider edge into the same `ConnectionFailed`, and telling them
/// apart is not possible from here.
fn is_connect_failure(e: &isahc::Error) -> bool {
    matches!(
        e.kind(),
        HttpErrorKind::ConnectionFailed | HttpErrorKind::NameResolution
    )
}

/// Only the delta-seconds form ("30", "60"). The HTTP-date form is legal but
/// providers do not send it, and guessing a backoff beats parsing dates.
///
/// `0` is not a hint. Anthropic's own usage endpoint answers a persistent 429
/// with `Retry-After: 0`, and sleeping for zero seconds before asking the same
/// rate limiter again is a tight loop, not a backoff.
fn parse_retry_after(value: &str) -> Option<Duration> {
    match value.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Some(Duration::from_secs(secs)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use maki_config::DEFAULT_MAX_RETRIES;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::{
        providers::opencode::{NON_LOGIN_FALLBACK_MESSAGE, QUOTA_FALLBACK_MESSAGE},
        retry::{RetryPolicy, RetryState},
    };

    const QUOTA_MESSAGE: &str = "Weekly usage limit reached. Resets in 3 hours.";
    const MODEL_MESSAGE: &str = "Your trial has ended.";
    const RATE_LIMITED_RETRY_MESSAGE: &str = "Rate limited";
    /// More rounds than any bounded budget allows, so a budget that survives
    /// all of them is the unbounded one.
    const ROUNDS: u32 = DEFAULT_MAX_RETRIES * 2;

    fn api(status: u16) -> AgentError {
        AgentError::api(status, "")
    }

    fn api_msg(status: u16, message: &str) -> AgentError {
        AgentError::api(status, message)
    }

    fn opencode_body(error_type: &str, message: &str) -> String {
        json!({"error": {"type": error_type, "message": message}}).to_string()
    }

    #[test_case(429, true  ; "rate_limit")]
    #[test_case(500, true  ; "server_error")]
    #[test_case(529, true  ; "overloaded")]
    #[test_case(400, false ; "bad_request")]
    #[test_case(401, false ; "unauthorized")]
    fn api_retryable(status: u16, expected: bool) {
        assert_eq!(api(status).is_retryable(), expected);
    }

    #[test_case(401, true  ; "unauthorized")]
    #[test_case(403, false ; "forbidden")]
    fn api_auth_error(status: u16, expected: bool) {
        assert_eq!(api(status).is_auth_error(), expected);
    }

    #[test_case("CreditsError", 401 ; "credits")]
    #[test_case("MonthlyLimitError", 401 ; "monthly_limit")]
    #[test_case("UserLimitError", 401 ; "user_limit")]
    #[test_case("GoUsageLimitError", 401 ; "go_unauthorized")]
    #[test_case("GoUsageLimitError", 429 ; "go_rate_limit")]
    #[test_case("BlackUsageLimitError", 429 ; "black_rate_limit")]
    #[test_case("FreeUsageLimitError", 429 ; "free_rate_limit")]
    fn opencode_usage_limits_preserve_details(error_type: &str, status: u16) {
        let err = api_msg(status, &opencode_body(error_type, QUOTA_MESSAGE));

        assert!(!err.is_auth_error());
        assert_eq!(err.user_message(), QUOTA_MESSAGE);
        assert_eq!(err.retry_message(), QUOTA_MESSAGE);
        // A weekly cap outlives any backoff, and every key on the account
        // shares it.
        assert!(!err.is_retryable());
        assert!(!err.should_rotate_key());
    }

    /// Not a usage cap: "trial ended", "model disabled", "no provider
    /// available". Logging in fixes none of them, but the normal status rules
    /// still apply.
    #[test_case(401, false ; "unauthorized")]
    #[test_case(429, true  ; "rate_limited")]
    fn opencode_model_error_is_not_a_login_problem(status: u16, retryable: bool) {
        let err = api_msg(status, &opencode_body("ModelError", MODEL_MESSAGE));

        assert!(!err.is_auth_error());
        assert_eq!(err.user_message(), MODEL_MESSAGE);
        assert_eq!(err.is_retryable(), retryable);
        assert!(err.should_rotate_key());
    }

    #[test_case(json!({"type": "GoUsageLimitError"}), QUOTA_FALLBACK_MESSAGE    ; "quota_missing_message")]
    #[test_case(json!({"type": "GoUsageLimitError", "message": " "}), QUOTA_FALLBACK_MESSAGE ; "quota_empty_message")]
    #[test_case(json!({"type": "ModelError"}), NON_LOGIN_FALLBACK_MESSAGE       ; "model_missing_message")]
    fn non_login_error_without_message_does_not_request_login(error: Value, expected: &str) {
        let err = api_msg(401, &json!({"error": error}).to_string());

        assert!(!err.is_auth_error());
        assert_eq!(err.user_message(), expected);
    }

    #[test_case(r#"{"error":{"type":"AuthError","message":"Invalid API key"}}"# ; "auth_error")]
    #[test_case(r#"{"error":{"type":"UnknownError","message":"quota"}}"# ; "unknown_type")]
    #[test_case(r#"{"message":"quota exceeded"}"# ; "untyped_message")]
    #[test_case("not JSON" ; "malformed_body")]
    fn other_unauthorized_errors_still_request_login(body: &str) {
        assert!(api_msg(401, body).is_auth_error());
    }

    // OpenCode's per-minute cap is a different type, and other providers ship
    // their own JSON envelopes on 429. Neither may lose its backoff.
    #[test_case(r#"{"error":{"type":"RateLimitError","message":"too many requests"}}"# ; "opencode_per_minute")]
    #[test_case(r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#       ; "anthropic_envelope")]
    #[test_case(r#"{"error":{"code":429,"message":"quota exceeded"}}"#                 ; "untyped_envelope")]
    fn other_rate_limits_stay_retryable(body: &str) {
        let err = api_msg(429, body);

        assert!(err.is_retryable());
        assert!(err.should_rotate_key());
        assert_eq!(err.retry_message(), RATE_LIMITED_RETRY_MESSAGE);
    }

    #[test_case(429, RATE_LIMITED_RETRY_MESSAGE ; "rate_limited")]
    #[test_case(529, "Provider is overloaded" ; "overloaded")]
    #[test_case(500, "Server error (500)"  ; "server_error")]
    fn retry_message_api(status: u16, expected: &str) {
        assert_eq!(api(status).retry_message(), expected);
    }

    #[test_case(429, "rate limited, try again in a moment"                              ; "user_msg_429")]
    #[test_case(529, "provider is overloaded, try again later"                           ; "user_msg_529")]
    #[test_case(500, "server error (500)"                                                 ; "user_msg_500")]
    #[test_case(401, "authentication failed, run `maki auth login` or check your API key" ; "user_msg_401")]
    #[test_case(400, "API error (400): bad input"                                         ; "user_msg_400")]
    fn user_message_api(status: u16, expected: &str) {
        let err = AgentError::api(status, "bad input");
        assert_eq!(err.user_message(), expected);
    }

    #[test_case(429, RetryKind::RateLimit ; "rate_limit")]
    #[test_case(500, RetryKind::Transient  ; "server_error")]
    #[test_case(529, RetryKind::Transient  ; "overloaded")]
    fn api_retry_kind(status: u16, expected: RetryKind) {
        assert_eq!(api(status).retry_kind(), Some(expected));
    }

    #[test]
    fn a_timeout_is_its_own_kind() {
        assert_eq!(
            AgentError::Timeout { secs: 30 }.retry_kind(),
            Some(RetryKind::Timeout)
        );
    }

    /// Retries this error is granted before the loop has to give up, stopping
    /// at `ROUNDS` for a budget with no ceiling.
    fn retries_granted(error: &AgentError) -> u32 {
        let kind = error.retry_kind().expect("a transport failure retries");
        let mut state = RetryState::new(RetryPolicy::default());
        (0..ROUNDS)
            .take_while(|_| state.next_delay(kind, error.retry_after()).is_some())
            .count() as u32
    }

    /// A dead provider edge and a local server nobody started both land in
    /// `ConnectionFailed`, so the run has to give up on it, while a failure on
    /// a connection that was made is the network being the network and is
    /// waited out.
    #[test_case(HttpErrorKind::ConnectionFailed, DEFAULT_MAX_RETRIES ; "a_refused_connection_gives_up")]
    #[test_case(HttpErrorKind::NameResolution, DEFAULT_MAX_RETRIES   ; "an_unresolvable_host_gives_up")]
    #[test_case(HttpErrorKind::Io, ROUNDS                            ; "a_read_error_keeps_retrying")]
    #[test_case(HttpErrorKind::TlsEngine, ROUNDS                     ; "a_tls_failure_keeps_retrying")]
    fn a_transport_failure_is_waited_out_only_once_connected(kind: HttpErrorKind, expected: u32) {
        assert_eq!(retries_granted(&AgentError::Http(kind.into())), expected);
    }

    #[test_case(io::ErrorKind::ConnectionRefused, DEFAULT_MAX_RETRIES ; "a_refused_socket_gives_up")]
    #[test_case(io::ErrorKind::UnexpectedEof, ROUNDS                  ; "a_truncated_read_keeps_retrying")]
    fn an_io_failure_gives_up_when_nothing_is_listening(kind: io::ErrorKind, expected: u32) {
        assert_eq!(retries_granted(&AgentError::Io(kind.into())), expected);
    }

    #[test_case(HttpErrorKind::ConnectionFailed, CONNECT_FAILED_MESSAGE ; "connect_failure_names_the_server")]
    #[test_case(HttpErrorKind::TlsEngine, NETWORK_ERROR_MESSAGE         ; "other_transport_blames_the_network")]
    fn user_message_transport(kind: HttpErrorKind, expected: &str) {
        assert_eq!(AgentError::Http(kind.into()).user_message(), expected);
    }

    #[test_case("30", Some(Duration::from_secs(30)) ; "delta_seconds")]
    #[test_case(" 5 ", Some(Duration::from_secs(5))  ; "whitespace")]
    #[test_case("Wed, 21 Oct 2015 07:28:00 GMT", None ; "http_date")]
    #[test_case("", None                             ; "empty")]
    #[test_case("-3", None                           ; "negative")]
    // A zero would be slept on and come straight back with the same 429.
    #[test_case("0", None                            ; "zero")]
    fn retry_after_header_is_read_as_seconds(value: &str, expected: Option<Duration>) {
        assert_eq!(parse_retry_after(value), expected);
    }

    // llama.cpp: https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-context.cpp
    #[test_case(400, "request (268914 tokens) exceeds the available context size (262144 tokens)", true   ; "llama_cpp_overshoot")]
    // OpenAI: https://platform.openai.com/docs/guides/error-codes
    #[test_case(400, "Input exceeds context limit", true                                                 ; "openai_style")]
    // OpenAI: https://platform.openai.com/docs/guides/error-codes
    #[test_case(400, "This model's maximum context length is 8192 tokens. However, you requested 9850 tokens", true ; "openai_max_context")]
    // Gemini: https://ai.google.dev/gemini-api/docs/troubleshooting
    #[test_case(400, "The input token count exceeds the maximum number of tokens allowed", true           ; "gemini_exceeds")]
    // Gemini: https://ai.google.dev/gemini-api/docs/troubleshooting
    #[test_case(400, "Request contains too many tokens. Please reduce the input size.", true              ; "gemini_too_many")]
    // Gemini: https://ai.google.dev/gemini-api/docs/troubleshooting
    #[test_case(400, "Your input context is too long.", true                                              ; "gemini_500_input")]
    // Ollama: https://docs.ollama.com/api/errors
    #[test_case(400, "context length exceeded", true                                                      ; "ollama")]
    // Anthropic: https://docs.anthropic.com/en/docs/errors
    #[test_case(413, "prompt is too long", true                                                           ; "anthropic_413")]
    // HTTP 413: https://www.rfc-editor.org/rfc/rfc9110.html#name-413-content-too-large
    #[test_case(413, "Payload too large", true                                                            ; "generic_413")]
    // DeepSeek: https://api-docs.deepseek.com/quick_start/pricing
    #[test_case(400, "This model's maximum context length is 131072 tokens. However, you requested 168754 tokens", true ; "deepseek")]
    // Mistral: https://docs.mistral.ai/resources/known-limitations
    #[test_case(400, "Prompt contains 321774 tokens and 0 draft tokens, too large for model with 262144 maximum context length", true ; "mistral")]
    // OpenRouter: https://openrouter.ai/docs/api/reference/errors-and-debugging.mdx
    #[test_case(400, "This endpoint's maximum context length is 200000 tokens. However, you requested about 5028244 tokens", true ; "openrouter")]
    // Bedrock: https://repost.aws/knowledge-center/bedrock-validation-exception-errors
    #[test_case(400, "Input is too long for requested model.", true                                                          ; "bedrock")]
    #[test_case(400, "Input is too long for the model", true                                              ; "too_long_input")]
    #[test_case(400, "Rate limit exceeded", false                                                         ; "not_context")]
    // Arithmetic in unrelated prose must not read as a quoted budget overflow.
    #[test_case(400, "Rate limit exceeded, 5 + 3 requests", false                                         ; "arithmetic_without_a_limit")]
    #[test_case(400, "Invalid API key", false                                                             ; "auth_error")]
    #[test_case(500, "Internal server error", false                                                       ; "server_error")]
    #[test_case(400, "The output is too long", false                                                      ; "output_not_context")]
    // A cap we sent, not a prompt we grew. Compacting cannot fix either of these.
    #[test_case(400, "Invalid 'max_tokens': integer above maximum value. Expected a value <= 32768", false ; "openai_max_tokens_cap")]
    #[test_case(400, "max_completion_tokens is too large: 100000", false                                  ; "openai_max_completion_tokens_cap")]
    #[test_case(400, "max_output_tokens exceeds the model maximum", false                                 ; "max_output_tokens_cap")]
    fn is_context_overflow(status: u16, message: &str, expected: bool) {
        assert_eq!(api_msg(status, message).is_context_overflow(), expected);
    }

    #[test]
    fn context_overflow_is_not_retryable() {
        let err = api_msg(400, "request exceeds the available context size");
        assert!(err.is_context_overflow());
        assert!(!err.is_retryable());
    }

    const ANTHROPIC_BUDGET: &str = "input length and `max_tokens` exceed context limit: 199773 + 8192 > 200000, decrease input length or max_tokens and try again";
    const VLLM_BUDGET: &str = "This model's maximum context length is 1048576 tokens. However, you requested 1051000 tokens (100000 in the messages, 951000 in the completion). Please reduce the length of the messages or completion.";
    const VLLM_PROMPT_ONLY: &str = "This model's maximum context length is 8192 tokens. However, you requested 9000 tokens (9000 in the messages, 0 in the completion).";

    /// These servers name `max_tokens` while reporting a budget overflow, which
    /// the output-cap guard used to read as a malformed cap of our own and
    /// report as unrecoverable.
    #[test_case(
        ANTHROPIC_BUDGET,
        Overflow::Budget { prompt: Some(199_773), limit: Some(200_000) }
        ; "anthropic_reports_the_sum"
    )]
    #[test_case(
        VLLM_BUDGET,
        Overflow::Budget { prompt: Some(100_000), limit: Some(1_048_576) }
        ; "vllm_reports_the_halves"
    )]
    // No budget is small enough to fit a prompt that already overflows alone.
    #[test_case(VLLM_PROMPT_ONLY, Overflow::Prompt ; "an_empty_completion_half_is_a_prompt_overflow")]
    #[test_case("prompt is too long: 250000 tokens > 200000 maximum", Overflow::Prompt ; "anthropic_prompt_alone")]
    fn overflow_kind_is_read_off_the_message(message: &str, expected: Overflow) {
        assert_eq!(api_msg(400, message).overflow(), Some(expected));
    }
}
