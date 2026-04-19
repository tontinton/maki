use crate::ToolOutput;
use maki_tool_macro::Tool;
use serde::Deserialize;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct LspWorkspaceSymbol {
    #[param(description = "Absolute path to any file in the project (used to pick the LSP server)")]
    path: String,
    #[param(description = "Symbol name or query to search for")]
    query: String,
}

impl LspWorkspaceSymbol {
    pub const NAME: &str = "lsp_workspace_symbol";
    pub const DESCRIPTION: &str = include_str!("lsp_workspace_symbol.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"path": "/project/src/main.rs", "query": "Config"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let handle = ctx.lsp_handle.as_ref().ok_or("no LSP servers configured")?;
        let path = super::resolve_path(&self.path)?;
        let result = handle
            .workspace_symbol(&path, &self.query)
            .await
            .map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(result))
    }

    pub fn start_header(&self) -> String {
        format!("{} in {}", self.query, relative_path(&self.path))
    }
}

super::impl_tool!(LspWorkspaceSymbol);

impl super::ToolInvocation for LspWorkspaceSymbol {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(LspWorkspaceSymbol::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { LspWorkspaceSymbol::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_valid_input() {
        let input = json!({"path": "/src/main.rs", "query": "Config"});
        let tool = LspWorkspaceSymbol::parse_input(&input).unwrap();
        assert_eq!(tool.path, "/src/main.rs");
        assert_eq!(tool.query, "Config");
    }

    #[test]
    fn parse_missing_query_fails() {
        let input = json!({"path": "/src/main.rs"});
        assert!(LspWorkspaceSymbol::parse_input(&input).is_err());
    }
}
