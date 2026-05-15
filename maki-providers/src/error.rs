//! Provider error types with retry semantics.
//! Retryable: 429, 5xx, IO, HTTP transport. Non-retryable: other 4xx, JSON parse, config,
//! channel closed, user cancel. `user_message()` returns human-readable text for each variant.

use isahc::AsyncReadResponseExt;

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
}

impl AgentError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            Self::Io(_) | Self::Http(_) | Self::Timeout { .. } => true,
            Self::Config { .. }
            | Self::Tool { .. }
            | Self::Channel
            | Self::Json(_)
            | Self::Cancelled
            | Self::HttpRequest(_) => false,
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
}
