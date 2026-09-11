//! Provider error types with retry semantics.
//! Retryable: 429, 5xx, IO, HTTP transport. Non-retryable: other 4xx, JSON parse, config,
//! channel closed, user cancel. `user_message()` returns human-readable text for each variant.

use isahc::AsyncReadResponseExt;

use crate::providers::opencode::{self, NonLoginError};

/// Request fields that cap the *output*. A 400 naming one of them is about the
/// cap we sent, never about the prompt being too big.
const OUTPUT_CAP_FIELDS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
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
    pub fn is_retryable(&self) -> bool {
        if self.is_context_overflow() || self.is_quota_exhausted() {
            return false;
        }
        match self {
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            Self::Io(_) | Self::Http(_) | Self::Timeout { .. } => true,
            Self::Config { .. }
            | Self::Tool { .. }
            | Self::Channel
            | Self::Json(_)
            | Self::Cancelled
            | Self::EmptySummary
            | Self::HttpRequest(_) => false,
        }
    }

    /// Returns true if the error indicates a context window overflow.
    ///
    /// Provider error formats:
    /// - Anthropic:  413 "prompt is too long"  <https://docs.anthropic.com/en/docs/errors>
    /// - OpenAI:     400 "maximum context length is X tokens"  <https://platform.openai.com/docs/guides/error-codes>
    /// - Gemini:     400 "input token count exceeds" / "too many tokens"  <https://ai.google.dev/gemini-api/docs/troubleshooting>
    /// - Ollama:     400 "context length exceeded"  <https://docs.ollama.com/api/errors>
    /// - llama.cpp:  400 "exceeds the available context size"  <https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-context.cpp>
    /// - Bedrock:    400 ValidationException "Input is too long for requested model"  <https://repost.aws/knowledge-center/bedrock-validation-exception-errors>
    /// - DeepSeek:   400 "maximum context length is X tokens"  <https://api-docs.deepseek.com/quick_start/pricing>
    /// - Mistral:    400 "too large for model with X maximum context length"  <https://docs.mistral.ai/resources/known-limitations>
    /// - OpenRouter: 400 "endpoint's maximum context length is X tokens"  <https://openrouter.ai/docs/api/reference/errors-and-debugging.mdx>
    /// - Synthetic:  400 pass-through from upstream models (OpenAI-compatible)  <https://synthetic.new>
    /// - OpenCode Go: 400 `MissingSessionID`, worded as a missing-header
    ///   problem rather than a size one -- in practice this is what their
    ///   gateway does with a compaction request for a very large session
    ///   (megabytes of previously-cached history sent in one uncached body),
    ///   not a genuine header bug: maki always sets `x-opencode-session` (see
    ///   `providers::opencode::QUIRKS`), and the same session id keeps
    ///   working for every ordinary turn right up until compaction has to
    ///   send the whole history at once. Treating it as overflow lets
    ///   compaction's existing shrink-and-retry handle this the same way it
    ///   already does every other provider's real overflow error, instead of
    ///   surfacing a confusing dead end. <https://github.com/tontinton/maki/issues/935>
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::Api { status: 413, .. } => true,
            Self::Api {
                status: 400,
                message,
                ..
            } => {
                let m = message.to_lowercase();
                if m.contains("missingsessionid") {
                    return true;
                }
                // `Invalid 'max_tokens': integer above maximum value` reads as
                // "token" plus "maximum" and would sail through the sniff
                // below, but the caller answers an overflow by summarizing the
                // whole session away, and no amount of that fixes a cap we
                // guessed too high. Our own 100k default for unknown
                // OpenAI-kind models is exactly how you hit this.
                if OUTPUT_CAP_FIELDS.iter().any(|field| m.contains(field)) {
                    return false;
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
                is_scope && is_overflow
            }
            _ => false,
        }
    }

    pub fn is_auth_error(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. }) && self.non_login_error().is_none()
    }

    /// OpenCode serves billing and plan failures on the statuses we otherwise
    /// read as a stale token, and only that provider knows its error types.
    fn non_login_error(&self) -> Option<NonLoginError> {
        let Self::Api { status, message } = self else {
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
            Self::Api { status, message } => format!("API error ({status}): {message}"),
            Self::Tool { tool, message } => format!("{tool}: {message}"),
            Self::Io(e) => format!("I/O error: {e}"),
            Self::Http(_) => "connection error, check your network".into(),
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
        let message = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read error body".into());
        Self::Api { status, message }
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
            other => Self::Api {
                status: 0,
                message: other.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::providers::opencode::{NON_LOGIN_FALLBACK_MESSAGE, QUOTA_FALLBACK_MESSAGE};

    const QUOTA_MESSAGE: &str = "Weekly usage limit reached. Resets in 3 hours.";
    const MODEL_MESSAGE: &str = "Your trial has ended.";
    const RATE_LIMITED_RETRY_MESSAGE: &str = "Rate limited";

    fn api(status: u16) -> AgentError {
        AgentError::Api {
            status,
            message: String::new(),
        }
    }

    fn api_msg(status: u16, message: &str) -> AgentError {
        AgentError::Api {
            status,
            message: message.into(),
        }
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
        let err = AgentError::Api {
            status,
            message: "bad input".into(),
        };
        assert_eq!(err.user_message(), expected);
    }

    #[test]
    fn timeout_is_retryable() {
        assert!(AgentError::Timeout { secs: 30 }.is_retryable());
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
    #[test_case(400, "Invalid API key", false                                                             ; "auth_error")]
    #[test_case(500, "Internal server error", false                                                       ; "server_error")]
    #[test_case(400, "The output is too long", false                                                      ; "output_not_context")]
    // A cap we sent, not a prompt we grew. Compacting cannot fix either of these.
    #[test_case(400, "Invalid 'max_tokens': integer above maximum value. Expected a value <= 32768", false ; "openai_max_tokens_cap")]
    #[test_case(400, "max_completion_tokens is too large: 100000", false                                  ; "openai_max_completion_tokens_cap")]
    #[test_case(400, "max_output_tokens exceeds the model maximum", false                                 ; "max_output_tokens_cap")]
    // OpenCode Go: https://github.com/tontinton/maki/issues/935 -- worded as
    // a missing-header error, but the header is always sent; this is their
    // gateway's response to a compaction request too large to route.
    #[test_case(400, r#"{"type":"error","error":{"type":"MissingSessionID","message":"Error from provider (Console Go): Request is missing x-opencode-session and cannot be routed efficiently. Please see https://opencode.ai/docs/go/#where-can-i-use-it"}}"#, true ; "opencode_missing_session_id")]
    fn is_context_overflow(status: u16, message: &str, expected: bool) {
        assert_eq!(api_msg(status, message).is_context_overflow(), expected);
    }

    #[test]
    fn context_overflow_is_not_retryable() {
        let err = api_msg(400, "request exceeds the available context size");
        assert!(err.is_context_overflow());
        assert!(!err.is_retryable());
    }
}
