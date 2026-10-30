use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::future::Future;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_compat::Compat;
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, StreamExt, future};
use maki_providers::http::error_chain;
use maki_providers::strip_provider_keys;
use maki_storage::StateDir;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderName, HeaderValue};
use reqwest::{Certificate, Client, Error as ReqwestError, StatusCode};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ClientRequest, ConstString,
    ContentBlock, ErrorCode, ErrorData, GetPromptRequestParams, Implementation, InitializeResult,
    InitializeResultMethod, ListPromptsRequest, ListPromptsRequestMethod, ListToolsRequest,
    ListToolsRequestMethod, PaginatedRequestParams, Prompt, ProtocolVersion, Role, ServerResult,
    Tool,
};
use rmcp::service::{ClientInitializeError, Peer, RunningService};
use rmcp::transport::common::http_header::{HEADER_LAST_EVENT_ID, HEADER_SESSION_ID};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransportConfig, StreamableHttpError,
};
use rmcp::transport::{
    AuthClient, AuthorizationManager, DynamicTransportError, IntoTransport,
    StreamableHttpClientTransport,
};
use rmcp::{RoleClient, ServiceError, ServiceExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use smol::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use smol::{Task, Timer};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};
use tracing::{info, warn};

use super::config::{ServerConfig, Transport};
use super::error::McpError;
use super::oauth;
use crate::ChildGuard;

pub(super) const CLIENT_NAME: &str = "maki";
const TOOL_ERROR_CODE: i64 = -1;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait McpTransport: Send + Sync {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<String, McpError>>;
    fn get_prompt<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a HashMap<String, String>,
    ) -> BoxFuture<'a, Result<Vec<PromptMessage>, McpError>>;
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
    fn child_pids(&self) -> Vec<u32> {
        Vec::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptRole {
    #[default]
    User,
    Assistant,
}

pub struct PromptMessage {
    pub role: PromptRole,
    pub text: String,
}

/// One live MCP session. Dropping it ends the session, and for a stdio server
/// `ChildGuard` then kills the whole process group.
pub struct Connection {
    name: Arc<str>,
    peer: Peer<RoleClient>,
    /// Taken by `shutdown`, which waits for the session to close, including
    /// the DELETE that ends an HTTP session.
    service: Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
    timeout: Duration,
    process: Option<StdioProcess>,
}

struct StdioProcess {
    child: ChildGuard,
    _stderr: Task<()>,
}

/// rmcp spawns its session tasks on tokio, even while building a transport,
/// so all of it runs inside [`Compat`]. `timeout` bounds the handshake and each
/// request after it, as the server config says.
pub async fn connect(
    config: &ServerConfig,
    storage: Option<&StateDir>,
) -> Result<Connection, McpError> {
    let name = config.name.as_str();
    let (service, process) = within(
        config.timeout,
        name,
        Compat::new(async {
            match &config.transport {
                Transport::Stdio {
                    program,
                    args,
                    environment,
                } => {
                    let (stdout, stdin, process) = spawn_stdio(name, program, args, environment)?;
                    let service = serve(name, (stdout.compat(), stdin.compat_write())).await?;
                    Ok((service, Some(process)))
                }
                Transport::Http {
                    url,
                    headers,
                    ca_file,
                    ..
                } => Ok((
                    connect_http(name, url, headers, ca_file.as_deref(), storage).await?,
                    None,
                )),
            }
        }),
    )
    .await?;
    Ok(Connection {
        name: Arc::from(name),
        peer: service.peer().clone(),
        service: Mutex::new(Some(service)),
        timeout: config.timeout,
        process,
    })
}

impl Connection {
    pub fn declares_tools(&self) -> bool {
        self.peer
            .peer_info()
            .is_some_and(|info| info.capabilities.tools.is_some())
    }

    pub fn declares_prompts(&self) -> bool {
        self.peer
            .peer_info()
            .is_some_and(|info| info.capabilities.prompts.is_some())
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        paginate(|params| async move {
            let page = match self.send(ListToolsRequest::with_param(params)).await? {
                ServerResult::ListToolsResult(page) => page,
                other => reparse(&self.name, ListToolsRequestMethod::VALUE, &other)?,
            };
            Ok((page.tools, page.next_cursor))
        })
        .await
    }

    pub async fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        let listed = paginate(|params| async move {
            let page = match self.send(ListPromptsRequest::with_param(params)).await? {
                ServerResult::ListPromptsResult(page) => page,
                other => reparse(&self.name, ListPromptsRequestMethod::VALUE, &other)?,
            };
            Ok((page.prompts, page.next_cursor))
        })
        .await;
        match listed {
            Err(McpError::RpcError { code, .. })
                if code == i64::from(ErrorCode::METHOD_NOT_FOUND.0) =>
            {
                Ok(Vec::new())
            }
            listed => listed,
        }
    }

    // TODO: back to `list_all_tools`/`list_all_prompts` when rmcp keeps the parse error:
    // https://github.com/modelcontextprotocol/rust-sdk/issues/1415
    /// rmcp's typed requests turn a reply they can't parse into a bare
    /// "unexpected response", so this hands back the raw result for
    /// [`reparse`].
    async fn send(&self, request: impl Into<ClientRequest>) -> Result<ServerResult, McpError> {
        self.request(self.peer.send_request(request.into())).await
    }

    /// Bounds a request by the server's timeout. A tool call dropped
    /// mid-flight, by this timeout or a cancelled turn, sends the server
    /// `notifications/cancelled`.
    async fn request<T>(
        &self,
        call: impl Future<Output = Result<T, ServiceError>>,
    ) -> Result<T, McpError> {
        within(self.timeout, &self.name, async {
            Compat::new(call)
                .await
                .map_err(|e| service_error(&self.name, e))
        })
        .await
    }
}

impl McpTransport for Connection {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<String, McpError>> {
        Box::pin(async move {
            let start = Instant::now();
            let mut params = CallToolRequestParams::new(name.to_owned());
            if let Value::Object(arguments) = args {
                params = params.with_arguments(arguments.clone());
            }
            let result = self.request(self.peer.call_tool(params)).await?;
            let text = joined_text(&result.content);
            if result.is_error == Some(true) {
                return Err(McpError::RpcError {
                    server: self.name.to_string(),
                    code: TOOL_ERROR_CODE,
                    message: text,
                });
            }
            info!(
                server = &*self.name,
                tool = name,
                duration_ms = start.elapsed().as_millis() as u64,
                "MCP tools/call response"
            );
            Ok(text)
        })
    }

    fn get_prompt<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a HashMap<String, String>,
    ) -> BoxFuture<'a, Result<Vec<PromptMessage>, McpError>> {
        Box::pin(async move {
            let arguments = arguments
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            let params = GetPromptRequestParams::new(name).with_arguments(arguments);
            let result = self.request(self.peer.get_prompt(params)).await?;
            Ok(result
                .messages
                .into_iter()
                .map(|message| PromptMessage {
                    role: match message.role {
                        Role::User => PromptRole::User,
                        Role::Assistant => PromptRole::Assistant,
                    },
                    text: match message.content {
                        ContentBlock::Text(content) => content.text,
                        _ => String::new(),
                    },
                })
                .collect())
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let service = self
                .service
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(mut service) = service {
                let _ = Compat::new(service.close()).await;
            }
        })
    }

    fn child_pids(&self) -> Vec<u32> {
        self.process.iter().map(|p| p.child.id()).collect()
    }
}

fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new(CLIENT_NAME, env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
}

async fn serve<T, E, A>(
    name: &str,
    transport: T,
) -> Result<RunningService<RoleClient, ClientConfig>, McpError>
where
    T: IntoTransport<RoleClient, E, A>,
    E: Error + Send + Sync + 'static,
{
    client_config()
        .serve(transport)
        .await
        .map_err(|e| init_error(name, e))
}

fn spawn_stdio(
    name: &str,
    program: &str,
    args: &[String],
    environment: &HashMap<String, String>,
) -> Result<(ChildStdout, ChildStdin, StdioProcess), McpError> {
    let mut std_cmd = std::process::Command::new(program);
    strip_provider_keys(&mut std_cmd)
        .args(args)
        .envs(environment);

    #[cfg(unix)]
    unsafe {
        std_cmd.pre_exec(|| {
            let _ = rustix::process::setsid();
            Ok(())
        });
    }

    let mut cmd: Command = std_cmd.into();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| start_failed(name, e))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(start_failed(name, "stdio pipes missing"));
    };
    let process = StdioProcess {
        child: ChildGuard::new(child),
        _stderr: smol::spawn(log_stderr(Arc::from(name), stderr)),
    };
    Ok((stdout, stdin, process))
}

async fn log_stderr(name: Arc<str>, stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Some(Ok(line)) = lines.next().await {
        let line = line.trim();
        if !line.is_empty() {
            warn!(server = &*name, "{line}");
        }
    }
}

async fn connect_http(
    name: &str,
    url: &str,
    headers: &HashMap<String, String>,
    ca_file: Option<&Path>,
    storage: Option<&StateDir>,
) -> Result<RunningService<RoleClient, ClientConfig>, McpError> {
    let client = http_client(ca_file, None).map_err(|reason| start_failed(name, reason))?;
    let custom_headers = header_map(name, headers).map_err(|reason| start_failed(name, reason))?;
    let configured_auth = custom_headers.contains_key(&AUTHORIZATION);
    let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom_headers);

    let stored = match storage {
        Some(storage) if !configured_auth => {
            let oauth_client = http_client(ca_file, Some(oauth::REFRESH_TIMEOUT))
                .map_err(|reason| start_failed(name, reason))?;
            // Without the manager the stored token can't go out, so a new
            // login has to replace it.
            oauth::stored_manager(storage, name, url, oauth_client)
                .await
                .map_err(|e| {
                    warn!(server = name, error = %e, "stored MCP OAuth login unusable");
                    McpError::OAuthRequired {
                        server: name.into(),
                    }
                })?
        }
        _ => None,
    };
    // Without stored tokens the manager has none to add, and requests go out
    // as configured. One client type keeps a single copy of rmcp's HTTP worker
    // in the binary.
    let manager = match stored {
        Some(manager) => manager,
        None => AuthorizationManager::new(url)
            .await
            .map_err(|e| start_failed(name, e))?,
    };
    let client = AuthClient::new(client, manager);
    serve(
        name,
        StreamableHttpClientTransport::with_client(client, config),
    )
    .await
}

/// The transport and OAuth both build their client here, so they reach a server the same way.
/// Only OAuth sets a `timeout`: it would also cut off the transport's event streams, whose
/// requests are bounded by the server's timeout instead.
///
/// We read `ca_file` here and not while parsing the config, so a disabled server with a
/// missing bundle still shows as disabled.
pub(super) fn http_client(
    ca_file: Option<&Path>,
    timeout: Option<Duration>,
) -> Result<Client, String> {
    let builder = Client::builder().user_agent(CLIENT_NAME);
    let builder = match timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder,
    };
    let builder = match ca_file {
        Some(path) => {
            let unreadable = |e: &dyn Error| format!("ca_file '{}': {e}", path.display());
            let pem = fs::read(path).map_err(|e| unreadable(&e))?;
            let certs = Certificate::from_pem_bundle(&pem).map_err(|e| unreadable(&e))?;
            builder.tls_certs_only(certs)
        }
        None => builder,
    };
    builder.build().map_err(|e| e.to_string())
}

fn header_map(
    server: &str,
    headers: &HashMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    let mut map = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let invalid = |e: &dyn Error| format!("header '{name}': {e}");
        let header = HeaderName::try_from(name.as_str()).map_err(|e| invalid(&e))?;
        let value = HeaderValue::try_from(value.as_str()).map_err(|e| invalid(&e))?;
        if is_reserved(&header) {
            warn!(server, header = name, "ignoring a header MCP sets itself");
        } else {
            map.insert(header, value);
        }
    }
    Ok(map)
}

/// rmcp sets these itself and fails every request that also carries one as a
/// custom header.
fn is_reserved(header: &HeaderName) -> bool {
    *header == ACCEPT
        || [HEADER_SESSION_ID, HEADER_LAST_EVENT_ID]
            .iter()
            .any(|reserved| header.as_str().eq_ignore_ascii_case(reserved))
}

async fn within<T>(
    timeout: Duration,
    server: &str,
    work: impl Future<Output = Result<T, McpError>>,
) -> Result<T, McpError> {
    future::or(work, async {
        Timer::after(timeout).await;
        Err(McpError::Timeout {
            server: server.into(),
            timeout_ms: timeout.as_millis() as u64,
        })
    })
    .await
}

async fn paginate<T, F>(
    mut page: impl FnMut(PaginatedRequestParams) -> F,
) -> Result<Vec<T>, McpError>
where
    F: Future<Output = Result<(Vec<T>, Option<String>), McpError>>,
{
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let (page_items, next_cursor) =
            page(PaginatedRequestParams::default().with_cursor(cursor.clone())).await?;
        items.extend(page_items);
        let advanced = next_cursor.filter(|next| !next.is_empty() && cursor.as_ref() != Some(next));
        let Some(next) = advanced else {
            return Ok(items);
        };
        cursor = Some(next);
    }
}

/// rmcp parses a reply into its untagged `ServerResult` before it knows which
/// request it answers, so a reply that misses a required field lands in
/// another variant, usually the catch-all `CustomResult`. Parsing it again as
/// the expected type says which field is missing and where. A reply that does
/// parse is used, since an untagged union can also file a valid reply under an
/// earlier variant.
fn reparse<T: DeserializeOwned>(
    server: &str,
    method: &'static str,
    result: &ServerResult,
) -> Result<T, McpError> {
    serde_json::to_value(result)
        .map_err(|e| e.to_string())
        .and_then(|value| serde_path_to_error::deserialize(value).map_err(|e| e.to_string()))
        .map_err(|reason| McpError::InvalidReply {
            server: server.into(),
            method,
            reason,
        })
}

fn joined_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(ContentBlock::as_text)
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn start_failed(server: &str, reason: impl ToString) -> McpError {
    McpError::StartFailed {
        server: server.into(),
        reason: reason.to_string(),
    }
}

fn rpc_error(server: &str, error: ErrorData) -> McpError {
    McpError::RpcError {
        server: server.into(),
        code: error.code.0.into(),
        message: error.message.into_owned(),
    }
}

/// A 401 reaches the manager as `HttpError`, which it turns into `NeedsAuth`
/// carrying the server's challenge for the login that follows. rmcp only makes
/// a 401 with a challenge `AuthRequired`, and leaves a bare one in its message.
fn unauthorized(server: &str, error: &DynamicTransportError) -> Option<McpError> {
    match error
        .error
        .downcast_ref::<StreamableHttpError<ReqwestError>>()?
    {
        StreamableHttpError::AuthRequired(challenge) => Some(McpError::HttpError {
            server: server.into(),
            status: StatusCode::UNAUTHORIZED.as_u16(),
            reason: challenge.www_authenticate_header.clone(),
        }),
        StreamableHttpError::UnexpectedServerResponse(message)
            if message.starts_with(&format!("HTTP {}", StatusCode::UNAUTHORIZED)) =>
        {
            Some(McpError::OAuthRequired {
                server: server.into(),
            })
        }
        _ => None,
    }
}

/// rmcp keeps a transport's error out of its own `source()` chain, and the
/// HTTP client's error out of the transport's, so both are unwrapped here.
fn transport_chain(error: &DynamicTransportError) -> String {
    match error
        .error
        .downcast_ref::<StreamableHttpError<ReqwestError>>()
    {
        Some(StreamableHttpError::Client(e)) => error_chain(e),
        _ => error_chain(&*error.error),
    }
}

fn init_error(server: &str, error: ClientInitializeError) -> McpError {
    if let ClientInitializeError::ExpectedInitResult(Some(result)) = &error
        && let Err(invalid) =
            reparse::<InitializeResult>(server, InitializeResultMethod::VALUE, result)
    {
        return invalid;
    }
    match error {
        ClientInitializeError::TransportError { error, context } => unauthorized(server, &error)
            .unwrap_or_else(|| {
                start_failed(server, format!("{context}: {}", transport_chain(&error)))
            }),
        ClientInitializeError::JsonRpcError(e) => rpc_error(server, e),
        e => start_failed(server, error_chain(&e)),
    }
}

fn service_error(server: &str, error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(e) => rpc_error(server, e),
        ServiceError::TransportClosed => McpError::ServerDied {
            server: server.into(),
        },
        ServiceError::TransportSend(e) => {
            unauthorized(server, &e).unwrap_or_else(|| McpError::WriteFailed {
                server: server.into(),
                reason: transport_chain(&e),
            })
        }
        e => McpError::WriteFailed {
            server: server.into(),
            reason: error_chain(&e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdBufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::thread;

    use maki_storage::auth::{McpAuthData, OAuthTokens, load_mcp_auth, now_millis, save_mcp_auth};
    use rmcp::transport::auth::AuthorizationMetadata;
    use serde_json::json;
    use test_case::test_case;
    use url::form_urlencoded;

    const SERVER: &str = "srv";
    /// The server is in-process on loopback, so this only has to catch a
    /// client that never gets an answer at all.
    const TIMEOUT: Duration = Duration::from_secs(10);
    const SESSION_ID: &str = "session-1";
    const TOOL_TEXT: &str = "hi";
    const FIRST_TOOL: &str = "first";
    const LAST_TOOL: &str = "last";
    const NEXT_CURSOR: &str = "page-2";
    const CONFIG_BEARER: &str = "Bearer from-config";
    const OLD_TOKEN: &str = "old-token";
    const NEW_TOKEN: &str = "new-token";
    const CHALLENGE: &str =
        r#"Bearer resource_metadata="http://127.0.0.1:1/.well-known/oauth-protected-resource""#;
    const HOUR_MS: u64 = 3_600_000;
    const TOKEN_PATH: &str = "/token";
    const MOVED_TOKEN_ENDPOINT: &str = "https://moved.example.com/token";
    const LOGIN_RESOURCE: &str = "https://resource.example.com/mcp";

    struct Request {
        method: String,
        path: String,
        auth: Option<String>,
        text: String,
        body: Value,
    }

    struct Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    }

    impl Reply {
        fn json(body: Value) -> Self {
            Self {
                status: 200,
                headers: vec![
                    ("Content-Type", "application/json".into()),
                    ("Mcp-Session-Id", SESSION_ID.into()),
                ],
                body: body.to_string(),
            }
        }

        fn empty(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: String::new(),
            }
        }

        fn unauthorized() -> Self {
            Self {
                headers: vec![("WWW-Authenticate", CHALLENGE.into())],
                ..Self::empty(401)
            }
        }
    }

    fn token_reply() -> Reply {
        Reply::json(json!({
            "access_token": NEW_TOKEN,
            "token_type": "Bearer",
            "expires_in": 3600,
        }))
    }

    /// A minimal stateful MCP server with one tool and no SSE stream.
    fn mcp_reply(req: &Request) -> Reply {
        match req.method.as_str() {
            "GET" => return Reply::empty(405),
            "DELETE" => return Reply::empty(200),
            _ => {}
        }
        let Some(id) = req.body.get("id").cloned() else {
            return Reply::empty(202);
        };
        let result = match req.body["method"].as_str() {
            Some("initialize") => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock", "version": "1"},
            }),
            Some("tools/list") => {
                json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})
            }
            Some("tools/call") => json!({"content": [{"type": "text", "text": TOOL_TEXT}]}),
            Some("prompts/list") => json!({"prompts": []}),
            _ => json!({}),
        };
        rpc_result(&id, result)
    }

    fn rpc_result(id: &Value, result: Value) -> Reply {
        Reply::json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn spawn_server(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let handler = Arc::clone(&handler);
                thread::spawn(move || serve_connection(stream, &*handler));
            }
        });
        base
    }

    fn serve_connection(mut stream: TcpStream, handler: &dyn Fn(&Request) -> Reply) {
        let mut reader = StdBufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.is_empty() {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or("/").to_owned();
        let (mut auth, mut length) = (None, 0);
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap_or_default();
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = Some(value.trim().to_owned()),
                "content-length" => length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        let reply = handler(&Request {
            method,
            path,
            auth,
            text: String::from_utf8_lossy(&body).into_owned(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        let headers: String = reply
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}\r\n"))
            .collect();
        let _ = write!(
            stream,
            "HTTP/1.1 {} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        );
    }

    fn http_config(base: &str, headers: &[(&str, &str)]) -> ServerConfig {
        ServerConfig {
            name: SERVER.into(),
            timeout: TIMEOUT,
            always_load: false,
            transport: Transport::Http {
                url: format!("{base}/mcp"),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                oauth: None,
                ca_file: None,
            },
        }
    }

    fn stored_record(base: &str, lifetime_ms: u64) -> McpAuthData {
        let mut metadata = AuthorizationMetadata::default();
        metadata.authorization_endpoint = format!("{base}/authorize");
        metadata.token_endpoint = format!("{base}{TOKEN_PATH}");
        McpAuthData {
            server_url: format!("{base}/mcp"),
            tokens: Some(OAuthTokens {
                access: OLD_TOKEN.into(),
                refresh: "refresh-1".into(),
                expires: now_millis() + lifetime_ms,
                account_id: None,
            }),
            client_id: "cid".into(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: Some(format!("{base}/callback")),
            token_endpoint: Some(metadata.token_endpoint.clone()),
            auth_server: serde_json::to_value(&metadata).ok(),
            resource: None,
        }
    }

    #[test]
    fn lists_and_calls_tools_over_http() {
        let base = spawn_server(mcp_reply);
        smol::block_on(async {
            let connection = connect(&http_config(&base, &[]), None).await.unwrap();
            assert!(connection.declares_tools());
            assert_eq!(connection.list_tools().await.unwrap().len(), 1);
            let text = connection.call_tool("echo", &json!({})).await.unwrap();
            assert_eq!(text, TOOL_TEXT);
        });
    }

    #[test]
    fn list_tools_follows_the_cursor() {
        let base = spawn_server(|req| {
            if req.body["method"] != "tools/list" {
                return mcp_reply(req);
            }
            let (name, next_cursor) = match req.body["params"]["cursor"].as_str() {
                Some(NEXT_CURSOR) => (LAST_TOOL, Value::Null),
                _ => (FIRST_TOOL, json!(NEXT_CURSOR)),
            };
            let page = json!({
                "tools": [{"name": name, "inputSchema": {"type": "object"}}],
                "nextCursor": next_cursor,
            });
            rpc_result(&req.body["id"], page)
        });
        let tools = smol::block_on(async {
            let connection = connect(&http_config(&base, &[]), None).await.unwrap();
            connection.list_tools().await.unwrap()
        });
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, [FIRST_TOOL, LAST_TOOL]);
    }

    #[test_case("", 1 ; "empty_cursor")]
    #[test_case("same", 2 ; "repeated_cursor")]
    fn paginate_stops_on_a_cursor_that_does_not_advance(stuck_cursor: &str, expected_calls: usize) {
        let mut calls = 0;
        let items = smol::block_on(paginate(|_| {
            calls += 1;
            let page = Ok((vec![calls], Some(stuck_cursor.to_owned())));
            async move { page }
        }))
        .unwrap();
        assert_eq!(items.len(), expected_calls);
    }

    #[test_case(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "serverInfo": {"name": "mock", "version": "1"}}),
        "missing field `capabilities`"
        ; "initialize_without_capabilities"
    )]
    #[test_case(
        "tools/list",
        json!({"tools": [{"name": "ok", "inputSchema": {"type": "object"}}, {"name": "bad"}]}),
        "tools[1]: missing field `inputSchema`"
        ; "tool_without_input_schema"
    )]
    #[test_case(
        "prompts/list",
        json!({"prompts": [{"description": "no name"}]}),
        "prompts[0]: missing field `name`"
        ; "prompt_without_name"
    )]
    fn invalid_reply_names_server_and_field(method: &'static str, result: Value, reason: &str) {
        let base = spawn_server(move |req| match req.body["method"].as_str() {
            Some(m) if m == method => rpc_result(&req.body["id"], result.clone()),
            _ => mcp_reply(req),
        });
        let listed = smol::block_on(async {
            let connection = connect(&http_config(&base, &[]), None).await?;
            connection.list_tools().await?;
            connection.list_prompts().await
        });
        let Err(McpError::InvalidReply {
            server,
            method: failed,
            reason: got,
        }) = listed
        else {
            panic!("expected InvalidReply, got {listed:?}");
        };
        assert_eq!(
            (server.as_str(), failed, got.as_str()),
            (SERVER, method, reason)
        );
    }

    #[test]
    fn config_authorization_header_wins_over_stored_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_server({
            let seen = Arc::clone(&seen);
            move |req| {
                seen.lock().unwrap().push(req.auth.clone());
                mcp_reply(req)
            }
        });
        save_mcp_auth(&storage, SERVER, &stored_record(&base, HOUR_MS)).unwrap();
        let config = http_config(&base, &[("Authorization", CONFIG_BEARER)]);
        smol::block_on(connect(&config, Some(&storage))).unwrap();
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|a| a.as_deref() == Some(CONFIG_BEARER)));
    }

    #[test]
    fn unauthorized_connect_carries_the_challenge() {
        let base = spawn_server(|_| Reply::unauthorized());
        let Err(McpError::HttpError { status, reason, .. }) =
            smol::block_on(connect(&http_config(&base, &[]), None))
        else {
            panic!("expected a 401");
        };
        assert_eq!(
            (status, reason.as_str()),
            (StatusCode::UNAUTHORIZED.as_u16(), CHALLENGE)
        );
    }

    #[test]
    fn bare_unauthorized_connect_needs_a_login() {
        let base = spawn_server(|_| Reply {
            body: json!({"error": "unauthorized"}).to_string(),
            ..Reply::empty(401)
        });
        let result = smol::block_on(connect(&http_config(&base, &[]), None));
        assert!(
            matches!(result, Err(McpError::OAuthRequired { .. })),
            "got: {:?}",
            result.err()
        );
    }

    #[test]
    fn headers_mcp_sets_itself_are_dropped_from_config() {
        let base = spawn_server(mcp_reply);
        let config = http_config(
            &base,
            &[("Accept", "text/plain"), ("Mcp-Session-Id", "from-config")],
        );
        smol::block_on(connect(&config, None)).unwrap();
    }

    #[test]
    fn shutdown_ends_the_http_session() {
        let deleted = Arc::new(Mutex::new(false));
        let base = spawn_server({
            let deleted = Arc::clone(&deleted);
            move |req| {
                if req.method == "DELETE" {
                    *deleted.lock().unwrap() = true;
                }
                mcp_reply(req)
            }
        });
        smol::block_on(async {
            let connection = connect(&http_config(&base, &[]), None).await.unwrap();
            connection.shutdown().await;
        });
        assert!(*deleted.lock().unwrap());
    }

    #[test_case(HOUR_MS, false, OLD_TOKEN ; "valid_token_is_sent")]
    #[test_case(0,       false, NEW_TOKEN ; "expired_token_is_refreshed_first")]
    #[test_case(HOUR_MS, true,  NEW_TOKEN ; "rejected_token_is_refreshed_and_retried")]
    fn stored_oauth_tokens(lifetime_ms: u64, reject_old: bool, sent: &str) {
        let dir = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_server({
            let seen = Arc::clone(&seen);
            move |req| {
                if req.path == TOKEN_PATH {
                    return token_reply();
                }
                seen.lock().unwrap().push(req.auth.clone());
                if reject_old && req.auth.as_deref() == Some(&format!("Bearer {OLD_TOKEN}")) {
                    return Reply::unauthorized();
                }
                mcp_reply(req)
            }
        });
        save_mcp_auth(&storage, SERVER, &stored_record(&base, lifetime_ms)).unwrap();

        smol::block_on(connect(&http_config(&base, &[]), Some(&storage))).unwrap();

        let bearer = format!("Bearer {sent}");
        let last_seen = seen.lock().unwrap().last().cloned().flatten();
        assert_eq!(last_seen.as_deref(), Some(bearer.as_str()));
        let url = format!("{base}/mcp");
        let saved = load_mcp_auth(&storage, SERVER, &url).unwrap();
        assert_eq!(saved.tokens.unwrap().access, sent);
    }

    #[test_case(Some(TOKEN_PATH), true  ; "matching_endpoint_is_pinned")]
    #[test_case(None,             false ; "moved_endpoint_needs_a_login")]
    fn legacy_record_pins_the_auth_server_it_discovers(token_path: Option<&str>, usable: bool) {
        let dir = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let base = spawn_server(mcp_reply);
        let record = McpAuthData {
            token_endpoint: Some(
                token_path.map_or(MOVED_TOKEN_ENDPOINT.into(), |path| format!("{base}{path}")),
            ),
            auth_server: None,
            ..stored_record(&base, HOUR_MS)
        };
        save_mcp_auth(&storage, SERVER, &record).unwrap();

        let result = smol::block_on(connect(&http_config(&base, &[]), Some(&storage)));

        assert_eq!(result.is_ok(), usable);
        assert_eq!(
            matches!(result, Err(McpError::OAuthRequired { .. })),
            !usable
        );
        let saved = load_mcp_auth(&storage, SERVER, &format!("{base}/mcp")).unwrap();
        assert_eq!(saved.auth_server.is_some(), usable);
    }

    #[test_case(None                 ; "server_url_for_older_logins")]
    #[test_case(Some(LOGIN_RESOURCE) ; "resource_of_the_login")]
    fn refresh_names_the_resource(pinned: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let resources = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_server({
            let resources = Arc::clone(&resources);
            move |req| {
                if req.path != TOKEN_PATH {
                    return mcp_reply(req);
                }
                resources.lock().unwrap().extend(
                    form_urlencoded::parse(req.text.as_bytes())
                        .filter(|(key, _)| key == "resource")
                        .map(|(_, value)| value.into_owned()),
                );
                token_reply()
            }
        });
        let record = McpAuthData {
            resource: pinned.map(Into::into),
            ..stored_record(&base, 0)
        };
        save_mcp_auth(&storage, SERVER, &record).unwrap();

        smol::block_on(connect(&http_config(&base, &[]), Some(&storage))).unwrap();

        let expected = pinned.map_or_else(|| format!("{base}/mcp"), Into::into);
        assert_eq!(*resources.lock().unwrap(), [expected]);
    }

    /// A bare 401 isn't retried by rmcp, so `authenticate` is what replaces a
    /// token the server turned down before it expired.
    #[test]
    fn authenticate_refreshes_a_token_that_has_not_expired() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let refreshes = Arc::new(Mutex::new(0));
        let base = spawn_server({
            let refreshes = Arc::clone(&refreshes);
            move |req| {
                if req.path != TOKEN_PATH {
                    return mcp_reply(req);
                }
                *refreshes.lock().unwrap() += 1;
                token_reply()
            }
        });
        let url = format!("{base}/mcp");
        save_mcp_auth(&storage, SERVER, &stored_record(&base, HOUR_MS)).unwrap();

        smol::block_on(oauth::authenticate(
            SERVER,
            &url,
            None,
            &storage,
            oauth::Interaction::Background,
            None,
            None,
        ))
        .unwrap();

        assert_eq!(*refreshes.lock().unwrap(), 1);
        let saved = load_mcp_auth(&storage, SERVER, &url).unwrap();
        assert_eq!(saved.tokens.unwrap().access, NEW_TOKEN);
    }

    #[test]
    fn dropped_call_sends_cancelled_to_the_server() {
        let (entered_tx, entered_rx) = flume::bounded(1);
        let (cancelled_tx, cancelled_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded::<()>(1);
        let base = spawn_server(move |req| {
            match req.body["method"].as_str() {
                Some("tools/call") => {
                    let _ = entered_tx.try_send(req.body["id"].clone());
                    let _ = release_rx.recv_timeout(TIMEOUT);
                }
                Some("notifications/cancelled") => {
                    let _ = cancelled_tx.try_send(req.body["params"]["requestId"].clone());
                }
                _ => {}
            }
            mcp_reply(req)
        });

        let (call_id, cancelled_id) = smol::block_on(async {
            let connection = connect(&http_config(&base, &[]), None).await.unwrap();
            let args = json!({});
            let call_id = future::or(
                async {
                    connection.call_tool("echo", &args).await.unwrap();
                    Value::Null
                },
                async { entered_rx.recv_async().await.unwrap() },
            )
            .await;
            let cancelled_id = future::or(async { cancelled_rx.recv_async().await.ok() }, async {
                Timer::after(TIMEOUT).await;
                None
            })
            .await;
            (call_id, cancelled_id)
        });

        assert_eq!(cancelled_id, Some(call_id));
        drop(release_tx);
    }

    #[test]
    fn unreachable_server_names_the_cause_without_the_url() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut config = http_config(&base, &[]);
        if let Transport::Http { url, .. } = &mut config.transport {
            url.push_str("?key=secret");
        }
        let Err(McpError::StartFailed { reason, .. }) = smol::block_on(connect(&config, None))
        else {
            panic!("expected StartFailed");
        };
        assert!(!reason.contains("secret"), "got: {reason}");
        assert!(reason.to_lowercase().contains("connect"), "got: {reason}");
    }

    #[test_case("missing.pem" ; "missing_file")]
    #[test_case(""            ; "directory")]
    fn unreadable_ca_file_fails_the_start(file_name: &str) {
        let dir = tempfile::tempdir().unwrap();
        let ca_file = dir.path().join(file_name);
        let mut config = http_config("http://127.0.0.1:1", &[]);
        if let Transport::Http { ca_file: slot, .. } = &mut config.transport {
            *slot = Some(ca_file.clone());
        }
        let Err(McpError::StartFailed { reason, .. }) = smol::block_on(connect(&config, None))
        else {
            panic!("expected StartFailed");
        };
        assert!(
            reason.contains(&ca_file.display().to_string()),
            "got: {reason}"
        );
    }
}
