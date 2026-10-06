//! Shared plumbing for tools. The registry itself lives in `registry.rs`; this file
//! holds the helpers every tool leans on: `ToolFilter` to enable/disable per caller,
//! `Deadline` so a parent tool can cap a child's timeout, the walker that skips `.git`,
//! and `sanitize_tool_input` which patches up small JSON mistakes models make (stray
//! quotes, camelCase keys, extra wrappers). Plan mode rejects writes to
//! anything but the plan file before they reach the tool.

mod file_access;
pub mod hook;
pub mod interpreter_bridge;
pub mod registry;
pub mod schema;

pub use file_access::{FileAccess, FileKey};
pub use hook::{Authority, HookCall, HookStage, ToolHook, Verdict};
pub use registry::{
    BoxFuture, ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes,
    RegisteredTool, RegistryError, Tool, ToolAudience, ToolExecResult, ToolInvocation,
    ToolRegistry, ToolSource,
};

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use humantime::format_duration;
use maki_config::ProjectConfig;
use serde_json::Value;

use crate::agent::{CallInstructions, LoadedInstructions};
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::{AgentConfig, AgentMode, EventSender, RunLedger, SharedBuf};
use maki_config::{ModelPolicy, ToolOutputLines};
use maki_providers::Model;
use maki_providers::RequestOptions;
use maki_providers::provider::Provider;
use maki_storage::id::SessionRef;

pub(crate) const TOOL_NAME_FIELD: &str = "name";
/// What `maki.task` calls the session's own chat.
pub const MAIN_TASK_ID: &str = "main";

/// Who made a tool call. A resumed session rebuilds its state from the `ToolUse`
/// blocks in history, and those hold the model's own calls only, so a nested
/// call must never change what the next request carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallOrigin {
    /// Emitted by the model and recorded in history under its own id.
    Model,
    /// Made on the model's behalf and invisible to history: batch children,
    /// `code_execution` scripts, Lua `call_tool`.
    Nested,
}

impl CallOrigin {
    pub fn is_model(self) -> bool {
        matches!(self, Self::Model)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Nested => "nested",
        }
    }
}

pub struct DescriptionContext<'a> {
    pub filter: &'a ToolFilter,
    pub audience: ToolAudience,
    pub workflow: bool,
    /// Whether the session being described can reach MCP tools.
    pub mcp: bool,
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

    pub fn from_config(config: &AgentConfig, model: &Model, extra_exclude: &[&str]) -> Self {
        let base = if config.allowed_tools.is_empty() {
            Self::All
        } else {
            Self::Only(
                config
                    .allowed_tools
                    .iter()
                    .filter(|s| is_builtin_tool(s))
                    .cloned()
                    .collect(),
            )
        };
        let mut exclude: Vec<&str> = extra_exclude.to_vec();
        exclude.extend(capability_exclusions(model));
        exclude.extend(config.disabled_tools.iter().map(|s| s.as_str()));
        base.excluding(&exclude)
    }
}

/// The tool array one run sends to the model, paired with the filter that
/// produced it. A run context carries the pair, never a loose array, so the
/// names dispatch offers in-process (`tool_dispatch::callable`, and through it
/// the `code_execution` sandbox) cannot drift from what the model was shown.
#[derive(Clone)]
pub struct RequestTools {
    definitions: Value,
    filter: Arc<ToolFilter>,
}

impl Default for RequestTools {
    fn default() -> Self {
        Self {
            definitions: Value::Array(Vec::new()),
            filter: Arc::default(),
        }
    }
}

impl RequestTools {
    /// The one place a run's tool array is built. Host exclusions (a client
    /// that cannot service `question`) go through here and nowhere else, so
    /// the array and the filter always agree on them.
    pub fn build(
        registry: &ToolRegistry,
        vars: &crate::template::Vars,
        model: &Model,
        config: &AgentConfig,
        excluded: &[&str],
        workflow: bool,
        mcp: bool,
    ) -> Self {
        let filter = ToolFilter::from_config(config, model, excluded);
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow,
            mcp,
        };
        Self {
            definitions: registry.definitions(vars, &ctx, model.supports_tool_examples()),
            filter: Arc::new(filter),
        }
    }

    /// For an array the host built itself, like a Lua subagent publishing what
    /// its caller picked. The filter is read back off that array, so the names
    /// the model was shown and the names a script may reach are one set, and
    /// the caller's `only`/`except` never has to be passed twice. MCP names are
    /// never matched against this filter and stay reachable.
    ///
    /// An array nobody can read a name out of says nothing about intent, so it
    /// falls back to the config's filter instead of an `Only` of nothing that
    /// would leave the session unable to do anything. An array that is
    /// genuinely empty is an answer, and is kept as one.
    pub fn assembled(definitions: Value, config: &AgentConfig, model: &Model) -> Self {
        let published: Option<Vec<String>> = definitions.as_array().and_then(|defs| {
            let names: Vec<String> = defs
                .iter()
                .filter_map(|def| def[TOOL_NAME_FIELD].as_str().map(str::to_owned))
                .collect();
            (defs.is_empty() || !names.is_empty()).then_some(names)
        });
        let filter = published.map_or_else(
            || ToolFilter::from_config(config, model, &[]),
            ToolFilter::Only,
        );
        Self {
            definitions,
            filter: Arc::new(filter),
        }
    }

    pub fn definitions(&self) -> &Value {
        &self.definitions
    }

    pub fn filter(&self) -> &Arc<ToolFilter> {
        &self.filter
    }
}

/// One gate for every definitions builder (main loop, headless, Lua): a model
/// without vision never learns `view_image` exists.
pub fn capability_exclusions(model: &Model) -> &'static [&'static str] {
    if model.supports_vision() {
        &[]
    } else {
        &[VIEW_IMAGE_TOOL_NAME]
    }
}

/// A tool is enabled unless named in `disabled_tools` (config, or the raw
/// list a Lua caller holds, e.g. `maki.api.get_tools`).
pub fn is_tool_enabled(disabled_tools: &[String], name: &str) -> bool {
    !disabled_tools.iter().any(|s| s == name)
}

pub const BASH_TOOL_NAME: &str = "bash";
pub const CODE_EXECUTION_TOOL_NAME: &str = "code_execution";
pub const EDIT_TOOL_NAME: &str = "edit";
pub const GLOB_TOOL_NAME: &str = "glob";
pub const GREP_TOOL_NAME: &str = "grep";
pub const MULTIEDIT_TOOL_NAME: &str = "multiedit";
pub const QUESTION_TOOL_NAME: &str = "question";
pub const READ_TOOL_NAME: &str = "read";
pub const TASK_TOOL_NAME: &str = "task";
pub const TODOWRITE_TOOL_NAME: &str = "todo_write";
pub const VIEW_IMAGE_TOOL_NAME: &str = "view_image";
pub const WRITE_TOOL_NAME: &str = "write";

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

pub fn timeout_annotation(secs: u64) -> String {
    let d = Duration::from_secs(secs);
    let formatted: String = format_duration(d)
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    format!("{formatted} timeout")
}

pub type LocalToolResult = BoxFuture<'static, Result<String, String>>;
pub type LocalToolFn = Arc<dyn Fn(Value, ToolContext) -> LocalToolResult + Send + Sync>;
pub type LocalTools = Arc<HashMap<String, LocalTool>>;

/// A tool the session's host supplies: an ACP client tool, a subagent's
/// `structured_output`. Dispatch puts it ahead of the registry, so it carries
/// its own audience: a host tool must not reach a sandbox by wearing the name
/// of a registry tool that may.
#[derive(Clone)]
pub struct LocalTool {
    pub handler: LocalToolFn,
    pub audience: ToolAudience,
}

/// Coerces a closure into a [`LocalTool`]; the bound gives the boxed
/// future a coercion target that `Arc::new` alone does not.
pub fn local_tool<F>(audience: ToolAudience, f: F) -> LocalTool
where
    F: Fn(Value, ToolContext) -> LocalToolResult + Send + Sync + 'static,
{
    LocalTool {
        handler: Arc::new(f),
        audience,
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub provider: Arc<dyn Provider>,
    pub model: Arc<Model>,
    pub event_tx: EventSender,
    pub mode: AgentMode,
    /// The session this run belongs to. A subagent inherits its parent's,
    /// so a tool can always tell which conversation it is serving. `None`
    /// when there is no session at all, like the `maki index` one-shot.
    pub session_id: Option<SessionRef>,
    /// `None` in the agent that owns the session ([`MAIN_TASK_ID`]), the
    /// spawning call's `tool_use_id` in a subagent. A subagent shares
    /// `session_id` with its parent on purpose (provider affinity, hooks,
    /// otel), so this is what tells their chats apart.
    pub task_id: Option<Arc<str>>,
    pub tool_use_id: Option<String>,
    pub user_response_rx: Option<Arc<smol::lock::Mutex<flume::Receiver<String>>>>,
    /// Session-wide: a file the model has already seen is never injected again.
    pub loaded_instructions: LoadedInstructions,
    /// Per model call, shared with its nested calls.
    pub call_instructions: CallInstructions,
    pub cancel: CancelToken,
    pub mcp: Option<McpSession>,
    pub deadline: Deadline,
    pub config: AgentConfig,
    pub tool_filter: Arc<ToolFilter>,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: maki_providers::Timeouts,
    pub file_access: Arc<FileAccess>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub opts: RequestOptions,
    pub subagent_cancels: Arc<CancelMap<String>>,
    /// Shared with the run that spawned this tool, so a subagent's spend lands
    /// in the parent turn's totals.
    pub ledger: Arc<RunLedger>,
    pub registry: Arc<ToolRegistry>,
    pub workflow: bool,
    pub audience: ToolAudience,
    pub local_tools: LocalTools,
    /// Streams a dispatched child's live bufs and annotations back to the
    /// caller (`maki.agent.call_tool` with `on_live_buf`/`on_annotation`).
    /// Never inherited: `to_tool_context` clears it, and each caller sets
    /// it for its own call only.
    pub live_sink: Option<flume::Sender<ToolLive>>,
    pub model_policy: Arc<ModelPolicy>,
}

/// Live progress of a dispatched child tool, streamed while it runs.
pub enum ToolLive {
    Buf(Arc<SharedBuf>),
    Annotation(String),
    Usage(String),
}

static CWD: LazyLock<Option<PathBuf>> = LazyLock::new(|| env::current_dir().ok());
static HOME: LazyLock<Option<PathBuf>> = LazyLock::new(maki_storage::paths::home);

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

pub fn truncate_output(text: String, max_lines: usize, max_bytes: usize) -> String {
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

pub fn is_builtin_tool(name: &str) -> bool {
    let bundled_tool = maki_config::DEFAULT_BUILTINS.contains(&name)
        && !maki_config::PROVIDER_BUILTINS.contains(&name);
    bundled_tool || maki_config::EDIT_SUB_TOOLS.contains(&name)
}

pub fn all_builtin_tool_names() -> Vec<&'static str> {
    maki_config::DEFAULT_BUILTINS
        .iter()
        .filter(|name| !maki_config::PROVIDER_BUILTINS.contains(name))
        .chain(maki_config::EDIT_SUB_TOOLS.iter())
        .copied()
        .collect()
}

use maki_providers::{Message, ProviderEvent, StreamResponse};

struct NullProvider;

impl Provider for NullProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a Value,
        _: &'a flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, crate::AgentError>> {
        Box::pin(async { unimplemented!() })
    }

    fn list_models(
        &self,
    ) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, crate::AgentError>> {
        Box::pin(async { unimplemented!() })
    }
}

pub fn interpreter_ctx(
    mode: &AgentMode,
    event_tx: &EventSender,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_access: Arc<FileAccess>,
    user_response_rx: Option<Arc<smol::lock::Mutex<flume::Receiver<String>>>>,
    registry: Arc<ToolRegistry>,
) -> ToolContext {
    static PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| Arc::new(NullProvider));
    static MODEL: LazyLock<Arc<Model>> =
        LazyLock::new(|| Arc::new(Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()));
    ToolContext {
        provider: Arc::clone(&PROVIDER),
        model: Arc::clone(&MODEL),
        event_tx: event_tx.clone(),
        mode: mode.clone(),
        session_id: None,
        task_id: None,
        tool_use_id: None,
        user_response_rx,
        loaded_instructions: LoadedInstructions::new(),
        call_instructions: CallInstructions::default(),
        cancel,
        mcp: None,
        deadline: Deadline::None,
        config: AgentConfig::default(),
        tool_filter: Arc::default(),
        tool_output_lines: ToolOutputLines::default(),
        permissions,
        timeouts: maki_providers::Timeouts::default(),
        file_access,
        prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
        opts: RequestOptions::default(),
        subagent_cancels: Arc::new(CancelMap::new()),
        ledger: Arc::default(),
        registry,
        workflow: false,
        audience: ToolAudience::MAIN,
        local_tools: LocalTools::default(),
        live_sink: None,
        model_policy: Arc::new(ModelPolicy::default()),
    }
}

/// Minimal ToolContext for CLI one-shot tool execution (e.g. `maki index`).
/// Allows everything, sends events to a dummy channel, uses no model.
pub fn cli_tool_ctx() -> ToolContext {
    let (tx, _rx) = flume::unbounded::<crate::Envelope>();
    let event_tx = crate::EventSender::new(tx, 0);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    interpreter_ctx(
        &AgentMode::Build,
        &event_tx,
        CancelToken::none(),
        Arc::new(PermissionManager::new(
            maki_config::PermissionsConfig {
                default: maki_config::DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            cwd.clone(),
            ProjectConfig::discover(&cwd),
            Arc::default(),
        )),
        FileAccess::fresh(),
        None,
        Arc::clone(ToolRegistry::global_arc()),
    )
}

pub mod test_support {
    use std::borrow::Cow;

    use crate::{Envelope, EventSender, ToolOutput};

    use super::*;

    pub const GUARDED_TOOL_NAME: &str = "guarded_mock";

    /// Registry and routing tests care about the name and audience only, never
    /// about what the tool returns.
    pub fn mock_tool(name: &str, audience: ToolAudience) -> Arc<dyn registry::Tool> {
        mock_tool_with_schema(
            name,
            audience,
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        )
    }

    pub fn mock_tool_with_schema(
        name: &str,
        audience: ToolAudience,
        schema: Value,
    ) -> Arc<dyn registry::Tool> {
        Arc::new(MockTool {
            name: name.to_owned(),
            audience,
            schema,
        })
    }

    struct MockTool {
        name: String,
        audience: ToolAudience,
        schema: Value,
    }

    struct MockInvocation;

    impl registry::ToolInvocation for MockInvocation {
        fn start_header(&self) -> registry::HeaderFuture {
            registry::HeaderFuture::Ready(registry::HeaderResult::plain("mock".into()))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> registry::ExecFuture<'a> {
            Box::pin(async { Ok(ToolOutput::Plain(String::new().into())).into() })
        }
    }

    impl registry::Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "mock tool".into()
        }
        fn schema(&self) -> Value {
            self.schema.clone()
        }
        fn audience(&self) -> ToolAudience {
            self.audience
        }
        fn parse(
            &self,
            _input: &Value,
        ) -> Result<Box<dyn registry::ToolInvocation>, registry::ParseError> {
            Ok(Box::new(MockInvocation))
        }
    }

    pub struct GuardedMock;

    struct GuardedInvocation;

    impl registry::ToolInvocation for GuardedInvocation {
        fn start_header(&self) -> registry::HeaderFuture {
            registry::HeaderFuture::Ready(registry::HeaderResult::plain("mock".into()))
        }
        fn permission_scopes(&self) -> registry::BoxFuture<'_, Option<registry::PermissionScopes>> {
            Box::pin(std::future::ready(Some(
                registry::PermissionScopes::single("guarded".into()),
            )))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> registry::ExecFuture<'a> {
            Box::pin(async {
                registry::ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            })
        }
    }

    impl registry::Tool for GuardedMock {
        fn name(&self) -> &str {
            GUARDED_TOOL_NAME
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "guarded mock".into()
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn parse(
            &self,
            _input: &Value,
        ) -> Result<Box<dyn registry::ToolInvocation>, registry::ParseError> {
            Ok(Box::new(GuardedInvocation))
        }
    }

    static TEST_PERMISSIONS: LazyLock<Arc<PermissionManager>> = LazyLock::new(|| {
        Arc::new(PermissionManager::new(
            maki_config::PermissionsConfig {
                default: maki_config::DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp"),
            ProjectConfig::discover(Path::new("/tmp")),
            Arc::default(),
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
            FileAccess::fresh(),
            None,
            Arc::new(ToolRegistry::new()),
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
            FileAccess::fresh(),
            None,
            Arc::new(ToolRegistry::new()),
        );
        ctx.tool_use_id = None;
        ctx
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const TEST_MODEL_SPEC: &str = "anthropic/claude-opus-4-8";

    /// The array a host hands in is the whole answer, but only where names can
    /// be read out of it. Something unreadable must not quietly come out as
    /// "no tools at all".
    #[test_case(serde_json::json!([{ TOOL_NAME_FIELD: READ_TOOL_NAME }]), true,  false ; "published_names_bind_the_filter")]
    #[test_case(serde_json::json!([]),                                    false, false ; "empty_array_publishes_nothing")]
    #[test_case(serde_json::json!([{ "description": "nameless" }]),       true,  true  ; "unreadable_array_falls_back_to_config")]
    #[test_case(serde_json::json!({}),                                    true,  true  ; "non_array_falls_back_to_config")]
    fn assembled_reads_its_filter_off_the_published_array(
        definitions: Value,
        read_matches: bool,
        write_matches: bool,
    ) {
        let model = Model::from_spec(TEST_MODEL_SPEC).unwrap();
        let tools = RequestTools::assembled(definitions, &AgentConfig::default(), &model);
        assert_eq!(tools.filter().matches(READ_TOOL_NAME), read_matches);
        assert_eq!(tools.filter().matches(WRITE_TOOL_NAME), write_matches);
    }

    #[test_case(true  ; "vision_model_keeps_view_image")]
    #[test_case(false ; "text_only_model_loses_view_image")]
    fn from_config_gates_view_image_on_vision(vision: bool) {
        let mut model = Model::from_spec(TEST_MODEL_SPEC).unwrap();
        model.supports_vision_override = Some(vision);
        let filter = ToolFilter::from_config(&AgentConfig::default(), &model, &[]);
        assert_eq!(filter.matches(VIEW_IMAGE_TOOL_NAME), vision);
        assert!(
            filter.matches(READ_TOOL_NAME),
            "unrelated tools stay enabled"
        );
    }

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
        let clamped = Deadline::after(Duration::from_secs(3600))
            .cap_timeout(7200)
            .unwrap();
        assert!(clamped <= 3600, "expected <= 3600, got {clamped}");
    }

    #[test]
    fn cap_timeout_expired() {
        let expired = Deadline::At(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        assert_eq!(expired.cap_timeout(120).unwrap_err(), DEADLINE_EXCEEDED);
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
    fn relative_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = maki_storage::paths::home().unwrap();

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
    fn all_builtin_names_no_duplicates() {
        let names = all_builtin_tool_names();
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            assert!(seen.insert(name), "duplicate builtin tool name: {name}");
        }
    }

    #[test]
    fn provider_plugins_are_not_tools() {
        for &name in maki_config::PROVIDER_BUILTINS {
            assert!(maki_config::DEFAULT_BUILTINS.contains(&name), "{name}");
            assert!(!is_builtin_tool(name), "{name}");
            assert!(!all_builtin_tool_names().contains(&name), "{name}");
        }
    }
}
