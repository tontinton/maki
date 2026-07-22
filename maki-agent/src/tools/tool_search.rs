use std::borrow::Cow;

use serde_json::{json, Value};

use crate::tools::registry::{HeaderFuture, HeaderResult, ParseError, ToolInvocation};
use crate::tools::schema::ToolInputErrorKind;
use crate::tools::{DescriptionContext, ToolContext, ToolExecResult};
use crate::ToolOutput;

pub struct ToolSearch;

impl ToolSearch {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ToolSearch {
    fn default() -> Self {
        Self::new()
    }
}

struct ToolSearchInvocation {
    query: String,
    namespace: Option<String>,
}

impl ToolInvocation for ToolSearchInvocation {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(format!("tool_search: {}", self.query)))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> crate::tools::ExecFuture<'a> {
        Box::pin(async move {
            let results = ctx.registry.search(&self.query);
            let filtered: Vec<_> = if let Some(ns) = &self.namespace {
                results
                    .into_iter()
                    .filter(|r| r.namespace.as_deref() == Some(ns.as_str()))
                    .collect()
            } else {
                results
            };
            let output = format!("[{}]", filtered
                .iter()
                .map(|r| format!(
                    r#"{{"name":"{}","namespace":{},"description":"{}"}}"#,
                    r.name,
                    r.namespace.as_ref().map(|s| format!(r#""{}""#, s)).unwrap_or("null".to_string()),
                    r.description.replace('"', r#"\""#)
                ))
                .collect::<Vec<_>>()
                .join(","));
            ToolExecResult::from(Ok(ToolOutput::Plain(output.into())))
        })
    }
}

impl crate::tools::registry::Tool for ToolSearch {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        "Search for deferred tools by name or description. Returns a list of tools that can be loaded on demand.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query to match tool names or descriptions"
                },
                "namespace": {
                    "type": "string",
                    "description": "Optional namespace filter"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ParseError {
                path: Default::default(),
                kind: ToolInputErrorKind::InternalBug {
                    detail: "missing required field 'query'".to_string(),
                },
            })?
            .to_string();
        let namespace = input
            .get("namespace")
            .and_then(|v| v.as_str())
            .map(String::from);
        Ok(Box::new(ToolSearchInvocation { query, namespace }))
    }
}

pub struct LoadNamespace;

impl LoadNamespace {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoadNamespace {
    fn default() -> Self {
        Self::new()
    }
}

struct LoadNamespaceInvocation {
    namespace: String,
}

impl ToolInvocation for LoadNamespaceInvocation {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(format!(
            "load_namespace: {}",
            self.namespace
        )))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> crate::tools::ExecFuture<'a> {
        Box::pin(async move {
            let tools: Vec<String> = ctx
                .registry
                .iter()
                .iter()
                .filter(|t| t.namespace.as_deref() == Some(self.namespace.as_str()))
                .map(|t| t.name().to_string())
                .collect();
            let output = format!(
                r#"{{"namespace":"{}","tools":[{}]}}"#,
                self.namespace,
                tools.iter().map(|t| format!(r#""{}""#, t)).collect::<Vec<_>>().join(",")
            );
            ToolExecResult::from(Ok(ToolOutput::Plain(output.into())))
        })
    }
}

impl crate::tools::registry::Tool for LoadNamespace {
    fn name(&self) -> &str {
        "load_namespace"
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        "Load all tools from a namespace. Returns the list of tools that were loaded.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "namespace": {
                    "type": "string",
                    "description": "Namespace to load"
                }
            },
            "required": ["namespace"],
            "additionalProperties": false
        })
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let namespace = input
            .get("namespace")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ParseError {
                path: Default::default(),
                kind: ToolInputErrorKind::InternalBug {
                    detail: "missing required field 'namespace'".to_string(),
                },
            })?
            .to_string();
        Ok(Box::new(LoadNamespaceInvocation { namespace }))
    }
}
