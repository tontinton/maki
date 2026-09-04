use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use maki_config_macro::ConfigSection;
use maki_storage::paths;
use maki_storage::sessions::{StoredThinking, ThinkingParseError};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use strum::VariantArray;
use thiserror::Error;
use tracing::warn;

const PROJECT_DIR: &str = ".maki";
const PERMISSIONS_FILE: &str = "permissions.toml";
const ENV_FILE: &str = ".env";

pub mod providers;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2000;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1500;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;
pub const DEFAULT_MAX_INPUT_LINES: u32 = 20;

pub const MIN_MAX_INPUT_LINES: u32 = 1;

pub const MAX_SERVER_NAME_LEN: usize = 64;

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
    "list",
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

/// A capability a lua plugin can hold. Declared in `plugin.toml`, recorded in
/// the package approval store, and named on every guarded `maki.*` function.
///
/// It lives here rather than in `maki-lua` so the tool layer can name the
/// permission a tool exposes without pulling in the lua runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, VariantArray)]
pub enum Permission {
    FsRead,
    FsWrite,
    Net,
    Run,
    Env,
    /// An always-allow reviewer silently bypasses prompting, so registering
    /// reviewers is an explicit grant.
    Reviewers,
}

impl Permission {
    /// Derived from the enum, because reading a manifest, rendering the docs
    /// and sizing a permission set all walk this, and a hand-written list is
    /// the one place a new variant gets forgotten.
    pub const ALL: &'static [Permission] = <Permission as VariantArray>::VARIANTS;

    /// A permission set is an array this long, indexed by `Permission as
    /// usize`, which is the position in [`Permission::ALL`] since both follow
    /// declaration order.
    pub const COUNT: usize = Permission::ALL.len();

    /// Parses the name used in `plugin.toml` and in the approval store.
    ///
    /// Both use one spelling on purpose. If an approval were recorded under a
    /// different name from the request, `intersect` would silently never
    /// match, and every managed package would run with nothing granted.
    pub fn from_key(key: &str) -> Option<Self> {
        Permission::ALL
            .iter()
            .copied()
            .find(|p| p.manifest_key() == key)
    }

    pub const fn manifest_key(self) -> &'static str {
        match self {
            Permission::FsRead => "fs_read",
            Permission::FsWrite => "fs_write",
            Permission::Net => "net",
            Permission::Run => "run",
            Permission::Env => "env",
            Permission::Reviewers => "reviewers",
        }
    }

    /// What the permission covers, in the words the reference renders. The
    /// boundaries live here so there is one answer to "which guard does this
    /// function belong under".
    pub const fn describes(self) -> &'static str {
        match self {
            Permission::FsRead => "reading files, and locating the directories maki keeps them in",
            Permission::FsWrite => "creating, changing, and removing files",
            Permission::Net => "outbound network requests",
            Permission::Run => "starting processes",
            Permission::Env => "reading the process environment, where secrets live",
            Permission::Reviewers => "registering reviewers that intercept permission prompts",
        }
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.manifest_key())
    }
}

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
    pub env: Option<&'static str>,
    pub description: &'static str,
}

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "always_yolo",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply)",
    },
    ConfigField {
        name: "always_fast",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with Anthropic fast mode (Opus only; ignored otherwise)",
    },
    ConfigField {
        name: "always_workflow",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with workflow mode (task callable inside code_execution)",
    },
    ConfigField {
        name: "always_thinking",
        ty: "bool | string",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with extended thinking (true/\"adaptive\", \"off\", an effort level (\"minimal\" to \"max\"), or a token budget)",
    },
];

/// Expand `${VAR}` references in a config value from the process environment.
/// A variable that is unset OR set to empty fails with `Err(var)`, so callers
/// reject the whole value instead of sending a partially expanded one (e.g.
/// `Bearer ` with nothing after it). An unterminated `${` passes through
/// literally; there is no escape syntax for a literal `${`.
pub fn expand_env(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let var = &after[..end];
                match std::env::var(var) {
                    Ok(v) if !v.is_empty() => out.push_str(&v),
                    _ => return Err(var.to_string()),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

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
        "invalid config: plugins.{plugin}: no bundled plugin or installed \
         package is named \"{plugin}\" (available: {valid})"
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
    #[error("invalid config: provider.{field} contains invalid glob pattern `{pattern}`: {source}")]
    InvalidModelPattern {
        field: &'static str,
        pattern: String,
        #[source]
        source: globset::Error,
    },
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
    pub net: NetFileConfig,
    pub telemetry: TelemetryConfig,
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
        self.net.merge(overlay.net);
        self.telemetry.merge(overlay.telemetry);
        for (name, plugin) in overlay.plugins {
            let entry = self.plugins.entry(name).or_default();
            if plugin.enabled.is_some() {
                entry.enabled = plugin.enabled;
            }
            entry.opts.extend(plugin.opts);
        }
        self.tools.extend(overlay.tools);
    }

    /// `packages` are the external package names discovery found. They are
    /// passed in rather than stored, because an installed package is host
    /// state: it is not written in any config file and must not survive a
    /// merge between two of them.
    pub fn into_config(self, packages: &[String]) -> Result<Config, ConfigError> {
        self.validate_plugin_tables(packages)?;
        Ok(Config {
            always_yolo: self.always_yolo.unwrap_or(false),
            always_fast: self.always_fast.unwrap_or(false),
            always_workflow: self.always_workflow.unwrap_or(false),
            always_thinking: self
                .always_thinking
                .map(AlwaysThinking::resolve)
                .transpose()?,
            ui: UiConfig::from_file(self.ui),
            agent: AgentConfig::from_file(self.agent),
            provider: ProviderConfig::from_file(self.provider)?,
            storage: StorageConfig::from_file(self.storage),
            net: NetConfig::from_file(self.net),
            telemetry: self.telemetry,
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::from_plugins_and_packages(self.plugins, packages),
        })
    }

    /// A `plugins.<name>` key that matches no bundled plugin is a typo or an
    /// old config, so fail loudly instead of letting it silently drift.
    fn validate_plugin_tables(&self, packages: &[String]) -> Result<(), ConfigError> {
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
            .filter(|name| !DEFAULT_BUILTINS.contains(&name.as_str()) && !packages.contains(name))
            .collect();
        unknown.sort();
        if let Some(&plugin) = unknown.first() {
            let mut valid: Vec<&str> = DEFAULT_BUILTINS.to_vec();
            valid.extend(packages.iter().map(String::as_str));
            valid.sort_unstable();
            return Err(ConfigError::UnknownPlugin {
                plugin: plugin.clone(),
                valid: valid.join(", "),
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
    pub notifications: Option<NotificationMethod>,
    pub flash_duration_ms: Option<u64>,
    pub typewriter_ms_per_char: Option<u64>,
    pub mouse_scroll_lines: Option<u32>,
    pub show_thinking: Option<bool>,
    pub theme: Option<String>,
    pub clock_format: Option<ClockFormat>,
    pub tool_output_lines: Option<ToolOutputLinesFile>,
    pub max_input_lines: Option<u32>,
}

impl UiFileConfig {
    fn merge(&mut self, overlay: UiFileConfig) {
        merge_option!(
            self,
            overlay,
            splash_animation,
            scrollbar,
            notifications,
            flash_duration_ms,
            typewriter_ms_per_char,
            mouse_scroll_lines,
            show_thinking,
            theme,
            clock_format,
            max_input_lines
        );
        match (self.tool_output_lines.as_mut(), overlay.tool_output_lines) {
            (Some(base), Some(over)) => base.merge(over),
            (None, Some(over)) => self.tool_output_lines = Some(over),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationMethod {
    #[default]
    Auto,
    Osc9,
    Bell,
    Off,
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
    pub compaction_instructions: Option<String>,
    pub post_compaction_instructions: Option<String>,
    pub stale_read_check: Option<bool>,
    pub rtk: Option<bool>,
}

impl AgentFileConfig {
    fn merge(&mut self, overlay: AgentFileConfig) {
        merge_option!(
            self,
            overlay,
            max_output_bytes,
            max_output_lines,
            max_continuation_turns,
            compaction_buffer,
            compaction_instructions,
            post_compaction_instructions,
            stale_read_check,
            rtk
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderFileConfig {
    pub default_model: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub excluded_models: Option<Vec<String>>,
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
            allowed_models,
            excluded_models,
            connect_timeout_secs,
            low_speed_timeout_secs,
            stream_timeout_secs
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct NetFileConfig {
    pub allowed_private_hosts: Option<Vec<String>>,
}

impl NetFileConfig {
    fn merge(&mut self, overlay: NetFileConfig) {
        merge_option!(self, overlay, allowed_private_hosts);
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
    pub net: NetConfig,
    pub telemetry: TelemetryConfig,
    pub permissions: PermissionsConfig,
    pub plugins: PluginsConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ClockFormat {
    #[serde(rename = "12h")]
    Hour12,
    #[serde(rename = "24h")]
    Hour24,
    #[default]
    #[serde(rename = "system")]
    System,
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "ui")]
pub struct UiConfig {
    #[config(default = true, desc = "Show splash animation on startup")]
    pub splash_animation: bool,

    #[config(default = true, desc = "Show vertical scrollbar in scrollable areas")]
    pub scrollbar: bool,

    #[config(
        default = NotificationMethod::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "Terminal notification method: auto, osc9, bell, or off"
    )]
    pub notifications: NotificationMethod,

    #[config(default = DEFAULT_FLASH_DURATION_MS, desc = "Duration of flash messages (ms)")]
    pub flash_duration_ms: u64,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

    #[config(default = DEFAULT_MAX_INPUT_LINES, min = MIN_MAX_INPUT_LINES, desc = "Maximum visible input lines")]
    pub max_input_lines: u32,

    #[config(
        default = true,
        desc = "When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes"
    )]
    pub show_thinking: bool,

    #[config(default = ClockFormat::System, ty = "String", default_doc = "system", desc = "Clock format for timestamps: \"12h\", \"24h\", or \"system\" (follow the OS preference, 24h when unknown)")]
    pub clock_format: ClockFormat,

    #[config(skip, default = "None")]
    pub theme: Option<String>,

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
            notifications: f.notifications.unwrap_or_default(),
            flash_duration_ms: f.flash_duration_ms.unwrap_or(DEFAULT_FLASH_DURATION_MS),
            typewriter_ms_per_char: f
                .typewriter_ms_per_char
                .unwrap_or(DEFAULT_TYPEWRITER_MS_PER_CHAR),
            mouse_scroll_lines: f.mouse_scroll_lines.unwrap_or(DEFAULT_MOUSE_SCROLL_LINES),
            max_input_lines: f.max_input_lines.unwrap_or(DEFAULT_MAX_INPUT_LINES),
            show_thinking: f.show_thinking.unwrap_or(true),
            clock_format: f.clock_format.unwrap_or_default(),
            theme: f.theme,
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

    #[config(
        ty = "String",
        default = "None",
        desc = "Extra instructions appended to the compaction summary prompt"
    )]
    pub compaction_instructions: Option<String>,

    #[config(
        ty = "String",
        default = "None",
        desc = "Extra instructions the agent receives after any compaction (e.g. re-read plan.md)"
    )]
    pub post_compaction_instructions: Option<String>,

    #[config(
        default = true,
        desc = "Require re-reading a file that changed on disk before editing it"
    )]
    pub stale_read_check: bool,

    #[config(
        default = true,
        desc = "Rewrite bash commands with [rtk](https://github.com/rtk-ai/rtk) when it is installed"
    )]
    pub rtk: bool,

    #[config(skip, default = "None")]
    pub max_turns: Option<u32>,

    #[config(skip, default = "Vec::new()")]
    pub allowed_tools: Vec<String>,

    /// Only from the CLI's `--disallowed-tools`. A disabled plugin never
    /// registers its tool, so its name stays free for another plugin to claim.
    #[config(skip, default = "Vec::new()")]
    pub disabled_tools: Vec<String>,
}

impl AgentConfig {
    fn from_file(file: AgentFileConfig) -> Self {
        Self {
            max_output_bytes: file.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES),
            max_continuation_turns: file
                .max_continuation_turns
                .unwrap_or(DEFAULT_MAX_CONTINUATION_TURNS),
            compaction_buffer: file.compaction_buffer.unwrap_or(DEFAULT_COMPACTION_BUFFER),
            compaction_instructions: file.compaction_instructions,
            post_compaction_instructions: file.post_compaction_instructions,
            stale_read_check: file.stale_read_check.unwrap_or(true),
            rtk: file.rtk.unwrap_or(true),
            max_turns: None,
            allowed_tools: Vec::new(),
            disabled_tools: Vec::new(),
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

    #[config(
        ty = "string[]",
        default_doc = "[]",
        desc = "Glob patterns for permitted qualified model specs; empty permits all models"
    )]
    pub allowed_models: Vec<String>,

    #[config(
        ty = "string[]",
        default_doc = "[]",
        desc = "Glob patterns for excluded qualified model specs; exclusions take precedence"
    )]
    pub excluded_models: Vec<String>,

    #[config(skip)]
    pub model_policy: ModelPolicy,

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
            allowed_models: Vec::new(),
            excluded_models: Vec::new(),
            model_policy: ModelPolicy::allow_all(),
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            low_speed_timeout: Duration::from_secs(DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            stream_timeout: Duration::from_secs(DEFAULT_STREAM_TIMEOUT_SECS),
        }
    }
}

impl ProviderConfig {
    fn from_file(f: ProviderFileConfig) -> Result<Self, ConfigError> {
        let allowed_models = f.allowed_models.unwrap_or_default();
        let excluded_models = f.excluded_models.unwrap_or_default();
        let model_policy = ModelPolicy::new(&allowed_models, &excluded_models)?;
        Ok(Self {
            default_model: f.default_model,
            allowed_models,
            excluded_models,
            model_policy,
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
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModelPolicy {
    allowed: GlobSet,
    excluded: GlobSet,
    has_allowed_models: bool,
}

impl Default for ModelPolicy {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl ModelPolicy {
    fn allow_all() -> Self {
        Self::new(&[], &[]).expect("empty model policy is valid")
    }

    pub fn new(allowed_models: &[String], excluded_models: &[String]) -> Result<Self, ConfigError> {
        Ok(Self {
            allowed: Self::compile("allowed_models", allowed_models)?,
            excluded: Self::compile("excluded_models", excluded_models)?,
            has_allowed_models: !allowed_models.is_empty(),
        })
    }

    fn compile(field: &'static str, patterns: &[String]) -> Result<GlobSet, ConfigError> {
        let mut globset = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .map_err(|source| ConfigError::InvalidModelPattern {
                    field,
                    pattern: pattern.clone(),
                    source,
                })?;
            globset.add(glob);
        }
        globset
            .build()
            .map_err(|source| ConfigError::InvalidModelPattern {
                field,
                pattern: String::new(),
                source,
            })
    }

    pub fn is_restrictive(&self) -> bool {
        self.has_allowed_models || !self.excluded.is_empty()
    }

    pub fn allows(&self, spec: &str) -> bool {
        (!self.has_allowed_models || self.allowed.is_match(spec)) && !self.excluded.is_match(spec)
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

/// Escape hatches for the SSRF guard in `maki.net`, which every HTTP tool
/// goes through. The model picks the URL, so private and metadata addresses
/// are refused unless the user named the host here.
#[derive(Debug, Clone, ConfigSection)]
#[config(section = "net")]
pub struct NetConfig {
    #[config(
        ty = "string[]",
        default = "Vec::new()",
        default_doc = "[]",
        desc = "Hosts allowed to resolve to a private or loopback address, as `host`, `host:port`, or a CIDR range. Plain `http://` is kept for them instead of being upgraded to `https://`"
    )]
    pub allowed_private_hosts: Vec<String>,
}

impl NetConfig {
    fn from_file(f: NetFileConfig) -> Self {
        Self {
            allowed_private_hosts: f.allowed_private_hosts.unwrap_or_default(),
        }
    }
}

/// OpenTelemetry export settings. Every field also has an `OTEL_*` (or
/// `MAKI_*`) environment variable, which wins over what is set here.
///
/// Fields stay optional: `maki-otel` owns the defaults, resolution and
/// validation, so the meaning of "unset" is decided in one place.
#[derive(Deserialize, Debug, Clone, ConfigSection)]
#[serde(default, deny_unknown_fields)]
#[config(section = "telemetry")]
pub struct TelemetryConfig {
    #[config(default = None, ty = "bool", default_doc = "false",
             env = "MAKI_ENABLE_TELEMETRY",
             desc = "Master switch")]
    pub enabled: Option<bool>,

    #[config(default = None, ty = "string", default_doc = "none",
             env = "OTEL_METRICS_EXPORTER",
             desc = "Where metrics go: `otlp`, `console`, `none`, or a comma-separated mix")]
    pub metrics_exporter: Option<String>,

    #[config(default = None, ty = "string", default_doc = "none",
             env = "OTEL_LOGS_EXPORTER",
             desc = "Where events go: `otlp`, `console`, `none`, or a comma-separated mix")]
    pub logs_exporter: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_PROTOCOL",
             desc = "OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an exporter is `otlp`")]
    pub protocol: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_ENDPOINT",
             desc = "Collector endpoint. HTTP appends `/v1/metrics` and `/v1/logs`")]
    pub endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_HEADERS",
             desc = "Extra headers sent with every export")]
    pub headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer", default_doc = "10000",
             env = "OTEL_EXPORTER_OTLP_TIMEOUT",
             desc = "Per-export request timeout (ms)")]
    pub timeout_ms: Option<u64>,

    #[config(default = None, ty = "string", default_doc = "none",
             env = "OTEL_EXPORTER_OTLP_COMPRESSION",
             desc = "Payload compression: `gzip` or `none`")]
    pub compression: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
             desc = "Metrics-only protocol override")]
    pub metrics_protocol: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
             desc = "Metrics-only endpoint, used verbatim with no path appended")]
    pub metrics_endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
             desc = "Metrics-only headers, merged over `headers`")]
    pub metrics_headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
             desc = "Metrics-only request timeout (ms)")]
    pub metrics_timeout_ms: Option<u64>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
             desc = "Logs-only protocol override")]
    pub logs_protocol: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
             desc = "Logs-only endpoint, used verbatim with no path appended")]
    pub logs_endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
             desc = "Logs-only headers, merged over `headers`")]
    pub logs_headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT",
             desc = "Logs-only request timeout (ms)")]
    pub logs_timeout_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "60000",
             env = "OTEL_METRIC_EXPORT_INTERVAL",
             desc = "How often metrics are exported (ms)")]
    pub metrics_interval_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "30000",
             env = "OTEL_METRIC_EXPORT_TIMEOUT",
             desc = "Deadline for one metrics export, retries included (ms)")]
    pub metrics_export_timeout_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "5000",
             env = "OTEL_LOGS_EXPORT_INTERVAL, OTEL_BLRP_SCHEDULE_DELAY",
             desc = "How often queued events are flushed (ms)")]
    pub logs_interval_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "2048",
             env = "OTEL_BLRP_MAX_QUEUE_SIZE",
             desc = "Event queue capacity. Events are dropped and counted when it is full")]
    pub logs_max_queue_size: Option<usize>,

    #[config(default = None, ty = "integer", default_doc = "512",
             env = "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE",
             desc = "Maximum events per export request")]
    pub logs_max_export_batch_size: Option<usize>,

    #[config(default = None, ty = "integer", default_doc = "30000",
             env = "OTEL_BLRP_EXPORT_TIMEOUT",
             desc = "Deadline for one events export, retries included (ms)")]
    pub logs_export_timeout_ms: Option<u64>,

    #[config(default = None, ty = "string", default_doc = "delta",
             env = "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
             desc = "Metric temporality: `delta` or `cumulative`")]
    pub metrics_temporality: Option<String>,

    #[config(default = None, ty = "string", default_doc = "maki",
             env = "OTEL_SERVICE_NAME",
             desc = "`service.name` on the exported resource")]
    pub service_name: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_RESOURCE_ATTRIBUTES",
             desc = "Extra resource attributes, your place for team or environment labels")]
    pub resource_attributes: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "bool", default_doc = "true",
             env = "OTEL_METRICS_INCLUDE_SESSION_ID",
             desc = "Attach `session.id` to metrics. Turn off to keep metric cardinality low")]
    pub metrics_include_session_id: Option<bool>,

    #[config(default = None, ty = "bool", default_doc = "false",
             env = "OTEL_METRICS_INCLUDE_VERSION",
             desc = "Attach `app.version` to metrics")]
    pub metrics_include_version: Option<bool>,

    #[config(default = None, ty = "bool", default_doc = "false",
             env = "OTEL_LOG_USER_PROMPTS",
             desc = "Include prompt text in `maki.user_prompt` events. Off by default")]
    pub log_user_prompts: Option<bool>,

    #[config(default = None, ty = "bool", default_doc = "false",
             env = "OTEL_LOG_TOOL_DETAILS",
             desc = "Include tool input in `maki.tool_result` events. Off by default")]
    pub log_tool_details: Option<bool>,

    #[config(default = None, ty = "integer", default_doc = "10240",
             env = "MAKI_OTEL_CONTENT_MAX_LENGTH",
             desc = "Character cap on any logged prompt or tool input")]
    pub content_max_length: Option<usize>,
}

impl TelemetryConfig {
    fn merge(&mut self, overlay: TelemetryConfig) {
        merge_option!(
            self,
            overlay,
            enabled,
            metrics_exporter,
            logs_exporter,
            protocol,
            endpoint,
            headers,
            timeout_ms,
            compression,
            metrics_protocol,
            metrics_endpoint,
            metrics_headers,
            metrics_timeout_ms,
            logs_protocol,
            logs_endpoint,
            logs_headers,
            logs_timeout_ms,
            metrics_interval_ms,
            metrics_export_timeout_ms,
            logs_interval_ms,
            logs_max_queue_size,
            logs_max_export_batch_size,
            logs_export_timeout_ms,
            metrics_temporality,
            service_name,
            resource_attributes,
            metrics_include_session_id,
            metrics_include_version,
            log_user_prompts,
            log_tool_details,
            content_max_length
        );
    }
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub enabled: bool,
    /// Enabled bundled plugins. Deliberately does not include packages: the
    /// host loads the two from different places, and mixing them here made
    /// `load_builtins` reject every installed package by name.
    pub names: Vec<String>,
    /// Enabled external packages.
    pub packages: Vec<String>,
    /// Per-plugin option tables, without `enabled`. Each plugin validates its
    /// own via `maki.api.register_options` at load time.
    pub opts: HashMap<String, JsonMap<String, JsonValue>>,
}

impl PluginsConfig {
    pub fn from_plugins(plugins: HashMap<String, PluginFileConfig>) -> Self {
        Self::from_plugins_and_packages(plugins, &[])
    }

    /// Installed packages default to enabled like Neovim `start/` packages.
    pub fn from_plugins_and_packages(
        plugins: HashMap<String, PluginFileConfig>,
        packages: &[String],
    ) -> Self {
        let enabled = |name: &String| plugins.get(name).and_then(|t| t.enabled).unwrap_or(true);

        let mut all: Vec<String> = DEFAULT_BUILTINS
            .iter()
            .map(|s| (*s).to_owned())
            .filter(|name| enabled(name))
            .collect();

        let mut enabled_packages: Vec<String> = packages
            .iter()
            .filter(|name| !DEFAULT_BUILTINS.contains(&name.as_str()))
            .filter(|name| enabled(name))
            .cloned()
            .collect();
        enabled_packages.sort();
        enabled_packages.dedup();

        let mut extra: Vec<&String> = plugins
            .iter()
            .filter(|(name, cfg)| {
                !DEFAULT_BUILTINS.contains(&name.as_str())
                    && !packages.contains(name)
                    && cfg.enabled.unwrap_or(false)
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
            packages: enabled_packages,
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
        && name.len() <= MAX_SERVER_NAME_LEN
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

pub fn load_env_files(cwd: &Path) {
    load_env_files_with_global(cwd, paths::find_config_path(ENV_FILE).as_deref());
}

fn load_env_files_with_global(cwd: &Path, global_env: Option<&Path>) {
    let mut vars = HashMap::new();
    if let Some(path) = global_env {
        collect_env_vars(path, &mut vars);
    }
    collect_env_vars(&cwd.join(PROJECT_DIR).join(ENV_FILE), &mut vars);

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

pub fn load_permissions(cwd: &Path) -> PermissionsConfig {
    load_permissions_inner(cwd, &paths::config_search_dirs())
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
    paths::config_dir().ok()
}

pub fn append_permission_rule(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
) -> Result<(), String> {
    let dir = paths::config_search_dirs().into_iter().last();
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

    const GLOBAL_ALLOWED_HOST: &str = "ollama.lan";
    const PROJECT_ALLOWED_HOST: &str = "searx.lan:8888";

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
        let config = RawConfig::default().into_config(&[]).unwrap();
        assert!(config.ui.splash_animation);
        assert_eq!(config.ui.notifications, NotificationMethod::Auto);
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

    #[test_case("auto", NotificationMethod::Auto ; "auto")]
    #[test_case("osc9", NotificationMethod::Osc9 ; "osc9")]
    #[test_case("bell", NotificationMethod::Bell ; "bell")]
    #[test_case("off", NotificationMethod::Off ; "off")]
    fn notifications_deserialize(value: &str, expected: NotificationMethod) {
        let raw: RawConfig =
            toml::from_str(&format!("[ui]\nnotifications = \"{value}\"\n")).unwrap();
        assert_eq!(raw.into_config(&[]).unwrap().ui.notifications, expected);
    }

    #[test]
    fn notifications_reject_unknown_value() {
        let result: Result<RawConfig, _> = toml::from_str("[ui]\nnotifications = \"desktop\"\n");
        assert!(result.is_err());
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
        let config = raw.into_config(&[]).unwrap();
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn merge_overlay_wins_field_by_field() {
        let mut base = RawConfig {
            always_yolo: Some(false),
            ui: UiFileConfig {
                splash_animation: Some(false),
                notifications: Some(NotificationMethod::Bell),
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
            ui: UiFileConfig {
                notifications: Some(NotificationMethod::Off),
                ..Default::default()
            },
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
        assert_eq!(
            base.ui.notifications,
            Some(NotificationMethod::Off),
            "overlay wins"
        );
        assert_eq!(base.ui.flash_duration_ms, Some(2000), "base preserved");
    }

    #[test]
    fn net_allowlist_is_empty_by_default_and_project_replaces_global() {
        let raw_with = |host: &str| RawConfig {
            net: NetFileConfig {
                allowed_private_hosts: Some(vec![host.into()]),
            },
            ..Default::default()
        };
        let defaults = RawConfig::default().into_config(&[]).unwrap();
        assert!(defaults.net.allowed_private_hosts.is_empty());

        let mut global = raw_with(GLOBAL_ALLOWED_HOST);
        global.merge(raw_with(PROJECT_ALLOWED_HOST));

        let net = global.into_config(&[]).unwrap().net;
        assert_eq!(net.allowed_private_hosts, [PROJECT_ALLOWED_HOST]);
    }

    #[test]
    fn provider_model_lists_inherit_replace_and_clear() {
        let mut global = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["anthropic/*".into()]),
                excluded_models: Some(vec!["*/*-preview".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        global.merge(RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(Vec::new()),
                excluded_models: None,
                ..Default::default()
            },
            ..Default::default()
        });

        let provider = global.into_config(&[]).unwrap().provider;
        assert!(provider.allowed_models.is_empty());
        assert_eq!(provider.excluded_models, ["*/*-preview"]);
        assert!(provider.model_policy.allows("openai/gpt-5"));
        assert!(!provider.model_policy.allows("openai/gpt-5-preview"));
    }

    #[test]
    fn model_policy_matches_qualified_specs() {
        let config = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["openai/gpt-5".into(), "opencode/*".into()]),
                excluded_models: Some(vec!["*/*-preview".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(&[])
        .unwrap();
        let policy = &config.provider.model_policy;

        assert!(policy.allows("openai/gpt-5"));
        assert!(policy.allows("opencode/nvidia/openai/gpt-oss-120b"));
        assert!(!policy.allows("anthropic/claude-sonnet-4-6"));
        assert!(!policy.allows("opencode/gpt-5-preview"));

        let exclude_only = RawConfig {
            provider: ProviderFileConfig {
                excluded_models: Some(vec!["anthropic/*".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(&[])
        .unwrap();
        assert!(exclude_only.provider.model_policy.allows("openai/gpt-5"));
        assert!(
            !exclude_only
                .provider
                .model_policy
                .allows("anthropic/claude-sonnet-4-6")
        );
    }

    #[test]
    fn invalid_model_pattern_is_a_config_error() {
        let result = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["[".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(&[]);

        assert!(matches!(
            result,
            Err(ConfigError::InvalidModelPattern { field: "allowed_models", pattern, .. }) if pattern == "["
        ));
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
        let defaults = RawConfig::default().into_config(&[]).unwrap();
        assert!(!defaults.always_workflow, "absent resolves to false");

        let raw = RawConfig {
            always_workflow: Some(true),
            ..Default::default()
        };
        assert!(raw.into_config(&[]).unwrap().always_workflow);
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
        let defaults = RawConfig::default().into_config(&[]).unwrap();
        assert!(defaults.always_thinking.is_none());

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("8192".into())),
            ..Default::default()
        };
        let config = raw.into_config(&[]).unwrap();
        assert_eq!(
            config.always_thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("fast".into())),
            ..Default::default()
        };
        let err = raw.into_config(&[]).err().expect("expected config error");
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
        let config = raw.into_config(&[]).unwrap();
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
    #[test_case("ui",       "max_input_lines",      0 ; "ui_zero_max_input_lines")]
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
            net: NetConfig::default(),
            telemetry: TelemetryConfig::default(),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::default(),
        };
        match (section, field) {
            ("provider", "connect_timeout_secs") => {
                config.provider.connect_timeout = Duration::from_secs(value)
            }
            ("storage", "max_log_files") => config.storage.max_log_files = value as u32,
            ("ui", "mouse_scroll_lines") => config.ui.mouse_scroll_lines = value as u32,
            ("ui", "max_input_lines") => config.ui.max_input_lines = value as u32,
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

        load_env_files_with_global(dir.path(), Some(&global.join(ENV_FILE)));

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
        let config = raw.into_config(&[]).unwrap();
        assert!(config.ui.show_thinking);
    }

    #[test]
    fn max_input_lines_defaults_and_deserializes() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(&[]).unwrap();
        assert_eq!(config.ui.max_input_lines, DEFAULT_MAX_INPUT_LINES);

        let raw: RawConfig = toml::from_str("[ui]\nmax_input_lines = 5\n").unwrap();
        assert_eq!(raw.ui.max_input_lines.unwrap(), 5);
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
        let config = raw.into_config(&[]).unwrap();
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
            let Err(err) = raw.into_config(&[]) else {
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
        let Err(err) = raw.into_config(&[]) else {
            panic!("plugins.gerp should be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("named \"gerp\"") && msg.contains("grep"),
            "error should name the typo and list what is available, got: {msg}"
        );
    }

    #[test]
    fn known_package_name_is_accepted() {
        let raw: RawConfig = toml::from_str("[plugins.my_pack]\nenabled = true\n").unwrap();
        let config = raw
            .into_config(&["my_pack".to_owned()])
            .expect("an installed package should be configurable");
        assert!(config.plugins.packages.contains(&"my_pack".to_owned()));
    }

    #[test]
    fn known_package_can_be_disabled() {
        let raw: RawConfig = toml::from_str("[plugins.my_pack]\nenabled = false\n").unwrap();
        let config = raw.into_config(&["my_pack".to_owned()]).unwrap();
        assert!(!config.plugins.packages.contains(&"my_pack".to_owned()));
    }

    #[test]
    fn packages_are_not_mixed_into_builtin_names() {
        let config = RawConfig::default()
            .into_config(&["my_pack".to_owned()])
            .unwrap();
        assert!(!config.plugins.names.contains(&"my_pack".to_owned()));
        assert!(config.plugins.packages.contains(&"my_pack".to_owned()));
        assert!(
            config
                .plugins
                .names
                .iter()
                .all(|n| DEFAULT_BUILTINS.contains(&n.as_str())),
            "names must hold only bundled plugins"
        );
    }

    #[test_case("grep", &[] ; "builtin")]
    #[test_case("my_pack", &["my_pack".to_owned()] ; "package")]
    fn disabling_a_plugin_leaves_its_tool_name_free(name: &str, packages: &[String]) {
        let raw: RawConfig =
            toml::from_str(&format!("[plugins.{name}]\nenabled = false\n")).unwrap();
        let config = raw.into_config(packages).unwrap();
        assert!(
            config.agent.disabled_tools.is_empty(),
            "a disabled plugin never registers, so nothing may filter its name away"
        );
    }

    #[test]
    fn unknown_name_still_rejected_when_packages_exist() {
        let raw: RawConfig = toml::from_str("[plugins.gerp]\nenabled = true\n").unwrap();
        let Err(err) = raw.into_config(&["my_pack".to_owned()]) else {
            panic!("gerp is neither a builtin nor an installed package");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("my_pack"),
            "should list packages too, got: {msg}"
        );
    }

    #[test]
    fn disabled_plugin_keeps_opts_but_not_load_entry() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = false\ntimeout_secs = 180\n").unwrap();
        let config = raw.into_config(&[]).unwrap();
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
        let Err(err) = raw.into_config(&[]) else {
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
        let config = raw.into_config(&[]).unwrap();
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

    // Uniquely named vars per test: env is process-global and tests run in parallel.

    #[test]
    fn expand_env_literal_text_passes_through() {
        assert_eq!(expand_env("plain value").as_deref(), Ok("plain value"));
    }

    #[test]
    fn expand_env_expands_whole_value_var() {
        unsafe { std::env::set_var("MAKI_TEST_HDR_WHOLE_71535", "secret") };
        assert_eq!(
            expand_env("${MAKI_TEST_HDR_WHOLE_71535}").as_deref(),
            Ok("secret")
        );
    }

    #[test]
    fn expand_env_expands_mid_string_var() {
        unsafe { std::env::set_var("MAKI_TEST_HDR_MID_71535", "tok") };
        assert_eq!(
            expand_env("Bearer ${MAKI_TEST_HDR_MID_71535}!").as_deref(),
            Ok("Bearer tok!")
        );
    }

    #[test]
    fn expand_env_expands_multiple_vars() {
        unsafe { std::env::set_var("MAKI_TEST_HDR_A_71535", "a") };
        unsafe { std::env::set_var("MAKI_TEST_HDR_B_71535", "b") };
        assert_eq!(
            expand_env("${MAKI_TEST_HDR_A_71535}-${MAKI_TEST_HDR_B_71535}").as_deref(),
            Ok("a-b")
        );
    }

    #[test]
    fn expand_env_unset_var_names_the_variable() {
        assert_eq!(
            expand_env("Bearer ${MAKI_TEST_HDR_UNSET_71535}"),
            Err("MAKI_TEST_HDR_UNSET_71535".to_string())
        );
    }

    #[test]
    fn expand_env_unterminated_brace_passes_through() {
        assert_eq!(
            expand_env("x ${NOT_CLOSED").as_deref(),
            Ok("x ${NOT_CLOSED")
        );
    }

    #[test]
    fn expand_env_empty_var_is_treated_as_unset() {
        unsafe { std::env::set_var("MAKI_TEST_HDR_EMPTY_71535", "") };
        assert_eq!(
            expand_env("Bearer ${MAKI_TEST_HDR_EMPTY_71535}"),
            Err("MAKI_TEST_HDR_EMPTY_71535".to_string())
        );
    }
}
