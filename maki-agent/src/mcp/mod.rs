//! MCP client: manages transports and routes tool calls to servers.
//!
//! Tool names are namespaced as `server.tool` so two servers can both expose `search`
//! without colliding. Names are deduped via `Arc<str>` in a small cache.
//!
//! All mutable state lives in the `run` task, which owns `McpManagerInner` exclusively.
//! Commands come in through a channel (one at a time, no interleaving). Reads go through
//! two lock-free `ArcSwap`s: a `ToolIndex` for tool calls and an `McpSnapshot` for the UI.
//! This way a slow tool call never blocks a toggle and vice versa.
//!
//! `McpSnapshotReader` is a read-only handle. Outside code physically cannot publish a
//! snapshot, so the "only `run` publishes" invariant is enforced by the type system.

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
use std::time::Duration;

use arc_swap::{ArcSwap, Guard};
use maki_providers::{ContentBlock, Message};
use serde_json::{Value, json};
use tracing::{info, warn};

use self::config::{
    McpConfig, McpConfigErrors, McpServerInfo, McpServerStatus, ServerConfig, Transport,
    load_config, parse_server, transport_kind,
};
use self::error::McpError;
use self::http::HttpTransport;
use self::stdio::StdioTransport;
use self::transport::McpTransport;

const SEPARATOR: &str = ".";
const WIRE_SEPARATOR: &str = "__";
pub const UNKNOWN_MCP: &str = "unknown_mcp";
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
/// Below this many deferrable tools, a search round-trip plus its
/// prompt-cache miss cost more than a handful of upfront definitions.
/// Overridden by `defer_tools` in mcp.toml.
const DEFAULT_DEFER_TOOLS: usize = 10;
/// Loads per search are capped so one broad query can't flood the context.
const MAX_SEARCH_LOADS: usize = 5;
const NAME_HIT_SCORE: usize = 2;
const DESCRIPTION_HIT_SCORE: usize = 1;
/// Overflow names shown to the model so it can re-search by exact name.
const MAX_OVERFLOW_NAMES: usize = 20;
const SEARCH_NO_MATCH: &str = "No deferred MCP tools matched";
const SEARCH_OVERFLOW_PREFIX: &str = "Also matched but not loaded: ";
pub(crate) const SEARCH_EMPTY_QUERY: &str = "query must not be empty";

/// Convert internal qualified name (`server.tool`) to wire format (`server__tool`)
/// for LLM provider APIs that reject dots in tool names.
///
/// Lossless: server names can't contain `__` (only alphanumeric + `-`),
/// so the first `__` in the wire name is always the separator boundary.
pub fn wire_tool_name(qualified: &str) -> String {
    qualified.replacen(SEPARATOR, WIRE_SEPARATOR, 1)
}

/// Convert wire format (`server__tool`) back to internal qualified name (`server.tool`).
///
/// Only the first `__` is the separator — tool names may contain underscores.
pub fn internal_tool_name(wire: &str) -> String {
    wire.replacen(WIRE_SEPARATOR, SEPARATOR, 1)
}
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

struct McpToolDef {
    qualified_name: Arc<str>,
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
            .filter(|info| {
                if !config::is_valid_tool_name(&info.name) {
                    warn!(tool = %info.name, server = %self.name, "skipping tool with invalid name");
                    return false;
                }
                // Wire format is server__tool — check total length fits LLM API limits
                let wire_len = self.name.len() + 2 + info.name.len();
                if wire_len > 64 {
                    warn!(
                        tool = %info.name,
                        server = %self.name,
                        wire_len,
                        "skipping tool — wire name exceeds 64 char LLM API limit"
                    );
                    return false;
                }
                true
            })
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
    tools: HashMap<Arc<str>, ToolRef>,
    prompts: HashMap<String, PromptRef>,
    descriptors: Arc<[ToolDescriptor]>,
}

/// One published MCP tool. Wire name and search text are derived from
/// `definition` on demand: searches are model-paced and rare, so nothing
/// to cache.
struct ToolDescriptor {
    qualified_name: Arc<str>,
    always_load: bool,
    definition: Value,
}

impl ToolDescriptor {
    fn wire_name(&self) -> &str {
        self.definition["name"].as_str().unwrap_or_default()
    }
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
    /// Never changes after startup, so it lives here instead of being
    /// copied into every republished `ToolIndex`.
    defer_tools: usize,
}

/// One session's view of MCP: the shared handle plus the deferred tools
/// this session loaded. Loads are per session, so a subagent's searches
/// never bloat the parent's context.
///
/// `extend_tools` output must never be stored: recompute it every request
/// or the `tool_search` catalog goes stale.
#[derive(Clone)]
pub struct McpSession {
    handle: McpHandle,
    loaded: Arc<Mutex<HashSet<Arc<str>>>>,
}

impl std::ops::Deref for McpSession {
    type Target = McpHandle;
    fn deref(&self) -> &McpHandle {
        &self.handle
    }
}

impl McpSession {
    /// `history` seeds the loaded set when resuming: tools the model was
    /// already calling stay declared across restarts. A pure string scan,
    /// so it is safe before servers connect, and unknown names are inert.
    pub fn new(handle: McpHandle, history: &[Message]) -> Self {
        let loaded = history
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::ToolUse { name, .. } if name.contains(WIRE_SEPARATOR) => {
                    Some(internal_tool_name(name).into())
                }
                _ => None,
            })
            .collect();
        Self {
            handle,
            loaded: Arc::new(Mutex::new(loaded)),
        }
    }

    /// A view over the same handle with no loads, for a new (sub)session.
    pub fn fresh(&self) -> Self {
        Self::new(self.handle.clone(), &[])
    }

    /// Append this request's MCP definitions: loaded and `always_load`
    /// tools in full, the rest as names inside one `tool_search` catalog.
    /// Names already in the array are skipped.
    ///
    /// The `defer_tools` threshold is measured against the full index, not
    /// what's left deferred, so loading tools mid-session can never flip
    /// the remainder into the context.
    pub fn extend_tools(&self, tools: &mut Value) {
        let Some(arr) = tools.as_array_mut() else {
            debug_assert!(false, "tools must be a JSON array");
            return;
        };
        let existing: HashSet<String> = arr
            .iter()
            .filter_map(|t| t["name"].as_str().map(String::from))
            .collect();
        let idx = self.handle.index.load();
        let defer =
            idx.descriptors.iter().filter(|d| !d.always_load).count() > self.handle.defer_tools;
        let loaded = self.lock_loaded();
        let mut deferred: Vec<&ToolDescriptor> = Vec::new();
        for d in idx.descriptors.iter() {
            if existing.contains(d.wire_name()) {
                continue;
            }
            if !defer || d.always_load || loaded.contains(&*d.qualified_name) {
                arr.push(d.definition.clone());
            } else {
                deferred.push(d);
            }
        }
        drop(loaded);
        if !deferred.is_empty() {
            if existing.contains(TOOL_SEARCH_TOOL_NAME) {
                warn!(
                    deferred = deferred.len(),
                    "a tool named {TOOL_SEARCH_TOOL_NAME} already exists; deferred MCP tools stay hidden"
                );
            } else {
                arr.push(tool_search_definition(&deferred));
            }
        }
    }

    /// Rank deferred tools against `query` keywords (exact name first,
    /// then name hits over description hits) and mark the top
    /// `MAX_SEARCH_LOADS` loaded; their definitions join the next request.
    pub fn search_tools(&self, query: &str) -> Result<String, String> {
        let q = query.trim().to_lowercase();
        let tokens: Vec<&str> = q
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return Err(SEARCH_EMPTY_QUERY.into());
        }
        let idx = self.handle.index.load();
        let mut matches: Vec<(bool, usize, &ToolDescriptor)> = idx
            .descriptors
            .iter()
            .filter(|d| !d.always_load)
            .filter_map(|d| {
                let name = d.wire_name().to_lowercase();
                let haystack = build_haystack(&d.definition);
                // The catalog shows bare tool names, so exact match must
                // accept both `server__tool` and `tool`.
                let exact = name == q
                    || d.qualified_name
                        .split_once(SEPARATOR)
                        .is_some_and(|(_, raw)| raw.eq_ignore_ascii_case(&q));
                let score: usize = tokens
                    .iter()
                    .map(|t| {
                        if name.contains(t) {
                            NAME_HIT_SCORE
                        } else if haystack.contains(t) {
                            DESCRIPTION_HIT_SCORE
                        } else {
                            0
                        }
                    })
                    .sum();
                (exact || score > 0).then_some((exact, score, d))
            })
            .collect();
        matches.sort_by(|a, b| {
            (b.0, b.1)
                .cmp(&(a.0, a.1))
                .then_with(|| a.2.wire_name().cmp(b.2.wire_name()))
        });
        let mut loaded = self.lock_loaded();
        let mut hits: Vec<&str> = Vec::new();
        let mut overflow: Vec<&str> = Vec::new();
        for (_, _, d) in &matches {
            if hits.len() < MAX_SEARCH_LOADS {
                loaded.insert(Arc::clone(&d.qualified_name));
                hits.push(d.wire_name());
            } else {
                overflow.push(d.wire_name());
            }
        }
        drop(loaded);
        info!(query = %q, loaded = hits.len(), overflow = overflow.len(), "MCP tool search");
        if hits.is_empty() {
            return Ok(format!(
                "{SEARCH_NO_MATCH} '{query}'. Try other keywords or an exact name from the catalog."
            ));
        }
        let plural = if hits.len() == 1 { "tool" } else { "tools" };
        let mut out = format!(
            "Loaded {} {plural}, callable from your next message:",
            hits.len()
        );
        for hit in &hits {
            out.push_str(&format!("\n- `{hit}`"));
        }
        if !overflow.is_empty() {
            let shown = overflow.len().min(MAX_OVERFLOW_NAMES);
            let names: Vec<String> = overflow[..shown].iter().map(|n| format!("`{n}`")).collect();
            out.push_str(&format!("\n{SEARCH_OVERFLOW_PREFIX}{}", names.join(", ")));
            if overflow.len() > shown {
                out.push_str(&format!(" and {} more", overflow.len() - shown));
            }
            out.push_str(". Search an exact tool name to load it.");
        }
        Ok(out)
    }

    /// Invoked on every MCP dispatch: a deferred tool the model calls by
    /// catalog name gets its full definition on the next request.
    pub fn mark_loaded(&self, qualified_name: &str) {
        self.lock_loaded().insert(Arc::from(qualified_name));
    }

    fn lock_loaded(&self) -> std::sync::MutexGuard<'_, HashSet<Arc<str>>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }
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

    pub fn interned_name(&self, name: &str) -> Arc<str> {
        self.index
            .load()
            .tools
            .get_key_value(name)
            .map(|(k, _)| Arc::clone(k))
            .unwrap_or_else(|| Arc::from(UNKNOWN_MCP))
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

    pub async fn shutdown(&self) {
        let (ack_tx, ack_rx) = flume::bounded(1);
        self.send(McpCommand::Shutdown { ack: ack_tx });
        let finished = futures_lite::future::or(
            async {
                let _ = ack_rx.recv_async().await;
                true
            },
            async {
                smol::Timer::after(MCP_SHUTDOWN_TIMEOUT).await;
                false
            },
        )
        .await;
        if !finished {
            warn!("MCP shutdown timed out after {MCP_SHUTDOWN_TIMEOUT:?}");
        }
    }
}

pub async fn start(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    tracing::info!(cwd = %cwd.display(), "starting MCP");
    let cwd = cwd.to_owned();
    let (config, config_errors) = smol::unblock(move || load_config(&cwd)).await;
    let handle = start_with_config(config).await;
    (handle, config_errors)
}

pub async fn start_with_config(config: McpConfig) -> Option<McpHandle> {
    if config.is_empty() {
        tracing::info!("no MCP servers configured, skipping");
        return None;
    }

    let defer_tools = config.defer_tools.unwrap_or(DEFAULT_DEFER_TOOLS);
    let mut inner = parse_entries(config);
    start_enabled(&mut inner).await;
    inner.generation += 1;

    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    let index: Arc<ArcSwap<ToolIndex>> = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    publish(&inner, &index, &snapshot);

    let (cmd_tx, cmd_rx) = flume::unbounded();
    let handle = McpHandle {
        cmd_tx,
        index: Arc::clone(&index),
        snapshot: Arc::clone(&snapshot),
        defer_tools,
    };

    info!(
        running = inner
            .entries
            .iter()
            .filter(|e| e.transport.is_some())
            .count(),
        total = inner.entries.len(),
        "MCP servers initialized"
    );

    smol::spawn(run(inner, index, snapshot, cmd_rx)).detach();
    Some(handle)
}

async fn run(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    cmd_rx: flume::Receiver<McpCommand>,
) {
    let mut ack: Option<flume::Sender<()>> = None;
    while let Ok(cmd) = cmd_rx.recv_async().await {
        match cmd {
            McpCommand::Toggle { server, enabled } => {
                handle_toggle(&mut inner, &server, enabled).await;
            }
            McpCommand::Reconnect { server } => {
                handle_reconnect(&mut inner, &server).await;
            }
            McpCommand::Shutdown { ack: tx } => {
                ack = Some(tx);
                break;
            }
        }
        inner.generation += 1;
        publish(&inner, &index, &snapshot);
    }
    shutdown_all(&mut inner).await;
    inner.generation += 1;
    publish(&inner, &index, &snapshot);
    if let Some(tx) = ack {
        let _ = tx.try_send(());
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
        if let Err(e) = refresh_server(inner, server_name).await {
            warn!(server = %server_name, error = %e, "MCP server refresh failed");
        }
    } else if let Some(entry) = inner.entries.iter_mut().find(|e| e.name == server_name) {
        entry.clear_connection().await;
        entry.status = McpServerStatus::Disabled;
    }

    info!(server = server_name, enabled, "MCP toggle complete");
}

/// Restart the server with its stored config. Fresh OAuth tokens are picked up
/// from storage by the transport, so no credentials travel through the command.
async fn handle_reconnect(inner: &mut McpManagerInner, server_name: &str) {
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
    if let Err(e) = refresh_server(inner, server_name).await {
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
async fn refresh_server(inner: &mut McpManagerInner, server_name: &str) -> Result<(), McpError> {
    let Some(idx) = inner.entries.iter().position(|e| e.name == server_name) else {
        return Err(McpError::Config(format!("unknown server '{server_name}'")));
    };

    let config = inner.entries[idx]
        .config
        .clone()
        .ok_or_else(|| McpError::Config(format!("server '{server_name}' has no config")))?;

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
            maki_storage::StateDir::resolve().ok(),
        )?),
    };
    let capabilities = transport::initialize(transport.as_ref()).await?;
    // Asymmetric on purpose: sloppy servers omit `capabilities` yet serve
    // tools/list fine, so always ask (fatal only when tools were declared).
    // Prompts only when declared: undeclared endpoints may answer junk,
    // and junk must not take down the server's tools.
    let tool_infos = match transport::list_tools(transport.as_ref()).await {
        Ok(tools) => tools,
        Err(e) if !capabilities.tools => {
            warn!(server = config.name, error = %e, "tools/list failed; server declared no tools");
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let prompt_infos = if capabilities.prompts {
        transport::list_prompts(transport.as_ref()).await?
    } else {
        Vec::new()
    };
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

    // Config maps are unordered; a stable order keeps the tool_search
    // catalog and tools array byte-identical across runs (prompt cache).
    entries.sort_by(|a, b| a.name.cmp(&b.name));

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
    let mut descriptors: Vec<ToolDescriptor> = Vec::new();
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
            let always_load = entry.config.as_ref().is_some_and(|c| c.always_load);
            for t in &entry.tools {
                tools.insert(
                    Arc::clone(&t.qualified_name),
                    ToolRef {
                        raw_name: t.raw_name.clone(),
                        transport: Arc::clone(transport),
                    },
                );
                descriptors.push(ToolDescriptor {
                    qualified_name: Arc::clone(&t.qualified_name),
                    always_load,
                    definition: json!({
                        "name": wire_tool_name(&t.qualified_name),
                        "description": t.description,
                        "input_schema": t.input_schema,
                    }),
                });
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

/// Session for dispatch-level tests outside this module, built through the
/// real `publish` path so it can't drift from production index construction.
#[cfg(test)]
pub(crate) fn stub_session(tools: &[(&str, &str)]) -> McpSession {
    let entry = ServerEntry {
        name: "stub".into(),
        config: None,
        transport_kind: "stub",
        origin: PathBuf::new(),
        status: McpServerStatus::Running,
        transport: Some(Arc::new(StubTransport(Arc::from("stub")))),
        tools: tools
            .iter()
            .map(|(qualified, description)| McpToolDef {
                qualified_name: Arc::from(*qualified),
                raw_name: qualified
                    .split_once(SEPARATOR)
                    .map_or(*qualified, |(_, r)| r)
                    .into(),
                description: (*description).into(),
                input_schema: json!({}),
            })
            .collect(),
        prompts: Vec::new(),
    };
    let inner = McpManagerInner {
        entries: vec![entry],
        generation: 0,
    };
    let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    publish(&inner, &index, &snapshot);
    McpSession::new(
        McpHandle {
            cmd_tx: flume::unbounded().0,
            index,
            snapshot,
            defer_tools: 0,
        },
        &[],
    )
}

#[cfg(test)]
pub(crate) fn tool_names(tools: &Value) -> Vec<&str> {
    tools
        .as_array()
        .expect("tools must be a JSON array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect()
}

#[cfg(test)]
struct StubTransport(Arc<str>);

#[cfg(test)]
impl McpTransport for StubTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        _params: Option<Value>,
    ) -> transport::BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            Err(McpError::UnknownTool {
                name: method.into(),
            })
        })
    }
    fn send_notification<'a>(
        &'a self,
        _method: &'a str,
        _params: Option<Value>,
    ) -> transport::BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }
    fn shutdown<'a>(&'a self) -> transport::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn server_name(&self) -> &Arc<str> {
        &self.0
    }
    fn transport_kind(&self) -> &'static str {
        "stub"
    }
}

fn build_haystack(definition: &Value) -> String {
    let mut hay = definition["description"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    if let Some(props) = definition["input_schema"]["properties"].as_object() {
        for key in props.keys() {
            hay.push(' ');
            hay.push_str(&key.to_lowercase());
        }
    }
    hay
}

fn tool_search_definition(deferred: &[&ToolDescriptor]) -> Value {
    // Grouping by server drops the repeated `server__` prefix, a few
    // tokens per tool. Descriptors arrive grouped because entries are
    // sorted and published per server.
    let mut catalog = String::new();
    let mut current_server = "";
    for d in deferred {
        let (server, raw) = d
            .qualified_name
            .split_once(SEPARATOR)
            .unwrap_or((UNKNOWN_MCP, &d.qualified_name));
        if server == current_server {
            catalog.push_str(", ");
        } else {
            if !catalog.is_empty() {
                catalog.push('\n');
            }
            catalog.push_str(server);
            catalog.push_str(": ");
            current_server = server;
        }
        catalog.push_str(raw);
    }
    json!({
        "name": TOOL_SEARCH_TOOL_NAME,
        "description": format!(
            "Search and load deferred MCP tools; they are not callable until \
             loaded, from your next message on. Keywords match tool names, \
             descriptions, and parameter names; an exact tool name always wins.\n\
             Deferred tools:\n{catalog}"
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or an exact tool name from the catalog"
                }
            },
            "required": ["query"]
        }
    })
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

/// Dedup cache for qualified MCP tool names. The set is bounded (finite per session)
/// and `Arc<str>` means entries get freed when the cache drops, unlike the old `Box::leak`.
fn intern(name: String) -> Arc<str> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<str>>>> = OnceLock::new();
    let mut map = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(&name) {
        return Arc::clone(existing);
    }
    let arc: Arc<str> = Arc::from(name.as_str());
    map.insert(name, Arc::clone(&arc));
    arc
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_lock::Mutex as AsyncMutex;
    use config::{RawServerConfig, RawStdioFields, RawTransport};
    use maki_providers::Role;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use test_case::test_case;

    const DEFAULT_TIMEOUT_MS: u64 = 30_000;

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            always_load: false,
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
        McpConfig {
            mcp,
            origins,
            ..Default::default()
        }
    }

    const TOOL_NAME: &str = "srv.tool";
    const WIRE_TOOL_NAME: &str = "srv__tool";

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
            always_load: false,
            transport: Transport::Stdio {
                program: "/nonexistent/definitely-not-here".into(),
                args: vec![],
                environment: HashMap::new(),
            },
        }
    }

    /// Build `inner`, publish it into fresh `ArcSwap`s, and return a live `McpSession` pointing
    /// at the same state so tests can hit both the mutation and the read path.
    fn setup(entries: Vec<ServerEntry>) -> (McpManagerInner, McpSession) {
        setup_with_defer(entries, 0)
    }

    fn setup_with_defer(
        entries: Vec<ServerEntry>,
        defer_tools: usize,
    ) -> (McpManagerInner, McpSession) {
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
            defer_tools,
        };
        (inner, McpSession::new(handle, &[]))
    }

    #[test]
    fn parse_entries_sorts_servers_by_name() {
        let config = make_config(vec![
            ("zeta", stdio_raw(&["z"])),
            ("alpha", stdio_raw(&["a"])),
            ("mid", stdio_raw(&["m"])),
        ]);
        let inner = parse_entries(config);
        let names: Vec<&str> = inner.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    fn always_load_entry(name: &str, transport: Arc<dyn McpTransport>) -> ServerEntry {
        let mut raw = stdio_raw(&["echo"]);
        raw.always_load = true;
        let mut entry = fake_entry(name, transport);
        entry.config = Some(parse_server(name.into(), raw).unwrap());
        entry
    }

    #[test]
    fn extend_tools_skips_deferral_at_or_below_threshold() {
        let (_inner, handle) = setup_with_defer(vec![fake_entry("srv", FakeTransport::new())], 1);
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![WIRE_TOOL_NAME]);
    }

    #[test]
    fn defer_threshold_ignores_always_load_tools() {
        let (_inner, handle) = setup_with_defer(
            vec![
                always_load_entry("eager", FakeTransport::new()),
                fake_entry("lazy", FakeTransport::new()),
            ],
            1,
        );
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec!["eager__tool", "lazy__tool"]);
    }

    #[test]
    fn extend_tools_defers_behind_tool_search_by_default() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![TOOL_SEARCH_TOOL_NAME]);
        let catalog = tools[0]["description"].as_str().unwrap();
        assert!(catalog.contains("srv: tool"), "catalog groups by server");
        assert!(handle.has_tool(TOOL_NAME), "deferred tools stay callable");
    }

    #[test]
    fn extend_tools_includes_always_load_server_upfront() {
        let (_inner, handle) = setup(vec![
            always_load_entry("eager", FakeTransport::new()),
            fake_entry("lazy", FakeTransport::new()),
        ]);
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        let names = tool_names(&tools);
        assert!(names.contains(&"eager__tool"));
        assert!(names.contains(&TOOL_SEARCH_TOOL_NAME));
        assert!(!names.contains(&"lazy__tool"));
    }

    #[test]
    fn search_loads_tools_into_next_extend() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let result = handle.search_tools("TOOL").unwrap();
        assert!(result.contains(WIRE_TOOL_NAME), "got: {result}");

        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![WIRE_TOOL_NAME]);
    }

    #[test]
    fn search_reports_no_match_without_loading() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let result = handle.search_tools("nonexistent-capability").unwrap();
        assert!(result.contains(SEARCH_NO_MATCH), "got: {result}");
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![TOOL_SEARCH_TOOL_NAME]);
    }

    #[test]
    fn search_caps_loads_and_reports_overflow() {
        let transport: Arc<dyn McpTransport> = FakeTransport::new();
        let mut entry = fake_entry("srv", Arc::clone(&transport));
        entry.tools = (0..MAX_SEARCH_LOADS + 2)
            .map(|i| McpToolDef {
                qualified_name: intern(format!("srv{SEPARATOR}tool-{i}")),
                raw_name: format!("tool-{i}"),
                description: String::new(),
                input_schema: json!({}),
            })
            .collect();
        let (_inner, handle) = setup(vec![entry]);

        let result = handle.search_tools("tool").unwrap();
        let expected = format!(
            "{SEARCH_OVERFLOW_PREFIX}`srv__tool-{}`, `srv__tool-{}`",
            MAX_SEARCH_LOADS,
            MAX_SEARCH_LOADS + 1
        );
        assert!(
            result.contains(&expected),
            "overflow must list names: {result}"
        );
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        // Loaded cap plus the search tool for the remaining deferred ones.
        assert_eq!(tools.as_array().unwrap().len(), MAX_SEARCH_LOADS + 1);
    }

    fn entry_with_tools(name: &str, tools: Vec<McpToolDef>) -> ServerEntry {
        let mut entry = fake_entry(name, FakeTransport::new());
        entry.tools = tools;
        entry
    }

    fn tool_def(server: &str, raw: &str, description: &str, schema: Value) -> McpToolDef {
        McpToolDef {
            qualified_name: intern(format!("{server}{SEPARATOR}{raw}")),
            raw_name: raw.into(),
            description: description.into(),
            input_schema: schema,
        }
    }

    #[test_case("srv__tool-" ; "wire_name")]
    #[test_case("tool-" ; "bare_name_as_shown_in_catalog")]
    fn search_exact_name_outranks_keyword_matches(prefix: &str) {
        let tools = (0..MAX_SEARCH_LOADS + 1)
            .map(|i| tool_def("srv", &format!("tool-{i}"), "", json!({})))
            .collect();
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        // Alphabetical tie-break alone would leave the last tool in overflow.
        let last = format!("{prefix}{MAX_SEARCH_LOADS}");
        let result = handle.search_tools(&last).unwrap();
        let overflow = result
            .lines()
            .find(|l| l.starts_with(SEARCH_OVERFLOW_PREFIX))
            .expect("one match past the cap must overflow");
        assert!(
            result.contains(&format!("srv__tool-{MAX_SEARCH_LOADS}"))
                && !overflow.contains(&format!("tool-{MAX_SEARCH_LOADS}")),
            "exact name must be loaded, not overflowed: {result}"
        );
    }

    #[test]
    fn search_ranks_name_hits_above_description_hits() {
        let tools = vec![
            tool_def("srv", "add_comment", "Comment on an issue", json!({})),
            tool_def("srv", "create_issue", "Open a ticket", json!({})),
        ];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        let result = handle.search_tools("issue").unwrap();
        let pos = |name: &str| {
            result
                .find(name)
                .unwrap_or_else(|| panic!("{name} must match: {result}"))
        };
        assert!(
            pos("srv__create_issue") < pos("srv__add_comment"),
            "name hit must rank above description hit: {result}"
        );
    }

    #[test]
    fn search_multi_word_query_matches_any_keyword() {
        let tools = vec![tool_def(
            "srv",
            "create_pr",
            "Open a pull request",
            json!({}),
        )];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        let result = handle.search_tools("pull request").unwrap();
        assert!(result.contains("srv__create_pr"), "got: {result}");
    }

    #[test]
    fn search_matches_schema_parameter_names() {
        let schema = json!({"type": "object", "properties": {"labels": {"type": "array"}}});
        let tools = vec![tool_def("srv", "update", "Update a thing", schema)];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        let result = handle.search_tools("labels").unwrap();
        assert!(result.contains("srv__update"), "got: {result}");
    }

    #[test]
    fn extend_tools_never_duplicates_existing_names() {
        let (_inner, handle) = setup(vec![always_load_entry("eager", FakeTransport::new())]);
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        handle.extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec!["eager__tool"]);
    }

    #[test]
    fn new_seeds_loads_only_from_wire_names_in_history() {
        let (_inner, session) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let tool_use = |name: &str| ContentBlock::ToolUse {
            id: "t".into(),
            name: name.into(),
            input: json!({}),
        };
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![
                tool_use(WIRE_TOOL_NAME),
                tool_use("read"),
                tool_use("gone__tool"),
            ],
            display_text: None,
        }];
        let restored = McpSession::new(session.handle.clone(), &history);
        let mut tools = json!([]);
        restored.extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec![WIRE_TOOL_NAME],
            "only wire names still in the index may load"
        );
    }

    #[test]
    fn mid_session_loads_never_flip_remainder_into_context() {
        let defs = vec![
            tool_def("srv", "alpha", "", json!({})),
            tool_def("srv", "beta", "", json!({})),
            tool_def("srv", "gamma", "", json!({})),
        ];
        let (_inner, handle) = setup_with_defer(vec![entry_with_tools("srv", defs)], 2);
        handle.mark_loaded("srv.alpha");
        handle.mark_loaded("srv.beta");
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        let names = tool_names(&tools);
        assert!(names.contains(&"srv__alpha") && names.contains(&"srv__beta"));
        assert!(
            !names.contains(&"srv__gamma"),
            "threshold must compare the full index, not the remaining deferred count: {names:?}"
        );
        let catalog = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == TOOL_SEARCH_TOOL_NAME)
            .expect("tool_search must stay while any tool is deferred");
        let description = catalog["description"].as_str().unwrap();
        assert!(description.contains("srv: gamma"), "got: {description}");
    }

    #[test]
    fn mark_loaded_declares_tool_and_drops_empty_catalog() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        handle.mark_loaded(TOOL_NAME);
        let mut tools = json!([]);
        handle.extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec![WIRE_TOOL_NAME],
            "loaded tool must be declared; an empty catalog must not be advertised"
        );
    }

    #[test]
    fn existing_wire_name_stays_out_of_catalog() {
        let defs = vec![
            tool_def("srv", "alpha", "", json!({})),
            tool_def("srv", "beta", "", json!({})),
        ];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", defs)]);
        let mut tools = json!([{ "name": "srv__alpha" }]);
        handle.extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec!["srv__alpha", TOOL_SEARCH_TOOL_NAME],
            "colliding name must be skipped, not deferred or re-added"
        );
        let catalog = tools[1]["description"].as_str().unwrap();
        assert!(catalog.contains("srv: beta"), "got: {catalog}");
        assert!(!catalog.contains("alpha"), "got: {catalog}");
    }

    #[test]
    fn history_seeding_handles_tool_names_with_double_underscores() {
        let defs = vec![tool_def("srv", "do__thing", "", json!({}))];
        let (_inner, session) = setup(vec![entry_with_tools("srv", defs)]);
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t".into(),
                name: "srv__do__thing".into(),
                input: json!({}),
            }],
            display_text: None,
        }];
        let restored = McpSession::new(session.handle.clone(), &history);
        let mut tools = json!([]);
        restored.extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec!["srv__do__thing"],
            "only the first __ is the server separator"
        );
    }

    #[test]
    fn search_ignores_always_load_tools() {
        let (_inner, handle) = setup(vec![always_load_entry("eager", FakeTransport::new())]);
        let result = handle.search_tools("tool").unwrap();
        assert!(
            result.contains(SEARCH_NO_MATCH),
            "always_load tools are already declared: {result}"
        );
    }

    #[test]
    fn search_loads_stay_scoped_to_their_session() {
        let (_inner, session_a) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let session_b = session_a.fresh();
        session_a.search_tools("tool").unwrap();

        let mut tools_a = json!([]);
        session_a.extend_tools(&mut tools_a);
        assert_eq!(tool_names(&tools_a), vec![WIRE_TOOL_NAME]);

        let mut tools_b = json!([]);
        session_b.extend_tools(&mut tools_b);
        assert_eq!(tool_names(&tools_b), vec![TOOL_SEARCH_TOOL_NAME]);
    }

    #[test]
    fn start_with_config_produces_terminal_statuses() {
        smol::block_on(async {
            let handle = start_with_config(McpConfig::default()).await;
            assert!(handle.is_none());

            let mut disabled = stdio_raw(&["unused-disabled-cmd"]);
            disabled.enabled = false;
            let config = make_config(vec![
                ("disabled-srv", disabled),
                ("bad-srv", stdio_raw(&[])),
            ]);
            let handle = start_with_config(config).await;
            let handle = handle.unwrap();
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

            assert!(refresh_server(&mut inner, "srv").await.is_err());

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
            assert_eq!(tools[0]["name"], TOOL_SEARCH_TOOL_NAME);

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

    #[test]
    fn is_valid_tool_name_enforces_wire_format() {
        use config::is_valid_tool_name;
        // Valid: alphanumeric, underscore, hyphen, 1-64 chars
        assert!(is_valid_tool_name("search"));
        assert!(is_valid_tool_name("web_search"));
        assert!(is_valid_tool_name("my-tool"));
        assert!(is_valid_tool_name(&"a".repeat(64)));
        // Invalid: empty, dots, special chars, too long
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("web.search"));
        assert!(!is_valid_tool_name("admin.delete"));
        assert!(!is_valid_tool_name("tool!"));
        assert!(!is_valid_tool_name("*"));
        assert!(!is_valid_tool_name(&"a".repeat(65)));
    }
}
