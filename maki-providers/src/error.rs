//! Provider error types with retry semantics.
//! Retryable: 429, 5xx, IO, HTTP transport. Non-retryable: other 4xx, JSON parse, config,
//! channel closed, user cancel. `retry_kind()` says which budget a retry draws
//! from, so a connection that was never made can stop while a connection that
//! dropped is waited out. `user_message()` returns human-readable text for each
//! variant.

use std::{io, time::Duration};

use isahc::{AsyncReadResponseExt, error::ErrorKind as HttpErrorKind};
use serde_json::Value;

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
const UNREADABLE_BODY_MESSAGE: &str = "unable to read error body";
const RETRY_AFTER_HEADER: &str = "retry-after";
/// The request field a server names when it refuses reasoning summaries. Every
/// Responses implementation spells the rejection with a different `code`, so
/// the field is the only stable part of the answer.
const REASONING_SUMMARY_PARAM: &str = "reasoning.summary";

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

/// Everything the retry loop and the compaction path can tell about an error,
/// and nothing else. Two errors that project the same behave the same, which
/// is the comparison replay goldens need out of a type that cannot be
/// `PartialEq` itself. The test at the bottom of this file is the proof, and it
/// is what lets each provider go without its own retry fixtures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorProjection {
    Api {
        status: u16,
        retry_after: Option<Duration>,
        /// The body is read only through these three answers, so two wordings
        /// that classify alike are the same error here.
        overflow: Option<Overflow>,
        quota_exhausted: bool,
        auth_error: bool,
    },
    Config,
    Tool,
    /// The kind is all anyone reads off an io error.
    Io(io::ErrorKind),
    /// Transports split on whether a connection was ever made.
    Http {
        connect_failure: bool,
    },
    HttpRequest,
    Json,
    Channel,
    Cancelled,
    Timeout,
    EmptySummary,
}

impl AgentError {
    pub fn projection(&self) -> ErrorProjection {
        match self {
            Self::Api {
                status,
                retry_after,
                ..
            } => ErrorProjection::Api {
                status: *status,
                retry_after: *retry_after,
                overflow: self.overflow(),
                quota_exhausted: self.is_quota_exhausted(),
                auth_error: self.is_auth_error(),
            },
            Self::Config { .. } => ErrorProjection::Config,
            Self::Tool { .. } => ErrorProjection::Tool,
            Self::Io(e) => ErrorProjection::Io(e.kind()),
            Self::Http(e) => ErrorProjection::Http {
                connect_failure: is_connect_failure(e),
            },
            Self::HttpRequest(_) => ErrorProjection::HttpRequest,
            Self::Json(_) => ErrorProjection::Json,
            Self::Channel => ErrorProjection::Channel,
            Self::Cancelled => ErrorProjection::Cancelled,
            Self::Timeout { .. } => ErrorProjection::Timeout,
            Self::EmptySummary => ErrorProjection::EmptySummary,
        }
    }

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

    pub fn is_unsupported_reasoning_summary(&self) -> bool {
        let Self::Api { message, .. } = self else {
            return false;
        };
        let Ok(body) = serde_json::from_str::<Value>(message) else {
            return false;
        };
        let error = &body["error"];
        error["param"].as_str() == Some(REASONING_SUMMARY_PARAM)
            || error["message"]
                .as_str()
                .is_some_and(|message| message.contains(REASONING_SUMMARY_PARAM))
    }

    /// Whether *this key* is the problem rather than the account, which is a
    /// different question from whether the request is worth retrying: the retry
    /// loop asks it for every error, since a 401 or a 403 is dead for this key
    /// and fine for the next one.
    pub fn should_rotate_key(&self) -> bool {
        // A plan quota is per-account, so the other keys are just as spent and
        // walking the pool only burns them in turn.
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
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| UNREADABLE_BODY_MESSAGE.into());
        Self::from_parts(
            response.status().as_u16(),
            |name| response.headers().get(name).and_then(|v| v.to_str().ok()),
            body,
        )
    }

    /// The one classifier for a failed HTTP exchange, built from what any
    /// client can hand over: the status, a header lookup by lowercase name,
    /// and the body text. [`Self::from_response`] is this over isahc, and a
    /// plugin's `maki.net` response reaches it through the same parts, so a
    /// failure reads the same whichever side made the request.
    pub fn from_parts<'a>(
        status: u16,
        header: impl Fn(&str) -> Option<&'a str>,
        body: String,
    ) -> Self {
        Self::Api {
            status,
            message: body,
            retry_after: header(RETRY_AFTER_HEADER).and_then(parse_retry_after),
        }
    }

    /// How long the server asked us to wait, when it bothered to say. Always a
    /// positive duration: only [`Self::from_parts`] ever reads headers, and an
    /// error built any other way answers `None` and the caller falls back on
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
///
/// `ErrorKind::Timeout` stays out: it covers both an expired connect timeout
/// and a stall on a stream that was already flowing, which want opposite
/// budgets, and the error alone cannot say which happened. So it retries as
/// [`RetryKind::Transient`], unbounded, and a blackholed SYN keeps trying where
/// a refused port gives up in seconds. Splitting the two means tracking whether
/// any byte ever arrived.
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
    use std::{collections::HashSet, mem::discriminant};

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
    /// Retry budgets are what these tests measure, so the key walk must not add
    /// attempts of its own.
    const ONE_KEY: usize = 1;

    fn api(status: u16) -> AgentError {
        AgentError::api(status, "")
    }

    fn api_msg(status: u16, message: &str) -> AgentError {
        AgentError::api(status, message)
    }

    const SUMMARY_BODY: &str = r#"{"error":{"message":"Your organization must be verified to generate reasoning summaries.","param":"reasoning.summary","code":"unsupported_value"}}"#;
    const UNKNOWN_PARAMETER_BODY: &str = r#"{"error":{"message":"Unknown parameter: 'reasoning.summary'.","param":"reasoning.summary","code":"unknown_parameter"}}"#;
    const NULL_CODE_BODY: &str =
        r#"{"error":{"message":"bad request","param":"reasoning.summary","code":null}}"#;
    const MESSAGE_ONLY_BODY: &str =
        r#"{"error":{"message":"Unsupported value for reasoning.summary."}}"#;
    const NO_PARAM_BODY: &str = r#"{"error":{"code":"unsupported_value","message":"Unsupported value: 'flash' is not one of the supported efforts."}}"#;

    #[test_case(SUMMARY_BODY, true  ; "summary_unsupported")]
    #[test_case(UNKNOWN_PARAMETER_BODY, true ; "unknown_parameter_code")]
    #[test_case(NULL_CODE_BODY, true ; "null_code")]
    #[test_case(MESSAGE_ONLY_BODY, true ; "message_names_param")]
    #[test_case(NO_PARAM_BODY, false ; "unsupported_value_without_param")]
    #[test_case(r#"{"error":{"code":"unsupported_value","param":"other.param"}}"#, false ; "other_param")]
    #[test_case("not json", false ; "not_json")]
    fn unsupported_reasoning_summary_matches_body(body: &str, expected: bool) {
        assert_eq!(
            api_msg(400, body).is_unsupported_reasoning_summary(),
            expected
        );
        assert!(!api(400).is_unsupported_reasoning_summary());
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
        let mut state = RetryState::new(RetryPolicy::default(), ONE_KEY);
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

    #[test_case(Some("7"), Some(Duration::from_secs(7)) ; "retry_after_read")]
    #[test_case(None, None                              ; "no_retry_after")]
    fn from_parts_reads_status_retry_after_and_body(
        retry_after: Option<&str>,
        expected: Option<Duration>,
    ) {
        const BODY: &str = r#"{"error":"slow down"}"#;
        let error = AgentError::from_parts(
            429,
            |name| retry_after.filter(|_| name == RETRY_AFTER_HEADER),
            BODY.into(),
        );
        assert!(matches!(
            &error,
            AgentError::Api { status: 429, message, .. } if message == BODY
        ));
        assert_eq!(error.retry_after(), expected);
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

    /// One sample per `AgentError` variant. A new variant means a new sample
    /// in [`corpus`] and a bump here, or the corpus quietly stops covering it.
    const VARIANT_COUNT: usize = 11;
    /// One status per branch the answers below split on: refused for size
    /// (400, 413), stale token (401), dead key but live account (403), rate
    /// limit (429), server trouble (500). The rest behave like one of these.
    const CORPUS_STATUSES: [u16; 6] = [400, 401, 403, 413, 429, 500];
    const CORPUS_RETRY_AFTER: Option<Duration> = Some(Duration::from_secs(30));
    const PLAIN_MESSAGE: &str = "bad input";
    const OVERFLOW_MESSAGE: &str = "Input exceeds context limit";
    /// A second wording of the same refusal: different body, same answers.
    const OVERFLOW_ALIAS: &str = "context length exceeded";
    const TOOL_NAME: &str = "bash";
    const CONFIG_MESSAGE: &str = "no model configured";
    const TIMEOUT_SECS: u64 = 30;
    const INVALID_URI: &str = "http://[";
    const INVALID_JSON: &str = "{";

    /// Every variant, and for `Api` every wording and status the answers are
    /// known to turn on, crossed with both `Retry-After` shapes.
    fn corpus() -> Vec<AgentError> {
        let quota = opencode_body("GoUsageLimitError", QUOTA_MESSAGE);
        let model = opencode_body("ModelError", MODEL_MESSAGE);
        let messages = [
            "",
            PLAIN_MESSAGE,
            OVERFLOW_MESSAGE,
            OVERFLOW_ALIAS,
            ANTHROPIC_BUDGET,
            quota.as_str(),
            model.as_str(),
        ];
        let mut corpus = Vec::new();
        for status in CORPUS_STATUSES {
            for message in messages {
                for retry_after in [None, CORPUS_RETRY_AFTER] {
                    corpus.push(AgentError::Api {
                        status,
                        message: message.into(),
                        retry_after,
                    });
                }
            }
        }
        // One kind on each side of the connect/transient split, plus the second
        // connect failure so the two that share a projection have to agree.
        corpus.extend(
            [
                io::ErrorKind::ConnectionRefused,
                io::ErrorKind::UnexpectedEof,
            ]
            .map(|kind| AgentError::Io(kind.into())),
        );
        corpus.extend(
            [
                HttpErrorKind::ConnectionFailed,
                HttpErrorKind::NameResolution,
                HttpErrorKind::TlsEngine,
            ]
            .map(|kind| AgentError::Http(kind.into())),
        );
        corpus.extend([
            AgentError::Config {
                message: CONFIG_MESSAGE.into(),
            },
            AgentError::Tool {
                tool: TOOL_NAME.into(),
                message: PLAIN_MESSAGE.into(),
            },
            AgentError::HttpRequest(
                isahc::http::Uri::try_from(INVALID_URI)
                    .expect_err("a malformed uri")
                    .into(),
            ),
            AgentError::Json(
                serde_json::from_str::<Value>(INVALID_JSON).expect_err("truncated json"),
            ),
            AgentError::Channel,
            AgentError::Cancelled,
            AgentError::Timeout { secs: TIMEOUT_SECS },
            AgentError::EmptySummary,
        ]);
        corpus
    }

    /// Every answer the retry loop and the compaction path ever ask an error
    /// for. Named fields rather than a tuple so a failure says which one moved.
    #[derive(Debug, PartialEq)]
    struct Observables {
        retry_kind: Option<RetryKind>,
        retry_after: Option<Duration>,
        overflow: Option<Overflow>,
        quota_exhausted: bool,
        auth_error: bool,
        rotate_key: bool,
    }

    impl Observables {
        fn of(error: &AgentError) -> Self {
            Self {
                retry_kind: error.retry_kind(),
                retry_after: error.retry_after(),
                overflow: error.overflow(),
                quota_exhausted: error.is_quota_exhausted(),
                auth_error: error.is_auth_error(),
                rotate_key: error.should_rotate_key(),
            }
        }
    }

    /// Why [`AgentError::projection`] earns its keep: the answers above are a
    /// function of the projection, so a replay golden that pins the projection
    /// pins the behaviour, and no provider needs retry fixtures of its own.
    #[test]
    fn equal_projections_agree_on_every_observable() {
        let mut seen: Vec<(ErrorProjection, Observables)> = Vec::new();
        let mut variants = HashSet::new();
        let mut shared_projections = false;

        for error in corpus() {
            variants.insert(discriminant(&error));
            let projection = error.projection();
            let observed = Observables::of(&error);
            match seen.iter().position(|(p, _)| *p == projection) {
                Some(twin) => {
                    shared_projections = true;
                    assert_eq!(
                        seen[twin].1, observed,
                        "{error:?} projects to {projection:?}"
                    );
                }
                None => seen.push((projection, observed)),
            }
        }

        assert_eq!(
            variants.len(),
            VARIANT_COUNT,
            "the corpus skipped a variant"
        );
        // All-distinct projections would pass without proving a thing.
        assert!(
            shared_projections,
            "the corpus needs errors that differ below the projection"
        );
    }
}
