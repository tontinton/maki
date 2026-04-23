//! Shared plumbing for native tools. The registry itself lives in `registry.rs`; this file
//! holds the helpers every tool leans on: `ToolFilter` to enable/disable per caller,
//! `Deadline` so a parent tool can cap a child's timeout, the walker that skips `.git`,
//! and `sanitize_tool_input` which patches up small JSON mistakes models make (stray
//! quotes, camelCase keys, extra wrappers). The `register_tools!` and `impl_tool!` macros
//! at the bottom wire each native tool into the registry through `Native<T>`. Plan mode
//! rejects writes to anything but the plan file before they reach the tool.

mod bash;
mod batch;
mod code_execution;
mod edit;
mod file_tracker;
mod fuzzy_replace;
mod glob;
mod grep;
pub mod memory;
mod multiedit;
mod question;
mod read;
pub mod registry;
pub mod schema;
mod skill;
mod task;
mod todowrite;
mod write;

pub use file_tracker::FileReadTracker;
pub use registry::{
    ExecFuture, HeaderFuture, HeaderResult, Native, ParseError, RegisteredTool, RegistryError,
    Tool, ToolAudience, ToolInvocation, ToolRegistry, ToolSource,
};

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use humantime::format_duration;
use ignore::WalkBuilder;
use serde_json::Value;
use tracing::warn;

pub(super) use crate::NO_FILES_FOUND;
use crate::agent::LoadedInstructions;
use crate::cancel::CancelToken;
use crate::mcp::McpHandle;
use crate::permissions::PermissionManager;
use crate::skill::Skill;
use crate::{AgentConfig, AgentEvent, AgentMode, EventSender, ToolStartEvent};
use maki_providers::Model;
use maki_providers::provider::Provider;

pub struct DescriptionContext<'a> {
    pub skills: &'a [Skill],
    pub filter: &'a ToolFilter,
}

#[derive(Debug, Clone, Default)]
pub enum ToolFilter {
    #[default]
    All,
    Only(Vec<String>),
    AllExcept(Vec<String>),
}

impl ToolFilter {
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(allowed) => allowed.iter().any(|n| n == name),
            Self::AllExcept(blocked) => !blocked.iter().any(|n| n == name),
        }
    }

    pub fn excluding(self, names: &[&str]) -> Self {
        if names.is_empty() {
            return self;
        }
        match self {
            Self::All => Self::AllExcept(names.iter().map(|s| (*s).to_owned()).collect()),
            Self::Only(allowed) => Self::Only(
                allowed
                    .into_iter()
                    .filter(|n| !names.iter().any(|x| *x == n))
                    .collect(),
            ),
            Self::AllExcept(mut blocked) => {
                for &n in names {
                    if !blocked.iter().any(|b| b == n) {
                        blocked.push(n.to_owned());
                    }
                }
                Self::AllExcept(blocked)
            }
        }
    }

    pub fn from_config(config: &AgentConfig, extra_exclude: &[&str]) -> Self {
        let base = if config.allowed_tools.is_empty() {
            Self::All
        } else {
            Self::Only(
                config
                    .allowed_tools
                    .iter()
                    .filter(|s| ToolRegistry::native().has(s))
                    .cloned()
                    .collect(),
            )
        };
        let mut exclude: Vec<&str> = extra_exclude.to_vec();
        exclude.extend(disabled_tool_names(config));
        base.excluding(&exclude)
    }
}

fn disabled_tool_names(_config: &AgentConfig) -> Vec<&'static str> {
    Vec::new()
}

pub fn is_tool_enabled(config: &AgentConfig, name: &str) -> bool {
    !disabled_tool_names(config).contains(&name)
}

pub const BASH_TOOL_NAME: &str = bash::Bash::NAME;
pub const BATCH_TOOL_NAME: &str = batch::Batch::NAME;
pub const EDIT_TOOL_NAME: &str = edit::Edit::NAME;
pub const GLOB_TOOL_NAME: &str = glob::Glob::NAME;
pub const GREP_TOOL_NAME: &str = grep::Grep::NAME;
pub const MULTIEDIT_TOOL_NAME: &str = multiedit::MultiEdit::NAME;
pub const QUESTION_TOOL_NAME: &str = question::Question::NAME;
pub const READ_TOOL_NAME: &str = read::Read::NAME;
pub const SKILL_TOOL_NAME: &str = skill::SkillTool::NAME;
pub const TASK_TOOL_NAME: &str = task::Task::NAME;
pub const TODOWRITE_TOOL_NAME: &str = todowrite::TodoWrite::NAME;
pub const WRITE_TOOL_NAME: &str = write::Write::NAME;
pub const MEMORY_TOOL_NAME: &str = memory::Memory::NAME;
pub const CODE_EXECUTION_TOOL_NAME: &str = code_execution::CodeExecution::NAME;

pub(crate) const PLAN_WRITE_RESTRICTED: &str = "write restricted to plan file in plan mode";
pub(crate) const DEADLINE_EXCEEDED: &str = "timeout exceeded";

#[derive(Clone, Copy, Debug, Default)]
pub enum Deadline {
    #[default]
    None,
    At(Instant),
}

impl Deadline {
    pub fn after(duration: Duration) -> Self {
        Self::At(Instant::now() + duration)
    }

    pub fn check(self) -> Result<(), String> {
        match self {
            Self::None => Ok(()),
            Self::At(instant) if instant.saturating_duration_since(Instant::now()).is_zero() => {
                Err(DEADLINE_EXCEEDED.into())
            }
            Self::At(_) => Ok(()),
        }
    }

    pub fn cap_timeout(self, timeout_secs: u64) -> Result<u64, String> {
        match self {
            Self::None => Ok(timeout_secs),
            Self::At(instant) => {
                let remaining = instant.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err(DEADLINE_EXCEEDED.into())
                } else {
                    Ok(timeout_secs.min(remaining.as_secs().max(1)))
                }
            }
        }
    }
}

pub(crate) fn timeout_annotation(secs: u64) -> String {
    let d = Duration::from_secs(secs);
    let formatted: String = format_duration(d)
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    format!("{formatted} timeout")
}

#[derive(Clone)]
pub struct ToolContext {
    pub provider: Arc<dyn Provider>,
    pub model: Arc<Model>,
    pub event_tx: EventSender,
    pub mode: AgentMode,
    pub tool_use_id: Option<String>,
    pub user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    pub skills: Arc<[Skill]>,
    pub loaded_instructions: LoadedInstructions,
    pub cancel: CancelToken,
    pub mcp: Option<McpHandle>,
    pub deadline: Deadline,
    pub config: AgentConfig,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: maki_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub(crate) batch_child: bool,
}

impl ToolContext {
    pub(crate) fn emit_tool_start(&self, start: ToolStartEvent) {
        if !self.batch_child {
            let _ = self.event_tx.send(AgentEvent::ToolStart(Box::new(start)));
        }
    }
}

pub(crate) fn resolve_path(path: &str) -> Result<String, String> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = HOME.as_deref().ok_or("cannot expand ~: HOME not set")?;
        format!("{}/{rest}", home.display())
    } else if path == "~" {
        let home = HOME.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.to_string_lossy().into_owned()
    } else {
        path.to_string()
    };

    if Path::new(&expanded).is_relative() {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        Ok(cwd.join(&expanded).to_string_lossy().into_owned())
    } else {
        Ok(expanded)
    }
}

pub(crate) fn resolve_search_path(path: Option<&str>) -> Result<String, String> {
    match path {
        Some(p) => resolve_path(p),
        None => env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("cwd error: {e}")),
    }
}

static CWD: LazyLock<Option<PathBuf>> = LazyLock::new(|| env::current_dir().ok());
static HOME: LazyLock<Option<PathBuf>> = LazyLock::new(dirs::home_dir);

pub(crate) fn relative_path(path: &str) -> String {
    let p = Path::new(path);
    if let Some(cwd) = CWD.as_deref()
        && let Ok(rel) = p.strip_prefix(cwd)
    {
        return format_rel("", ".", rel);
    }
    if let Some(home) = HOME.as_deref()
        && let Ok(rel) = p.strip_prefix(home)
    {
        return format_rel("~/", "~", rel);
    }
    path.to_string()
}

fn format_rel(prefix: &str, fallback: &str, rel: &Path) -> String {
    let s = rel.to_string_lossy();
    if s.is_empty() {
        fallback.into()
    } else {
        format!("{prefix}{s}")
    }
}

/// Returns a `WalkBuilder` with `.hidden(false)` and `!.git` exclusion enforced.
pub(crate) fn walk_builder(root: &str, patterns: &[&str]) -> Result<WalkBuilder, String> {
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    ob.add("!.git").expect("!.git is a valid glob");

    for p in patterns {
        ob.add(p)
            .map_err(|e| format!("invalid glob pattern: {e}"))?;
    }

    let overrides = ob
        .build()
        .map_err(|e| format!("invalid glob pattern: {e}"))?;

    let mut wb = WalkBuilder::new(root);
    wb.hidden(false).overrides(overrides);
    Ok(wb)
}

pub(crate) fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

pub(crate) fn truncate_bytes(line: &str, max_bytes: usize) -> String {
    if line.len() > max_bytes {
        let boundary = line.floor_char_boundary(max_bytes);
        format!("{}...", &line[..boundary])
    } else {
        line.to_owned()
    }
}

pub(crate) fn truncate_output(text: String, max_lines: usize, max_bytes: usize) -> String {
    const TRUNCATED_MARKER: &str = "[truncated]";
    let mut lines = text.lines();
    let mut result = String::new();
    let mut truncated = false;

    for _ in 0..max_lines {
        let Some(line) = lines.next() else { break };
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
        if result.len() > max_bytes {
            let boundary = result.floor_char_boundary(max_bytes);
            result.truncate(boundary);
            truncated = true;
            break;
        }
    }

    if !truncated && lines.next().is_some() {
        truncated = true;
    }

    if truncated {
        result.push('\n');
        result.push_str(TRUNCATED_MARKER);
    }
    result
}

fn format_tool_signature(name: &str, schema: &Value) -> String {
    let empty_props = serde_json::Map::new();
    let props = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .unwrap_or(&empty_props);
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let params: Vec<String> = props
        .iter()
        .map(|(pname, pschema)| {
            let ptype = pschema
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any");
            let ptype_py = match ptype {
                "string" => "str",
                "integer" => "int",
                "boolean" => "bool",
                "array" => "list",
                _ => "any",
            };
            if required.contains(pname.as_str()) {
                format!("{pname}: {ptype_py}")
            } else {
                format!("{pname}: {ptype_py} = None")
            }
        })
        .collect();

    format!("- {name}({}) -> str", params.join(", "))
}

/// Walks the registry so adding a new `INTERPRETER` tool shows up automatically.
pub(crate) fn build_interpreter_tools_description(filter: &ToolFilter) -> String {
    let mut desc =
        String::from("\n\nAvailable tools (called as Python functions with keyword arguments):\n");
    let registry = ToolRegistry::native();
    for entry in registry.iter().iter() {
        let name = entry.name();
        if !entry.tool.audience().contains(ToolAudience::INTERPRETER) {
            continue;
        }
        if !filter.matches(name) {
            continue;
        }
        let schema = entry.tool.schema();
        desc.push_str(&format_tool_signature(name, &schema));
        desc.push('\n');
    }
    desc
}

pub(crate) fn sanitize_tool_input(input: &Value) -> Value {
    let obj = match input.as_object() {
        Some(o) => o,
        None => {
            return input
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .filter(|v| v.is_object())
                .map(|v| sanitize_tool_input(&v))
                .unwrap_or_else(|| input.clone());
        }
    };

    if let Some(inner) = obj.get("parameters").filter(|_| obj.len() == 1) {
        return sanitize_tool_input(inner);
    }

    let mut out = serde_json::Map::new();
    for (key, val) in obj {
        let norm_key = to_snake_case(key);
        let new_val = match val.as_str() {
            Some(s) => Value::String(strip_stray_quotes(key, s)),
            None => val.clone(),
        };
        if norm_key != *key {
            warn!(original = %key, normalized = %norm_key, "normalized camelCase key to snake_case");
        }
        out.insert(norm_key, new_val);
    }
    Value::Object(out)
}

fn strip_stray_quotes(field: &str, s: &str) -> String {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        && !inner.contains('"')
    {
        warn!(field = %field, original = %s, fixed = %inner, "stripped stray quotes from tool param");
        return inner.to_string();
    }
    s.to_string()
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

/// Builds the `Tool` impl for `Native<T>` from the consts `#[derive(Tool)]` produces, so
/// tool files only write logic. `augment` lets a tool tweak its description at request
/// time (e.g. appending the interpreter tool list).
macro_rules! impl_tool {
    (@augment_body $desc:ident, $ctx:ident,) => {};
    (@augment_body $desc:ident, $ctx:ident, $f:expr) => { ($f)(&mut $desc, $ctx); };
    (@audience_body) => { $crate::tools::ToolAudience::all() };
    (@audience_body $aud:expr) => { $aud };
    (
        $ty:ty
        $(, audience = $aud:expr)?
        $(, augment = $augment:expr)?
        $(,)?
    ) => {
        impl $crate::tools::Tool for $crate::tools::Native<$ty> {
            fn name(&self) -> &str { <$ty>::NAME }

            fn description(&self, _ctx: &$crate::tools::DescriptionContext)
                -> std::borrow::Cow<'_, str>
            {
                #[allow(unused_mut)]
                let mut s = <$ty>::DESCRIPTION.to_owned();
                $crate::tools::impl_tool!(@augment_body s, _ctx, $($augment)?);
                std::borrow::Cow::Owned(s)
            }

            fn schema(&self) -> serde_json::Value {
                $crate::tools::schema::to_json_schema(<$ty>::SCHEMA)
            }

            fn examples(&self) -> Option<serde_json::Value> {
                let raw = <$ty>::EXAMPLES?;
                Some(serde_json::from_str(raw).unwrap_or_else(|e|
                    panic!("invalid EXAMPLES JSON for {}: {e}", <$ty>::NAME)))
            }

            fn audience(&self) -> $crate::tools::ToolAudience {
                $crate::tools::impl_tool!(@audience_body $($aud)?)
            }

            fn parse(&self, input: &serde_json::Value)
                -> Result<Box<dyn $crate::tools::ToolInvocation>, $crate::tools::ParseError>
            {
                Ok(Box::new(<$ty>::parse_input(input)?))
            }
        }
    };
}
pub(crate) use impl_tool;

/// Checks at compile time that no two tools share a `NAME`. Without this, duplicates would
/// only show up as a confused model calling the wrong tool at runtime.
macro_rules! register_tools {
    ($($inner:path),+ $(,)?) => {
        const _: () = {
            const NAMES: &[&str] = &[$(<$inner>::NAME),+];
            const fn str_eq(a: &str, b: &str) -> bool {
                let (a, b) = (a.as_bytes(), b.as_bytes());
                if a.len() != b.len() { return false; }
                let mut i = 0;
                while i < a.len() {
                    if a[i] != b[i] { return false; }
                    i += 1;
                }
                true
            }
            let mut i = 0;
            while i < NAMES.len() {
                let mut j = i + 1;
                while j < NAMES.len() {
                    assert!(!str_eq(NAMES[i], NAMES[j]), "duplicate tool NAME detected");
                    j += 1;
                }
                i += 1;
            }
        };

        pub(crate) fn native_tools() -> Vec<std::sync::Arc<dyn $crate::tools::Tool>> {
            vec![
                $(std::sync::Arc::new($crate::tools::Native::<$inner>::new())),+
            ]
        }

        pub const NATIVE_TOOL_NAMES: &[&str] = &[$(<$inner>::NAME),+];
    };
}

register_tools! {
    bash::Bash,
    read::Read,
    write::Write,
    edit::Edit,
    multiedit::MultiEdit,
    glob::Glob,
    grep::Grep,
    question::Question,
    todowrite::TodoWrite,
    skill::SkillTool,
    task::Task,
    batch::Batch,
    code_execution::CodeExecution,
    memory::Memory,
}

use maki_providers::provider::BoxFuture;
use maki_providers::{Message, ProviderEvent, StreamResponse, ThinkingConfig};

struct NullProvider;

impl Provider for NullProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a Value,
        _: &'a flume::Sender<ProviderEvent>,
        _: ThinkingConfig,
        _: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, crate::AgentError>> {
        Box::pin(async { unimplemented!() })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, crate::AgentError>> {
        Box::pin(async { unimplemented!() })
    }
}

pub(crate) fn interpreter_ctx(
    mode: &AgentMode,
    event_tx: &EventSender,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
) -> ToolContext {
    static PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| Arc::new(NullProvider));
    static MODEL: LazyLock<Arc<Model>> =
        LazyLock::new(|| Arc::new(Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()));
    static SKILLS: LazyLock<Arc<[Skill]>> = LazyLock::new(|| Arc::from([]));
    ToolContext {
        provider: Arc::clone(&PROVIDER),
        model: Arc::clone(&MODEL),
        event_tx: event_tx.clone(),
        mode: mode.clone(),
        tool_use_id: None,
        user_response_rx,
        skills: Arc::clone(&SKILLS),
        loaded_instructions: LoadedInstructions::new(),
        cancel,
        mcp: None,
        deadline: Deadline::None,
        config: AgentConfig::default(),
        permissions,
        timeouts: maki_providers::Timeouts::default(),
        file_tracker,
        batch_child: false,
    }
}

/// Minimal ToolContext for CLI one-shot tool execution (e.g. `maki index`).
/// Allows everything, sends events to a dummy channel, uses no model.
pub fn cli_tool_ctx() -> ToolContext {
    let (tx, _rx) = flume::unbounded::<crate::Envelope>();
    let event_tx = crate::EventSender::new(tx, 0);
    interpreter_ctx(
        &AgentMode::Build,
        &event_tx,
        CancelToken::none(),
        Arc::new(PermissionManager::new(
            maki_config::PermissionsConfig {
                allow_all: true,
                rules: vec![],
            },
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        )),
        Arc::new(FileReadTracker::new()),
        None,
    )
}

pub mod test_support {
    use crate::{Envelope, EventSender};

    use super::*;

    static TEST_PERMISSIONS: LazyLock<Arc<PermissionManager>> = LazyLock::new(|| {
        Arc::new(PermissionManager::new(
            maki_config::PermissionsConfig {
                allow_all: true,
                rules: vec![],
            },
            std::path::PathBuf::from("/tmp"),
        ))
    });

    pub fn stub_ctx_with(
        mode: &AgentMode,
        event_tx: Option<&EventSender>,
        tool_use_id: Option<&str>,
    ) -> ToolContext {
        let fallback_tx;
        let event_tx = match event_tx {
            Some(tx) => tx,
            None => {
                fallback_tx = EventSender::new(flume::unbounded::<Envelope>().0, 0);
                &fallback_tx
            }
        };
        let mut ctx = interpreter_ctx(
            mode,
            event_tx,
            CancelToken::none(),
            Arc::clone(&TEST_PERMISSIONS),
            Arc::new(FileReadTracker::new()),
            None,
        );
        ctx.tool_use_id = tool_use_id.map(String::from);
        ctx
    }

    pub fn stub_ctx(mode: &AgentMode) -> ToolContext {
        stub_ctx_with(mode, None, None)
    }

    #[cfg(test)]
    pub(crate) fn stub_ctx_with_permissions(
        mode: &AgentMode,
        permissions: Arc<PermissionManager>,
    ) -> ToolContext {
        let (tx, _rx) = flume::unbounded::<crate::Envelope>();
        let event_tx = EventSender::new(tx, 0);
        let mut ctx = interpreter_ctx(
            mode,
            &event_tx,
            CancelToken::none(),
            permissions,
            Arc::new(FileReadTracker::new()),
            None,
        );
        ctx.tool_use_id = None;
        ctx
    }

    #[cfg(test)]
    pub(crate) fn pre_read(ctx: &ToolContext, path: &str) {
        ctx.file_tracker.record_read(Path::new(path));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::test_support::stub_ctx;
    use super::*;
    use crate::agent::tool_dispatch;
    use crate::template::Vars;
    use crate::{AgentError, NO_FILES_FOUND, ToolOutput};

    const LINE_LIMIT: usize = 500;
    const PARSE_INTERNAL_BUG: &str = "internal validator bug";

    #[test_case(30,  "30s timeout"   ; "seconds_only")]
    #[test_case(120, "2m timeout"    ; "minutes_only")]
    #[test_case(90,  "1m30s timeout" ; "mixed")]
    fn timeout_annotation_cases(secs: u64, expected: &str) {
        assert_eq!(timeout_annotation(secs), expected);
    }

    #[test_case(Deadline::None,                          120, 120 ; "none_passes_through")]
    #[test_case(Deadline::after(Duration::from_secs(60)), 10,  10  ; "requested_under_remaining")]
    fn cap_timeout_ok(deadline: Deadline, requested: u64, expected: u64) {
        assert_eq!(deadline.cap_timeout(requested).unwrap(), expected);
    }

    #[test]
    fn cap_timeout_clamps_to_remaining() {
        let clamped = Deadline::after(Duration::from_secs(30))
            .cap_timeout(600)
            .unwrap();
        assert!(
            (1..=30).contains(&clamped),
            "expected 1..=30, got {clamped}"
        );
    }

    #[test]
    fn cap_timeout_expired() {
        let expired = Deadline::At(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        assert_eq!(expired.cap_timeout(120).unwrap_err(), DEADLINE_EXCEEDED);
    }

    #[test_case("short",                            "short"                             ; "short_passthrough")]
    #[test_case(&"x".repeat(LINE_LIMIT),       &"x".repeat(LINE_LIMIT)        ; "exact_boundary")]
    #[test_case(&"x".repeat(LINE_LIMIT + 500), &format!("{}...", "x".repeat(LINE_LIMIT)) ; "long_truncated")]
    #[test_case(&format!("{}\u{1F600}", "a".repeat(LINE_LIMIT - 1)), &format!("{}...", "a".repeat(LINE_LIMIT - 1)) ; "multibyte_char_boundary")]
    fn truncate_bytes_cases(input: &str, expected: &str) {
        let result = truncate_bytes(input, LINE_LIMIT);
        assert_eq!(result, expected);
    }

    #[test]
    fn truncate_output_respects_line_and_byte_limits() {
        const MAX_LINES: usize = 2000;
        const MAX_BYTES: usize = 50 * 1024;

        let many_lines: String = (0..MAX_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_output(many_lines, MAX_LINES, MAX_BYTES);
        assert!(result.ends_with("[truncated]"));

        let many_bytes = "x".repeat(MAX_BYTES + 1000);
        let result = truncate_output(many_bytes, MAX_LINES, MAX_BYTES);
        assert!(result.ends_with("[truncated]"));
    }

    #[test]
    fn read_with_offset_and_limit() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("test.txt");
            let content = (1..=10)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&path, &content).unwrap();
            let path = path.to_string_lossy().to_string();
            let ctx = stub_ctx(&AgentMode::Build);

            let r = read::Read::parse_input(&json!({"path": path})).unwrap();
            let full = r.execute(&ctx).await.unwrap().as_text().to_string();
            assert!(full.contains("1: line1"));
            assert!(full.contains("10: line10"));

            let r =
                read::Read::parse_input(&json!({"path": path, "offset": 3, "limit": 2})).unwrap();
            let slice = r.execute(&ctx).await.unwrap().as_text().to_string();
            assert!(slice.contains("3: line3"));
            assert!(slice.contains("4: line4"));
            assert!(!slice.contains("5: line5"));
        });
    }

    #[test]
    fn glob_finds_and_misses() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("a.txt"), "hello").unwrap();
            fs::write(dir.path().join("b.txt"), "world").unwrap();
            fs::write(dir.path().join("c.rs"), "fn main(){}").unwrap();
            let dir_str = dir.path().to_string_lossy().to_string();
            let ctx = stub_ctx(&AgentMode::Build);

            let g = glob::Glob::parse_input(&json!({"pattern": "*.txt", "path": dir_str})).unwrap();
            let output = g.execute(&ctx).await.unwrap();
            let ToolOutput::GlobResult { files } = &output else {
                panic!("expected GlobResult");
            };
            assert_eq!(files.len(), 2);
            assert!(files.iter().all(|f| f.contains(".txt")));
            assert!(files.iter().all(|f| !f.contains(".rs")));

            let g =
                glob::Glob::parse_input(&json!({"pattern": "*.nope", "path": dir_str})).unwrap();
            let output = g.execute(&ctx).await.unwrap();
            assert!(matches!(output, ToolOutput::GlobResult { files } if files.is_empty()));
        });
    }

    #[test]
    fn grep_finds_filters_and_misses() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("a.txt"), "hello world\ngoodbye world").unwrap();
            fs::write(dir.path().join("b.rs"), "hello rust").unwrap();
            let dir_str = dir.path().to_string_lossy().to_string();
            let ctx = stub_ctx(&AgentMode::Build);

            let g = grep::Grep::parse_input(&json!({"pattern": "hello", "path": dir_str})).unwrap();
            let hit = g.execute(&ctx).await.unwrap().as_text().to_string();
            assert!(hit.contains("a.txt"));
            assert!(hit.contains("b.rs"));

            let g = grep::Grep::parse_input(
                &json!({"pattern": "hello", "path": dir_str, "include": "*.rs"}),
            )
            .unwrap();
            let filtered = g.execute(&ctx).await.unwrap().as_text().to_string();
            assert!(filtered.contains("b.rs"));
            assert!(!filtered.contains("a.txt"));

            let g = grep::Grep::parse_input(&json!({"pattern": "zzzznotfound", "path": dir_str}))
                .unwrap();
            assert_eq!(g.execute(&ctx).await.unwrap().as_text(), NO_FILES_FOUND);
        });
    }

    #[test]
    fn grep_single_file_preserves_filename() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let file = dir.path().join("demo.rs");
            fs::write(&file, "fn main() {}\n").unwrap();
            let ctx = stub_ctx(&AgentMode::Build);

            let path = file.to_string_lossy().to_string();
            let g = grep::Grep::parse_input(&json!({"pattern": "fn main", "path": path})).unwrap();
            let out = g.execute(&ctx).await.unwrap();
            let crate::ToolOutput::GrepResult { entries } = &out else {
                panic!("expected GrepResult, got: {out:?}");
            };
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, "demo.rs");
        });
    }

    #[test]
    fn grep_skips_binary_files() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("text.txt"), "findme here").unwrap();
            fs::write(dir.path().join("binary.bin"), b"findme \x00 binary content").unwrap();
            let dir_str = dir.path().to_string_lossy().to_string();
            let ctx = stub_ctx(&AgentMode::Build);

            let g =
                grep::Grep::parse_input(&json!({"pattern": "findme", "path": dir_str})).unwrap();
            let out = g.execute(&ctx).await.unwrap().as_text().to_string();
            assert!(out.contains("text.txt"));
            assert!(!out.contains("binary.bin"));
        });
    }

    #[test]
    fn grep_invalid_regex_returns_error() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let dir_str = dir.path().to_string_lossy().to_string();
            let ctx = stub_ctx(&AgentMode::Build);

            let g =
                grep::Grep::parse_input(&json!({"pattern": "[invalid", "path": dir_str})).unwrap();
            let err = g.execute(&ctx).await.unwrap_err();
            assert!(err.contains(grep::INVALID_REGEX), "got: {err}");
        });
    }

    /// Every registered tool, poked with four shapes of garbage, should come
    /// back with a plain validator error the LLM can read (Missing,
    /// TypeMismatch, NotInEnum). If any of them trips the `InternalBug`
    /// path, the schema no longer matches the Rust type and serde exploded
    /// after validation said it was fine.
    #[test]
    fn every_tool_rejects_bogus_input_without_internal_bug() {
        const BOGUS_INPUTS: &[&str] = &[
            "{}",
            "\"raw string\"",
            "{\"unknown_field\": 42}",
            "{\"path\": 123, \"pattern\": []}",
        ];
        let registry = ToolRegistry::native();
        for entry in registry.iter().iter() {
            let name = entry.name().to_owned();
            for raw in BOGUS_INPUTS {
                let input: Value = serde_json::from_str(raw).unwrap();
                match entry.tool.parse(&input) {
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        assert!(
                            !msg.contains(PARSE_INTERNAL_BUG),
                            "tool `{name}` with input `{raw}` produced InternalBug: {msg}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tool_definitions_schema_requires_additional_properties_false() {
        fn check_object_schemas(schema: &Value, path: &str) {
            if schema.get("type").and_then(|v| v.as_str()) == Some("object")
                && schema.get("properties").is_some()
            {
                assert_eq!(
                    schema.get("additionalProperties"),
                    Some(&json!(false)),
                    "{path} missing additionalProperties: false",
                );
            }
            if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
                for (key, val) in props {
                    check_object_schemas(val, &format!("{path}.properties.{key}"));
                }
            }
            if let Some(items) = schema.get("items") {
                check_object_schemas(items, &format!("{path}.items"));
            }
        }

        let vars = Vars::new().set("{cwd}", "/tmp");
        let all = ToolRegistry::native().definitions(
            &vars,
            &DescriptionContext {
                skills: &[],
                filter: &ToolFilter::All,
            },
            true,
        );
        for def in all.as_array().unwrap() {
            let name = def["name"].as_str().unwrap();
            check_object_schemas(&def["input_schema"], name);
        }
    }

    #[test]
    fn definitions_filtered_returns_only_requested() {
        let vars = Vars::new().set("{cwd}", "/tmp");
        let filter = ToolFilter::Only(vec!["bash".into(), "read".into()]);
        let ctx = DescriptionContext {
            skills: &[],
            filter: &filter,
        };
        let filtered = ToolRegistry::native().definitions(&vars, &ctx, true);
        let names: Vec<&str> = filtered
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["bash", "read"]);
    }

    #[test_case("write",     |p: &str, _: &str| json!({"path": p, "content": "plan"})                          , |_: &str, o: &str| json!({"path": o, "content": "x"})                           ; "write")]
    #[test_case("edit",      |p: &str, _: &str| json!({"path": p, "old_string": "old", "new_string": "new"})  , |_: &str, o: &str| json!({"path": o, "old_string": "old", "new_string": "new"})  ; "edit")]
    #[test_case("multiedit", |p: &str, _: &str| json!({"path": p, "edits": [{"old_string": "old", "new_string": "new"}]}) , |_: &str, o: &str| json!({"path": o, "edits": [{"old_string": "old", "new_string": "new"}]}) ; "multiedit")]
    fn plan_mode_restricts_mutations(
        tool: &str,
        plan_input: fn(&str, &str) -> Value,
        other_input: fn(&str, &str) -> Value,
    ) {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let plan_path = dir.path().join("plan.md");
            fs::write(&plan_path, "old").unwrap();
            let other = dir.path().join("other.rs");
            fs::write(&other, "old").unwrap();
            let mode = AgentMode::Plan(plan_path.clone());
            let ctx = stub_ctx(&mode);

            let plan_str = plan_path.to_str().unwrap();
            let other_str = other.to_str().unwrap();

            ctx.file_tracker.record_read(Path::new(plan_str));
            ctx.file_tracker.record_read(Path::new(other_str));

            let registry = ToolRegistry::native();
            let blocked = tool_dispatch::run(
                registry,
                None,
                "t1".into(),
                tool,
                &other_input(plan_str, other_str),
                &ctx,
            )
            .await;
            assert!(
                blocked.is_error,
                "{tool} should be blocked on non-plan file"
            );

            let allowed = tool_dispatch::run(
                registry,
                None,
                "t2".into(),
                tool,
                &plan_input(plan_str, other_str),
                &ctx,
            )
            .await;
            assert!(!allowed.is_error, "{tool} should be allowed on plan file");
        });
    }

    #[test_case(
        json!({"pattern": "\"TODO\""}),
        json!({"pattern": "TODO"})
        ; "wrapping_quotes"
    )]
    #[test_case(
        json!({"pattern": "\"TODO\"", "path": "\"src/\""}),
        json!({"pattern": "TODO", "path": "src/"})
        ; "wrapping_quotes_multiple_fields"
    )]
    #[test_case(
        json!({"pattern": "TODO"}),
        json!({"pattern": "TODO"})
        ; "no_quotes_unchanged"
    )]
    #[test_case(
        json!({"pattern": "TODO\""}),
        json!({"pattern": "TODO\""})
        ; "trailing_quote_preserved"
    )]
    #[test_case(
        json!({"pattern": "say \"hello\""}),
        json!({"pattern": "say \"hello\""})
        ; "interior_quotes_preserved"
    )]
    #[test_case(
        json!({"limit": 10}),
        json!({"limit": 10})
        ; "non_string_values_unchanged"
    )]
    #[test_case(
        json!({"parameters": {"path": "/tmp/x"}}),
        json!({"path": "/tmp/x"})
        ; "unwrap_nested_parameters"
    )]
    #[test_case(
        json!({"oldString": "foo", "newString": "bar"}),
        json!({"old_string": "foo", "new_string": "bar"})
        ; "camel_to_snake_case"
    )]
    #[test_case(
        json!("{\"path\": \"/tmp/x\"}"),
        json!({"path": "/tmp/x"})
        ; "parse_stringified_json"
    )]
    fn sanitize_tool_input_cases(input: Value, expected: Value) {
        assert_eq!(sanitize_tool_input(&input), expected);
    }

    #[test]
    fn walk_builder_excludes_dot_git_but_shows_dotfiles() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join(".git/config"), "repositoryformatversion = 0").unwrap();
        fs::write(root.join(".git/objects/abc123"), "blob").unwrap();

        fs::write(root.join(".env"), "SECRET=42").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let root_str = root.to_string_lossy();
        let paths: Vec<String> = walk_builder(&root_str, &[])
            .unwrap()
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
            .map(|e| {
                e.path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert!(paths.contains(&"main.rs".to_string()));
        assert!(paths.contains(&".env".to_string()));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn walk_builder_excludes_dot_git_with_extra_patterns() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "stuff").unwrap();
        fs::write(root.join("lib.rs"), "pub fn foo() {}").unwrap();
        fs::write(root.join("main.py"), "print('hi')").unwrap();
        fs::write(root.join(".hidden.rs"), "// hidden").unwrap();

        let root_str = root.to_string_lossy();
        let paths: Vec<String> = walk_builder(&root_str, &["*.rs"])
            .unwrap()
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
            .map(|e| {
                e.path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert!(paths.contains(&"lib.rs".to_string()));
        assert!(paths.contains(&".hidden.rs".to_string()));
        assert!(!paths.contains(&"main.py".to_string()));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn relative_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = dirs::home_dir().unwrap();

        let cases: &[(&str, &str)] = &[
            (&format!("{}/src/main.rs", cwd.display()), "src/main.rs"),
            (&cwd.to_string_lossy(), "."),
            (
                &format!("{}/.config/something.toml", home.display()),
                "~/.config/something.toml",
            ),
            ("/etc/hosts", "/etc/hosts"),
        ];
        for (input, expected) in cases {
            assert_eq!(relative_path(input), *expected, "input: {input}");
        }

        let no_partial = format!("{}sibling/file.txt", home.display());
        assert_eq!(relative_path(&no_partial), no_partial);
    }

    #[test]
    fn resolve_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = dirs::home_dir().unwrap();

        assert_eq!(
            resolve_path("~/foo/bar").unwrap(),
            format!("{}/foo/bar", home.display())
        );
        assert_eq!(resolve_path("~").unwrap(), home.to_string_lossy());
        assert_eq!(resolve_path("/etc/hosts").unwrap(), "/etc/hosts");
        assert_eq!(
            resolve_path("src/main.rs").unwrap(),
            cwd.join("src/main.rs").to_string_lossy()
        );
    }

    #[test]
    fn multiedit_reports_inner_shape_with_path() {
        let registry = ToolRegistry::native();
        let entry = registry.get("multiedit").unwrap();
        let err = match entry.tool.parse(&json!({
            "path": "/x",
            "edits": [{"old_string": "a"}]
        })) {
            Ok(_) => panic!("expected parse error"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("edits[0].new_string"), "got: {msg}");
        assert!(msg.contains("required"), "got: {msg}");
    }

    #[test]
    fn multiedit_huge_string_error_is_bounded() {
        let huge: String = "x".repeat(50 * 1024);
        let registry = ToolRegistry::native();
        let entry = registry.get("multiedit").unwrap();
        let err = match entry.tool.parse(&json!({"path": "/x", "edits": huge})) {
            Ok(_) => panic!("expected parse error"),
            Err(e) => e,
        };
        let message = err.to_string();
        assert!(
            message.len() < schema::BOUNDED_ERR_MAX,
            "error message too long: {} bytes",
            message.len()
        );
        // Check the error fits inside `AgentError::Tool` without tripping the bounded-error
        // cap, since that is the shape the real agent loop constructs.
        let _ = AgentError::Tool {
            tool: "multiedit".into(),
            message,
        };
    }
}
