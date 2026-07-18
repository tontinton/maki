use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex;
use isahc::HttpClient;
use isahc::config::{Configurable, RedirectPolicy};
use isahc::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use isahc::http::{Method, Request, StatusCode, header::HeaderMap};
use maki_storage::StateDir;
use maki_storage::auth::load_mcp_auth;
use serde_json::Value;

use super::error::McpError;
use super::oauth;
use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::{BoxFuture, McpTransport};
use tracing::{info, warn};

pub(super) const MAX_REDIRECTS: u32 = 10;
const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const INITIALIZE_METHOD: &str = "initialize";
const PROTOCOL_VERSION_KEY: &str = "protocolVersion";
const CT_JSON: &str = "application/json";
const CT_SSE: &str = "text/event-stream";
const ACCEPT_VALUE: &str = "application/json, text/event-stream";

pub struct HttpTransport {
    name: Arc<str>,
    url: String,
    client: HttpClient,
    headers: HashMap<String, String>,
    auth: Mutex<Option<String>>,
    storage: Option<StateDir>,
    negotiated: Mutex<Negotiated>,
    next_id: AtomicU64,
}

/// The server picks both during initialize and the spec wants them echoed as
/// headers on every later request, so one lock keeps them in sync.
#[derive(Clone, Default)]
struct Negotiated {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl HttpTransport {
    pub fn new(
        name: &str,
        url: &str,
        headers: &HashMap<String, String>,
        timeout: Duration,
        storage: Option<StateDir>,
    ) -> Result<Self, McpError> {
        let client = HttpClient::builder()
            .redirect_policy(RedirectPolicy::Limit(MAX_REDIRECTS))
            .timeout(timeout)
            .build()
            .map_err(|e: isahc::Error| McpError::StartFailed {
                server: name.into(),
                reason: e.to_string(),
            })?;

        let mut headers = headers.clone();
        let auth = headers
            .keys()
            .find(|k| k.eq_ignore_ascii_case(AUTHORIZATION.as_str()))
            .cloned()
            .and_then(|k| headers.remove(&k))
            .or_else(|| {
                let tokens = load_mcp_auth(storage.as_ref()?, name, url)?.tokens?;
                Some(format!("Bearer {}", tokens.access))
            });

        Ok(Self {
            name: Arc::from(name),
            url: url.to_string(),
            client,
            headers,
            auth: Mutex::new(auth),
            storage,
            negotiated: Mutex::new(Negotiated::default()),
            next_id: AtomicU64::new(1),
        })
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    fn build_request(
        &self,
        method: Method,
        body: Vec<u8>,
        negotiated: &Negotiated,
        auth: Option<&str>,
    ) -> Result<Request<Vec<u8>>, McpError> {
        let mut builder = Request::builder()
            .method(method)
            .uri(&self.url)
            .header(CONTENT_TYPE, CT_JSON)
            .header(ACCEPT, ACCEPT_VALUE);

        if let Some(sid) = &negotiated.session_id {
            builder = builder.header(SESSION_HEADER, sid);
        }

        if let Some(version) = &negotiated.protocol_version {
            builder = builder.header(PROTOCOL_HEADER, version);
        }

        if let Some(auth) = auth {
            builder = builder.header(AUTHORIZATION, auth);
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

    fn parse_rpc_response(&self, body_str: &str, is_sse: bool, id: u64) -> Result<Value, McpError> {
        let events = if is_sse {
            parse_sse_events(body_str)
        } else {
            vec![
                serde_json::from_str(body_str).map_err(|e| McpError::InvalidResponse {
                    server: self.server(),
                    reason: e.to_string(),
                })?,
            ]
        };

        let rpc_value = events
            .into_iter()
            .find(|e| is_response_to(e, id))
            .ok_or_else(|| McpError::InvalidResponse {
                server: self.server(),
                reason: format!("no response matching request id {id}"),
            })?;

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

    /// Single-flight token refresh after a 401. Holds the `auth` lock across the
    /// refresh so concurrent callers park instead of racing the (rotating)
    /// refresh token. If the stored value no longer matches the one the failed
    /// request used, another caller already refreshed: reuse it.
    async fn refreshed_auth(&self, used: Option<&str>) -> Option<String> {
        let storage = self.storage.as_ref()?;
        let mut guard = self.auth.lock().await;

        if guard.as_deref() != used {
            return guard.clone();
        }

        match oauth::silent_refresh(storage, &self.name, &self.url).await {
            Ok(Some(data)) => {
                let header = format!("Bearer {}", data.tokens?.access);
                *guard = Some(header.clone());

                info!(server = %self.name, "MCP OAuth token refreshed after 401");

                Some(header)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(server = %self.name, error = %e, "MCP OAuth token refresh failed");

                None
            }
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
            let encode = || {
                serde_json::to_vec(&req).map_err(|e| McpError::InvalidResponse {
                    server: self.server(),
                    reason: e.to_string(),
                })
            };

            let mut auth = self.auth.lock().await.clone();
            let mut refreshed = false;

            loop {
                let negotiated = self.negotiated.lock().await.clone();
                let http_req =
                    self.build_request(Method::POST, encode()?, &negotiated, auth.as_deref())?;

                let (status, headers, body_str) = self.send_http(http_req).await?;

                if status == StatusCode::UNAUTHORIZED
                    && !refreshed
                    && let Some(new_auth) = self.refreshed_auth(auth.as_deref()).await
                {
                    auth = Some(new_auth);
                    refreshed = true;

                    continue;
                }

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

                let is_sse = headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| ct.contains(CT_SSE));

                let result = self.parse_rpc_response(&body_str, is_sse, id);

                {
                    let mut negotiated = self.negotiated.lock().await;
                    if let Some(sid) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
                        negotiated.session_id = Some(sid.to_string());
                    }
                    if method == INITIALIZE_METHOD
                        && let Ok(val) = &result
                        && let Some(version) = val.get(PROTOCOL_VERSION_KEY).and_then(Value::as_str)
                    {
                        negotiated.protocol_version = Some(version.to_string());
                    }
                }

                info!(server = %self.server(), method, status = %status, refreshed, duration_ms = start.elapsed().as_millis() as u64, "MCP HTTP request");

                return result;
            }
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

            let negotiated = self.negotiated.lock().await.clone();
            let auth = self.auth.lock().await.clone();
            let http_req = self.build_request(Method::POST, body, &negotiated, auth.as_deref())?;

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
            let negotiated = self.negotiated.lock().await.clone();
            if negotiated.session_id.is_none() {
                return;
            }

            let auth = self.auth.lock().await.clone();
            let Ok(req) =
                self.build_request(Method::DELETE, Vec::new(), &negotiated, auth.as_deref())
            else {
                return;
            };

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

/// A null or missing id only counts for errors: that is what JSON-RPC sends
/// back when it could not parse the request itself.
fn is_response_to(event: &Value, id: u64) -> bool {
    match event.get("id").and_then(Value::as_u64) {
        Some(event_id) => {
            event_id == id && (event.get("result").is_some() || event.get("error").is_some())
        }
        None => event.get("error").is_some(),
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

    use maki_storage::auth::{McpAuthData, OAuthTokens, save_mcp_auth};
    use std::io::{BufRead, BufReader, Write as IoWrite};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    const NOTIFICATION: &str =
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n";
    const REQUEST_ID: u64 = 7;
    const RESPONSE_EVENT: &str =
        "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
    const STALE_RESPONSE_EVENT: &str =
        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stale\":true}}\n\n";
    const NULL_ID_ERROR_EVENT: &str = "data: {\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"parse error\"}}\n\n";
    const NEGOTIATED_VERSION: &str = "2025-03-26";
    const OLD_BEARER: &str = "Bearer old-token";
    const NEW_BEARER: &str = "Bearer new-token";

    fn rpc_ok(id: u64) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"ok":true}}}}"#)
    }

    struct Req {
        path: String,
        auth: Option<String>,
        protocol: Option<String>,
    }

    fn spawn_server<F>(make_handler: impl FnOnce(String) -> F) -> String
    where
        F: Fn(&Req) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = make_handler(base.clone());

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();

                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }

                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut auth = None;
                let mut protocol = None;
                let mut content_length = 0usize;

                loop {
                    let mut header = String::new();

                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }

                    let lower = header.to_ascii_lowercase();

                    if let Some(v) = lower.strip_prefix("authorization:") {
                        let start = header.len() - v.len();
                        auth = Some(header[start..].trim().to_string());
                    } else if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    } else if let Some(v) = lower.strip_prefix("mcp-protocol-version:") {
                        protocol = Some(v.trim().to_string());
                    }
                }

                let mut body = vec![0u8; content_length];
                let _ = std::io::Read::read_exact(&mut reader, &mut body);

                let (status, resp_body) = handler(&Req {
                    path,
                    auth,
                    protocol,
                });

                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len(),
                );

                let _ = stream.write_all(response.as_bytes());
            }
        });
        base
    }

    fn stored_auth(server_url: &str, access: &str, refresh: &str) -> McpAuthData {
        McpAuthData {
            server_url: server_url.to_string(),
            tokens: Some(OAuthTokens {
                access: access.to_string(),
                refresh: refresh.to_string(),
                expires: 0,
                account_id: None,
            }),
            client_id: "cid".to_string(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: None,
        }
    }

    fn transport_with(
        url: &str,
        headers: HashMap<String, String>,
        storage: Option<StateDir>,
    ) -> HttpTransport {
        HttpTransport::new("srv", url, &headers, Duration::from_secs(5), storage).unwrap()
    }

    fn oauth_routes(base: &str, req: &Req) -> Option<(u16, String)> {
        if req.path.contains("oauth-protected-resource") {
            return Some((
                200,
                format!(r#"{{"authorization_servers":["{base}"],"resource":"{base}/mcp"}}"#),
            ));
        }

        if req.path.contains("oauth-authorization-server")
            || req.path.contains("openid-configuration")
        {
            return Some((
                200,
                format!(
                    r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ));
        }

        if req.path == "/token" {
            return Some((
                200,
                r#"{"access_token":"new-token","expires_in":3600}"#.into(),
            ));
        }

        None
    }

    #[test_case("data: {\"id\":1}\n\n",                                      &[json!({"id":1})]                  ; "single_event")]
    #[test_case("data: {\"id\":1}\n\ndata: {\"id\":2}\n\n",                  &[json!({"id":1}), json!({"id":2})] ; "multiple_events")]
    #[test_case("data: {\"id\":1,\ndata:  \"result\":{}}\n\n",               &[json!({"id":1, "result":{}})]     ; "multiline_data")]
    #[test_case(": comment\ndata: {\"id\":1}\n\n",                           &[json!({"id":1})]                  ; "ignores_comments")]
    #[test_case("event: message\nid: 42\nretry: 5000\ndata: {\"id\":1}\n\n", &[json!({"id":1})]                  ; "ignores_non_data_fields")]
    #[test_case("",                                                          &[]                                 ; "empty_body")]
    #[test_case("event: ping\n\n",                                           &[]                                 ; "no_data_field")]
    #[test_case("data: not json\n\ndata: {\"id\":1}\n\n",                    &[json!({"id":1})]                  ; "malformed_json_skipped")]
    #[test_case("data: {\"id\":1}",                                          &[json!({"id":1})]                  ; "no_trailing_newline")]
    #[test_case("data:{\"id\":1}\n\n",                                       &[json!({"id":1})]                  ; "no_space_after_colon")]
    fn parse_sse(input: &str, expected: &[Value]) {
        let events = parse_sse_events(input);
        assert_eq!(events, expected);
    }

    #[test_case(&format!("{NOTIFICATION}{RESPONSE_EVENT}"),         true,  Some(json!({"ok": true})) ; "sse_skips_interleaved_notifications")]
    #[test_case(&format!("{STALE_RESPONSE_EVENT}{RESPONSE_EVENT}"), true,  Some(json!({"ok": true})) ; "sse_skips_stale_response_ids")]
    #[test_case(NOTIFICATION,                                       true,  None                      ; "sse_notification_only_rejected")]
    #[test_case(&rpc_ok(3),                                         false, None                      ; "json_wrong_id_rejected")]
    fn response_id_matching(body: &str, is_sse: bool, expected: Option<Value>) {
        let transport = transport_with("http://127.0.0.1:1/mcp", HashMap::new(), None);
        let result = transport.parse_rpc_response(body, is_sse, REQUEST_ID);
        match expected {
            Some(value) => assert_eq!(result.unwrap(), value),
            None => assert!(matches!(
                result.unwrap_err(),
                McpError::InvalidResponse { .. }
            )),
        }
    }

    #[test]
    fn null_id_error_event_maps_to_rpc_error() {
        let transport = transport_with("http://127.0.0.1:1/mcp", HashMap::new(), None);
        let err = transport
            .parse_rpc_response(NULL_ID_ERROR_EVENT, true, REQUEST_ID)
            .unwrap_err();
        assert!(matches!(err, McpError::RpcError { code: -32700, .. }));
    }

    #[test]
    fn build_request_applies_all_headers() {
        let headers = HashMap::from([("x-custom".to_string(), "yes".to_string())]);
        let transport = transport_with("http://127.0.0.1:1/mcp", headers, None);
        let negotiated = Negotiated {
            session_id: Some("sid".to_string()),
            protocol_version: Some(NEGOTIATED_VERSION.to_string()),
        };

        let req = transport
            .build_request(Method::POST, Vec::new(), &negotiated, Some(OLD_BEARER))
            .unwrap();

        let headers = req.headers();
        assert_eq!(headers.get(SESSION_HEADER).unwrap(), "sid");
        assert_eq!(headers.get(PROTOCOL_HEADER).unwrap(), NEGOTIATED_VERSION);
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), OLD_BEARER);
        assert_eq!(headers.get("x-custom").unwrap(), "yes");
    }

    #[test]
    fn server_negotiated_protocol_version_echoed_after_initialize() {
        let base = spawn_server(|_| {
            move |req: &Req| match req.protocol.as_deref() {
                None => (
                    200,
                    format!(
                        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{NEGOTIATED_VERSION}"}}}}"#
                    ),
                ),
                Some(NEGOTIATED_VERSION) => (200, rpc_ok(2)),
                Some(_) => (400, String::new()),
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        smol::block_on(transport.send_request("initialize", None)).unwrap();

        let result = smol::block_on(transport.send_request("tools/list", None)).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn refreshes_token_and_retries_on_401() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());

        let base = spawn_server(|base| {
            move |req: &Req| {
                if let Some(resp) = oauth_routes(&base, req) {
                    return resp;
                }
                if req.auth.as_deref() == Some(NEW_BEARER) {
                    (200, rpc_ok(1))
                } else {
                    (401, String::new())
                }
            }
        });

        let url = format!("{base}/mcp");
        save_mcp_auth(&storage, "srv", &stored_auth(&url, "old-token", "r1")).unwrap();

        let transport = transport_with(&url, HashMap::new(), Some(storage.clone()));
        let result = smol::block_on(transport.send_request("tools/list", None)).unwrap();
        assert_eq!(result, json!({"ok": true}));

        let saved = load_mcp_auth(&storage, "srv", &url).unwrap();
        let tokens = saved.tokens.unwrap();
        assert_eq!(tokens.access, "new-token");
        assert_eq!(tokens.refresh, "r1");
    }

    #[test]
    fn unauthorized_without_storage_fails_without_retry() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);
        let base = spawn_server(move |_| {
            move |_req: &Req| {
                hits_srv.fetch_add(1, Ordering::SeqCst);
                (401, String::new())
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        let err = smol::block_on(transport.send_request("tools/list", None)).unwrap_err();
        assert!(matches!(err, McpError::HttpError { status: 401, .. }));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn config_authorization_header_is_sent() {
        let base = spawn_server(|_| {
            move |req: &Req| {
                if req.auth.as_deref() == Some(OLD_BEARER) {
                    (200, rpc_ok(1))
                } else {
                    (401, String::new())
                }
            }
        });

        let headers = HashMap::from([("Authorization".to_string(), OLD_BEARER.to_string())]);
        let transport = transport_with(&format!("{base}/mcp"), headers, None);
        let result = smol::block_on(transport.send_request("tools/list", None)).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn stored_token_injected_at_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());

        let base = spawn_server(|_| {
            move |req: &Req| {
                if req.auth.as_deref() == Some(OLD_BEARER) {
                    (200, rpc_ok(1))
                } else {
                    (401, String::new())
                }
            }
        });
        let url = format!("{base}/mcp");
        save_mcp_auth(&storage, "srv", &stored_auth(&url, "old-token", "r1")).unwrap();

        let transport = transport_with(&url, HashMap::new(), Some(storage));
        let result = smol::block_on(transport.send_request("tools/list", None)).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }
}
