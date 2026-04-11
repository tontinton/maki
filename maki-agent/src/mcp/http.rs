use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex;
use isahc::HttpClient;
use isahc::config::Configurable;
use isahc::http::header::{ACCEPT, CONTENT_TYPE};
use isahc::http::{Method, Request, StatusCode, header::HeaderMap};
use serde_json::Value;

use super::error::McpError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::{BoxFuture, McpTransport};
use tracing::info;

const SESSION_HEADER: &str = "mcp-session-id";
const CT_JSON: &str = "application/json";
const CT_SSE: &str = "text/event-stream";
const ACCEPT_VALUE: &str = "application/json, text/event-stream";

pub struct HttpTransport {
    name: Arc<str>,
    url: String,
    client: HttpClient,
    headers: HashMap<String, String>,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpTransport {
    pub fn new(
        name: &str,
        url: &str,
        headers: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let client =
            HttpClient::builder()
                .timeout(timeout)
                .build()
                .map_err(|e: isahc::Error| McpError::StartFailed {
                    server: name.into(),
                    reason: e.to_string(),
                })?;

        Ok(Self {
            name: Arc::from(name),
            url: url.to_string(),
            client,
            headers: headers.clone(),
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    fn build_request(
        &self,
        body: Vec<u8>,
        session_id: Option<&str>,
    ) -> Result<Request<Vec<u8>>, McpError> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(&self.url)
            .header(CONTENT_TYPE, CT_JSON)
            .header(ACCEPT, ACCEPT_VALUE);

        if let Some(sid) = session_id {
            builder = builder.header(SESSION_HEADER, sid);
        }

        for (k, v) in &self.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        builder.body(body).map_err(|e| McpError::InvalidResponse {
            server: self.server(),
            reason: e.to_string(),
        })
    }

    async fn send_http(
        &self,
        http_req: Request<Vec<u8>>,
    ) -> Result<(StatusCode, HeaderMap, String), McpError> {
        let server = self.server();
        smol::unblock({
            let client = self.client.clone();
            move || {
                let mut response = client.send(http_req).map_err(|e| McpError::WriteFailed {
                    server: server.clone(),
                    reason: e.to_string(),
                })?;
                let status = response.status();
                let headers = response.headers().clone();
                let mut body = String::new();
                response.body_mut().read_to_string(&mut body).map_err(|e| {
                    McpError::InvalidResponse {
                        server,
                        reason: e.to_string(),
                    }
                })?;
                Ok((status, headers, body))
            }
        })
        .await
    }

    fn parse_rpc_response(&self, body_str: &str, content_type: &str) -> Result<Value, McpError> {
        let rpc_value: Value = if content_type.contains(CT_SSE) {
            parse_sse_events(body_str)
                .into_iter()
                .next()
                .ok_or_else(|| McpError::InvalidResponse {
                    server: self.server(),
                    reason: "no SSE events in response".into(),
                })?
        } else {
            serde_json::from_str(body_str).map_err(|e| McpError::InvalidResponse {
                server: self.server(),
                reason: e.to_string(),
            })?
        };

        let resp: JsonRpcResponse =
            serde_json::from_value(rpc_value).map_err(|e| McpError::InvalidResponse {
                server: self.server(),
                reason: e.to_string(),
            })?;

        if let Some(err) = resp.error {
            return Err(McpError::RpcError {
                server: self.server(),
                code: err.code,
                message: err.message,
            });
        }

        Ok(resp.result.unwrap_or(Value::Null))
    }

    async fn capture_session_id(&self, headers: &HeaderMap) {
        if let Some(sid) = headers.get(SESSION_HEADER)
            && let Ok(sid_str) = sid.to_str()
        {
            *self.session_id.lock().await = Some(sid_str.to_string());
        }
    }
}

impl McpTransport for HttpTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            let start = Instant::now();
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let req = JsonRpcRequest::new(id, method, params);
            let body = serde_json::to_vec(&req).map_err(|e| McpError::InvalidResponse {
                server: self.server(),
                reason: e.to_string(),
            })?;

            let session_id = self.session_id.lock().await;
            let http_req = self.build_request(body, session_id.as_deref())?;
            drop(session_id);

            let (status, headers, body_str) = self.send_http(http_req).await?;

            if !status.is_success() {
                let reason = if status == StatusCode::UNAUTHORIZED {
                    headers
                        .get("www-authenticate")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or(&body_str)
                        .to_string()
                } else {
                    body_str
                };
                return Err(McpError::HttpError {
                    server: self.server(),
                    status: status.as_u16(),
                    reason,
                });
            }

            self.capture_session_id(&headers).await;

            let is_sse = headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains(CT_SSE));

            let result = self.parse_rpc_response(&body_str, if is_sse { CT_SSE } else { CT_JSON });
            info!(server = %self.server(), method, status = %status, duration_ms = start.elapsed().as_millis() as u64, "MCP HTTP request");
            result
        })
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let notif = JsonRpcNotification::new(method, params);
            let body = serde_json::to_vec(&notif).map_err(|e| McpError::InvalidResponse {
                server: self.server(),
                reason: e.to_string(),
            })?;

            let session_id = self.session_id.lock().await;
            let http_req = self.build_request(body, session_id.as_deref())?;
            drop(session_id);

            let (status, _, _) = self.send_http(http_req).await?;

            if !status.is_success() && status != StatusCode::ACCEPTED {
                return Err(McpError::HttpError {
                    server: self.server(),
                    status: status.as_u16(),
                    reason: format!("notification rejected: {status}"),
                });
            }

            Ok(())
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let session_id = self.session_id.lock().await.clone();
            let Some(sid) = session_id else { return };

            let req = Request::builder()
                .method(Method::DELETE)
                .uri(&self.url)
                .header(SESSION_HEADER, &sid)
                .body(Vec::new());

            let Ok(req) = req else { return };

            let client = self.client.clone();
            let _ = smol::unblock(move || client.send(req)).await;
        })
    }

    fn server_name(&self) -> &Arc<str> {
        &self.name
    }

    fn transport_kind(&self) -> &'static str {
        "http"
    }
}

fn parse_sse_events(body: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut data_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        if line.is_empty() {
            if !data_lines.is_empty() {
                let combined = data_lines.join("\n");
                if let Ok(val) = serde_json::from_str(&combined) {
                    events.push(val);
                }
                data_lines.clear();
            }
            continue;
        }

        if line.starts_with(':') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("data:") {
            let data = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(data);
        }
    }

    if !data_lines.is_empty() {
        let combined = data_lines.join("\n");
        if let Ok(val) = serde_json::from_str(&combined) {
            events.push(val);
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    #[test_case("data: {\"id\":1}\n\n",                                     &[json!({"id":1})]                ; "single_event")]
    #[test_case("data: {\"id\":1}\n\ndata: {\"id\":2}\n\n",                 &[json!({"id":1}), json!({"id":2})]; "multiple_events")]
    #[test_case("data: {\"id\":1,\ndata:  \"result\":{}}\n\n",              &[json!({"id":1, "result":{}})]    ; "multiline_data")]
    #[test_case(": comment\ndata: {\"id\":1}\n\n",                           &[json!({"id":1})]                ; "ignores_comments")]
    #[test_case("event: message\nid: 42\nretry: 5000\ndata: {\"id\":1}\n\n",&[json!({"id":1})]                ; "ignores_non_data_fields")]
    #[test_case("",                                                          &[]                               ; "empty_body")]
    #[test_case("event: ping\n\n",                                           &[]                               ; "no_data_field")]
    #[test_case("data: not json\n\ndata: {\"id\":1}\n\n",                   &[json!({"id":1})]                ; "malformed_json_skipped")]
    #[test_case("data: {\"id\":1}",                                          &[json!({"id":1})]                ; "no_trailing_newline")]
    #[test_case("data:{\"id\":1}\n\n",                                       &[json!({"id":1})]                ; "no_space_after_colon")]
    fn parse_sse(input: &str, expected: &[Value]) {
        let events = parse_sse_events(input);
        assert_eq!(events, expected);
    }
}
