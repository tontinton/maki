use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use maki_config_macro::ConfigSection;
use maki_storage::paths;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

const PROJECT_DIR: &str = ".maki";
pub const PROJECT_CONFIG_FILE: &str = ".maki/config.toml";
const CONFIG_FILE: &str = "config.toml";
const PERMISSIONS_FILE: &str = "permissions.toml";

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2000;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
pub const DEFAULT_MAX_LINE_BYTES: usize = 500;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1500;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;

pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_CODE_EXECUTION_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_MAX_CONTINUATION_TURNS: u32 = 3;
pub const DEFAULT_COMPACTION_BUFFER: u32 = 40_000;
pub const DEFAULT_SEARCH_RESULT_LIMIT: usize = 100;
pub const DEFAULT_INTERPRETER_MAX_MEMORY_MB: usize = 50;

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_LOW_SPEED_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 300;

pub const DEFAULT_MAX_LOG_BYTES_MB: u64 = 200;
pub const DEFAULT_MAX_LOG_FILES: u32 = 10;
pub const DEFAULT_INPUT_HISTORY_SIZE: usize = 100;

pub const DEFAULT_MAX_FILE_SIZE_MB: u64 = 2;

pub const MIN_OUTPUT_BYTES: usize = 1024;
pub const MIN_OUTPUT_LINES: usize = 10;
pub const MIN_RESPONSE_BYTES: usize = 1024;
pub const MIN_LINE_BYTES: usize = 80;
pub const MIN_BASH_TIMEOUT_SECS: u64 = 5;
pub const MIN_CODE_EXECUTION_TIMEOUT_SECS: u64 = 5;
pub const MIN_MAX_CONTINUATION_TURNS: u32 = 1;
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
pub const MIN_SEARCH_RESULT_LIMIT: usize = 10;
pub const MIN_INTERPRETER_MAX_MEMORY_MB: usize = 10;
pub const MIN_MOUSE_SCROLL_LINES: u32 = 1;
pub const MIN_TOOL_OUTPUT_LINES: usize = 1;
pub const MIN_MAX_LOG_BYTES_MB: u64 = 1;
pub const MIN_MAX_LOG_FILES: u32 = 1;
pub const MIN_INPUT_HISTORY_SIZE: usize = 10;
pub const MIN_MAX_FILE_SIZE_MB: u64 = 1;
pub const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
pub const MIN_LOW_SPEED_TIMEOUT_SECS: u64 = 1;
pub const MIN_STREAM_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy)]
pub enum ConfigValue {
    Bool(bool),
    U32(u32),
    U64(u64),
    Usize(usize),
    OptionalString,
}

impl ConfigValue {
    pub fn format_default(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::U32(v) => v.to_string(),
            Self::U64(v) => v.to_string(),
            Self::Usize(v) => v.to_string(),
            Self::OptionalString => "none".to_string(),
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

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[ConfigField {
    name: "always_yolo",
    ty: "bool",
    default: ConfigValue::Bool(false),
    min: None,
    description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply)",
}];

pub const INDEX_FIELDS: &[ConfigField] = &[ConfigField {
    name: "max_file_size_mb",
    ty: "u64",
    default: ConfigValue::U64(DEFAULT_MAX_FILE_SIZE_MB),
    min: Some(MIN_MAX_FILE_SIZE_MB),
    description: "Max file size for indexing (MB)",
}];

#[derive(Debug, Error)]
#[error("invalid config: {section}.{field} = {value} is below minimum ({min})")]
pub struct ConfigError {
    section: &'static str,
    field: &'static str,
    value: u64,
    min: u64,
}

fn check(
    section: &'static str,
    field: &'static str,
    value: u64,
    min: u64,
) -> Result<(), ConfigError> {
    if value < min {
        return Err(ConfigError {
            section,
            field,
            value,
            min,
        });
    }
    Ok(())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    always_yolo: Option<bool>,
    #[serde(default)]
    ui: UiFileConfig,
    agent: AgentFileConfig,
    provider: ProviderFileConfig,
    storage: StorageFileConfig,
    index: IndexFileConfig,
    plugins: PluginsFileConfig,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UiFileConfig {
    splash_animation: Option<bool>,
    flash_duration_ms: Option<u64>,
    typewriter_ms_per_char: Option<u64>,
    mouse_scroll_lines: Option<u32>,
    tool_output_lines: Option<ToolOutputLinesFile>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ToolOutputLinesFile {
    bash: Option<usize>,
    code_execution: Option<usize>,
    task: Option<usize>,
    index: Option<usize>,
    grep: Option<usize>,
    read: Option<usize>,
    write: Option<usize>,
    web: Option<usize>,
    other: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AgentFileConfig {
    max_output_bytes: Option<usize>,
    max_output_lines: Option<usize>,
    max_response_bytes: Option<usize>,
    max_line_bytes: Option<usize>,
    bash_timeout_secs: Option<u64>,
    code_execution_timeout_secs: Option<u64>,
    max_continuation_turns: Option<u32>,
    compaction_buffer: Option<u32>,
    search_result_limit: Option<usize>,
    interpreter_max_memory_mb: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProviderFileConfig {
    default_model: Option<String>,
    connect_timeout_secs: Option<u64>,
    low_speed_timeout_secs: Option<u64>,
    stream_timeout_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StorageFileConfig {
    max_log_bytes_mb: Option<u64>,
    max_log_files: Option<u32>,
    input_history_size: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IndexFileConfig {
    max_file_size_mb: Option<u64>,
}

#[derive(Default)]
struct PermissionsFileConfig {
    allow_all: Option<bool>,
    tools: HashMap<String, ToolPermissions>,
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let allow_all = table.get("allow_all").and_then(|v| v.as_bool());
        let mut tools = HashMap::new();
        for (k, v) in &table {
            if k == "allow_all" {
                continue;
            }
            if let Ok(tp) = v.clone().try_into::<ToolPermissions>() {
                tools.insert(k.clone(), tp);
            }
        }
        Ok(Self { allow_all, tools })
    }
}

#[derive(Deserialize)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    deny: Option<ScopeSet>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub enum PermissionTarget {
    Global,
    Project(PathBuf),
}

#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub tool: String,
    pub scope: Option<String>,
    pub effect: Effect,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionsConfig {
    pub allow_all: bool,
    pub rules: Vec<PermissionRule>,
}

pub struct Config {
    pub always_yolo: bool,
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

    #[config(default = DEFAULT_FLASH_DURATION_MS, desc = "Duration of flash messages (ms)")]
    pub flash_duration_ms: u64,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

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
            flash_duration_ms: f.flash_duration_ms.unwrap_or(DEFAULT_FLASH_DURATION_MS),
            typewriter_ms_per_char: f
                .typewriter_ms_per_char
                .unwrap_or(DEFAULT_TYPEWRITER_MS_PER_CHAR),
            mouse_scroll_lines: f.mouse_scroll_lines.unwrap_or(DEFAULT_MOUSE_SCROLL_LINES),
            tool_output_lines: ToolOutputLines::from_file(f.tool_output_lines),
        }
    }

    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        self.tool_output_lines.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
            "write" | "edit" | "multiedit" | "memory" => self.write,
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

    #[config(default = DEFAULT_MAX_RESPONSE_BYTES, min = MIN_RESPONSE_BYTES, desc = "Max LLM response size (bytes)")]
    pub max_response_bytes: usize,

    #[config(default = DEFAULT_MAX_LINE_BYTES, min = MIN_LINE_BYTES, desc = "Max bytes per line before truncation")]
    pub max_line_bytes: usize,

    #[config(default = DEFAULT_BASH_TIMEOUT_SECS, min = MIN_BASH_TIMEOUT_SECS, desc = "Bash command timeout (seconds)")]
    pub bash_timeout_secs: u64,

    #[config(default = DEFAULT_CODE_EXECUTION_TIMEOUT_SECS, min = MIN_CODE_EXECUTION_TIMEOUT_SECS, desc = "Code execution timeout (seconds)")]
    pub code_execution_timeout_secs: u64,

    #[config(default = DEFAULT_MAX_CONTINUATION_TURNS, min = MIN_MAX_CONTINUATION_TURNS, desc = "Max automatic continuation turns")]
    pub max_continuation_turns: u32,

    #[config(default = DEFAULT_COMPACTION_BUFFER, min = MIN_COMPACTION_BUFFER, desc = "Token buffer reserved during compaction")]
    pub compaction_buffer: u32,

    #[config(default = DEFAULT_SEARCH_RESULT_LIMIT, min = MIN_SEARCH_RESULT_LIMIT, desc = "Max results from grep/glob searches")]
    pub search_result_limit: usize,

    #[config(default = DEFAULT_INTERPRETER_MAX_MEMORY_MB, min = MIN_INTERPRETER_MAX_MEMORY_MB, desc = "Memory limit for code interpreter (MB)")]
    pub interpreter_max_memory_mb: usize,

    #[config(skip, default = false)]
    pub no_rtk: bool,

    #[config(skip, default = "DEFAULT_MAX_FILE_SIZE_MB * 1024 * 1024")]
    pub index_max_file_size: u64,

    #[config(skip, default = "Vec::new()")]
    pub allowed_tools: Vec<String>,
}

impl AgentConfig {
    fn from_file(file: AgentFileConfig, no_rtk: bool, index_file_config: &IndexFileConfig) -> Self {
        Self {
            no_rtk,
            max_output_bytes: file.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES),
            max_response_bytes: file
                .max_response_bytes
                .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
            max_line_bytes: file.max_line_bytes.unwrap_or(DEFAULT_MAX_LINE_BYTES),
            bash_timeout_secs: file.bash_timeout_secs.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS),
            code_execution_timeout_secs: file
                .code_execution_timeout_secs
                .unwrap_or(DEFAULT_CODE_EXECUTION_TIMEOUT_SECS),
            max_continuation_turns: file
                .max_continuation_turns
                .unwrap_or(DEFAULT_MAX_CONTINUATION_TURNS),
            compaction_buffer: file.compaction_buffer.unwrap_or(DEFAULT_COMPACTION_BUFFER),
            search_result_limit: file
                .search_result_limit
                .unwrap_or(DEFAULT_SEARCH_RESULT_LIMIT),
            interpreter_max_memory_mb: file
                .interpreter_max_memory_mb
                .unwrap_or(DEFAULT_INTERPRETER_MAX_MEMORY_MB),
            index_max_file_size: index_file_config
                .max_file_size_mb
                .unwrap_or(DEFAULT_MAX_FILE_SIZE_MB)
                * 1024
                * 1024,
            allowed_tools: Vec::new(),
        }
    }

    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        check(
            "agent",
            "max_file_size_mb",
            self.index_max_file_size / (1024 * 1024),
            MIN_MAX_FILE_SIZE_MB,
        )?;
        Ok(())
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

const DEFAULT_BUILTINS: &[&str] = &["index", "webfetch", "websearch"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct PluginsFileConfig {
    enabled: Option<bool>,
    builtins: Option<Vec<String>>,
    init_file: Option<PathBuf>,
    experimental_bash_lua: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub builtins: Vec<String>,
    pub init_file: Option<PathBuf>,
    pub experimental_bash_lua: bool,
}

impl PluginsConfig {
    fn from_file(f: PluginsFileConfig) -> Self {
        let enabled = f.enabled.unwrap_or(true);
        let experimental_bash_lua = f.experimental_bash_lua.unwrap_or(false);
        let init_file = f.init_file.or_else(|| {
            for dir in config_search_dirs(global_dir().as_deref()).iter().rev() {
                let path = dir.join("init.lua");
                if path.is_file() {
                    return Some(path);
                }
            }
            None
        });
        let builtins_explicit = f.builtins.is_some();
        let mut builtins = f
            .builtins
            .unwrap_or_else(|| DEFAULT_BUILTINS.iter().map(|s| (*s).to_string()).collect());
        if experimental_bash_lua && !builtins_explicit {
            builtins.push("bash".to_string());
        }
        Self {
            enabled,
            builtins,
            init_file,
            experimental_bash_lua,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.ui.validate_all()?;
        self.agent.validate_all()?;
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
                tool: tool.clone(),
                scope: None,
                effect,
            }),
            ScopeSet::Scopes(scopes) => {
                for s in scopes {
                    rules.push(PermissionRule {
                        tool: tool.clone(),
                        scope: Some(s.clone()),
                        effect,
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
}

fn build_permissions(
    global: PermissionsFileConfig,
    project: PermissionsFileConfig,
) -> PermissionsConfig {
    let allow_all = global.allow_all.unwrap_or(false);
    let mut rules = Vec::new();
    for tools in [&global.tools, &project.tools] {
        push_rules(&mut rules, tools, Effect::Deny);
        push_rules(&mut rules, tools, Effect::Allow);
    }
    PermissionsConfig { allow_all, rules }
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

pub fn load_config(cwd: &Path, no_rtk: bool) -> Config {
    let global_dirs = config_search_dirs(global_dir().as_deref());
    load_config_inner(cwd, no_rtk, &global_dirs)
}

fn load_config_inner(cwd: &Path, no_rtk: bool, global_dirs: &[PathBuf]) -> Config {
    load_env_files_with_global(cwd, global_dirs.first().map(|p| p.as_path()));

    let mut base = toml::Table::new();
    for dir in global_dirs {
        if let Some(t) = read_table(&dir.join(CONFIG_FILE)) {
            merge_tables(&mut base, t);
        }
    }
    if let Some(t) = read_table(&cwd.join(PROJECT_DIR).join(CONFIG_FILE)) {
        merge_tables(&mut base, t);
    }
    let raw: RawConfig = match toml::Value::Table(base).try_into() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to deserialize config, using defaults");
            RawConfig::default()
        }
    };

    Config {
        always_yolo: raw.always_yolo.unwrap_or(false),
        ui: UiConfig::from_file(raw.ui),
        agent: AgentConfig::from_file(raw.agent, no_rtk, &raw.index),
        provider: ProviderConfig::from_file(raw.provider),
        storage: StorageConfig::from_file(raw.storage),
        permissions: load_permissions_inner(cwd, global_dirs),
        plugins: PluginsConfig::from_file(raw.plugins),
    }
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

fn read_permissions_file(path: &Path) -> Option<PermissionsFileConfig> {
    let content = fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse permissions");
            None
        }
    }
}

fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (k, v) in overlay {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_tables(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

fn read_table(path: &Path) -> Option<toml::Table> {
    let content = fs::read_to_string(path).ok()?;
    match content.parse::<toml::Table>() {
        Ok(t) => Some(t),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse config");
            None
        }
    }
}

pub fn global_config_path() -> Option<PathBuf> {
    let dirs = config_search_dirs(global_dir().as_deref());
    for dir in dirs.iter().rev() {
        let path = dir.join(CONFIG_FILE);
        if path.is_file() {
            return Some(path);
        }
    }
    dirs.first().map(|d| d.join(CONFIG_FILE))
}

pub fn append_permission_rule(
    tool: &str,
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
    tool: &str,
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
    tool: &str,
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
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("cannot write permissions: {e}"))?;
    Ok(())
}

fn append_project_permission(
    tool: &str,
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
    std::fs::write(&path, doc.to_string())
        .map_err(|e| format!("cannot write .maki/{PERMISSIONS_FILE}: {e}"))?;
    Ok(())
}

fn insert_permission_entry(
    doc: &mut toml_edit::DocumentMut,
    tool: &str,
    scope: Option<&str>,
    effect: Effect,
) -> Result<(), String> {
    let key = match effect {
        Effect::Allow => "allow",
        Effect::Deny => "deny",
    };

    let tool_table = doc
        .entry(tool)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let tool_table = tool_table
        .as_table_mut()
        .ok_or_else(|| format!("[{tool}] is not a table"))?;

    match scope {
        Some(s) => {
            let arr = tool_table.entry(key).or_insert_with(|| {
                toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new()))
            });
            let arr = arr
                .as_array_mut()
                .ok_or_else(|| format!("[{tool}].{key} is not an array"))?;
            let already_exists = arr
                .iter()
                .any(|v| v.as_str().is_some_and(|existing| existing == s));
            if !already_exists {
                arr.push(s);
                arr.set_trailing("\n");
                arr.set_trailing_comma(true);
                for item in arr.iter_mut() {
                    item.decor_mut().set_prefix("\n    ");
                }
            }
        }
        None => {
            tool_table.insert(key, toml_edit::value(true));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    fn write_global_permissions(dir: &Path, content: &str) {
        let perms_dir = dir.join(".config/maki");
        fs::create_dir_all(&perms_dir).unwrap();
        fs::write(perms_dir.join("permissions.toml"), content).unwrap();
    }

    fn global_config_dir(dir: &Path) -> PathBuf {
        dir.join(".config/maki")
    }

    #[test]
    fn empty_config_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let config = load_config(dir.path(), false);
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
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("config.toml"),
            "[agent]\nmax_output_lines = 5000\nbash_timeout_secs = 60\n",
        )
        .unwrap();
        let config = load_config(dir.path(), false);
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.bash_timeout_secs, 60);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn project_overrides_global_field_by_field() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("config.toml"),
            "[ui]\nsplash_animation = false\nflash_duration_ms = 2000\n\
             [agent]\nmax_output_lines = 3000\nmax_line_bytes = 800\n",
        )
        .unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("config.toml"),
            "[agent]\nmax_output_lines = 5000\n",
        )
        .unwrap();

        let config = load_config_inner(dir.path(), false, &[global]);

        assert!(!config.ui.splash_animation);
        assert_eq!(config.ui.flash_duration_ms, 2000);
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.max_line_bytes, 800);
    }

    #[test]
    fn invalid_toml_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(maki_dir.join("config.toml"), "not valid {{{{ toml").unwrap();
        let config = load_config(dir.path(), false);
        assert!(config.ui.splash_animation);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn merge_tables_recursive() {
        let mut base: toml::Table = toml::from_str("[a]\nx = 1\ny = 2").unwrap();
        let overlay: toml::Table = toml::from_str("[a]\ny = 99\nz = 3").unwrap();
        merge_tables(&mut base, overlay);
        let a = base["a"].as_table().unwrap();
        assert_eq!(a["x"].as_integer(), Some(1));
        assert_eq!(a["y"].as_integer(), Some(99));
        assert_eq!(a["z"].as_integer(), Some(3));
    }

    #[test]
    fn agent_config_from_file_uses_provided_values() {
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("config.toml"),
            "[agent]\nmax_output_bytes = 100\nmax_output_lines = 200\nmax_response_bytes = 300\nmax_line_bytes = 400\n",
        )
        .unwrap();
        let config = load_config(dir.path(), true);
        assert_eq!(config.agent.max_output_bytes, 100);
        assert_eq!(config.agent.max_output_lines, 200);
        assert_eq!(config.agent.max_response_bytes, 300);
        assert_eq!(config.agent.max_line_bytes, 400);
        assert!(config.agent.no_rtk);
    }

    #[test_case("max_output_bytes",  0 ; "zero_output_bytes")]
    #[test_case("max_output_lines",  0 ; "zero_output_lines")]
    #[test_case("max_response_bytes", 0 ; "zero_response_bytes")]
    #[test_case("max_line_bytes",    0 ; "zero_line_bytes")]
    #[test_case("max_output_bytes",  500 ; "below_min_output_bytes")]
    #[test_case("max_line_bytes",    10 ; "below_min_line_bytes")]
    fn validate_rejects_invalid_agent(field: &str, value: usize) {
        let mut config = AgentConfig::default();
        match field {
            "max_output_bytes" => config.max_output_bytes = value,
            "max_output_lines" => config.max_output_lines = value,
            "max_response_bytes" => config.max_response_bytes = value,
            "max_line_bytes" => config.max_line_bytes = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert_eq!(err.field, field);
    }

    #[test]
    fn tool_output_lines_per_tool_override() {
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("config.toml"),
            "[ui.tool_output_lines]\nbash = 20\nread = 20\n",
        )
        .unwrap();
        let config = load_config(dir.path(), false);
        assert_eq!(config.ui.tool_output_lines.bash, 20);
        assert_eq!(config.ui.tool_output_lines.read, 20);
        assert_eq!(
            config.ui.tool_output_lines.index,
            ToolOutputLines::DEFAULT.index
        );
    }

    #[test]
    fn all_sections_load_together() {
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(
            maki_dir.join("config.toml"),
            "[provider]\ndefault_model = \"anthropic/claude-opus-4-6\"\nconnect_timeout_secs = 15\n\
             [storage]\nmax_log_files = 5\n\
             [index]\nmax_file_size_mb = 4\n\
             [agent]\nbash_timeout_secs = 60\n",
        )
        .unwrap();
        let config = load_config(dir.path(), false);
        assert_eq!(
            config.provider.default_model.as_deref(),
            Some("anthropic/claude-opus-4-6")
        );
        assert_eq!(config.provider.connect_timeout, Duration::from_secs(15));
        assert_eq!(config.storage.max_log_files, 5);
        assert_eq!(config.agent.index_max_file_size, 4 * 1024 * 1024);
        assert_eq!(config.agent.bash_timeout_secs, 60);
    }

    #[test_case("provider", "connect_timeout_secs", 0 ; "provider_zero_connect_timeout")]
    #[test_case("storage",  "max_log_files",        0 ; "storage_zero_log_files")]
    #[test_case("agent",    "max_file_size_mb",     0 ; "agent_zero_file_size")]
    #[test_case("ui",       "mouse_scroll_lines",   0 ; "ui_zero_scroll_lines")]
    #[test_case("agent",    "bash_timeout_secs",    1 ; "agent_bash_timeout_too_low")]
    fn validate_rejects_invalid_sections(section: &str, field: &str, value: u64) {
        let mut config = Config {
            always_yolo: false,
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
            ("agent", "max_file_size_mb") => config.agent.index_max_file_size = value * 1024 * 1024,
            ("ui", "mouse_scroll_lines") => config.ui.mouse_scroll_lines = value as u32,
            ("agent", "bash_timeout_secs") => config.agent.bash_timeout_secs = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert_eq!(err.section, section);
        assert_eq!(err.field, field);
    }

    #[test]
    fn permissions_loaded_from_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "allow_all = true\n\n\
             [bash]\nallow = [\n    \"cargo *\",\n]\ndeny = [\n    \"rm -rf *\",\n]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(perms.allow_all);
        assert_eq!(perms.rules.len(), 2);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[0].tool, "bash");
        assert_eq!(perms.rules[0].scope.as_deref(), Some("rm -rf *"));
        assert_eq!(perms.rules[1].effect, Effect::Allow);
        assert_eq!(perms.rules[1].tool, "bash");
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
        assert!(!perms.allow_all);
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
        assert_eq!(deny_rules[0].tool, "bash");
        assert_eq!(deny_rules[1].tool, "write");

        assert_eq!(allow_rules.len(), 2);
        assert_eq!(allow_rules[0].tool, "bash");
        assert_eq!(allow_rules[1].tool, "read");
    }

    #[test]
    fn project_allow_all_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(maki_dir.join("permissions.toml"), "allow_all = true\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(!perms.allow_all);
    }

    #[test]
    fn append_permission_rule_writes_to_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            "bash",
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            "bash",
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
    fn no_permissions_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(!perms.allow_all);
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
    fn append_permission_rule_deduplicates() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            "bash",
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            "bash",
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            "bash",
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

        let _config = load_config_inner(dir.path(), false, &[global]);

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
    fn plugins_config_parses_fields() {
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        let init = maki_dir.join("init.lua");
        fs::write(&init, "").unwrap();
        fs::write(
            maki_dir.join("config.toml"),
            format!(
                "[plugins]\nenabled = true\ninit_file = \"{}\"\n",
                init.to_str().unwrap().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let config = load_config(dir.path(), false);
        assert!(config.plugins.enabled);
        assert_eq!(config.plugins.init_file, Some(init));
    }

    #[test]
    fn plugins_default_builtins_populated_when_enabled() {
        let dir = TempDir::new().unwrap();
        let maki_dir = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).unwrap();
        fs::write(maki_dir.join("config.toml"), "[plugins]\nenabled = true\n").unwrap();
        let config = load_config(dir.path(), false);
        assert!(
            !config.plugins.builtins.is_empty(),
            "enabled plugins should have default builtins"
        );
    }
}
