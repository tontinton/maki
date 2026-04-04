//! MCP client: manages transports and routes tool calls to servers.
//!
//! Tool names are namespaced as `server__tool` (double underscore) to avoid collisions across servers.
//! Names are leaked into `&'static str` so they can be used in tool descriptors without lifetime friction.

pub mod config;
pub mod error;
pub mod http;
pub mod oauth;
pub mod protocol;
pub mod stdio;
pub mod transport;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_lock::RwLock;
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

struct McpToolDef {
    qualified_name: &'static str,
    server_name: Arc<str>,
    raw_name: String,
    description: String,
    input_schema: Value,
}

struct McpPromptDef {
    qualified_name: String,
    server_name: Arc<str>,
    raw_name: String,
    description: String,
    arguments: Vec<protocol::PromptArgument>,
}

impl McpPromptDef {
    fn from_info(server_name: &Arc<str>, info: protocol::PromptInfo) -> Self {
        let qualified_name = format!("{server_name}{SEPARATOR}{}", info.name);
        Self {
            qualified_name,
            server_name: Arc::clone(server_name),
            raw_name: info.name,
            description: info.description.unwrap_or_default(),
            arguments: info.arguments,
        }
    }

    fn display_name(&self) -> String {
        format!("{}:{}", self.server_name, self.raw_name)
    }

    fn to_info(&self) -> McpPromptInfo {
        McpPromptInfo {
            display_name: self.display_name(),
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
    transport_kind: &'static str,
    origin: PathBuf,
    status: McpServerStatus,
    url: Option<String>,
    timeout: Duration,
}

struct McpManagerInner {
    transports: HashMap<Arc<str>, Box<dyn McpTransport>>,
    tools: Vec<McpToolDef>,
    tool_index: HashMap<&'static str, usize>,
    prompts: Vec<McpPromptDef>,
    entries: Vec<ServerEntry>,
}

pub struct McpManager {
    inner: RwLock<McpManagerInner>,
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

fn transport_url(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Http { url, .. } => Some(url.clone()),
        Transport::Stdio { .. } => None,
    }
}

impl McpManager {
    pub async fn start(cwd: &Path) -> Option<Arc<Self>> {
        let cwd = cwd.to_owned();
        let config = smol::unblock(move || load_config(&cwd)).await;
        Self::start_with_config(config).await
    }

    pub async fn start_with_config(config: McpConfig) -> Option<Arc<Self>> {
        if config.is_empty() {
            return None;
        }

        let origins = config.origins;
        let mut transports: HashMap<Arc<str>, Box<dyn McpTransport>> = HashMap::new();
        let mut tools = Vec::new();
        let mut tool_index = HashMap::new();
        let mut prompts = Vec::new();
        let mut entries = Vec::new();

        struct Pending {
            config: ServerConfig,
            kind: &'static str,
            origin: PathBuf,
        }

        let mut pending = Vec::new();

        for (name, raw) in config.mcp {
            let kind = transport_kind(&raw.transport);
            let origin = origins.get(&name).cloned().unwrap_or_default();

            if !raw.enabled {
                entries.push(ServerEntry {
                    name,
                    transport_kind: kind,
                    origin,
                    status: McpServerStatus::Disabled,
                    url: None,
                    timeout: Duration::default(),
                });
                continue;
            }

            match parse_server(name.clone(), raw) {
                Ok(sc) => pending.push(Pending {
                    config: sc,
                    kind,
                    origin,
                }),
                Err(e) => {
                    warn!(server = %name, error = %e, "invalid MCP server config");
                    entries.push(ServerEntry {
                        name,
                        transport_kind: kind,
                        origin,
                        status: McpServerStatus::Failed(e.to_string()),
                        url: None,
                        timeout: Duration::default(),
                    });
                }
            }
        }

        let handles: Vec<_> = pending
            .into_iter()
            .map(|p| {
                smol::spawn(async move {
                    let result = Self::start_server(&p.config).await;
                    (p, result)
                })
            })
            .collect();

        for handle in handles {
            let (p, result) = handle.await;
            let url = transport_url(&p.config.transport);
            let timeout = p.config.timeout;
            match result {
                Ok((t, server_tools, server_prompts)) => {
                    let server_name: Arc<str> = Arc::from(p.config.name.as_str());
                    for tool_info in server_tools {
                        let qualified = format!("{}{SEPARATOR}{}", p.config.name, tool_info.name);
                        let interned = intern(qualified);
                        let idx = tools.len();
                        tools.push(McpToolDef {
                            qualified_name: interned,
                            server_name: Arc::clone(&server_name),
                            raw_name: tool_info.name,
                            description: tool_info.description,
                            input_schema: tool_info.input_schema,
                        });
                        tool_index.insert(interned, idx);
                    }
                    for info in server_prompts {
                        prompts.push(McpPromptDef::from_info(&server_name, info));
                    }
                    transports.insert(Arc::clone(&server_name), t);
                    entries.push(ServerEntry {
                        name: p.config.name,
                        transport_kind: p.kind,
                        origin: p.origin,
                        status: McpServerStatus::Running,
                        url,
                        timeout,
                    });
                }
                Err(e) => {
                    let status = if let McpError::HttpError {
                        status: 401,
                        ref reason,
                        ..
                    } = e
                    {
                        McpServerStatus::NeedsAuth {
                            url: Some(reason.clone()),
                        }
                    } else {
                        warn!(server = %p.config.name, error = %e, "failed to start MCP server");
                        McpServerStatus::Failed(e.to_string())
                    };
                    entries.push(ServerEntry {
                        name: p.config.name,
                        transport_kind: p.kind,
                        origin: p.origin,
                        status,
                        url,
                        timeout,
                    });
                }
            }
        }

        info!(
            running = transports.len(),
            tools = tools.len(),
            prompts = prompts.len(),
            total = entries.len(),
            "MCP servers initialized"
        );

        Some(Arc::new(Self {
            inner: RwLock::new(McpManagerInner {
                transports,
                tools,
                tool_index,
                prompts,
                entries,
            }),
        }))
    }

    async fn start_server(
        config: &ServerConfig,
    ) -> Result<
        (
            Box<dyn McpTransport>,
            Vec<protocol::ToolInfo>,
            Vec<protocol::PromptInfo>,
        ),
        McpError,
    > {
        let t: Box<dyn McpTransport> = match &config.transport {
            Transport::Stdio {
                program,
                args,
                environment,
            } => Box::new(StdioTransport::spawn(
                &config.name,
                program,
                args,
                environment,
                config.timeout,
            )?),
            Transport::Http { url, headers } => Box::new(HttpTransport::new(
                &config.name,
                url,
                headers,
                config.timeout,
            )?),
        };
        transport::initialize(t.as_ref()).await?;
        let tools = transport::list_tools(t.as_ref()).await?;
        let prompts = transport::list_prompts(t.as_ref()).await?;
        info!(
            server = config.name,
            tool_count = tools.len(),
            prompt_count = prompts.len(),
            "MCP server initialized"
        );
        Ok((t, tools, prompts))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.inner.read_blocking().tool_index.contains_key(name)
    }

    pub fn interned_name(&self, name: &str) -> &'static str {
        self.inner
            .read_blocking()
            .tool_index
            .get_key_value(name)
            .map(|(&k, _)| k)
            .unwrap_or("unknown_mcp")
    }

    pub async fn call_tool(&self, qualified_name: &str, args: &Value) -> Result<String, McpError> {
        let inner = self.inner.read().await;
        let idx = inner
            .tool_index
            .get(qualified_name)
            .ok_or_else(|| McpError::UnknownTool {
                name: qualified_name.into(),
            })?;
        let def = &inner.tools[*idx];
        let t = inner
            .transports
            .get(&def.server_name)
            .ok_or_else(|| McpError::ServerDied {
                server: (*def.server_name).into(),
            })?;
        transport::call_tool(t.as_ref(), &def.raw_name, args).await
    }

    pub fn extend_tools(&self, tools: &mut Value, disabled: &[String]) {
        let inner = self.inner.read_blocking();
        for t in inner
            .tools
            .iter()
            .filter(|t| !disabled.contains(&t.server_name.to_string()))
        {
            if let Some(arr) = tools.as_array_mut() {
                arr.push(json!({
                    "name": t.qualified_name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }));
            }
        }
    }

    pub fn server_infos(&self, disabled: &[String]) -> Vec<McpServerInfo> {
        let inner = self.inner.read_blocking();
        let mut counts: HashMap<&str, (usize, usize)> = HashMap::new();
        for tool in &inner.tools {
            counts.entry(&tool.server_name).or_default().0 += 1;
        }
        for prompt in &inner.prompts {
            counts.entry(&prompt.server_name).or_default().1 += 1;
        }
        inner
            .entries
            .iter()
            .map(|entry| {
                let status = if disabled.contains(&entry.name) {
                    McpServerStatus::Disabled
                } else {
                    entry.status.clone()
                };
                let (tool_count, prompt_count) = if matches!(status, McpServerStatus::Running) {
                    counts.get(entry.name.as_str()).copied().unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                McpServerInfo {
                    name: entry.name.clone(),
                    transport_kind: entry.transport_kind,
                    tool_count,
                    prompt_count,
                    status,
                    config_path: entry.origin.clone(),
                    url: entry.url.clone(),
                }
            })
            .collect()
    }

    pub fn prompt_infos(&self, disabled: &[String]) -> Vec<McpPromptInfo> {
        let inner = self.inner.read_blocking();
        inner
            .prompts
            .iter()
            .filter(|p| !disabled.iter().any(|d| **d == *p.server_name))
            .map(|p| p.to_info())
            .collect()
    }

    pub async fn get_prompt(
        &self,
        qualified_name: &str,
        arguments: &HashMap<String, String>,
    ) -> Result<Vec<protocol::PromptMessage>, McpError> {
        let inner = self.inner.read().await;
        let def = inner
            .prompts
            .iter()
            .find(|p| p.qualified_name == qualified_name)
            .ok_or_else(|| McpError::UnknownPrompt {
                name: qualified_name.into(),
            })?;
        let t = inner
            .transports
            .get(&def.server_name)
            .ok_or_else(|| McpError::ServerDied {
                server: (*def.server_name).into(),
            })?;
        transport::get_prompt(t.as_ref(), &def.raw_name, arguments).await
    }

    pub async fn shutdown(self) {
        let inner = self.inner.into_inner();
        let handles: Vec<_> = inner
            .transports
            .into_iter()
            .map(|(name, t)| {
                smol::spawn(async move {
                    info!(server = &*name, "shutting down MCP server");
                    t.shutdown().await;
                })
            })
            .collect();
        for h in handles {
            h.await;
        }
    }

    pub fn child_pids(&self) -> Vec<u32> {
        self.inner
            .read_blocking()
            .transports
            .values()
            .flat_map(|t| t.child_pids())
            .collect()
    }

    pub async fn reconnect_server(
        &self,
        server_name: &str,
        server_url: &str,
        access_token: &str,
    ) -> Result<(), McpError> {
        let timeout = {
            let inner = self.inner.read().await;
            inner
                .entries
                .iter()
                .find(|e| e.name == server_name)
                .map(|e| e.timeout)
                .unwrap_or_default()
        };
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        let transport: Box<dyn McpTransport> = Box::new(HttpTransport::new(
            server_name,
            server_url,
            &headers,
            timeout,
        )?);
        transport::initialize(transport.as_ref()).await?;
        let new_tools = transport::list_tools(transport.as_ref()).await?;
        let new_prompts = transport::list_prompts(transport.as_ref()).await?;

        let mut inner = self.inner.write().await;
        let server_key: Arc<str> = Arc::from(server_name);

        inner.tools.retain(|t| *t.server_name != *server_name);
        inner.prompts.retain(|p| *p.server_name != *server_name);
        inner.tool_index.clear();
        let rebind: Vec<_> = inner
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.qualified_name, i))
            .collect();
        for (name, i) in rebind {
            inner.tool_index.insert(name, i);
        }

        for tool_info in new_tools {
            let qualified = format!("{server_name}{SEPARATOR}{}", tool_info.name);
            let interned = intern(qualified);
            let idx = inner.tools.len();
            inner.tools.push(McpToolDef {
                qualified_name: interned,
                server_name: Arc::clone(&server_key),
                raw_name: tool_info.name,
                description: tool_info.description,
                input_schema: tool_info.input_schema,
            });
            inner.tool_index.insert(interned, idx);
        }

        for info in new_prompts {
            inner
                .prompts
                .push(McpPromptDef::from_info(&server_key, info));
        }

        inner.transports.insert(Arc::clone(&server_key), transport);

        for entry in &mut inner.entries {
            if entry.name == server_name {
                entry.status = McpServerStatus::Running;
            }
        }

        info!(
            server = server_name,
            tools = inner.tools.len(),
            "MCP server reconnected after OAuth"
        );
        Ok(())
    }
}

#[cfg(unix)]
pub fn kill_process_groups(pids: &[u32]) {
    for &pid in pids {
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
pub fn kill_process_groups(_pids: &[u32]) {}

fn intern(name: String) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
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
    use config::{McpServerStatus, RawServerConfig, RawStdioFields, RawTransport};
    use std::collections::HashMap;
    use std::path::PathBuf;

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

    #[test]
    fn empty_config_returns_none() {
        smol::block_on(async {
            let config = McpConfig::default();
            let result = McpManager::start_with_config(config).await;
            assert!(result.is_none());
        });
    }

    #[test]
    fn mixed_config_produces_correct_entries() {
        smol::block_on(async {
            let mut disabled = stdio_raw(&["echo"]);
            disabled.enabled = false;
            let config = make_config(vec![
                ("disabled-srv", disabled),
                ("bad-srv", stdio_raw(&[])),
                ("also-bad", stdio_raw(&[])),
            ]);
            let mgr = McpManager::start_with_config(config).await.unwrap();
            let mut infos = mgr.server_infos(&[]);
            infos.sort_by(|a, b| a.name.cmp(&b.name));
            assert_eq!(infos.len(), 3);
            assert!(matches!(infos[0].status, McpServerStatus::Failed(_)));
            assert_eq!(infos[0].tool_count, 0);
            assert!(matches!(infos[1].status, McpServerStatus::Failed(_)));
            assert_eq!(infos[2].status, McpServerStatus::Disabled);
            assert_eq!(infos[2].config_path, PathBuf::from("/test/config.toml"));
        });
    }
}
