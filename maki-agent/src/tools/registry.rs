//! The one place that knows about tools. Built-in, MCP, and future plugins all land here,
//! so there is no second list to forget and no parallel `match name { ... }` table to drift.
//!
//! Tool identity is an `Arc<str>` owned by the entry, so MCP tools don't need leaked
//! strings to look like natives at call sites.

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use bitflags::bitflags;
use serde_json::{Value, json};

use crate::template::Vars;
use crate::{ToolInput as ToolInputEvent, ToolOutput};

use super::{DescriptionContext, ToolContext};

bitflags! {
    /// Subagents and `code_execution` filter on this instead of each keeping their own
    /// allow-list of tool names.
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

/// Carried on every entry so log lines can say `tool_source=mcp:github` and the "why is
/// this one behaving differently" question answers itself.
#[derive(Clone, Debug)]
pub enum ToolSource {
    Native,
    Mcp { server: Arc<str> },
}

impl ToolSource {
    pub fn as_log_field(&self) -> Cow<'static, str> {
        match self {
            Self::Native => Cow::Borrowed("native"),
            Self::Mcp { server } => Cow::Owned(format!("mcp:{server}")),
        }
    }
}

pub type ParseError = super::schema::ToolInputError;

pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send + 'a>>;

/// Owns the parsed input so start-event construction and `execute` share one parse.
///
/// `permission_scope` and `mutable_path` live here, not on `Tool`, because "am I writing
/// to the plan file?" is a question only the parsed call can answer.
pub trait ToolInvocation: Send + Sync {
    fn start_summary(&self) -> String;
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

    /// Lets crates outside `maki-agent` parse without naming `ParseError`.
    pub fn try_parse(&self, input: &serde_json::Value) -> Option<Box<dyn ToolInvocation>> {
        self.tool.parse(input).ok()
    }
}

/// `ArcSwap` keeps the hot path (`get` on every tool call) lock-free, while MCP handshakes
/// and future plugin loads publish a whole new snapshot in one atomic swap.
pub struct ToolRegistry {
    tools: ArcSwap<Vec<RegisteredTool>>,
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
        }
    }

    pub fn native() -> &'static Self {
        static NATIVE: LazyLock<ToolRegistry> = LazyLock::new(ToolRegistry::build_native);
        &NATIVE
    }

    /// Duplicate names panic. The `register_tools!` macro catches them at compile time,
    /// but plugins and MCP skip that check, so this is the runtime safety belt.
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

    /// All-or-nothing: a name clash rolls back the whole batch so a half-registered MCP
    /// server never leaves the registry partially populated.
    pub fn register_many(
        &self,
        entries: impl IntoIterator<Item = (Arc<dyn Tool>, ToolSource)>,
    ) -> Result<(), RegistryError> {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut conflict = None;
        self.tools.rcu(|current| {
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

    /// Called on MCP server disconnect or reconnect so stale entries cannot outlive
    /// their transport.
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

    pub fn names(&self) -> Vec<Arc<str>> {
        self.tools
            .load()
            .iter()
            .map(|t| Arc::from(t.name()))
            .collect()
    }

    /// Built fresh each request so an MCP server registering mid-session shows up on the
    /// next turn.
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

/// `impl_tool!` hangs the `Tool` impl off this wrapper using the consts from
/// `#[derive(Tool)]`, so tool files only write their logic.
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
        fn start_summary(&self) -> String {
            "mock".into()
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

    /// MCP tools registered mid-session must appear on the very next `definitions()` call.
    /// This is the guarantee that lets us build tool schemas per-request instead of at
    /// startup.
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
}
