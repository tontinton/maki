//! Provider error types with retry semantics.
//! Retryable: 429, 5xx, IO, HTTP transport. Non-retryable: other 4xx, JSON parse, config,
//! channel closed, user cancel. `user_message()` returns human-readable text for each variant.

use isahc::AsyncReadResponseExt;

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
        if self.is_context_overflow() {
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
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::Api { status: 413, .. } => true,
            Self::Api {
                status: 400,
                message,
                ..
            } => {
                let m = message.to_lowercase();
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
        matches!(self, Self::Api { status: 401, .. })
    }

    pub fn should_rotate_key(&self) -> bool {
        matches!(self, Self::Api { status, .. } if *status == 429 || *status == 401 || *status == 403)
    }

    pub fn user_message(&self) -> String {
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
    use super::*;
    use test_case::test_case;

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

    #[test_case(429, "Rate limited"        ; "rate_limited")]
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
