//! Single source of truth for all tools (native, MCP, Lua). One registry, one lookup
//! path, no parallel lists that can drift.

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use arc_swap::ArcSwap;
use bitflags::bitflags;
use serde_json::{Value, json};

use crate::template::Vars;
use crate::{BufferSnapshot, ToolInput as ToolInputEvent, ToolOutput};

use super::{DescriptionContext, ToolContext};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ToolAudience: u8 {
        const MAIN         = 0b0001;
        const RESEARCH_SUB = 0b0010;
        const GENERAL_SUB  = 0b0100;
        const INTERPRETER  = 0b1000;
    }
}

impl Default for ToolAudience {
    fn default() -> Self {
        Self::all()
    }
}

#[derive(Clone, Debug)]
pub enum ToolSource {
    Native,
    Mcp { server: Arc<str> },
    Lua { plugin: Arc<str> },
}

impl ToolSource {
    pub fn as_log_field(&self) -> Cow<'static, str> {
        match self {
            Self::Native => Cow::Borrowed("native"),
            Self::Mcp { server } => Cow::Owned(format!("mcp:{server}")),
            Self::Lua { plugin } => Cow::Owned(format!("lua:{plugin}")),
        }
    }
}

pub type ParseError = super::schema::ToolInputError;

pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum HeaderResult {
    Plain(String),
    Styled(BufferSnapshot),
}

impl HeaderResult {
    pub fn plain(text: String) -> Self {
        Self::Plain(text)
    }

    pub fn text(&self) -> String {
        match self {
            Self::Plain(t) => t.clone(),
            Self::Styled(snap) => snap.first_line_text(),
        }
    }

    pub fn snapshot(self) -> Option<BufferSnapshot> {
        match self {
            Self::Plain(_) => None,
            Self::Styled(snap) => Some(snap),
        }
    }
}

pub enum HeaderFuture {
    Ready(HeaderResult),
    Pending {
        fallback: String,
        fut: Pin<Box<dyn Future<Output = HeaderResult> + Send>>,
    },
}

impl HeaderFuture {
    pub fn into_ready(self) -> HeaderResult {
        match self {
            Self::Ready(r) => r,
            Self::Pending { fallback, .. } => HeaderResult::plain(fallback),
        }
    }
}

impl Future for HeaderFuture {
    type Output = HeaderResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<HeaderResult> {
        match self.get_mut() {
            Self::Ready(r) => Poll::Ready(std::mem::replace(r, HeaderResult::plain(String::new()))),
            Self::Pending { fut, .. } => fut.as_mut().poll(cx),
        }
    }
}

/// Holds the parsed input so start-event and `execute` share one parse pass.
/// `permission_scope` and `mutable_path` belong here because only the parsed
/// call knows which file it will touch.
pub trait ToolInvocation: Send + Sync {
    fn start_header(&self) -> HeaderFuture;
    fn start_annotation(&self) -> Option<String> {
        None
    }
    fn start_input(&self) -> Option<ToolInputEvent> {
        None
    }
    fn start_output(&self) -> Option<ToolOutput> {
        None
    }
    fn mutable_path(&self) -> Option<&Path> {
        None
    }
    fn permission_scope(&self) -> Option<String> {
        None
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a>;
}

pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self, ctx: &DescriptionContext) -> Cow<'_, str>;
    fn schema(&self) -> Value;
    fn examples(&self) -> Option<Value> {
        None
    }
    fn audience(&self) -> ToolAudience {
        ToolAudience::default()
    }
    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError>;
}

#[derive(Clone)]
pub struct RegisteredTool {
    pub tool: Arc<dyn Tool>,
    pub source: ToolSource,
}

impl RegisteredTool {
    pub fn name(&self) -> &str {
        self.tool.name()
    }

    /// Parse without naming `ParseError`, handy for crates outside `maki-agent`.
    pub fn try_parse(&self, input: &serde_json::Value) -> Option<Box<dyn ToolInvocation>> {
        self.tool.parse(input).ok()
    }
}

/// Lock-free reads via `ArcSwap`, writes swap in a new snapshot atomically.
///
/// Bundled Lua plugins can replace a native tool with their own version.
/// The original native tool is kept in `native_fallbacks` so we can still
/// look up its header info. User plugins are not allowed to replace tools
/// that aren't native (that gives a `NameConflict` error).
pub struct ToolRegistry {
    tools: ArcSwap<Vec<RegisteredTool>>,
    native_fallbacks: ArcSwap<Vec<RegisteredTool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("tool '{name}' is already registered (existing source: {existing})")]
    NameConflict { name: String, existing: String },
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: ArcSwap::from_pointee(Vec::new()),
            native_fallbacks: ArcSwap::from_pointee(Vec::new()),
        }
    }

    pub fn native() -> &'static Self {
        Self::native_arc()
    }

    pub fn native_arc() -> &'static Arc<Self> {
        static NATIVE: LazyLock<Arc<ToolRegistry>> =
            LazyLock::new(|| Arc::new(ToolRegistry::build_native()));
        &NATIVE
    }

    pub fn with_natives() -> Self {
        Self::build_native()
    }

    /// `register_tools!` catches dupes at compile time. Plugins and MCP skip
    /// that macro, so this runtime check is the safety net.
    fn build_native() -> Self {
        let registry = Self::new();
        let natives = super::native_tools();
        let mut vec: Vec<RegisteredTool> = Vec::with_capacity(natives.len());
        for tool in natives {
            let name = tool.name().to_owned();
            assert!(
                !vec.iter().any(|t| t.name() == name),
                "duplicate native tool name: {name}"
            );
            vec.push(RegisteredTool {
                tool,
                source: ToolSource::Native,
            });
        }
        registry.tools.store(Arc::new(vec));
        registry
    }

    pub fn get(&self, name: &str) -> Option<RegisteredTool> {
        self.tools.load().iter().find(|t| t.name() == name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.load().iter().any(|t| t.name() == name)
    }

    pub fn register(&self, tool: Arc<dyn Tool>, source: ToolSource) -> Result<(), RegistryError> {
        let name = tool.name().to_owned();
        let mut conflict = None;
        self.tools.rcu(|current| {
            conflict = None;
            if let Some(existing) = current.iter().find(|t| t.name() == name) {
                conflict = Some(existing.source.as_log_field().into_owned());
                return Vec::clone(current);
            }
            let mut next = Vec::with_capacity(current.len() + 1);
            next.extend(current.iter().cloned());
            next.push(RegisteredTool {
                tool: Arc::clone(&tool),
                source: source.clone(),
            });
            next
        });
        if let Some(existing) = conflict {
            return Err(RegistryError::NameConflict { name, existing });
        }
        Ok(())
    }

    /// All-or-nothing: a name clash rolls back the whole batch so an MCP server
    /// never ends up half-registered.
    pub fn register_many(
        &self,
        entries: impl IntoIterator<Item = (Arc<dyn Tool>, ToolSource)>,
    ) -> Result<(), RegistryError> {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut conflict = None;
        self.tools.rcu(|current| {
            conflict = None;
            let mut next = Vec::clone(current);
            for (tool, source) in &entries {
                let name = tool.name();
                if let Some(existing) = next.iter().find(|t| t.name() == name) {
                    conflict = Some(RegistryError::NameConflict {
                        name: name.to_owned(),
                        existing: existing.source.as_log_field().into_owned(),
                    });
                    return Vec::clone(current);
                }
                next.push(RegisteredTool {
                    tool: Arc::clone(tool),
                    source: source.clone(),
                });
            }
            next
        });
        if let Some(e) = conflict {
            return Err(e);
        }
        Ok(())
    }

    pub fn clear_mcp_server(&self, server: &str) {
        self.tools.rcu(|current| {
            current
                .iter()
                .filter(
                    |t| !matches!(&t.source, ToolSource::Mcp { server: s } if s.as_ref() == server),
                )
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    pub fn replace_plugin(
        &self,
        plugin: &str,
        new_entries: Vec<(Arc<dyn Tool>, ToolSource)>,
    ) -> Result<(), RegistryError> {
        let mut conflict = None;
        let mut displaced: Vec<RegisteredTool> = Vec::new();
        self.tools.rcu(|current| {
            conflict = None;
            displaced = Vec::new();
            let mut next: Vec<RegisteredTool> = current
                .iter()
                .filter(
                    |t| !matches!(&t.source, ToolSource::Lua { plugin: p } if p.as_ref() == plugin),
                )
                .cloned()
                .collect();
            for (tool, source) in &new_entries {
                let name = tool.name();
                if let Some(idx) = next.iter().position(|t| t.name() == name) {
                    if matches!(next[idx].source, ToolSource::Native) {
                        displaced.push(next.remove(idx));
                    } else {
                        conflict = Some(RegistryError::NameConflict {
                            name: name.to_owned(),
                            existing: next[idx].source.as_log_field().into_owned(),
                        });
                        displaced = Vec::new();
                        return Vec::clone(current);
                    }
                }
                next.push(RegisteredTool {
                    tool: Arc::clone(tool),
                    source: source.clone(),
                });
            }
            next
        });
        if let Some(e) = conflict {
            return Err(e);
        }
        if !displaced.is_empty() {
            self.native_fallbacks.rcu(|current| {
                let mut next = Vec::clone(current);
                next.extend(displaced.iter().cloned());
                next
            });
        }
        Ok(())
    }

    pub fn clear_plugin(&self, plugin: &str) {
        let fallbacks = self.native_fallbacks.load();
        let tools_guard = self.tools.load();
        let plugin_names: Vec<&str> = tools_guard
            .iter()
            .filter(|t| matches!(&t.source, ToolSource::Lua { plugin: p } if p.as_ref() == plugin))
            .map(|t| t.name())
            .collect();
        let restore: Vec<RegisteredTool> = fallbacks
            .iter()
            .filter(|t| plugin_names.contains(&t.name()))
            .cloned()
            .collect();
        self.tools.rcu(|current| {
            let mut next: Vec<RegisteredTool> = current
                .iter()
                .filter(
                    |t| !matches!(&t.source, ToolSource::Lua { plugin: p } if p.as_ref() == plugin),
                )
                .cloned()
                .collect();
            next.extend(restore.iter().cloned());
            next
        });
        if !restore.is_empty() {
            self.native_fallbacks.rcu(|current| {
                current
                    .iter()
                    .filter(|t| !plugin_names.contains(&t.name()))
                    .cloned()
                    .collect::<Vec<_>>()
            });
        }
    }

    fn native_fallback(&self, name: &str) -> Option<RegisteredTool> {
        self.native_fallbacks
            .load()
            .iter()
            .find(|t| t.name() == name)
            .cloned()
    }

    /// Resolve a human-friendly summary for a tool invocation.
    /// Prefers the native tool's summary (which parses args into a readable
    /// string like a file path), falling back to the current registered tool,
    /// then to the raw tool name.
    pub fn resolve_header(&self, name: &str, input: &Value) -> String {
        self.resolve_invocation(name, input)
            .map(|inv| inv.start_header().into_ready().text())
            .unwrap_or_else(|| name.to_owned())
    }

    /// Like [`resolve_header`] but awaits async headers (e.g. Lua plugins).
    pub async fn resolve_header_async(&self, name: &str, input: &Value) -> String {
        match self.resolve_invocation(name, input) {
            Some(inv) => inv.start_header().await.text(),
            None => name.to_owned(),
        }
    }

    fn resolve_invocation(&self, name: &str, input: &Value) -> Option<Box<dyn ToolInvocation>> {
        self.native_fallback(name)
            .and_then(|e| e.try_parse(input))
            .or_else(|| self.get(name).and_then(|e| e.try_parse(input)))
    }

    pub fn names(&self) -> Vec<Arc<str>> {
        self.tools
            .load()
            .iter()
            .map(|t| Arc::from(t.name()))
            .collect()
    }

    /// Rebuilt each request so tools registered mid-session (MCP handshake) show
    /// up on the very next turn.
    pub fn definitions(
        &self,
        vars: &Vars,
        ctx: &DescriptionContext,
        supports_examples: bool,
    ) -> Value {
        let snapshot = self.tools.load();
        let mut out = Vec::with_capacity(snapshot.len());
        for entry in snapshot.iter() {
            if !ctx.filter.matches(entry.name()) {
                continue;
            }
            let description = vars.apply(&entry.tool.description(ctx)).into_owned();
            let mut def = json!({
                "name": entry.name(),
                "description": description,
                "input_schema": entry.tool.schema(),
            });
            if let Some(examples) = entry.tool.examples() {
                if supports_examples {
                    def["input_examples"] = examples;
                } else if let Some(text) = format_examples_as_text(&examples) {
                    let merged =
                        format!("{}\n\n{}", def["description"].as_str().unwrap_or(""), text);
                    def["description"] = Value::String(merged);
                }
            }
            out.push(def);
        }
        Value::Array(out)
    }

    pub fn iter(&self) -> RegistrySnapshot {
        RegistrySnapshot(self.tools.load_full())
    }
}

pub struct RegistrySnapshot(Arc<Vec<RegisteredTool>>);

impl RegistrySnapshot {
    pub fn iter(&self) -> std::slice::Iter<'_, RegisteredTool> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn format_examples_as_text(examples: &Value) -> Option<String> {
    let arr = examples.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut text = String::from("Examples:");
    for ex in arr {
        if let Some(code) = ex.get("code").and_then(|c| c.as_str()) {
            text.push_str("\n```\n");
            text.push_str(code);
            text.push_str("\n```");
        }
    }
    Some(text)
}

/// `impl_tool!` wires up the `Tool` trait on this wrapper using consts from
/// `#[derive(Tool)]`. Tool files only need to write their actual logic.
pub struct Native<T: 'static>(std::marker::PhantomData<T>);

impl<T: 'static> Native<T> {
    pub const fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<T: 'static> Default for Native<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::Vars;

    struct MockTool {
        name: String,
    }

    struct MockInvocation;

    impl ToolInvocation for MockInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("mock".into()))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a super::ToolContext) -> ExecFuture<'a> {
            Box::pin(async { Ok(ToolOutput::Plain(String::new())) })
        }
    }

    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "mock tool".into()
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(MockInvocation))
        }
    }

    fn mock(name: &str) -> Arc<dyn Tool> {
        Arc::new(MockTool {
            name: name.to_owned(),
        })
    }

    #[test]
    fn name_conflict_is_rejected() {
        let reg = ToolRegistry::new();
        reg.register(mock("dupe"), ToolSource::Native).unwrap();
        let err = reg.register(mock("dupe"), ToolSource::Native).unwrap_err();
        assert!(matches!(err, RegistryError::NameConflict { .. }));
    }

    /// Tools added mid-session must show up in the next `definitions()` call.
    /// That is the whole reason we build schemas per-request.
    #[test]
    fn definitions_reflects_mid_session_registration() {
        let reg = ToolRegistry::new();
        reg.register(
            mock("late_server__probe"),
            ToolSource::Mcp {
                server: "late_server".into(),
            },
        )
        .unwrap();

        let filter = crate::tools::ToolFilter::All;
        let ctx = DescriptionContext {
            skills: &[],
            filter: &filter,
        };
        let vars = Vars::new();
        let defs = reg.definitions(&vars, &ctx, false);
        let arr = defs.as_array().expect("definitions returns array");
        assert!(
            arr.iter()
                .any(|d| d["name"].as_str() == Some("late_server__probe")),
            "mid-session tool missing from definitions"
        );
    }

    #[test]
    fn clear_mcp_server_removes_only_that_server() {
        let reg = ToolRegistry::new();
        reg.register(
            mock("serverA__one"),
            ToolSource::Mcp {
                server: "serverA".into(),
            },
        )
        .unwrap();
        reg.register(
            mock("serverB__one"),
            ToolSource::Mcp {
                server: "serverB".into(),
            },
        )
        .unwrap();
        reg.register(mock("native_tool"), ToolSource::Native)
            .unwrap();

        reg.clear_mcp_server("serverA");

        assert!(!reg.has("serverA__one"));
        assert!(reg.has("serverB__one"));
        assert!(reg.has("native_tool"));
    }

    #[test]
    fn clear_plugin_removes_only_that_plugin() {
        let reg = ToolRegistry::new();
        reg.register(
            mock("pluginA__foo"),
            ToolSource::Lua {
                plugin: "pluginA".into(),
            },
        )
        .unwrap();
        reg.register(
            mock("pluginB__bar"),
            ToolSource::Lua {
                plugin: "pluginB".into(),
            },
        )
        .unwrap();
        reg.register(mock("native_tool2"), ToolSource::Native)
            .unwrap();

        reg.clear_plugin("pluginA");

        assert!(!reg.has("pluginA__foo"));
        assert!(reg.has("pluginB__bar"));
        assert!(reg.has("native_tool2"));
    }

    #[test]
    fn replace_plugin_displaces_native_and_clear_restores_it() {
        let reg = ToolRegistry::new();
        reg.register(mock("mytool"), ToolSource::Native).unwrap();

        reg.replace_plugin(
            "myplugin",
            vec![(
                mock("mytool"),
                ToolSource::Lua {
                    plugin: "myplugin".into(),
                },
            )],
        )
        .unwrap();

        let entry = reg.get("mytool").unwrap();
        assert!(matches!(entry.source, ToolSource::Lua { .. }));

        let fallback = reg.native_fallback("mytool");
        assert!(fallback.is_some());
        assert!(matches!(fallback.unwrap().source, ToolSource::Native));

        reg.clear_plugin("myplugin");
        let restored = reg.get("mytool").unwrap();
        assert!(matches!(restored.source, ToolSource::Native));
        assert!(reg.native_fallback("mytool").is_none());
    }

    #[test]
    fn replace_plugin_rejects_conflict_with_non_native() {
        let reg = ToolRegistry::new();
        reg.register(mock("shared"), ToolSource::Mcp { server: "s".into() })
            .unwrap();

        let err = reg
            .replace_plugin(
                "myplugin",
                vec![(
                    mock("shared"),
                    ToolSource::Lua {
                        plugin: "myplugin".into(),
                    },
                )],
            )
            .unwrap_err();
        assert!(matches!(err, RegistryError::NameConflict { .. }));
    }
}
