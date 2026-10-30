use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{ACTION, CedarError, PRINCIPAL, TOOL_TYPE};

/// Default cedar-agent listen port (see permitio/cedar-agent).
pub const DEFAULT_AGENT_PORT: u16 = 8180;

/// One policy as cedar-agent stores and returns it on `GET /v1/policies`.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentPolicy {
    pub id: String,
    pub content: String,
}

/// The body cedar-agent expects on `POST /v1/is_authorized`.
#[derive(Debug, Serialize)]
struct AuthorizationQuery {
    principal: String,
    action: String,
    resource: String,
    context: serde_json::Value,
}

/// cedar-agent's answer to an authorization query.
#[derive(Debug, Deserialize)]
struct AuthorizationResponse {
    decision: String,
}

/// A client for a remote `cedar-agent` policy store.
///
/// The primary mode is to [`fetch_policies`](Self::fetch_policies) and
/// [`fetch_schema`](Self::fetch_schema) once at startup (and periodically),
/// then evaluate locally with a [`crate::CedarEngine`] — a network blip never
/// stalls a tool call. [`is_authorized`](Self::is_authorized) is the fallback
/// that asks the agent to decide per call, for deployments that want the agent
/// to own evaluation and have entity data maki does not.
pub struct AgentClient {
    base_url: String,
    http: reqwest::Client,
}

impl AgentClient {
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Result<Self, CedarError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| CedarError::Request(e.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    /// Fetch all policies and concatenate their `content` into one Cedar
    /// source string ready for [`crate::CedarEngine::from_source`].
    pub async fn fetch_policies(&self) -> Result<String, CedarError> {
        let url = format!("{}/v1/policies", self.base_url);
        let policies: Vec<AgentPolicy> = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| CedarError::Request(e.to_string()))?
            .json()
            .await
            .map_err(|e| CedarError::Policies(e.to_string()))?;
        Ok(policies
            .into_iter()
            .map(|p| p.content)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Fetch the schema, if the agent has one. cedar-agent serves JSON schema;
    /// the caller passes it to a [`crate::CedarEngine`] built from JSON, or
    /// omits it. Returns `None` when the agent has no schema set.
    pub async fn fetch_schema_json(&self) -> Result<Option<serde_json::Value>, CedarError> {
        let url = format!("{}/v1/schema", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| CedarError::Request(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| CedarError::Request(e.to_string()))?;
        let schema: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CedarError::Schema(e.to_string()))?;
        if schema.is_null() {
            Ok(None)
        } else {
            Ok(Some(schema))
        }
    }

    /// Fallback: ask cedar-agent to decide one `(tool, scope)` directly.
    /// Returns `Ok(true)` for Allow, `Ok(false)` for Deny. Only used when the
    /// local-sync path is disabled.
    pub async fn is_authorized(
        &self,
        tool_key: &str,
        scope: Option<&str>,
    ) -> Result<bool, CedarError> {
        let query = AuthorizationQuery {
            principal: PRINCIPAL.to_string(),
            action: ACTION.to_string(),
            resource: format!("{TOOL_TYPE}::{}", quote(tool_key)),
            context: serde_json::json!({
                crate::CTX_TOOL: tool_key,
                crate::CTX_SCOPE: scope.unwrap_or(""),
            }),
        };
        let url = format!("{}/v1/is_authorized", self.base_url);
        let resp: AuthorizationResponse = self
            .http
            .post(&url)
            .json(&query)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| CedarError::Request(e.to_string()))?
            .json()
            .await
            .map_err(|e| CedarError::Request(e.to_string()))?;
        Ok(resp.decision.eq_ignore_ascii_case("allow"))
    }
}

fn quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::{AgentPolicy, quote};

    #[test]
    fn policies_deserialize_from_agent_shape() {
        let json = r#"[{"id":"a","content":"permit(principal, action, resource);"}]"#;
        let got: Vec<AgentPolicy> = serde_json::from_str(json).expect("parse");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a");
        assert!(got[0].content.starts_with("permit"));
    }

    #[test]
    fn quote_escapes_specials() {
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
