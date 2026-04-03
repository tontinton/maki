//! Tool registry and dispatch.
//!
//! `register_tools!` enforces unique names at compile time and generates `ToolCall` + dispatch glue.
//! Plan mode only permits writes to the designated plan file; all other write-like tools are rejected.
//! `Deadline` lets batch/code_execution cap child tool timeouts without exceeding their own budget.
//! `RESEARCH_SUBAGENT_TOOLS` is read-only; `GENERAL_SUBAGENT_TOOLS` is the full set.
//! `sanitize_tool_input` fixes common LLM JSON mistakes: stray quotes, type mismatches, embedded fields.

mod bash;
mod batch;
mod code_execution;
mod edit;
mod find_symbol;
mod fuzzy_replace;
mod glob;
mod grep;
mod index;
pub mod memory;
mod multiedit;
mod question;
mod read;
mod skill;
mod task;
mod todowrite;
mod webfetch;
mod websearch;
mod write;

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use humantime::format_duration;
use serde_json::{Value, json};
use std::future::Future;
use tracing::{error, info, warn};

use crate::agent::LoadedInstructions;
use crate::cancel::CancelToken;
use crate::mcp::McpManager;
use crate::permissions::PermissionManager;
use crate::skill::Skill;
use crate::template::Vars;
use crate::{
    AgentConfig, AgentError, AgentMode, EventSender, NO_FILES_FOUND, ToolDoneEvent, ToolInput,
    ToolOutput, ToolStartEvent,
};
use maki_providers::Model;
use maki_providers::provider::Provider;

pub struct DescriptionContext<'a> {
    pub skills: &'a [Skill],
}

#[derive(Debug, Clone, Default)]
pub enum ToolFilter {
    #[default]
    All,
    Only(Vec<&'static str>),
    AllExcept(Vec<&'static str>),
}

impl ToolFilter {
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(allowed) => allowed.contains(&name),
            Self::AllExcept(blocked) => !blocked.contains(&name),
        }
    }
}

pub const BASH_TOOL_NAME: &str = bash::Bash::NAME;
pub const BATCH_TOOL_NAME: &str = batch::Batch::NAME;
pub const EDIT_TOOL_NAME: &str = edit::Edit::NAME;
pub const FIND_SYMBOL_TOOL_NAME: &str = find_symbol::FindSymbol::NAME;
pub const GLOB_TOOL_NAME: &str = glob::Glob::NAME;
pub const GREP_TOOL_NAME: &str = grep::Grep::NAME;
pub const INDEX_TOOL_NAME: &str = index::Index::NAME;
pub const MULTIEDIT_TOOL_NAME: &str = multiedit::MultiEdit::NAME;
pub const QUESTION_TOOL_NAME: &str = question::Question::NAME;
pub const READ_TOOL_NAME: &str = read::Read::NAME;
pub const SKILL_TOOL_NAME: &str = skill::SkillTool::NAME;
pub const TASK_TOOL_NAME: &str = task::Task::NAME;
pub const TODOWRITE_TOOL_NAME: &str = todowrite::TodoWrite::NAME;
pub const WEBFETCH_TOOL_NAME: &str = webfetch::WebFetch::NAME;
pub const WEBSEARCH_TOOL_NAME: &str = websearch::WebSearch::NAME;
pub const WRITE_TOOL_NAME: &str = write::Write::NAME;
pub const MEMORY_TOOL_NAME: &str = memory::Memory::NAME;
pub const CODE_EXECUTION_TOOL_NAME: &str = code_execution::CodeExecution::NAME;

pub(crate) const INTERPRETER_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "multiedit",
    "glob",
    "grep",
    "bash",
    "webfetch",
    "websearch",
    "find_symbol",
];

pub(crate) const RESEARCH_SUBAGENT_TOOLS: &[&str] = &[
    "bash",
    "read",
    "index",
    "glob",
    "grep",
    "webfetch",
    "batch",
    "code_execution",
    "find_symbol",
];

pub(crate) const GENERAL_SUBAGENT_TOOLS: &[&str] = &[
    "bash",
    "read",
    "index",
    "write",
    "edit",
    "multiedit",
    "glob",
    "grep",
    "webfetch",
    "batch",
    "code_execution",
    "memory",
    "find_symbol",
];

const PLAN_WRITE_RESTRICTED: &str = "write restricted to plan file in plan mode";
const DEADLINE_EXCEEDED: &str = "timeout exceeded";

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
    pub mcp: Option<Arc<McpManager>>,
    pub deadline: Deadline,
    pub config: AgentConfig,
    pub permissions: Arc<PermissionManager>,
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

pub(crate) fn line_at_offset(content: &str, offset: usize) -> usize {
    content[..offset].matches('\n').count() + 1
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

fn format_examples_as_text(examples: &[Value]) -> Option<String> {
    if examples.is_empty() {
        return None;
    }
    let mut text = String::from("Examples:");
    for ex in examples {
        if let Some(code) = ex.get("code").and_then(|c| c.as_str()) {
            text.push_str("\n```\n");
            text.push_str(code);
            text.push_str("\n```");
        }
    }
    Some(text)
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

pub(crate) fn build_interpreter_tools_description() -> String {
    let mut desc =
        String::from("\n\nAvailable tools (called as Python functions with keyword arguments):\n");
    for name in INTERPRETER_TOOLS {
        if let Some(schema) = ToolCall::schema_for(name) {
            desc.push_str(&format_tool_signature(name, &schema));
            desc.push('\n');
        }
    }
    desc
}

pub(crate) trait ToolDefaults {
    fn start_input(&self) -> Option<ToolInput> {
        None
    }
    fn start_output(&self) -> Option<ToolOutput> {
        None
    }
    fn start_annotation(&self) -> Option<String> {
        None
    }
    fn mutable_path(&self) -> Option<&str> {
        None
    }
    fn permission(&self) -> Option<String> {
        None
    }
    fn augment_description(_description: &mut String, _ctx: &DescriptionContext) {}
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
        let new_val = val
            .as_str()
            .map(|s| strip_stray_quotes(key, s))
            .unwrap_or_else(|| val.clone());
        if norm_key != *key {
            warn!(original = %key, normalized = %norm_key, "normalized camelCase key to snake_case");
        }
        out.insert(norm_key, new_val);
    }
    Value::Object(out)
}

fn strip_stray_quotes(field: &str, s: &str) -> Value {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        && !inner.contains('"')
    {
        warn!(field = %field, original = %s, fixed = %inner, "stripped stray quotes from tool param");
        return Value::String(inner.to_string());
    }
    Value::String(s.to_string())
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

fn coerce_value(val: &Value, field: &str, json_type: &str) -> Option<Value> {
    let coerced = match (val, json_type) {
        (Value::String(s), "integer") => parse_number::<i64>(s).map(Value::from),
        (Value::String(s), "number") => parse_number::<f64>(s).map(Value::from),
        (Value::String(s), "boolean") => match s.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }?;
    warn!(field = %field, original = %val, fixed = %coerced, "coerced tool param type");
    Some(coerced)
}

pub(crate) fn deserialize_with_coercion<T: serde::de::DeserializeOwned>(
    raw: &Value,
    field: &str,
    json_type: &str,
) -> Result<T, String> {
    serde_json::from_value::<T>(raw.clone()).or_else(|_| {
        coerce_value(raw, field, json_type)
            .and_then(|c| serde_json::from_value(c).ok())
            .ok_or_else(|| {
                let got_type = match raw {
                    Value::String(_) => "string",
                    Value::Number(_) => "number",
                    Value::Bool(_) => "boolean",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                    Value::Null => "null",
                };
                format!(
                    "The parameter '{}' type is expected as '{}' but provided as '{}'",
                    field, json_type, got_type
                )
            })
    })
}

fn parse_number<T: std::str::FromStr>(s: &str) -> Option<T> {
    s.trim().parse().ok()
}

macro_rules! register_tools {
    ($($Variant:ident($inner:path)),+ $(,)?) => {
        $(const _: () = { fn _assert_defaults<T: ToolDefaults>() {} fn _check() { _assert_defaults::<$inner>() } };)+

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

        #[derive(Debug, Clone)]
        pub enum ToolCall {
            $($Variant(Box<$inner>)),+
        }

        macro_rules! dispatch {
            ($self:expr, |$t:ident| $body:expr) => {
                match $self { $(ToolCall::$Variant($t) => $body),+ }
            };
        }

        impl ToolCall {
            pub fn from_api(name: &str, input: &Value) -> Result<Self, AgentError> {
                match name {
                    $(<$inner>::NAME => {
                        <$inner>::parse_input(input)
                            .map(|v| ToolCall::$Variant(Box::new(v)))
                            .map_err(|msg| AgentError::Tool { tool: name.to_string(), message: msg })
                    })+
                    _ => Err(AgentError::Tool {
                        tool: name.to_string(),
                        message: format!("unknown variant `{name}`"),
                    })
                }
            }

            pub fn name(&self) -> &'static str {
                match self {
                    $(ToolCall::$Variant(_) => <$inner>::NAME),+
                }
            }

            pub fn name_static(name: &str) -> Option<&'static str> {
                match name {
                    $(<$inner>::NAME => Some(<$inner>::NAME),)+
                    _ => None,
                }
            }

            pub fn start_summary(&self) -> String {
                dispatch!(self, |t| t.start_summary())
            }

            pub fn start_annotation(&self) -> Option<String> {
                dispatch!(self, |t| t.start_annotation())
            }

            pub fn start_input(&self) -> Option<ToolInput> {
                dispatch!(self, |t| t.start_input())
            }

            pub fn start_event(&self, id: String) -> ToolStartEvent {
                dispatch!(self, |t| ToolStartEvent {
                    id,
                    tool: self.name(),
                    summary: t.start_summary(),
                    annotation: t.start_annotation(),
                    input: t.start_input(),
                    output: t.start_output(),
                })
            }

            pub fn execute<'a>(&'a self, ctx: &'a ToolContext, id: String) -> Pin<Box<dyn Future<Output = ToolDoneEvent> + Send + 'a>> {
                Box::pin(async move {
                    if let Some(path) = self.mutable_path()
                        && let AgentMode::Plan(plan_path) = &ctx.mode
                        && Path::new(path) != plan_path.as_path()
                    {
                        return ToolDoneEvent {
                            id,
                            tool: self.name(),
                            output: ToolOutput::Plain(PLAN_WRITE_RESTRICTED.into()),
                            is_error: true,
                        };
                    }

                    if let Some(scope) = dispatch!(self, |t| t.permission()) {
                        if let Err(e) = ctx.permissions.enforce(
                            self.name(),
                            &scope,
                            &ctx.event_tx,
                            ctx.user_response_rx.as_deref(),
                            &id,
                            &ctx.cancel,
                        ).await {
                            return ToolDoneEvent {
                                id,
                                tool: self.name(),
                                output: ToolOutput::Plain(e.to_string()),
                                is_error: true,
                            };
                        }
                    }

                    let start = Instant::now();
                    let result = match self {
                        $(ToolCall::$Variant(inner) => inner.execute(ctx).await),+
                    };
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let (output, is_error) = match result {
                        Ok(o) => (o, false),
                        Err(e) => {
                            error!(tool = self.name(), duration_ms, error = %e, "tool execution failed");
                            (ToolOutput::Plain(e), true)
                        }
                    };
                    if !is_error {
                        info!(tool = self.name(), duration_ms, "tool completed");
                    }
                    ToolDoneEvent { id, tool: self.name(), output, is_error }
                })
            }

            fn mutable_path(&self) -> Option<&str> {
                dispatch!(self, |t| t.mutable_path())
            }

            pub fn schema_for(name: &str) -> Option<Value> {
                match name {
                    $(<$inner>::NAME => Some(<$inner>::schema()),)+
                    _ => None,
                }
            }

            fn all_defs(vars: &Vars, skills: &[Skill], supports_examples: bool) -> Vec<(&'static str, Value)> {
                let ctx = DescriptionContext { skills };
                vec![
                    $((<$inner>::NAME, {
                        let mut description = vars.apply(<$inner>::DESCRIPTION).into_owned();
                        <$inner>::augment_description(&mut description, &ctx);
                        let mut def = json!({
                            "name": <$inner>::NAME,
                            "description": &description,
                            "input_schema": <$inner>::schema()
                        });
                        if let Some(json) = <$inner>::EXAMPLES {
                            let examples: Vec<Value> = serde_json::from_str(json)
                                .expect(concat!("invalid EXAMPLES JSON for ", stringify!($inner)));
                            if supports_examples {
                                def["input_examples"] = Value::Array(examples);
                            } else if let Some(text) = format_examples_as_text(&examples) {
                                def["description"] = Value::String(format!("{}\n\n{}", description, text));
                            }
                        }
                        def
                    })),+
                ]
            }

            pub fn static_name(name: &str) -> Option<&'static str> {
                match name {
                    $(<$inner>::NAME => Some(<$inner>::NAME),)+
                    _ => None,
                }
            }

            pub fn all_names() -> &'static [&'static str] {
                &[$(<$inner>::NAME),+]
            }

            pub fn definitions_with_filter(vars: &Vars, skills: &[Skill], filter: &ToolFilter, supports_examples: bool) -> Value {
                Value::Array(
                    Self::all_defs(vars, skills, supports_examples).into_iter()
                        .filter(|(name, _)| filter.matches(name))
                        .map(|(_, def)| def)
                        .collect()
                )
            }

            pub fn definitions(vars: &Vars, skills: &[Skill], supports_examples: bool) -> Value {
                Self::definitions_with_filter(vars, skills, &ToolFilter::All, supports_examples)
            }
        }
    };
}

register_tools! {
    Bash(bash::Bash),
    Read(read::Read),
    Write(write::Write),
    Edit(edit::Edit),
    MultiEdit(multiedit::MultiEdit),
    Glob(glob::Glob),
    Grep(grep::Grep),
    Index(index::Index),
    FindSymbol(find_symbol::FindSymbol),
    Question(question::Question),
    TodoWrite(todowrite::TodoWrite),
    WebFetch(webfetch::WebFetch),
    WebSearch(websearch::WebSearch),
    Skill(skill::SkillTool),
    Task(task::Task),
    Batch(batch::Batch),
    CodeExecution(code_execution::CodeExecution),
    Memory(memory::Memory),
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
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async { unimplemented!() })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async { unimplemented!() })
    }
}

pub(crate) fn interpreter_ctx(
    mode: &AgentMode,
    event_tx: &EventSender,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
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
        user_response_rx: None,
        skills: Arc::clone(&SKILLS),
        loaded_instructions: LoadedInstructions::new(),
        cancel,
        mcp: None,
        deadline: Deadline::None,
        config: AgentConfig::default(),
        permissions,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
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

    pub(crate) fn stub_ctx_with(
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
        );
        ctx.tool_use_id = tool_use_id.map(String::from);
        ctx
    }

    pub(crate) fn stub_ctx(mode: &AgentMode) -> ToolContext {
        stub_ctx_with(mode, None, None)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::test_support::stub_ctx;
    use super::*;

    const LINE_LIMIT: usize = 500;

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
            let ToolOutput::GrepResult { entries } = &out else {
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

    #[test]
    fn from_api_unknown_tool_returns_error() {
        let err = ToolCall::from_api("nonexistent_tool", &json!({})).unwrap_err();
        let AgentError::Tool { tool, .. } = err else {
            panic!("expected AgentError::Tool, got {err:?}");
        };
        assert_eq!(tool, "nonexistent_tool");
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
        let all = ToolCall::definitions(&vars, &[], true);
        for def in all.as_array().unwrap() {
            let name = def["name"].as_str().unwrap();
            check_object_schemas(&def["input_schema"], name);
        }
    }

    #[test]
    fn definitions_filtered_returns_only_requested() {
        let vars = Vars::new().set("{cwd}", "/tmp");
        let filter = ToolFilter::Only(vec!["bash", "read"]);
        let filtered = ToolCall::definitions_with_filter(&vars, &[], &filter, true);
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

            let blocked = ToolCall::from_api(tool, &other_input(plan_str, other_str)).unwrap();
            let result = blocked.execute(&ctx, "t1".into()).await;
            assert!(result.is_error, "{tool} should be blocked on non-plan file");

            let allowed = ToolCall::from_api(tool, &plan_input(plan_str, other_str)).unwrap();
            let result = allowed.execute(&ctx, "t2".into()).await;
            assert!(!result.is_error, "{tool} should be allowed on plan file");
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

    #[test_case(Value::String("30".into()),                       "integer", Some(json!(30))    ; "string_to_integer")]
    #[test_case(Value::String("-5".into()),                       "integer", Some(json!(-5))    ; "negative_string_to_integer")]
    #[test_case(Value::String(" 42".into()),                      "integer", Some(json!(42))    ; "leading_whitespace")]
    #[test_case(Value::String("30, \"offset\": 2075".into()),     "integer", None               ; "embedded_trailing_fields_rejected")]
    #[test_case(Value::String("-3-5".into()),                     "integer", None               ; "malformed_number_rejected")]
    #[test_case(Value::String("true".into()),                     "boolean", Some(json!(true))  ; "string_true_to_bool")]
    #[test_case(Value::String("false".into()),                    "boolean", Some(json!(false)) ; "string_false_to_bool")]
    #[test_case(Value::String("1.25".into()),                      "number",  Some(json!(1.25))  ; "string_to_float")]
    #[test_case(Value::String("hello".into()),                    "integer", None               ; "non_numeric_string")]
    #[test_case(json!(30),                                        "integer", None               ; "already_correct_type")]
    #[test_case(Value::String("".into()),                         "integer", None               ; "empty_string")]
    fn coerce_value_cases(val: Value, json_type: &str, expected: Option<Value>) {
        assert_eq!(coerce_value(&val, "test_field", json_type), expected);
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
}
