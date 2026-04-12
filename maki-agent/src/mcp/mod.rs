//! MCP client: manages transports and routes tool calls to servers.
//!
//! Tool names are namespaced as `server__tool` (double underscore) so two servers can both
//! expose a tool called `search` without stepping on each other. The qualified names are leaked
//! into `&'static str` so tool descriptors can hold them without dragging a lifetime everywhere.
//!
//! # Architecture
//!
//! All mutable state lives inside one task, `run`, which owns `McpManagerInner` outright. Nobody
//! else gets to touch it. Two doors lead in and out:
//!
//! * Writes come in as `McpCommand`s over a channel. `run` handles them one at a time, so a
//!   toggle, a reconnect and a shutdown can never interleave.
//! * Reads go through two `ArcSwap`s that `run` republishes after every change: a `ToolIndex`
//!   for the hot tool-call path and an `McpSnapshot` for the UI. Both sides are lock-free, so a
//!   slow tool call can't stall a toggle and a toggle can't stall an in-flight call.
//!
//! `McpSnapshotReader` is a read-only newtype around the snapshot `ArcSwap`. Handing that out
//! instead of the raw `ArcSwap` means outside code physically cannot publish a snapshot, which
//! keeps the "only `run` publishes" rule enforced by the type system.
//!
//! The snapshot is the single source of truth for every per-server fact, including whether a
//! server is disabled. There is no second list to keep in sync.

pub mod config;
pub mod error;
pub mod http;
pub mod oauth;
pub mod protocol;
pub mod stdio;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::{ArcSwap, Guard};
use serde_json::{Value, json};
use tracing::{info, warn};

use self::config::{
    McpConfig, McpServerInfo, McpServerStatus, ServerConfig, Transport, load_config, parse_server,
    transport_kind,
};
use self::error::McpError;
use self::http::HttpTransport;
use self::stdio::StdioTransport;
use self::transport::McpTransport;

const SEPARATOR: &str = "__";
pub const UNKNOWN_MCP: &str = "unknown_mcp";

struct McpToolDef {
    qualified_name: &'static str,
    raw_name: String,
    description: String,
    input_schema: Value,
}

struct McpPromptDef {
    qualified_name: String,
    raw_name: String,
    description: String,
    arguments: Vec<protocol::PromptArgument>,
}

impl McpPromptDef {
    fn from_info(server_name: &str, info: protocol::PromptInfo) -> Self {
        Self {
            qualified_name: format!("{server_name}{SEPARATOR}{}", info.name),
            raw_name: info.name,
            description: info.description.unwrap_or_default(),
            arguments: info.arguments,
        }
    }

    fn to_info(&self, server_name: &str) -> McpPromptInfo {
        McpPromptInfo {
            display_name: format!("{server_name}:{}", self.raw_name),
            qualified_name: self.qualified_name.clone(),
            description: self.description.clone(),
            arguments: self
                .arguments
                .iter()
                .map(|a| McpPromptArg {
                    name: a.name.clone(),
                    description: a.description.clone().unwrap_or_default(),
                    required: a.required,
                })
                .collect(),
        }
    }
}

struct ServerEntry {
    name: String,
    config: Option<ServerConfig>,
    transport_kind: &'static str,
    origin: PathBuf,
    status: McpServerStatus,
    transport: Option<Arc<dyn McpTransport>>,
    tools: Vec<McpToolDef>,
    prompts: Vec<McpPromptDef>,
}

impl ServerEntry {
    async fn clear_connection(&mut self) {
        if let Some(old) = self.transport.take() {
            // A live tool call can still be holding an `Arc` to this transport via the
            // `ToolIndex`, so we cannot rely on `Drop` to reap the child in time.
            kill_process_groups(&old.child_pids());
            old.shutdown().await;
        }
        self.tools.clear();
        self.prompts.clear();
    }

    fn populate(&mut self, result: StartResult) {
        let StartResult {
            transport,
            tool_infos,
            prompt_infos,
        } = result;
        self.tools = tool_infos
            .into_iter()
            .map(|info| McpToolDef {
                qualified_name: intern(format!("{}{SEPARATOR}{}", self.name, info.name)),
                raw_name: info.name,
                description: info.description,
                input_schema: info.input_schema,
            })
            .collect();
        self.prompts = prompt_infos
            .into_iter()
            .map(|info| McpPromptDef::from_info(&self.name, info))
            .collect();
        self.transport = Some(transport);
        self.status = McpServerStatus::Running;
    }
}

struct McpManagerInner {
    entries: Vec<ServerEntry>,
    generation: u64,
}

#[derive(Default)]
struct ToolIndex {
    tools: HashMap<&'static str, ToolRef>,
    prompts: HashMap<String, PromptRef>,
    descriptors: Arc<[Value]>,
}

struct ToolRef {
    raw_name: String,
    transport: Arc<dyn McpTransport>,
}

struct PromptRef {
    raw_name: String,
    transport: Arc<dyn McpTransport>,
}

#[derive(Clone)]
pub struct McpPromptInfo {
    pub display_name: String,
    pub qualified_name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
}

#[derive(Clone)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Clone, Default)]
pub struct McpSnapshot {
    pub infos: Vec<McpServerInfo>,
    pub prompts: Vec<McpPromptInfo>,
    pub pids: Vec<u32>,
    pub generation: u64,
}

/// Read-only view of the latest published `McpSnapshot`. Handing this out instead of the
/// raw `ArcSwap` keeps outside code from publishing snapshots of its own.
#[derive(Clone)]
pub struct McpSnapshotReader(Arc<ArcSwap<McpSnapshot>>);

impl McpSnapshotReader {
    pub fn empty() -> Self {
        Self::from_snapshot(McpSnapshot::default())
    }

    pub fn from_snapshot(snapshot: McpSnapshot) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(snapshot)))
    }

    pub fn load(&self) -> Guard<Arc<McpSnapshot>> {
        self.0.load()
    }
}

pub enum McpCommand {
    Toggle {
        server: String,
        enabled: bool,
    },
    Reconnect {
        server: String,
        url: String,
        token: String,
    },
    /// Drain every running transport and stop the loop. The loop sends `()` on `ack` once
    /// every shutdown has finished, so callers can wait with a timeout.
    Shutdown {
        ack: flume::Sender<()>,
    },
}

#[derive(Clone)]
pub struct McpHandle {
    cmd_tx: flume::Sender<McpCommand>,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
}

impl McpHandle {
    pub fn send(&self, cmd: McpCommand) {
        if let Err(e) = self.cmd_tx.try_send(cmd) {
            warn!(error = %e, "MCP command loop is gone");
        }
    }

    pub fn reader(&self) -> McpSnapshotReader {
        McpSnapshotReader(Arc::clone(&self.snapshot))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.index.load().tools.contains_key(name)
    }

    pub fn interned_name(&self, name: &str) -> &'static str {
        self.index
            .load()
            .tools
            .get_key_value(name)
            .map(|(&k, _)| k)
            .unwrap_or(UNKNOWN_MCP)
    }

    pub async fn call_tool(&self, qualified_name: &str, args: &Value) -> Result<String, McpError> {
        let (raw_name, transport) = {
            let idx = self.index.load();
            let Some(t) = idx.tools.get(qualified_name) else {
                return Err(McpError::UnknownTool {
                    name: qualified_name.into(),
                });
            };
            (t.raw_name.clone(), Arc::clone(&t.transport))
        };
        transport::call_tool(transport.as_ref(), &raw_name, args).await
    }

    pub async fn get_prompt(
        &self,
        qualified_name: &str,
        arguments: &HashMap<String, String>,
    ) -> Result<Vec<protocol::PromptMessage>, McpError> {
        let (raw_name, transport) = {
            let idx = self.index.load();
            let Some(p) = idx.prompts.get(qualified_name) else {
                return Err(McpError::UnknownPrompt {
                    name: qualified_name.into(),
                });
            };
            (p.raw_name.clone(), Arc::clone(&p.transport))
        };
        transport::get_prompt(transport.as_ref(), &raw_name, arguments).await
    }

    pub fn extend_tools(&self, tools: &mut Value) {
        if let Some(arr) = tools.as_array_mut() {
            arr.extend(self.index.load().descriptors.iter().cloned());
        }
    }
}

pub async fn start(cwd: &Path) -> Option<McpHandle> {
    let cwd = cwd.to_owned();
    let config = smol::unblock(move || load_config(&cwd)).await;
    start_with_config(config).await
}

pub async fn start_with_config(config: McpConfig) -> Option<McpHandle> {
    if config.is_empty() {
        return None;
    }

    let inner = parse_entries(config);
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    let index: Arc<ArcSwap<ToolIndex>> = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));

    publish(&inner, &index, &snapshot);

    let (cmd_tx, cmd_rx) = flume::unbounded();
    let handle = McpHandle {
        cmd_tx,
        index: Arc::clone(&index),
        snapshot: Arc::clone(&snapshot),
    };

    smol::spawn(run_with_init(inner, index, snapshot, cmd_rx)).detach();
    Some(handle)
}

async fn run_with_init(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    cmd_rx: flume::Receiver<McpCommand>,
) {
    start_enabled(&mut inner).await;
    inner.generation += 1;
    publish(&inner, &index, &snapshot);

    info!(
        running = inner
            .entries
            .iter()
            .filter(|e| e.transport.is_some())
            .count(),
        total = inner.entries.len(),
        "MCP servers initialized"
    );

    run(inner, index, snapshot, cmd_rx).await;
}

async fn run(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    cmd_rx: flume::Receiver<McpCommand>,
) {
    while let Ok(cmd) = cmd_rx.recv_async().await {
        match cmd {
            McpCommand::Toggle { server, enabled } => {
                handle_toggle(&mut inner, &server, enabled).await;
            }
            McpCommand::Reconnect { server, url, token } => {
                handle_reconnect(&mut inner, &server, &url, &token).await;
            }
            McpCommand::Shutdown { ack } => {
                shutdown_all(&mut inner).await;
                inner.generation += 1;
                publish(&inner, &index, &snapshot);
                let _ = ack.try_send(());
                return;
            }
        }
        inner.generation += 1;
        publish(&inner, &index, &snapshot);
    }
}

async fn handle_toggle(inner: &mut McpManagerInner, server_name: &str, enabled: bool) {
    if let Some(path) = inner
        .entries
        .iter()
        .find(|e| e.name == server_name)
        .map(|e| e.origin.clone())
    {
        spawn_persist_enabled(path, server_name.to_owned(), enabled);
    }

    if enabled {
        if let Err(e) = refresh_server(inner, server_name, None).await {
            warn!(server = %server_name, error = %e, "MCP server refresh failed");
        }
    } else if let Some(entry) = inner.entries.iter_mut().find(|e| e.name == server_name) {
        entry.clear_connection().await;
        entry.status = McpServerStatus::Disabled;
    }

    info!(server = server_name, enabled, "MCP toggle complete");
}

async fn handle_reconnect(
    inner: &mut McpManagerInner,
    server_name: &str,
    server_url: &str,
    access_token: &str,
) {
    let Some(entry) = inner.entries.iter().find(|e| e.name == server_name) else {
        warn!(server = server_name, "reconnect for unknown server");
        return;
    };
    if entry.status == McpServerStatus::Disabled {
        info!(
            server = server_name,
            "ignoring reconnect for disabled server"
        );
        return;
    }
    let timeout = entry.config.as_ref().map(|c| c.timeout).unwrap_or_default();
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    );
    let cfg = ServerConfig {
        name: server_name.to_string(),
        timeout,
        transport: Transport::Http {
            url: server_url.to_string(),
            headers,
        },
    };
    if let Err(e) = refresh_server(inner, server_name, Some(cfg)).await {
        warn!(server = %server_name, error = %e, "reconnect failed");
    }
    info!(server = server_name, "MCP reconnect complete");
}

async fn shutdown_all(inner: &mut McpManagerInner) {
    for entry in &mut inner.entries {
        entry.clear_connection().await;
        if entry.status != McpServerStatus::Disabled {
            entry.status = McpServerStatus::Failed("shutdown".into());
        }
    }
    info!("MCP command loop shutting down");
}

/// Tear the old transport down and wipe tools/prompts *before* starting the new one. That way
/// a failed start leaves the entry empty instead of holding zombie tool references into a dead
/// transport.
async fn refresh_server(
    inner: &mut McpManagerInner,
    server_name: &str,
    config_override: Option<ServerConfig>,
) -> Result<(), McpError> {
    let Some(idx) = inner.entries.iter().position(|e| e.name == server_name) else {
        return Err(McpError::Config(format!("unknown server '{server_name}'")));
    };

    let config = match config_override {
        Some(c) => {
            inner.entries[idx].config = Some(c.clone());
            c
        }
        None => inner.entries[idx]
            .config
            .clone()
            .ok_or_else(|| McpError::Config(format!("server '{server_name}' has no config")))?,
    };

    {
        let entry = &mut inner.entries[idx];
        entry.status = McpServerStatus::Connecting;
        entry.clear_connection().await;
    }

    let result = start_server(&config).await;
    apply_start_result(&mut inner.entries[idx], result, "refresh")?;
    info!(
        server = server_name,
        tools = inner.entries[idx].tools.len(),
        "MCP server refreshed"
    );
    Ok(())
}

fn status_from_err(e: &McpError) -> McpServerStatus {
    if let McpError::HttpError {
        status: 401,
        reason,
        ..
    } = e
    {
        McpServerStatus::NeedsAuth {
            url: Some(reason.clone()),
        }
    } else {
        McpServerStatus::Failed(e.to_string())
    }
}

struct StartResult {
    transport: Arc<dyn McpTransport>,
    tool_infos: Vec<protocol::ToolInfo>,
    prompt_infos: Vec<protocol::PromptInfo>,
}

async fn start_server(config: &ServerConfig) -> Result<StartResult, McpError> {
    let transport: Arc<dyn McpTransport> = match &config.transport {
        Transport::Stdio {
            program,
            args,
            environment,
        } => Arc::new(StdioTransport::spawn(
            &config.name,
            program,
            args,
            environment,
            config.timeout,
        )?),
        Transport::Http { url, headers } => Arc::new(HttpTransport::new(
            &config.name,
            url,
            headers,
            config.timeout,
        )?),
    };
    transport::initialize(transport.as_ref()).await?;
    let tool_infos = transport::list_tools(transport.as_ref()).await?;
    let prompt_infos = transport::list_prompts(transport.as_ref()).await?;
    info!(
        server = config.name,
        tool_count = tool_infos.len(),
        prompt_count = prompt_infos.len(),
        "MCP server initialized"
    );
    Ok(StartResult {
        transport,
        tool_infos,
        prompt_infos,
    })
}

fn parse_entries(config: McpConfig) -> McpManagerInner {
    let origins = config.origins;
    let mut entries = Vec::with_capacity(config.mcp.len());

    for (name, raw) in config.mcp {
        let transport_kind = transport_kind(&raw.transport);
        let origin = origins.get(&name).cloned().unwrap_or_default();
        let disabled = !raw.enabled;
        let (config, status) = match parse_server(name.clone(), raw) {
            Ok(sc) if disabled => (Some(sc), McpServerStatus::Disabled),
            Ok(sc) => (Some(sc), McpServerStatus::Connecting),
            Err(e) => {
                warn!(server = %name, error = %e, "invalid MCP server config");
                (None, McpServerStatus::Failed(e.to_string()))
            }
        };
        entries.push(ServerEntry {
            name,
            config,
            transport_kind,
            origin,
            status,
            transport: None,
            tools: Vec::new(),
            prompts: Vec::new(),
        });
    }

    McpManagerInner {
        entries,
        generation: 0,
    }
}

async fn start_enabled(inner: &mut McpManagerInner) {
    let tasks: Vec<_> = inner
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == McpServerStatus::Connecting)
        .filter_map(|(i, e)| e.config.clone().map(|c| (i, c)))
        .map(|(i, cfg)| smol::spawn(async move { (i, start_server(&cfg).await) }))
        .collect();

    for task in tasks {
        let (i, result) = task.await;
        let _ = apply_start_result(&mut inner.entries[i], result, "start");
    }
}

fn apply_start_result(
    entry: &mut ServerEntry,
    result: Result<StartResult, McpError>,
    action: &'static str,
) -> Result<(), McpError> {
    match result {
        Ok(start) => {
            entry.populate(start);
            Ok(())
        }
        Err(e) => {
            entry.status = status_from_err(&e);
            if !matches!(entry.status, McpServerStatus::NeedsAuth { .. }) {
                warn!(server = %entry.name, action, error = %e, "MCP server start failed");
            }
            Err(e)
        }
    }
}

/// The only place read-side state is updated. Every mutation in the command loop ends here.
fn publish(inner: &McpManagerInner, index: &ArcSwap<ToolIndex>, snapshot: &ArcSwap<McpSnapshot>) {
    let mut tools = HashMap::new();
    let mut prompts = HashMap::new();
    let mut descriptors: Vec<Value> = Vec::new();
    let mut server_infos = Vec::with_capacity(inner.entries.len());
    let mut prompt_infos = Vec::new();
    let mut pids = Vec::new();

    for entry in &inner.entries {
        let url = entry
            .config
            .as_ref()
            .and_then(|c| transport_url(&c.transport));

        if let Some(ref transport) = entry.transport
            && entry.status != McpServerStatus::Disabled
        {
            for t in &entry.tools {
                tools.insert(
                    t.qualified_name,
                    ToolRef {
                        raw_name: t.raw_name.clone(),
                        transport: Arc::clone(transport),
                    },
                );
                descriptors.push(json!({
                    "name": t.qualified_name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }));
            }
            for p in &entry.prompts {
                prompts.insert(
                    p.qualified_name.clone(),
                    PromptRef {
                        raw_name: p.raw_name.clone(),
                        transport: Arc::clone(transport),
                    },
                );
                prompt_infos.push(p.to_info(&entry.name));
            }
            pids.extend(transport.child_pids());
        }

        server_infos.push(McpServerInfo {
            name: entry.name.clone(),
            transport_kind: entry.transport_kind,
            tool_count: entry.tools.len(),
            prompt_count: entry.prompts.len(),
            status: entry.status.clone(),
            config_path: entry.origin.clone(),
            url,
        });
    }

    index.store(Arc::new(ToolIndex {
        tools,
        prompts,
        descriptors: descriptors.into(),
    }));
    snapshot.store(Arc::new(McpSnapshot {
        infos: server_infos,
        prompts: prompt_infos,
        pids,
        generation: inner.generation,
    }));
}

fn transport_url(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Http { url, .. } => Some(url.clone()),
        Transport::Stdio { .. } => None,
    }
}

fn spawn_persist_enabled(path: PathBuf, name: String, enabled: bool) {
    let log_name = name.clone();
    smol::spawn(async move {
        if let Err(e) = smol::unblock(move || config::persist_enabled(&path, &name, enabled)).await
        {
            warn!(error = %e, server = %log_name, "failed to persist MCP toggle");
        }
    })
    .detach();
}

#[cfg(unix)]
pub fn kill_process_groups(pids: &[u32]) {
    for &pid in pids {
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
pub fn kill_process_groups(_pids: &[u32]) {}

/// Leak the name into a `&'static str` so tool descriptors can use it without a lifetime. Names
/// come from config, so the set is small and fixed per session, and the leak stays bounded.
fn intern(name: String) -> &'static str {
    static CACHE: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = CACHE
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&existing) = set.get(name.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    set.insert(leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_lock::Mutex as AsyncMutex;
    use config::{RawServerConfig, RawStdioFields, RawTransport};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DEFAULT_TIMEOUT_MS: u64 = 30_000;

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Stdio(RawStdioFields {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                environment: HashMap::new(),
            }),
        }
    }

    fn make_config(entries: Vec<(&str, RawServerConfig)>) -> McpConfig {
        let mut mcp = HashMap::new();
        let mut origins = HashMap::new();
        for (name, cfg) in entries {
            origins.insert(name.to_string(), PathBuf::from("/test/config.toml"));
            mcp.insert(name.to_string(), cfg);
        }
        McpConfig { mcp, origins }
    }

    const TOOL_NAME: &str = "srv__tool";

    /// Counts shutdowns, signals on `call_entered` the moment a `tools/call` begins, and holds
    /// the call inside `call_gate` until tests release it. That way tests can meet an in-flight
    /// RPC at a known point without polling.
    struct FakeTransport {
        name: Arc<str>,
        shutdowns: AtomicUsize,
        call_entered: flume::Sender<()>,
        call_entered_rx: flume::Receiver<()>,
        call_gate: AsyncMutex<()>,
    }

    impl FakeTransport {
        fn new() -> Arc<Self> {
            let (call_entered, call_entered_rx) = flume::bounded(1);
            Arc::new(Self {
                name: Arc::from("fake"),
                shutdowns: AtomicUsize::new(0),
                call_entered,
                call_entered_rx,
                call_gate: AsyncMutex::new(()),
            })
        }

        fn shutdowns(&self) -> usize {
            self.shutdowns.load(Ordering::SeqCst)
        }
    }

    impl McpTransport for FakeTransport {
        fn send_request<'a>(
            &'a self,
            method: &'a str,
            _params: Option<Value>,
        ) -> transport::BoxFuture<'a, Result<Value, McpError>> {
            Box::pin(async move {
                if method == "tools/call" {
                    let _ = self.call_entered.try_send(());
                    let _g = self.call_gate.lock().await;
                    Ok(json!({ "content": [{ "type": "text", "text": "ok" }] }))
                } else {
                    Ok(Value::Null)
                }
            })
        }
        fn send_notification<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> transport::BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async move { Ok(()) })
        }
        fn shutdown<'a>(&'a self) -> transport::BoxFuture<'a, ()> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn server_name(&self) -> &Arc<str> {
            &self.name
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
    }

    fn fake_entry(name: &str, transport: Arc<dyn McpTransport>) -> ServerEntry {
        let qualified = intern(format!("{name}{SEPARATOR}tool"));
        ServerEntry {
            name: name.into(),
            config: None,
            transport_kind: "fake",
            origin: PathBuf::new(),
            status: McpServerStatus::Running,
            transport: Some(transport),
            tools: vec![McpToolDef {
                qualified_name: qualified,
                raw_name: "tool".into(),
                description: String::new(),
                input_schema: json!({}),
            }],
            prompts: Vec::new(),
        }
    }

    fn bad_stdio_config(name: &str) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            timeout: std::time::Duration::from_secs(1),
            transport: Transport::Stdio {
                program: "/nonexistent/definitely-not-here".into(),
                args: vec![],
                environment: HashMap::new(),
            },
        }
    }

    /// Build `inner`, publish it into fresh `ArcSwap`s, and return a live `McpHandle` pointing
    /// at the same state so tests can hit both the mutation and the read path.
    fn setup(entries: Vec<ServerEntry>) -> (McpManagerInner, McpHandle) {
        let inner = McpManagerInner {
            entries,
            generation: 0,
        };
        let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
        let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
        publish(&inner, &index, &snapshot);
        let handle = McpHandle {
            cmd_tx: flume::unbounded().0,
            index,
            snapshot,
        };
        (inner, handle)
    }

    #[test]
    fn start_with_config_produces_terminal_statuses() {
        smol::block_on(async {
            assert!(start_with_config(McpConfig::default()).await.is_none());

            let mut disabled = stdio_raw(&["unused-disabled-cmd"]);
            disabled.enabled = false;
            let config = make_config(vec![
                ("disabled-srv", disabled),
                ("bad-srv", stdio_raw(&[])),
            ]);
            let handle = start_with_config(config).await.unwrap();
            let infos = handle.reader().load().infos.clone();

            let bad = infos.iter().find(|i| i.name == "bad-srv").unwrap();
            assert!(matches!(bad.status, McpServerStatus::Failed(_)));
            assert_eq!(bad.tool_count, 0);

            let disabled = infos.iter().find(|i| i.name == "disabled-srv").unwrap();
            assert_eq!(disabled.status, McpServerStatus::Disabled);
        });
    }

    /// If a refresh fails, the entry must end up empty. A zombie tool left behind would be
    /// handed to the model on the next turn and then try to call into a dead transport.
    #[test]
    fn failed_refresh_clears_entry() {
        smol::block_on(async {
            let t = FakeTransport::new();
            let (mut inner, _) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
            inner.entries[0].config = Some(bad_stdio_config("srv"));

            assert!(refresh_server(&mut inner, "srv", None).await.is_err());

            let entry = &inner.entries[0];
            assert_eq!(t.shutdowns(), 1);
            assert!(entry.tools.is_empty());
            assert!(entry.prompts.is_empty());
            assert!(entry.transport.is_none());
            assert!(matches!(entry.status, McpServerStatus::Failed(_)));
        });
    }

    #[test]
    fn disable_purges_entry_and_published_view() {
        smol::block_on(async {
            let t = FakeTransport::new();
            let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

            assert!(handle.has_tool(TOOL_NAME));
            let mut tools = json!([]);
            handle.extend_tools(&mut tools);
            assert_eq!(tools[0]["name"], TOOL_NAME);

            handle_toggle(&mut inner, "srv", false).await;
            publish(&inner, &handle.index, &handle.snapshot);

            let entry = &inner.entries[0];
            assert_eq!(t.shutdowns(), 1);
            assert!(entry.tools.is_empty());
            assert!(entry.transport.is_none());
            assert_eq!(entry.status, McpServerStatus::Disabled);
            assert!(!handle.has_tool(TOOL_NAME));
            let mut tools = json!([]);
            handle.extend_tools(&mut tools);
            assert!(tools.as_array().unwrap().is_empty());
        });
    }

    /// Regression: the lock-free refactor fixed a case where `call_tool` held the inner read
    /// lock across the transport await, so any in-flight call blocked every publish behind it.
    /// The rendezvous here stays deterministic: the call signals on `call_entered`, the test
    /// waits for that signal, then calls `publish` while the call is still parked on `call_gate`.
    #[test]
    fn slow_tool_call_does_not_block_publish() {
        smol::block_on(async {
            let t = FakeTransport::new();
            let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

            let held = t.call_gate.lock().await;
            let entered = t.call_entered_rx.clone();
            let call_handle = {
                let handle = handle.clone();
                smol::spawn(async move { handle.call_tool(TOOL_NAME, &json!({})).await.unwrap() })
            };

            entered.recv_async().await.unwrap();
            inner.generation += 1;
            publish(&inner, &handle.index, &handle.snapshot);
            assert_eq!(handle.snapshot.load().generation, 1);

            drop(held);
            call_handle.await;
        });
    }

    #[test]
    fn shutdown_command_drains_and_acks() {
        smol::block_on(async {
            let (t1, t2) = (FakeTransport::new(), FakeTransport::new());
            let inner = McpManagerInner {
                entries: vec![
                    fake_entry("a", Arc::clone(&t1) as _),
                    fake_entry("b", Arc::clone(&t2) as _),
                ],
                generation: 0,
            };
            let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
            let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
            let (cmd_tx, cmd_rx) = flume::unbounded();
            let loop_task = smol::spawn(run(
                inner,
                Arc::clone(&index),
                Arc::clone(&snapshot),
                cmd_rx,
            ));

            let (ack_tx, ack_rx) = flume::bounded(1);
            cmd_tx.send(McpCommand::Shutdown { ack: ack_tx }).unwrap();
            ack_rx.recv_async().await.unwrap();
            loop_task.await;

            assert_eq!(t1.shutdowns(), 1);
            assert_eq!(t2.shutdowns(), 1);
            assert!(snapshot.load().infos.iter().all(|i| i.tool_count == 0));
        });
    }
}
