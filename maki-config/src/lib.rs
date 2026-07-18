use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use maki_config_macro::ConfigSection;
use maki_storage::paths;
use maki_storage::sessions::{StoredThinking, ThinkingParseError};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;
use tracing::warn;

const PROJECT_DIR: &str = ".maki";
const PERMISSIONS_FILE: &str = "permissions.toml";

pub mod providers;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2000;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1500;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;

pub const DEFAULT_MAX_CONTINUATION_TURNS: u32 = 3;
pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_LOW_SPEED_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 300;

pub const DEFAULT_MAX_LOG_BYTES_MB: u64 = 200;
pub const DEFAULT_MAX_LOG_FILES: u32 = 10;
pub const DEFAULT_INPUT_HISTORY_SIZE: usize = 100;

pub const MIN_OUTPUT_BYTES: usize = 1024;
pub const MIN_OUTPUT_LINES: usize = 10;
pub const MIN_MAX_CONTINUATION_TURNS: u32 = 1;
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
const MAX_COMPACTION_PERCENT: u8 = 99;
const COMPACTION_BUFFER_EXPECTED: &str =
    r#"a token count (e.g. 12000) or a percent of the context window (e.g. "20%")"#;
pub const MIN_MOUSE_SCROLL_LINES: u32 = 1;
pub const MIN_TOOL_OUTPUT_LINES: usize = 1;
pub const MIN_MAX_LOG_BYTES_MB: u64 = 1;
pub const MIN_MAX_LOG_FILES: u32 = 1;
pub const MIN_INPUT_HISTORY_SIZE: usize = 10;
pub const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
pub const MIN_LOW_SPEED_TIMEOUT_SECS: u64 = 1;
pub const MIN_STREAM_TIMEOUT_SECS: u64 = 10;

pub const DEFAULT_BUILTINS: &[&str] = &[
    "bash",
    "batch",
    "code_execution",
    "edit",
    "glob",
    "grep",
    "index",
    "memory",
    "question",
    "read",
    "sessions",
    "skill",
    "task",
    "todo_write",
    "view_image",
    "webfetch",
    "websearch",
    "write",
];

/// These used to be their own `tools.<name>` tables and are now edit plugin
/// options; the config layer uses this list to reject the old form with a
/// pointer to the new one.
pub const EDIT_SUB_TOOLS: &[&str] = &["edit_lines", "insert_lines", "multiedit"];

pub const FILE_WRITE_TOOLS: &[&str] = &["write", "edit", "multiedit", "edit_lines", "insert_lines"];

#[derive(Debug, Clone, Copy)]
pub enum ConfigValue {
    Bool(bool),
    U64(u64),
    Str(&'static str),
}

impl ConfigValue {
    pub fn format_default(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::U64(v) => v.to_string(),
            Self::Str(s) => (*s).to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigField {
    pub name: &'static str,
    pub ty: &'static str,
    pub default: ConfigValue,
    pub min: Option<u64>,
    pub description: &'static str,
}

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "always_yolo",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply)",
    },
    ConfigField {
        name: "always_fast",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with Anthropic fast mode (Opus only; ignored otherwise)",
    },
    ConfigField {
        name: "always_workflow",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with workflow mode (task callable inside code_execution)",
    },
    ConfigField {
        name: "always_thinking",
        ty: "bool | string",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with extended thinking (true/\"adaptive\", \"off\", an effort level (\"minimal\" to \"max\"), or a token budget)",
    },
];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config: {section}.{field} = {value} is below minimum ({min})")]
    BelowMinimum {
        section: &'static str,
        field: &'static str,
        value: u64,
        min: u64,
    },
    #[error("invalid config: always_thinking: {0}")]
    Thinking(#[from] ThinkingParseError),
    #[error(
        "invalid config: plugins.{tool} was removed; {tool} is provided by the edit plugin, \
         set plugins.edit = {{ {tool} = true|false }} instead"
    )]
    RemovedEditSubTool { tool: &'static str },
    #[error(
        "invalid config: plugins.{plugin}: no bundled plugin is named \"{plugin}\" \
         (bundled plugins: {valid})"
    )]
    UnknownPlugin { plugin: String, valid: String },
    #[error(
        "invalid config: the `tools` table in maki.setup was renamed to `plugins` \
         (plugins can provide more than tools).\n\n\
         Fix your config with:\n\n    \
         sed -i.bak 's/^\\( *\\)tools *=/\\1plugins =/' ~/.config/maki/init.lua\n\n\
         Run it on .maki/init.lua too if you keep a project config. \
         A .bak backup is left next to the file."
    )]
    RenamedToolsTable,
}

fn check(
    section: &'static str,
    field: &'static str,
    value: u64,
    min: u64,
) -> Result<(), ConfigError> {
    if value < min {
        return Err(ConfigError::BelowMinimum {
            section,
            field,
            value,
            min,
        });
    }
    Ok(())
}

macro_rules! merge_option {
    ($self:ident, $overlay:ident, $($field:ident),+) => {
        $(if $overlay.$field.is_some() { $self.$field = $overlay.$field; })+
    };
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum AlwaysThinking {
    Toggle(bool),
    Budget(u32),
    Mode(String),
}

impl AlwaysThinking {
    fn resolve(self) -> Result<StoredThinking, ThinkingParseError> {
        match self {
            Self::Toggle(true) => Ok(StoredThinking::Adaptive),
            Self::Toggle(false) => Ok(StoredThinking::Off),
            Self::Budget(n) => StoredThinking::parse_setting(&n.to_string()),
            Self::Mode(s) => StoredThinking::parse_setting(&s),
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    pub always_yolo: Option<bool>,
    pub always_fast: Option<bool>,
    pub always_workflow: Option<bool>,
    pub always_thinking: Option<AlwaysThinking>,
    #[serde(default)]
    pub ui: UiFileConfig,
    pub agent: AgentFileConfig,
    pub provider: ProviderFileConfig,
    pub storage: StorageFileConfig,
    pub plugins: HashMap<String, PluginFileConfig>,
    /// Renamed to `plugins`; kept so old configs fail with a pointer to the
    /// new name instead of a generic unknown-field error.
    tools: HashMap<String, PluginFileConfig>,
}

impl RawConfig {
    pub fn merge(&mut self, overlay: RawConfig) {
        merge_option!(
            self,
            overlay,
            always_yolo,
            always_fast,
            always_workflow,
            always_thinking
        );
        self.ui.merge(overlay.ui);
        self.agent.merge(overlay.agent);
        self.provider.merge(overlay.provider);
        self.storage.merge(overlay.storage);
        for (name, plugin) in overlay.plugins {
            let entry = self.plugins.entry(name).or_default();
            if plugin.enabled.is_some() {
                entry.enabled = plugin.enabled;
            }
            entry.opts.extend(plugin.opts);
        }
        self.tools.extend(overlay.tools);
    }

    pub fn into_config(self, no_rtk: bool) -> Result<Config, ConfigError> {
        self.validate_plugin_tables()?;
        let disabled_tools: Vec<String> = self
            .plugins
            .iter()
            .filter(|(_, cfg)| cfg.enabled == Some(false))
            .map(|(name, _)| name.clone())
            .collect();
        Ok(Config {
            always_yolo: self.always_yolo.unwrap_or(false),
            always_fast: self.always_fast.unwrap_or(false),
            always_workflow: self.always_workflow.unwrap_or(false),
            always_thinking: self
                .always_thinking
                .map(AlwaysThinking::resolve)
                .transpose()?,
            ui: UiConfig::from_file(self.ui),
            agent: AgentConfig::from_file(self.agent, no_rtk, disabled_tools),
            provider: ProviderConfig::from_file(self.provider),
            storage: StorageConfig::from_file(self.storage),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::from_plugins(self.plugins),
        })
    }

    /// A `plugins.<name>` key that matches no bundled plugin is a typo or an
    /// old config, so fail loudly instead of letting it silently drift.
    fn validate_plugin_tables(&self) -> Result<(), ConfigError> {
        if !self.tools.is_empty() {
            return Err(ConfigError::RenamedToolsTable);
        }
        for &name in EDIT_SUB_TOOLS {
            if self.plugins.contains_key(name) {
                return Err(ConfigError::RemovedEditSubTool { tool: name });
            }
        }
        let mut unknown: Vec<&String> = self
            .plugins
            .keys()
            .filter(|name| !DEFAULT_BUILTINS.contains(&name.as_str()))
            .collect();
        unknown.sort();
        if let Some(&plugin) = unknown.first() {
            return Err(ConfigError::UnknownPlugin {
                plugin: plugin.clone(),
                valid: DEFAULT_BUILTINS.join(", "),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct PluginFileConfig {
    pub enabled: Option<bool>,
    /// Plugin-specific options passed through opaquely; each plugin declares
    /// and validates its own via `maki.api.register_options`.
    #[serde(flatten)]
    pub opts: JsonMap<String, JsonValue>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct UiFileConfig {
    pub splash_animation: Option<bool>,
    pub scrollbar: Option<bool>,
    pub flash_duration_ms: Option<u64>,
    pub typewriter_ms_per_char: Option<u64>,
    pub mouse_scroll_lines: Option<u32>,
    pub show_thinking: Option<bool>,
    pub tool_output_lines: Option<ToolOutputLinesFile>,
}

impl UiFileConfig {
    fn merge(&mut self, overlay: UiFileConfig) {
        merge_option!(
            self,
            overlay,
            splash_animation,
            scrollbar,
            flash_duration_ms,
            typewriter_ms_per_char,
            mouse_scroll_lines,
            show_thinking
        );
        match (self.tool_output_lines.as_mut(), overlay.tool_output_lines) {
            (Some(base), Some(over)) => base.merge(over),
            (None, Some(over)) => self.tool_output_lines = Some(over),
            _ => {}
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ToolOutputLinesFile {
    pub bash: Option<usize>,
    pub code_execution: Option<usize>,
    pub task: Option<usize>,
    pub index: Option<usize>,
    pub grep: Option<usize>,
    pub read: Option<usize>,
    pub write: Option<usize>,
    pub web: Option<usize>,
    pub other: Option<usize>,
}

impl ToolOutputLinesFile {
    fn merge(&mut self, overlay: ToolOutputLinesFile) {
        merge_option!(
            self,
            overlay,
            bash,
            code_execution,
            task,
            index,
            grep,
            read,
            write,
            web,
            other
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBuffer {
    Tokens(u32),
    Percent(u8),
}

impl CompactionBuffer {
    pub fn resolve(self, context_window: u32) -> u32 {
        match self {
            Self::Tokens(n) => n,
            Self::Percent(p) => (u64::from(context_window) * u64::from(p) / 100) as u32,
        }
    }
}

impl Serialize for CompactionBuffer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tokens(n) => s.serialize_u32(*n),
            Self::Percent(p) => s.collect_str(&format_args!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for CompactionBuffer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BufferVisitor;

        impl serde::de::Visitor<'_> for BufferVisitor {
            type Value = CompactionBuffer;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(COMPACTION_BUFFER_EXPECTED)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u32::try_from(v)
                    .ok()
                    .filter(|n| *n >= MIN_COMPACTION_BUFFER)
                    .map(CompactionBuffer::Tokens)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "compaction_buffer must be at least {MIN_COMPACTION_BUFFER} tokens"
                        ))
                    })
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                self.visit_u64(u64::try_from(v).unwrap_or(0))
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                s.strip_suffix('%')
                    .and_then(|n| n.trim().parse::<u8>().ok())
                    .filter(|p| (1..=MAX_COMPACTION_PERCENT).contains(p))
                    .map(CompactionBuffer::Percent)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "invalid compaction_buffer {s:?}: expected {COMPACTION_BUFFER_EXPECTED}"
                        ))
                    })
            }
        }

        d.deserialize_any(BufferVisitor)
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    pub max_output_bytes: Option<usize>,
    pub max_output_lines: Option<usize>,
    pub max_continuation_turns: Option<u32>,
    pub compaction_buffer: Option<CompactionBuffer>,
}

impl AgentFileConfig {
    fn merge(&mut self, overlay: AgentFileConfig) {
        merge_option!(
            self,
            overlay,
            max_output_bytes,
            max_output_lines,
            max_continuation_turns,
            compaction_buffer
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderFileConfig {
    pub default_model: Option<String>,
    pub connect_timeout_secs: Option<u64>,
    pub low_speed_timeout_secs: Option<u64>,
    pub stream_timeout_secs: Option<u64>,
}

impl ProviderFileConfig {
    fn merge(&mut self, overlay: ProviderFileConfig) {
        merge_option!(
            self,
            overlay,
            default_model,
            connect_timeout_secs,
            low_speed_timeout_secs,
            stream_timeout_secs
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct StorageFileConfig {
    pub max_log_bytes_mb: Option<u64>,
    pub max_log_files: Option<u32>,
    pub input_history_size: Option<usize>,
}

impl StorageFileConfig {
    fn merge(&mut self, overlay: StorageFileConfig) {
        merge_option!(
            self,
            overlay,
            max_log_bytes_mb,
            max_log_files,
            input_history_size
        );
    }
}

#[derive(Default)]
struct PermissionsFileConfig {
    default: Option<DefaultEffect>,
    tools: HashMap<String, ToolPermissions>,
    mcp_rules: Vec<PermissionRule>,
    mcp_defaults: HashMap<ToolKey, DefaultEffect>,
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let default = table
            .get("default")
            .and_then(|v| DefaultEffect::deserialize(v.clone()).ok())
            .or_else(|| {
                table
                    .get("allow_all")?
                    .as_bool()?
                    .then_some(DefaultEffect::Allow)
            });

        let mut tools = HashMap::new();
        let mut mcp_rules = Vec::new();
        let mut mcp_defaults = HashMap::new();

        for (k, v) in table.iter() {
            if k.is_empty() || k == "allow_all" || k == "default" {
                continue;
            }
            if k == "mcp" {
                // TOML [mcp.server] creates nested table: mcp → {server → {...}}
                if let Some(mcp_table) = v.as_table() {
                    for (server_name, server_value) in mcp_table {
                        if let Some(server_table) = server_value.as_table() {
                            parse_mcp_server_table(
                                server_name,
                                server_table,
                                &mut mcp_rules,
                                &mut mcp_defaults,
                            );
                        } else {
                            tracing::warn!(
                                server = server_name.as_str(),
                                "[mcp.{server_name}] is not a table — skipping"
                            );
                        }
                    }
                } else {
                    tracing::warn!("[mcp] is not a table (got {}) — skipping", v.type_str());
                }
            } else if let Ok(tp) = v.clone().try_into::<ToolPermissions>() {
                if k.contains('.') {
                    tracing::warn!(
                        key = k.as_str(),
                        "tool section [{k}] contains a dot — did you mean [mcp.{k}]? Skipping."
                    );
                } else {
                    tools.insert(k.clone(), tp);
                }
            }
        }

        Ok(Self {
            default,
            tools,
            mcp_rules,
            mcp_defaults,
        })
    }
}

#[derive(Deserialize)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    Allow,
    Deny,
    #[default]
    Prompt,
}

impl From<Effect> for DefaultEffect {
    fn from(e: Effect) -> Self {
        match e {
            Effect::Allow => DefaultEffect::Allow,
            Effect::Deny => DefaultEffect::Deny,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PermissionTarget {
    Global,
    Project(PathBuf),
}

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolKey {
    Wildcard,
    Native(Arc<str>),
    McpServer { server: Arc<str> },
    McpTool { server: Arc<str>, tool: Arc<str> },
}

/// NOTE: `ToolKey` deliberately does not implement `serde::Deserialize`.
/// Use `ToolKey::parse(&str)` at deserialization boundaries — it performs
/// validation (wire format, server name, length) that a blanket Deserialize
/// would skip. All current deserialization paths go through `parse`.
impl serde::Serialize for ToolKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Check if a name matches the LLM wire format: `^[a-zA-Z0-9_-]{1,64}$`.
/// Tool names with dots, over 64 chars, or special characters are rejected.
pub fn is_valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ToolKey {
    /// Parse a qualified tool name into a `ToolKey`.
    ///
    /// Returns `Err` for malformed input (empty names, empty server/tool parts,
    /// tool names that don't match the wire format `^[a-zA-Z0-9_-]{1,64}$`).
    /// Use this at config/dispatch boundaries where input is untrusted.
    pub fn parse(name: &str) -> Result<Self, ToolKeyParseError> {
        if name.is_empty() {
            return Err(ToolKeyParseError::EmptyName);
        }
        if name == "*" {
            return Ok(Self::Wildcard);
        }
        match name.split_once('.') {
            Some(("", _)) | Some((_, "")) => {
                Err(ToolKeyParseError::MalformedParts(name.to_string()))
            }
            Some((server, "*")) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                Ok(Self::McpServer {
                    server: server.into(),
                })
            }
            Some((server, tool)) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                if !is_valid_wire_name(tool) {
                    return Err(ToolKeyParseError::InvalidToolName(tool.to_string()));
                }
                // Wire format is server__tool — check total length fits LLM API limits
                let wire_len = server.len() + 2 + tool.len();
                if wire_len > 64 {
                    return Err(ToolKeyParseError::WireNameTooLong {
                        server: server.to_string(),
                        tool: tool.to_string(),
                        len: wire_len,
                    });
                }
                Ok(Self::McpTool {
                    server: server.into(),
                    tool: tool.into(),
                })
            }
            None => {
                if !is_valid_wire_name(name) {
                    return Err(ToolKeyParseError::InvalidToolName(name.to_string()));
                }
                Ok(Self::Native(name.into()))
            }
        }
    }

    /// Create a `ToolKey` from a known-valid native tool name.
    ///
    /// # Panics
    ///
    /// Panics if `name` is empty or contains dots. Use `ToolKey::parse` for
    /// untrusted input or MCP tool names.
    pub fn native(name: &str) -> Self {
        match name {
            "*" => Self::Wildcard,
            _ => {
                assert!(!name.is_empty(), "native tool name must not be empty");
                assert!(
                    !name.contains('.'),
                    "native tool name must not contain dots: {name:?} - use ToolKey::parse for MCP tools"
                );
                Self::Native(name.into())
            }
        }
    }

    pub fn is_mcp(&self) -> bool {
        matches!(self, Self::McpServer { .. } | Self::McpTool { .. })
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wildcard => write!(f, "*"),
            Self::Native(name) => write!(f, "{name}"),
            Self::McpServer { server } => write!(f, "{server}.*"),
            Self::McpTool { server, tool } => write!(f, "{server}.{tool}"),
        }
    }
}

/// Error returned when a tool key string fails validation.
#[derive(Debug, thiserror::Error)]
pub enum ToolKeyParseError {
    #[error("tool name is empty")]
    EmptyName,
    #[error("malformed tool key: empty server or tool part in {0:?}")]
    MalformedParts(String),
    #[error("invalid server name {0:?}: must match [a-zA-Z0-9-]{{1,64}}")]
    InvalidServerName(String),
    #[error("invalid tool name {0:?}: must match [a-zA-Z0-9_-]{{1,64}}")]
    InvalidToolName(String),
    #[error("wire name {server}__{tool} is {len} chars, max 64")]
    WireNameTooLong {
        server: String,
        tool: String,
        len: usize,
    },
}

#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub tool: ToolKey,
    pub scope: Option<String>,
    pub effect: Effect,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionsConfig {
    pub default: DefaultEffect,
    pub tool_defaults: HashMap<ToolKey, DefaultEffect>,
    pub rules: Vec<PermissionRule>,
    pub yolo: bool,
}

#[derive(Clone)]
pub struct Config {
    pub always_yolo: bool,
    pub always_fast: bool,
    pub always_workflow: bool,
    pub always_thinking: Option<StoredThinking>,
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub provider: ProviderConfig,
    pub storage: StorageConfig,
    pub permissions: PermissionsConfig,
    pub plugins: PluginsConfig,
}

#[derive(Debug, Clone, Copy, ConfigSection)]
#[config(section = "ui")]
pub struct UiConfig {
    #[config(default = true, desc = "Show splash animation on startup")]
    pub splash_animation: bool,

    #[config(default = true, desc = "Show vertical scrollbar in scrollable areas")]
    pub scrollbar: bool,

    #[config(default = DEFAULT_FLASH_DURATION_MS, desc = "Duration of flash messages (ms)")]
    pub flash_duration_ms: u64,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

    #[config(
        default = true,
        desc = "When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes"
    )]
    pub show_thinking: bool,

    #[config(skip, default = "ToolOutputLines::default()")]
    pub tool_output_lines: ToolOutputLines,
}

impl UiConfig {
    pub fn flash_duration(&self) -> Duration {
        Duration::from_millis(self.flash_duration_ms)
    }

    fn from_file(f: UiFileConfig) -> Self {
        Self {
            splash_animation: f.splash_animation.unwrap_or(true),
            scrollbar: f.scrollbar.unwrap_or(true),
            flash_duration_ms: f.flash_duration_ms.unwrap_or(DEFAULT_FLASH_DURATION_MS),
            typewriter_ms_per_char: f
                .typewriter_ms_per_char
                .unwrap_or(DEFAULT_TYPEWRITER_MS_PER_CHAR),
            mouse_scroll_lines: f.mouse_scroll_lines.unwrap_or(DEFAULT_MOUSE_SCROLL_LINES),
            show_thinking: f.show_thinking.unwrap_or(true),
            tool_output_lines: ToolOutputLines::from_file(f.tool_output_lines),
        }
    }

    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        self.tool_output_lines.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputLines {
    pub bash: usize,
    pub code_execution: usize,
    pub task: usize,
    pub index: usize,
    pub grep: usize,
    pub read: usize,
    pub write: usize,
    pub web: usize,
    pub other: usize,
}

impl ToolOutputLines {
    pub const DEFAULT: Self = Self {
        bash: 5,
        code_execution: 5,
        task: 5,
        index: 3,
        grep: 3,
        read: 3,
        write: 7,
        web: 3,
        other: 3,
    };

    pub const FIELD_DEFAULTS: &[(&'static str, usize)] = &[
        ("bash", Self::DEFAULT.bash),
        ("code_execution", Self::DEFAULT.code_execution),
        ("task", Self::DEFAULT.task),
        ("index", Self::DEFAULT.index),
        ("grep", Self::DEFAULT.grep),
        ("read", Self::DEFAULT.read),
        ("write", Self::DEFAULT.write),
        ("web", Self::DEFAULT.web),
        ("other", Self::DEFAULT.other),
    ];

    fn from_file(f: Option<ToolOutputLinesFile>) -> Self {
        let d = Self::DEFAULT;
        let f = f.unwrap_or_default();
        Self {
            bash: f.bash.unwrap_or(d.bash),
            code_execution: f.code_execution.unwrap_or(d.code_execution),
            task: f.task.unwrap_or(d.task),
            index: f.index.unwrap_or(d.index),
            grep: f.grep.unwrap_or(d.grep),
            read: f.read.unwrap_or(d.read),
            write: f.write.unwrap_or(d.write),
            web: f.web.unwrap_or(d.web),
            other: f.other.unwrap_or(d.other),
        }
    }

    fn fields(&self) -> [(&'static str, usize); 9] {
        [
            ("bash", self.bash),
            ("code_execution", self.code_execution),
            ("task", self.task),
            ("index", self.index),
            ("grep", self.grep),
            ("read", self.read),
            ("write", self.write),
            ("web", self.web),
            ("other", self.other),
        ]
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in self.fields() {
            check(
                "ui.tool_output_lines",
                name,
                value as u64,
                MIN_TOOL_OUTPUT_LINES as u64,
            )?;
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> usize {
        match name {
            "bash" => self.bash,
            "code_execution" => self.code_execution,
            "task" => self.task,
            "index" => self.index,
            "grep" | "glob" => self.grep,
            "read" => self.read,
            "memory" => self.write,
            name if FILE_WRITE_TOOLS.contains(&name) => self.write,
            "webfetch" | "websearch" => self.web,
            _ => self.other,
        }
    }
}

impl Default for ToolOutputLines {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, ConfigSection, Serialize)]
#[config(section = "agent")]
pub struct AgentConfig {
    #[config(default = DEFAULT_MAX_OUTPUT_BYTES, min = MIN_OUTPUT_BYTES, desc = "Max tool output size (bytes)")]
    pub max_output_bytes: usize,

    #[config(default = DEFAULT_MAX_OUTPUT_LINES, min = MIN_OUTPUT_LINES, desc = "Max tool output lines")]
    pub max_output_lines: usize,

    #[config(default = DEFAULT_MAX_CONTINUATION_TURNS, min = MIN_MAX_CONTINUATION_TURNS, desc = "Max automatic continuation turns")]
    pub max_continuation_turns: u32,

    #[config(default = DEFAULT_COMPACTION_BUFFER, ty = "u32 | string", default_doc = "20%", desc = "Context reserved for compaction: token count or percent of the context window (e.g. \"20%\")")]
    pub compaction_buffer: CompactionBuffer,

    #[config(skip, default = false)]
    pub no_rtk: bool,

    #[config(skip, default = "None")]
    pub max_turns: Option<u32>,

    #[config(skip, default = "Vec::new()")]
    pub allowed_tools: Vec<String>,

    #[config(skip, default = "Vec::new()")]
    pub disabled_tools: Vec<String>,
}

impl AgentConfig {
    fn from_file(file: AgentFileConfig, no_rtk: bool, disabled_tools: Vec<String>) -> Self {
        Self {
            no_rtk,
            max_output_bytes: file.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES),
            max_continuation_turns: file
                .max_continuation_turns
                .unwrap_or(DEFAULT_MAX_CONTINUATION_TURNS),
            compaction_buffer: file.compaction_buffer.unwrap_or(DEFAULT_COMPACTION_BUFFER),
            max_turns: None,
            allowed_tools: Vec::new(),
            disabled_tools,
        }
    }
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "provider", fields_only)]
pub struct ProviderConfig {
    #[config(
        ty = "String",
        desc = "Default model identifier (e.g. `anthropic/claude-sonnet-4-6`)"
    )]
    pub default_model: Option<String>,

    #[config(key = "connect_timeout_secs", ty = "u64", default = DEFAULT_CONNECT_TIMEOUT_SECS,
             min = MIN_CONNECT_TIMEOUT_SECS, val = "self.connect_timeout.as_secs()",
             desc = "HTTP connect timeout (seconds)")]
    pub connect_timeout: Duration,

    #[config(key = "low_speed_timeout_secs", ty = "u64", default = DEFAULT_LOW_SPEED_TIMEOUT_SECS,
             min = MIN_LOW_SPEED_TIMEOUT_SECS, val = "self.low_speed_timeout.as_secs()",
             desc = "Low speed timeout (seconds with less than 1 byte received)")]
    pub low_speed_timeout: Duration,

    #[config(key = "stream_timeout_secs", ty = "u64", default = DEFAULT_STREAM_TIMEOUT_SECS,
             min = MIN_STREAM_TIMEOUT_SECS, val = "self.stream_timeout.as_secs()",
             desc = "Streaming response timeout (seconds)")]
    pub stream_timeout: Duration,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            low_speed_timeout: Duration::from_secs(DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            stream_timeout: Duration::from_secs(DEFAULT_STREAM_TIMEOUT_SECS),
        }
    }
}

impl ProviderConfig {
    fn from_file(f: ProviderFileConfig) -> Self {
        Self {
            default_model: f.default_model,
            connect_timeout: Duration::from_secs(
                f.connect_timeout_secs
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            ),
            low_speed_timeout: Duration::from_secs(
                f.low_speed_timeout_secs
                    .unwrap_or(DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            ),
            stream_timeout: Duration::from_secs(
                f.stream_timeout_secs.unwrap_or(DEFAULT_STREAM_TIMEOUT_SECS),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, ConfigSection)]
#[config(section = "storage", fields_only)]
pub struct StorageConfig {
    #[config(key = "max_log_bytes_mb", ty = "u64", default = DEFAULT_MAX_LOG_BYTES_MB,
             min = MIN_MAX_LOG_BYTES_MB, val = "self.max_log_bytes / (1024 * 1024)",
             desc = "Max total log size (MB)")]
    pub max_log_bytes: u64,

    #[config(default = DEFAULT_MAX_LOG_FILES, min = MIN_MAX_LOG_FILES,
             desc = "Max number of log files to keep")]
    pub max_log_files: u32,

    #[config(default = DEFAULT_INPUT_HISTORY_SIZE, min = MIN_INPUT_HISTORY_SIZE,
             desc = "Number of input history entries to retain")]
    pub input_history_size: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_log_bytes: DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024,
            max_log_files: DEFAULT_MAX_LOG_FILES,
            input_history_size: DEFAULT_INPUT_HISTORY_SIZE,
        }
    }
}

impl StorageConfig {
    fn from_file(f: StorageFileConfig) -> Self {
        Self {
            max_log_bytes: f.max_log_bytes_mb.unwrap_or(DEFAULT_MAX_LOG_BYTES_MB) * 1024 * 1024,
            max_log_files: f.max_log_files.unwrap_or(DEFAULT_MAX_LOG_FILES),
            input_history_size: f.input_history_size.unwrap_or(DEFAULT_INPUT_HISTORY_SIZE),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub names: Vec<String>,
    /// Per-plugin option tables, without `enabled`. Each plugin validates its
    /// own via `maki.api.register_options` at load time.
    pub opts: HashMap<String, JsonMap<String, JsonValue>>,
}

impl PluginsConfig {
    pub fn from_plugins(plugins: HashMap<String, PluginFileConfig>) -> Self {
        let mut all: Vec<String> = DEFAULT_BUILTINS
            .iter()
            .filter(|name| plugins.get(**name).and_then(|t| t.enabled).unwrap_or(true))
            .map(|s| s.to_string())
            .collect();

        let mut extra: Vec<&String> = plugins
            .iter()
            .filter(|(name, cfg)| {
                !DEFAULT_BUILTINS.contains(&name.as_str()) && cfg.enabled.unwrap_or(false)
            })
            .map(|(name, _)| name)
            .collect();
        extra.sort();
        all.extend(extra.into_iter().cloned());

        let opts = plugins
            .iter()
            .filter(|(_, cfg)| !cfg.opts.is_empty())
            .map(|(name, cfg)| (name.clone(), cfg.opts.clone()))
            .collect();

        Self {
            enabled: true,
            names: all,
            opts,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.ui.validate_all()?;
        self.agent.validate()?;
        self.provider.validate()?;
        self.storage.validate()?;
        Ok(())
    }
}

fn push_rules(
    rules: &mut Vec<PermissionRule>,
    tools: &HashMap<String, ToolPermissions>,
    effect: Effect,
) {
    for (tool, perms) in tools {
        let scope_set = match effect {
            Effect::Deny => &perms.deny,
            Effect::Allow => &perms.allow,
        };
        let Some(scope_set) = scope_set else {
            continue;
        };
        match scope_set {
            ScopeSet::All(true) => rules.push(PermissionRule {
                tool: ToolKey::native(tool),
                scope: None,
                effect,
            }),
            ScopeSet::Scopes(scopes) => {
                for s in scopes {
                    rules.push(PermissionRule {
                        tool: ToolKey::native(tool),
                        scope: Some(s.clone()),
                        effect,
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
}

pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Validates the *tool* portion of an MCP qualified name.
/// Currently identical to `is_valid_wire_name`, but kept distinct
/// in case MCP tools need different constraints from native wire names.
fn is_valid_tool_name(name: &str) -> bool {
    is_valid_wire_name(name)
}

fn push_mcp_tool_rule(
    rules: &mut Vec<PermissionRule>,
    server_name: &str,
    tool_name: &str,
    effect: Effect,
) {
    let qualified = format!("{server_name}.{tool_name}");
    match ToolKey::parse(&qualified) {
        Ok(key) => {
            rules.push(PermissionRule {
                tool: key,
                scope: None,
                effect,
            });
        }
        Err(e) => {
            tracing::warn!(
                server = server_name,
                tool = tool_name,
                error = %e,
                "skipping invalid MCP tool name"
            );
        }
    }
}

fn child_table<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, String> {
    table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("[{key}] is not a table"))
}

fn push_unique(table: &mut toml_edit::Table, key: &str, value: &str) -> Result<(), String> {
    let arr = table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())))
        .as_array_mut()
        .ok_or_else(|| format!("{key} is not an array"))?;
    if !arr.iter().any(|v| v.as_str() == Some(value)) {
        arr.push(value);
        arr.set_trailing("\n");
        arr.set_trailing_comma(true);
        for item in arr.iter_mut() {
            item.decor_mut().set_prefix("\n    ");
        }
    }
    Ok(())
}

fn parse_mcp_server_table(
    server_name: &str,
    table: &toml::Table,
    rules: &mut Vec<PermissionRule>,
    mcp_defaults: &mut HashMap<ToolKey, DefaultEffect>,
) {
    if !is_valid_server_name(server_name) {
        tracing::warn!(
            server = server_name,
            "skipping [mcp.{server_name}] — invalid server name; \
             must contain only alphanumeric characters and hyphens"
        );
        return;
    }

    for (key, value) in table {
        match key.as_str() {
            "allow" | "deny" => {
                let effect = if key == "allow" {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                match value {
                    toml::Value::Array(arr) => {
                        for item in arr {
                            if let Some(tool_name) = item.as_str() {
                                if tool_name == "*" {
                                    // `allow = ["*"]` / `deny = ["*"]` means server-wide.
                                    // Create an McpServer rule so deny-wins logic applies:
                                    // McpServer deny blocks all tools on the server.
                                    // No allow can override a deny — any deny wins.
                                    rules.push(PermissionRule {
                                        tool: ToolKey::McpServer {
                                            server: server_name.into(),
                                        },
                                        scope: None,
                                        effect,
                                    });
                                    continue;
                                }
                                push_mcp_tool_rule(rules, server_name, tool_name, effect);
                            }
                        }
                    }
                    toml::Value::Boolean(true) => {
                        tracing::warn!(
                            server = server_name,
                            key = key.as_str(),
                            "{key} = true is deprecated — use default = \"{key}\" instead; ignoring"
                        );
                    }
                    toml::Value::Boolean(false) => {
                        // No-op: explicitly disabled.
                    }
                    toml::Value::String(s) => {
                        let tool_name = s.as_str();
                        if tool_name == "*" {
                            // Treat `allow = "*"` the same as `allow = ["*"]` —
                            // create a hard McpServer rule, not a default.
                            rules.push(PermissionRule {
                                tool: ToolKey::McpServer {
                                    server: server_name.into(),
                                },
                                scope: None,
                                effect,
                            });
                        } else {
                            tracing::info!(
                                server = server_name,
                                tool = tool_name,
                                "{key} = \"{tool_name}\" coerced to {key} = [\"{tool_name}\"] — \
                                 consider using array syntax"
                            );
                            push_mcp_tool_rule(rules, server_name, tool_name, effect);
                        }
                    }
                    other => {
                        tracing::warn!(
                            server = server_name,
                            key = key.as_str(),
                            value = ?other,
                            "unexpected value for [mcp.{server_name}].{key} — \
                             expected array of tool names or default = \"allow\"/\"deny\""
                        );
                    }
                }
            }
            "default" => {
                if let Ok(d) = value.clone().try_into::<DefaultEffect>() {
                    mcp_defaults.insert(
                        ToolKey::McpServer {
                            server: server_name.into(),
                        },
                        d,
                    );
                } else {
                    tracing::warn!(
                        server = server_name,
                        value = ?value,
                        "invalid [mcp.{server_name}].default value — expected \"allow\", \"deny\", or \"prompt\""
                    );
                }
            }
            other => {
                if value.is_table() {
                    tracing::warn!(
                        server = server_name,
                        key = other,
                        "unknown key [mcp.{server_name}.{other}] — server names cannot \
                         contain dots; use [mcp.{other}] instead if this is a server name"
                    );
                } else {
                    tracing::warn!(
                        server = server_name,
                        key = other,
                        "unknown key in [mcp.{server_name}] — ignored"
                    );
                }
            }
        }
    }
}

fn build_permissions(
    global: PermissionsFileConfig,
    project: PermissionsFileConfig,
) -> PermissionsConfig {
    let global_default = global.default.unwrap_or(DefaultEffect::Prompt);
    let default = match project.default {
        Some(DefaultEffect::Allow) => global_default,
        Some(d) => d,
        None => global_default,
    };

    let mut tool_defaults = HashMap::new();
    for (tool, perms) in &global.tools {
        if let Some(d) = perms.default {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring [\"*\"].default — use the top-level `default` field instead \
                     for global fallback behavior"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &global.mcp_defaults {
        tool_defaults.insert(key.clone(), *d);
    }
    for (tool, perms) in &project.tools {
        if let Some(d) = perms.default
            && d != DefaultEffect::Allow
        {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring project [\"*\"].default — use the top-level `default` field instead"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &project.mcp_defaults {
        if *d != DefaultEffect::Allow {
            tool_defaults.insert(key.clone(), *d);
        }
    }

    let mut rules = Vec::new();
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    for tools in [&global.tools, &project.tools] {
        push_rules(&mut rules, tools, Effect::Deny);
        push_rules(&mut rules, tools, Effect::Allow);
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    PermissionsConfig {
        default,
        tool_defaults,
        rules,
        yolo: false,
    }
}

fn global_dir() -> Option<PathBuf> {
    paths::config_dir().ok()
}

fn config_search_dirs(global: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = global {
        dirs.push(d.to_path_buf());
    }
    if let Ok(xdg) = paths::xdg_config_dir()
        && dirs.first() != Some(&xdg)
    {
        dirs.push(xdg);
    }
    dirs
}

fn load_env_files_with_global(cwd: &Path, global: Option<&Path>) {
    let mut vars = HashMap::new();
    if let Some(path) = global {
        collect_env_vars(&path.join(".env"), &mut vars);
    }
    collect_env_vars(&cwd.join(PROJECT_DIR).join(".env"), &mut vars);

    for (key, value) in vars {
        if std::env::var_os(&key).is_none() {
            // SAFETY: single-threaded at startup, before any async runtime
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

fn collect_env_vars(path: &Path, vars: &mut HashMap<String, String>) {
    let Ok(iter) = dotenvy::from_path_iter(path) else {
        return;
    };
    for item in iter.flatten() {
        vars.insert(item.0, item.1);
    }
}

pub fn load_env_files(cwd: &Path) {
    load_env_files_with_global(cwd, global_dir().as_deref());
}

pub fn load_permissions(cwd: &Path) -> PermissionsConfig {
    let global_dirs = config_search_dirs(global_dir().as_deref());
    load_permissions_inner(cwd, &global_dirs)
}

fn load_permissions_inner(cwd: &Path, global_dirs: &[PathBuf]) -> PermissionsConfig {
    let mut global_perms = PermissionsFileConfig::default();
    for dir in global_dirs {
        if let Some(p) = read_permissions_file(&dir.join(PERMISSIONS_FILE)) {
            global_perms = p;
        }
    }

    let project_perms =
        read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap_or_default();

    build_permissions(global_perms, project_perms)
}

fn migrate_mcp_entry(
    doc: &mut toml_edit::DocumentMut,
    server_name: &str,
    tool_name: &str,
    item: &toml_edit::Item,
) {
    // Old format: ["mcp:server__tool"] with booleans or scope-string arrays.
    // New format: [mcp.server] allow = ["tool_name"]. Old scope strings were
    // dead code (MCP scopes are always wildcarded), so only the effect survives.
    let mut push = |effect_key: &str| {
        let res = child_table(doc.as_table_mut(), "mcp")
            .and_then(|mcp| child_table(mcp, server_name))
            .and_then(|server| push_unique(server, effect_key, tool_name));
        if let Err(e) = res {
            warn!(
                server = server_name,
                tool = tool_name,
                error = %e,
                "skipping MCP entry migration"
            );
        }
    };

    // Bare boolean: old format like [mcp]\ndeepwiki__search = true
    // means "allow this tool".
    if let Some(b) = item.as_bool() {
        if b {
            push("allow");
        }
        return;
    }

    if let Some(old_table) = item.as_table() {
        for (key, value) in old_table.iter() {
            match key {
                "allow" | "deny" => {
                    if value.as_bool() == Some(true) || value.as_array().is_some() {
                        push(key);
                    }
                }
                _ => {
                    warn!(
                        key,
                        server = server_name,
                        tool = tool_name,
                        "dropping unknown key in old MCP entry during migration"
                    );
                }
            }
        }
    }
}

/// Migrates old permission formats and returns the (possibly rewritten)
/// file content. The rewrite to disk is best-effort: loading uses the
/// migrated content even when the write fails.
fn migrate_permissions_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let Ok(mut doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Some(content);
    };
    let mut migrated = false;

    if let Some(item) = doc.remove("allow_all") {
        migrated = true;
        if item.as_bool() == Some(true) {
            doc.insert("default", toml_edit::value("allow"));
        }
    }

    // Migrate flat MCP keys: "mcp:server__tool" → [mcp.server]
    // Two TOML representations to handle:
    // 1. Quoted keys: ["mcp:server__tool"] → flat top-level key
    // 2. Bare keys: [mcp:server__tool] → nested "mcp" → {"server__tool": ...}

    // Path 1: Flat quoted keys starting with "mcp:" containing "__"
    let flat_old_keys: Vec<String> = doc
        .iter()
        .filter_map(|(k, _)| {
            k.strip_prefix("mcp:")
                .and_then(|rest| rest.contains("__").then(|| k.to_string()))
        })
        .collect();

    for old_key in flat_old_keys {
        if let Some(item) = doc.remove(&old_key) {
            let rest = &old_key[4..]; // strip "mcp:"
            if let Some((server, tool)) = rest.split_once("__") {
                if !is_valid_server_name(server) || !is_valid_tool_name(tool) {
                    tracing::error!(
                        key = old_key.as_str(),
                        server = server,
                        tool = tool,
                        "SECURITY: skipping migration of malformed MCP key — \
                         rules for this tool will not be restored"
                    );
                    continue;
                }
                migrate_mcp_entry(&mut doc, server, tool, &item);
                migrated = true;
            }
        }
    }

    // Path 2: Nested "mcp" sub-table (bare key mcp: created nesting)
    let nested_old_entries: Vec<(String, String, toml_edit::Item)> = {
        let mut entries = Vec::new();
        if let Some(toml_edit::Item::Table(mcp_table)) = doc.get("mcp") {
            for (key, _) in mcp_table.iter() {
                if key.contains("__")
                    && let Some((server, tool)) = key.split_once("__")
                {
                    let item = mcp_table.get(key).cloned();
                    if let Some(item) = item {
                        entries.push((server.to_string(), tool.to_string(), item));
                    }
                }
            }
        }
        entries
    };

    for (server_name, tool_name, item) in nested_old_entries {
        if !is_valid_server_name(&server_name) || !is_valid_tool_name(&tool_name) {
            tracing::error!(
                server = server_name.as_str(),
                tool = &*tool_name,
                "SECURITY: skipping migration of malformed nested MCP key — \
                 rules for this tool will not be restored"
            );
            continue;
        }
        if let Some(toml_edit::Item::Table(mcp_table)) = doc.get_mut("mcp") {
            mcp_table.remove(&format!("{server_name}__{tool_name}"));
        }
        migrate_mcp_entry(&mut doc, &server_name, &tool_name, &item);
        migrated = true;
    }

    // Clean up the now-empty "mcp" parent table if it has no children
    if let Some(toml_edit::Item::Table(mcp_table)) = doc.get("mcp")
        && mcp_table.is_empty()
    {
        doc.remove("mcp");
    }

    if !migrated {
        return Some(content);
    }
    let new_content = doc.to_string();
    if let Err(e) = maki_storage::atomic_write(path, new_content.as_bytes()) {
        warn!(path = %path.display(), error = %e, "failed to persist migrated permissions file");
    }
    Some(new_content)
}

fn read_permissions_file(path: &Path) -> Option<PermissionsFileConfig> {
    let content = migrate_permissions_file(path)?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse permissions");
            None
        }
    }
}

pub fn global_config_dir() -> Option<PathBuf> {
    global_dir()
}

pub fn global_config_dirs() -> Vec<PathBuf> {
    config_search_dirs(global_dir().as_deref())
}

pub fn append_permission_rule(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
) -> Result<(), String> {
    let dir = config_search_dirs(global_dir().as_deref())
        .into_iter()
        .last();
    append_permission_rule_with_global(tool, scope, effect, target, dir)
}

fn append_permission_rule_with_global(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
    global: Option<PathBuf>,
) -> Result<(), String> {
    match target {
        PermissionTarget::Global => append_global_permission(tool, scope, effect, global),
        PermissionTarget::Project(cwd) => append_project_permission(tool, scope, effect, cwd),
    }
}

fn append_global_permission(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    global: Option<PathBuf>,
) -> Result<(), String> {
    let path = global
        .ok_or_else(|| "cannot determine home directory".to_string())?
        .join(PERMISSIONS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("failed to parse permissions: {e}"))?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create config dir: {e}"))?;
    }
    maki_storage::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("cannot write permissions: {e}"))?;
    Ok(())
}

fn append_project_permission(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    cwd: &Path,
) -> Result<(), String> {
    let path = cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("failed to parse .maki/{PERMISSIONS_FILE}: {e}"))?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create .maki dir: {e}"))?;
    }
    maki_storage::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("cannot write .maki/{PERMISSIONS_FILE}: {e}"))?;
    Ok(())
}

fn insert_permission_entry(
    doc: &mut toml_edit::DocumentMut,
    tool_key: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
) -> Result<(), String> {
    let key = match effect {
        Effect::Allow => "allow",
        Effect::Deny => "deny",
    };

    match tool_key {
        // MCP scopes are always wildcarded, so `scope` is ignored for MCP keys.
        ToolKey::McpTool { server, tool } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            push_unique(server_table, key, tool)?;
        }
        ToolKey::McpServer { server } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            server_table.insert("default", toml_edit::value(key));
        }
        ToolKey::Wildcard => {
            // Wildcard rules are config-only; runtime never writes them.
            return Err("cannot write wildcard permission rule to config".to_string());
        }
        ToolKey::Native(name) => {
            let tool_table = child_table(doc.as_table_mut(), name)?;
            match scope {
                Some(s) => push_unique(tool_table, key, s)?,
                None => {
                    tool_table.insert(key, toml_edit::value(true));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_storage::sessions::Effort;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    fn plugin_enabled(enabled: bool) -> PluginFileConfig {
        PluginFileConfig {
            enabled: Some(enabled),
            opts: JsonMap::new(),
        }
    }

    fn write_global_permissions(dir: &Path, content: &str) {
        let perms_dir = dir.join(".config/maki");
        fs::create_dir_all(&perms_dir).unwrap();
        fs::write(perms_dir.join("permissions.toml"), content).unwrap();
    }

    fn global_config_dir(dir: &Path) -> PathBuf {
        dir.join(".config/maki")
    }

    #[test_case("12000", CompactionBuffer::Tokens(12_000) ; "tokens_number")]
    #[test_case("\"20%\"", CompactionBuffer::Percent(20) ; "percent_string")]
    #[test_case("\" 5 %\"", CompactionBuffer::Percent(5) ; "percent_with_spaces")]
    fn compaction_buffer_deserializes(json: &str, expected: CompactionBuffer) {
        let parsed: CompactionBuffer = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test_case("500" ; "tokens_below_min")]
    #[test_case("-1" ; "negative_tokens")]
    #[test_case("\"0%\"" ; "zero_percent")]
    #[test_case("\"100%\"" ; "percent_too_high")]
    #[test_case("\"abc%\"" ; "non_numeric_percent")]
    fn compaction_buffer_rejects(json: &str) {
        assert!(serde_json::from_str::<CompactionBuffer>(json).is_err());
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 64_000, 10_000 ; "tokens_ignore_window")]
    #[test_case(CompactionBuffer::Percent(20), 64_000, 12_800 ; "percent_of_window")]
    fn compaction_buffer_resolves(buffer: CompactionBuffer, window: u32, expected: u32) {
        assert_eq!(buffer.resolve(window), expected);
    }

    #[test]
    fn compaction_buffer_serializes_percent_as_string() {
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Percent(20)).unwrap(),
            serde_json::json!("20%")
        );
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Tokens(9_000)).unwrap(),
            serde_json::json!(9_000)
        );
    }

    #[test]
    fn empty_config_returns_defaults() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(config.ui.splash_animation);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(
            config.provider.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
        assert_eq!(
            config.storage.max_log_bytes,
            DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024
        );
    }

    #[test]
    fn partial_agent_config_preserves_unset_fields() {
        let raw = RawConfig {
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn merge_overlay_wins_field_by_field() {
        let mut base = RawConfig {
            always_yolo: Some(false),
            ui: UiFileConfig {
                splash_animation: Some(false),
                flash_duration_ms: Some(2000),
                ..Default::default()
            },
            agent: AgentFileConfig {
                max_output_lines: Some(3000),
                max_output_bytes: Some(80_000),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            always_yolo: Some(true),
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_yolo, Some(true), "overlay wins");
        assert_eq!(base.agent.max_output_lines, Some(5000), "overlay wins");
        assert_eq!(base.agent.max_output_bytes, Some(80_000), "base preserved");
        assert_eq!(base.ui.splash_animation, Some(false), "base preserved");
        assert_eq!(base.ui.flash_duration_ms, Some(2000), "base preserved");
    }

    #[test]
    fn merge_always_flags_overlay_wins() {
        let mut base = RawConfig {
            always_fast: Some(false),
            always_workflow: Some(false),
            always_thinking: Some(AlwaysThinking::Mode("off".into())),
            ..Default::default()
        };
        let overlay = RawConfig {
            always_fast: Some(true),
            always_workflow: Some(true),
            always_thinking: Some(AlwaysThinking::Toggle(true)),
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_fast, Some(true), "overlay wins");
        assert_eq!(base.always_workflow, Some(true), "overlay wins");
        assert_eq!(
            base.always_thinking,
            Some(AlwaysThinking::Toggle(true)),
            "overlay wins"
        );
    }

    #[test]
    fn always_workflow_resolves_default_and_set() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        assert!(!defaults.always_workflow, "absent resolves to false");

        let raw = RawConfig {
            always_workflow: Some(true),
            ..Default::default()
        };
        assert!(raw.into_config(false).unwrap().always_workflow);
    }

    #[test_case(AlwaysThinking::Toggle(true), StoredThinking::Adaptive ; "toggle_true")]
    #[test_case(AlwaysThinking::Toggle(false), StoredThinking::Off ; "toggle_false")]
    #[test_case(AlwaysThinking::Budget(8192), StoredThinking::Budget { tokens: 8192 } ; "budget_number")]
    #[test_case(AlwaysThinking::Mode("xhigh".into()), StoredThinking::Effort { level: Effort::XHigh } ; "effort_xhigh")]
    #[test_case(AlwaysThinking::Mode("minimal".into()), StoredThinking::Effort { level: Effort::Minimal } ; "effort_minimal")]
    fn always_thinking_toggle_resolve(input: AlwaysThinking, expected: StoredThinking) {
        assert_eq!(input.resolve(), Ok(expected));
    }

    #[test]
    fn into_config_resolves_always_thinking() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        assert!(defaults.always_thinking.is_none());

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("8192".into())),
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.always_thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("fast".into())),
            ..Default::default()
        };
        let err = raw.into_config(false).err().expect("expected config error");
        assert!(matches!(err, ConfigError::Thinking(_)));
    }

    #[test_case("max_output_bytes",  0 ; "zero_output_bytes")]
    #[test_case("max_output_lines",  0 ; "zero_output_lines")]
    #[test_case("max_output_bytes",  500 ; "below_min_output_bytes")]
    fn validate_rejects_invalid_agent(field: &str, value: usize) {
        let mut config = AgentConfig::default();
        match field {
            "max_output_bytes" => config.max_output_bytes = value,
            "max_output_lines" => config.max_output_lines = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::BelowMinimum { field: f, .. } if f == field));
    }

    #[test]
    fn tool_output_lines_per_tool_override() {
        let raw = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(20),
                    read: Some(20),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.tool_output_lines.bash, 20);
        assert_eq!(config.ui.tool_output_lines.read, 20);
        assert_eq!(
            config.ui.tool_output_lines.index,
            ToolOutputLines::DEFAULT.index
        );
    }

    #[test_case("provider", "connect_timeout_secs", 0 ; "provider_zero_connect_timeout")]
    #[test_case("storage",  "max_log_files",        0 ; "storage_zero_log_files")]
    #[test_case("ui",       "mouse_scroll_lines",   0 ; "ui_zero_scroll_lines")]
    #[test_case("agent",    "max_output_lines",     1 ; "agent_output_lines_too_low")]
    fn validate_rejects_invalid_sections(section: &str, field: &str, value: u64) {
        let mut config = Config {
            always_yolo: false,
            always_fast: false,
            always_workflow: false,
            always_thinking: None,
            ui: UiConfig::default(),
            agent: AgentConfig::default(),
            provider: ProviderConfig::default(),
            storage: StorageConfig::default(),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::default(),
        };
        match (section, field) {
            ("provider", "connect_timeout_secs") => {
                config.provider.connect_timeout = Duration::from_secs(value)
            }
            ("storage", "max_log_files") => config.storage.max_log_files = value as u32,
            ("ui", "mouse_scroll_lines") => config.ui.mouse_scroll_lines = value as u32,
            ("agent", "max_output_lines") => config.agent.max_output_lines = value as usize,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::BelowMinimum { section: s, field: f, .. } if s == section && f == field
        ));
    }

    #[test]
    fn permissions_loaded_from_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"allow\"\n\n\
             [bash]\nallow = [\n    \"cargo *\",\n]\ndeny = [\n    \"rm -rf *\",\n]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Allow);
        assert_eq!(perms.rules.len(), 2);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[0].tool, ToolKey::native("bash"));
        assert_eq!(perms.rules[0].scope.as_deref(), Some("rm -rf *"));
        assert_eq!(perms.rules[1].effect, Effect::Allow);
        assert_eq!(perms.rules[1].tool, ToolKey::native("bash"));
        assert_eq!(perms.rules[1].scope.as_deref(), Some("cargo *"));
    }

    #[test]
    fn permissions_merge_global_and_project() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm -rf *\"]\n",
        );
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("permissions.toml"),
            "[read]\nallow = true\n\
             [write]\ndeny = [\"/etc/*\"]\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 4);

        let deny_rules: Vec<_> = perms
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Deny)
            .collect();
        let allow_rules: Vec<_> = perms
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Allow)
            .collect();

        assert_eq!(deny_rules.len(), 2);
        assert_eq!(deny_rules[0].tool, ToolKey::native("bash"));
        assert_eq!(deny_rules[1].tool, ToolKey::native("write"));

        assert_eq!(allow_rules.len(), 2);
        assert_eq!(allow_rules[0].tool, ToolKey::native("bash"));
        assert_eq!(allow_rules[1].tool, ToolKey::native("read"));
    }

    #[test]
    fn project_default_allow_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(maki_dir.join("permissions.toml"), "default = \"allow\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);
    }

    #[test]
    fn append_permission_rule_writes_to_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("rm -rf *"),
            Effect::Deny,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[bash]"));
        assert!(content.contains("cargo *"));
        assert!(content.contains("rm -rf *"));
        assert!(!content.contains("[permissions]"));
    }

    #[test]
    fn append_permission_rule_writes_mcp_nested_form() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::parse("deepwiki.search").unwrap(),
            Some("*"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "nested table present");
        assert!(content.contains("\"search\""), "tool name in array");
        assert!(!content.contains("deepwiki.search"), "no flat key");
        assert!(!content.contains("__"), "no __ separator");
    }

    #[test]
    fn no_permissions_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert!(perms.rules.is_empty());
    }

    #[test]
    fn deny_rules_before_allow_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[1].effect, Effect::Allow);
    }

    #[test]
    fn permissions_default_deny_global() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "default = \"deny\"\n");

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn permissions_default_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[bash]\ndefault = \"allow\"\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("bash")).copied(),
            Some(DefaultEffect::Allow)
        );
    }

    #[test]
    fn permissions_default_merge_project_overrides_global_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[bash]\ndefault = \"allow\"\n");
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("permissions.toml"),
            "[bash]\ndefault = \"deny\"\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("bash")).copied(),
            Some(DefaultEffect::Deny)
        );
    }

    #[test]
    fn permissions_allow_all_migrated() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "allow_all = true\n\n[bash]\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Allow);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(!content.contains("allow_all"));
        assert!(content.contains("default = \"allow\""));
    }

    #[test]
    fn permissions_allow_all_false_migrated_removed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "allow_all = false\n");

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(!content.contains("allow_all"));
        assert!(!content.contains("default"));
    }

    #[test]
    fn project_default_deny_allowed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(maki_dir.join("permissions.toml"), "default = \"deny\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn append_permission_rule_deduplicates() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert_eq!(content.matches("cargo *").count(), 1);
    }

    #[test]
    fn env_file_precedence() {
        const GLOBAL_ONLY: &str = "TEST_MAKI_GLOBAL_ONLY";
        const PROJECT_SHADOWS: &str = "TEST_MAKI_PROJECT_SHADOWS";
        const PROCESS_WINS: &str = "TEST_MAKI_PROCESS_WINS";

        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join(".env"),
            format!("{GLOBAL_ONLY}=global\n{PROJECT_SHADOWS}=global\n{PROCESS_WINS}=global"),
        )
        .unwrap();

        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join(".env"),
            format!("{PROJECT_SHADOWS}=project\n{PROCESS_WINS}=project"),
        )
        .unwrap();

        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::set_var(PROCESS_WINS, "process");
        }

        load_env_files_with_global(dir.path(), Some(&global));

        assert_eq!(std::env::var(GLOBAL_ONLY).unwrap(), "global");
        assert_eq!(std::env::var(PROJECT_SHADOWS).unwrap(), "project");
        assert_eq!(std::env::var(PROCESS_WINS).unwrap(), "process");

        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::remove_var(PROCESS_WINS);
        }
    }

    #[test]
    fn merge_plugins_overlay_wins_per_key() {
        let mut base: RawConfig = toml::from_str(
            "[plugins.index]\nenabled = true\n\
             [plugins.websearch]\nenabled = true\n\
             [plugins.grep]\nenabled = true\nsearch_result_limit = 200\nmax_line_bytes = 900\n",
        )
        .unwrap();
        let overlay: RawConfig = toml::from_str(
            "[plugins.websearch]\nenabled = false\n\
             [plugins.alpha_tool]\nenabled = true\n\
             [plugins.grep]\nsearch_result_limit = 50\n",
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(
            base.plugins["index"].enabled,
            Some(true),
            "base-only key preserved"
        );
        assert_eq!(
            base.plugins["websearch"].enabled,
            Some(false),
            "overlay replaces"
        );
        assert_eq!(
            base.plugins["alpha_tool"].enabled,
            Some(true),
            "overlay-only key added"
        );
        let grep = &base.plugins["grep"];
        assert_eq!(
            grep.enabled,
            Some(true),
            "enabled preserved when overlay omits it"
        );
        assert_eq!(
            grep.opts["search_result_limit"],
            serde_json::json!(50),
            "overlay opt wins"
        );
        assert_eq!(
            grep.opts["max_line_bytes"],
            serde_json::json!(900),
            "base opt preserved"
        );
    }

    #[test]
    fn show_thinking_deserializes_true() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = true\n").unwrap();
        assert!(raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_deserializes_false() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = false\n").unwrap();
        assert!(!raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_missing_defaults_true() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.ui.show_thinking);
    }

    #[test_case("[ui]\nsplash_animaton = true\n" ; "top_level_typo")]
    #[test_case("agent = { bash_timeout_secs = 60 }\n" ; "moved_bash_timeout")]
    #[test_case("agent = { search_result_limit = 50 }\n" ; "moved_search_limit")]
    #[test_case("[index]\nmax_file_size_mb = 4\n" ; "removed_index_section")]
    fn deny_unknown_fields_rejects(toml_str: &str) {
        let result: Result<RawConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown field should be rejected: {toml_str}"
        );
    }

    #[test]
    fn deny_unknown_fields_accepts_valid_plugins() {
        const VALID: &str =
            "[plugins.bash]\nenabled = true\n[plugins.websearch]\nenabled = false\n";
        let result: Result<RawConfig, _> = toml::from_str(VALID);
        assert!(
            result.is_ok(),
            "valid plugins section should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn plugin_extra_keys_parse_into_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = true\ntimeout_secs = 180\n").unwrap();
        let bash = &raw.plugins["bash"];
        assert_eq!(bash.enabled, Some(true));
        assert_eq!(bash.opts["timeout_secs"], serde_json::json!(180));
    }

    #[test]
    fn into_config_wires_plugin_names_and_opts() {
        let raw: RawConfig = toml::from_str(
            "[plugins.bash]\ntimeout_secs = 180\n[plugins.websearch]\nenabled = false\n",
        )
        .unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.plugins.names.contains(&"bash".to_string()));
        assert!(!config.plugins.names.contains(&"websearch".to_string()));
        assert!(
            config.plugins.names.contains(&"index".to_string()),
            "untouched builtin stays"
        );
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180)
        );
        assert!(
            !config.plugins.opts.contains_key("websearch"),
            "enabled-only tables produce no opts"
        );
    }

    #[test]
    fn from_plugins_default() {
        let plugins = PluginsConfig::from_plugins(HashMap::new());
        let expected: Vec<String> = DEFAULT_BUILTINS.iter().map(|s| s.to_string()).collect();
        assert_eq!(plugins.names, expected);
        assert!(plugins.enabled);
    }

    #[test]
    fn from_plugins_enable_disable_and_sort() {
        let mut entries = HashMap::new();
        entries.insert("websearch".to_string(), plugin_enabled(false));
        entries.insert("zeta".to_string(), plugin_enabled(true));
        entries.insert("alpha".to_string(), plugin_enabled(true));
        entries.insert("custom_tool".to_string(), PluginFileConfig::default());

        let plugins = PluginsConfig::from_plugins(entries);
        assert!(
            !plugins.names.contains(&"websearch".to_string()),
            "disabled builtin removed"
        );
        assert!(
            plugins.names.contains(&"index".to_string()),
            "untouched builtin stays"
        );
        assert!(
            plugins.names.contains(&"bash".to_string()),
            "bash is a default builtin"
        );
        assert!(
            !plugins.names.contains(&"custom_tool".to_string()),
            "enabled=None non-default ignored"
        );

        let extras: Vec<_> = plugins
            .names
            .iter()
            .filter(|t| !DEFAULT_BUILTINS.contains(&t.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            extras,
            vec!["alpha", "zeta"],
            "extras sorted alphabetically"
        );
    }

    #[test]
    fn merge_tool_output_lines_field_level_overlay() {
        let mut base = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(50),
                    read: Some(30),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(100),
                    grep: Some(15),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);
        let tol = base.ui.tool_output_lines.as_ref().unwrap();
        assert_eq!(tol.bash, Some(100), "overlay wins");
        assert_eq!(tol.read, Some(30), "base preserved");
        assert_eq!(tol.grep, Some(15), "overlay added");
    }

    #[test]
    fn default_builtins_sorted() {
        for pair in DEFAULT_BUILTINS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "DEFAULT_BUILTINS not sorted: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn removed_sub_tool_tables_error() {
        for &tool in EDIT_SUB_TOOLS {
            let raw: RawConfig = toml::from_str(&format!("[plugins.{tool}]\n")).unwrap();
            let Err(err) = raw.into_config(false) else {
                panic!("plugins.{tool} should be rejected");
            };
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("plugins.{tool} was removed"))
                    && msg.contains("plugins.edit = {"),
                "error should point at plugins.edit, got: {msg}"
            );
        }
    }

    #[test_case("enabled = false" ; "enabled_false")]
    #[test_case("search_result_limit = 50" ; "opts_only")]
    fn unknown_plugin_name_errors(body: &str) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.gerp]\n{body}\n")).unwrap();
        let Err(err) = raw.into_config(false) else {
            panic!("plugins.gerp should be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no bundled plugin is named \"gerp\"") && msg.contains("grep"),
            "error should name the typo and list bundled plugins, got: {msg}"
        );
    }

    #[test]
    fn disabled_plugin_keeps_opts_but_not_load_entry() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = false\ntimeout_secs = 180\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(!config.plugins.names.contains(&"bash".to_string()));
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180),
            "opts survive for when the plugin is re-enabled"
        );
    }

    #[test]
    fn renamed_tools_table_errors() {
        let raw: RawConfig = toml::from_str("[tools.bash]\nenabled = true\n").unwrap();
        let Err(err) = raw.into_config(false) else {
            panic!("old tools table should be rejected");
        };
        assert!(
            err.to_string().contains("renamed to `plugins`"),
            "got: {err}"
        );
    }

    #[test]
    fn edit_sub_tool_toggles_flow_as_edit_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.edit]\nmultiedit = false\nedit_lines = true\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.plugins.opts["edit"]["multiedit"],
            serde_json::json!(false)
        );
        assert_eq!(
            config.plugins.opts["edit"]["edit_lines"],
            serde_json::json!(true)
        );
        assert!(config.agent.disabled_tools.is_empty());
    }

    #[test]
    fn permissions_mcp_per_tool_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.deepwiki]\nallow = [\"search\", \"fetch\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.iter().any(|r| r.tool
            == ToolKey::McpTool {
                server: "deepwiki".into(),
                tool: "search".into()
            }
            && r.effect == Effect::Allow));
        assert!(perms.rules.iter().any(|r| r.tool
            == ToolKey::McpTool {
                server: "deepwiki".into(),
                tool: "fetch".into()
            }
            && r.effect == Effect::Allow));
    }

    #[test]
    fn permissions_mcp_server_wide_allow_true_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.deepwiki]\nallow = true\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 0, "no rules generated");
        assert!(
            !perms.tool_defaults.contains_key(&ToolKey::McpServer {
                server: "deepwiki".into()
            }),
            "allow = true is deprecated and ignored — no default injected"
        );
    }

    #[test]
    fn permissions_mcp_deny_true_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.server]\ndeny = true\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(
            !perms.tool_defaults.contains_key(&ToolKey::McpServer {
                server: "server".into()
            }),
            "deny = true is deprecated and ignored — no default injected"
        );
    }

    #[test]
    fn explicit_default_preserved_with_deprecated_deny_true() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.server]\ndefault = \"allow\"\ndeny = true\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "server".into()
            }),
            Some(&DefaultEffect::Allow),
            "explicit default still works; deprecated deny = true is ignored"
        );
    }

    #[test]
    fn permissions_mcp_deny_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.github]\ndeny = [\"admin_delete\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "github".into(),
                tool: "admin_delete".into()
            }
        );
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn permissions_mcp_dotted_tool_name_rejected() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.myserver]\nallow = [\"web.search\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 0, "dotted tool name should be rejected");
    }

    #[test]
    fn permissions_mcp_default_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[mcp.exa]\ndefault = \"allow\"\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Allow),
            "MCP server default should be extracted"
        );
    }

    #[test]
    fn permissions_mcp_default_prompt() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.exa]\ndefault = \"prompt\"\nallow = [\"search\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Prompt),
            "MCP server default = prompt should be extracted"
        );
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "exa".into(),
                tool: "search".into()
            }
        );
    }

    #[test]
    fn migrate_mcp_old_flat_keys() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        // Old maki format used quoted TOML keys for mcp:server__tool
        fs::write(
            global.join("permissions.toml"),
            "[\"mcp:deepwiki__search\"]\nallow = true\n\
             [\"mcp:github__issue\"]\nallow = [\"read\"]\n",
        )
        .unwrap();

        let _perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "server table present");
        assert!(content.contains("[mcp.github]"), "server table present");
        assert!(content.contains("\"search\""), "tool name migrated");
        assert!(content.contains("\"issue\""), "tool name migrated");
        assert!(
            !content.contains("mcp:deepwiki__search"),
            "old flat key gone"
        );
        assert!(!content.contains("mcp:github__issue"), "old flat key gone");
        assert!(!content.contains("__"), "no old __ separator remains");
    }

    #[test]
    fn migrate_mcp_nested_bare_keys() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        // Bare TOML key [mcp.deepwiki__search] creates nested mcp → deepwiki__search
        fs::write(
            global.join("permissions.toml"),
            "[mcp]\n\
             deepwiki__search = true\n\
             github__issue = true\n",
        )
        .unwrap();

        let _perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "server table present");
        assert!(content.contains("[mcp.github]"), "server table present");
        assert!(content.contains("\"search\""), "tool name migrated");
        assert!(content.contains("\"issue\""), "tool name migrated");
        assert!(!content.contains("__"), "no old __ separator remains");
    }

    #[test]
    fn empty_tool_key_sections_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[\"\"]\ndefault = \"allow\"\nallow = [\"x\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(perms.rules.is_empty());
        assert!(perms.tool_defaults.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn migration_applies_in_memory_when_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("permissions.toml"),
            "[\"mcp:github__delete\"]\ndeny = true\n",
        )
        .unwrap();
        fs::set_permissions(&global, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(global.join("probe"), b"x").is_ok() {
            return; // running as root, cannot simulate a read-only dir
        }

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        fs::set_permissions(&global, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(perms.rules.len(), 1);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::parse("github.delete").unwrap()
        );
    }
}
