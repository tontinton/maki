use crate::ToolOutput;
use maki_tool_macro::Tool;
use serde::Deserialize;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct LspReferences {
    #[param(description = "Absolute path to the file")]
    path: String,
    #[param(description = "Line number (1-indexed)")]
    line: usize,
    #[param(description = "Column number (1-indexed)")]
    column: usize,
}

impl LspReferences {
    pub const NAME: &str = "lsp_references";
    pub const DESCRIPTION: &str = include_str!("lsp_references.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"path": "/project/src/lib.rs", "line": 15, "column": 8}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let handle = ctx.lsp_handle.as_ref().ok_or("no LSP servers configured")?;
        let path = super::resolve_path(&self.path)?;
        let line = self.line.saturating_sub(1) as u32;
        let col = self.column.saturating_sub(1) as u32;
        let result = handle
            .find_references(&path, line, col)
            .await
            .map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(result))
    }

    pub fn start_header(&self) -> String {
        format!(
            "{}:{}:{}",
            relative_path(&self.path),
            self.line,
            self.column
        )
    }
}

super::impl_tool!(LspReferences);

impl super::ToolInvocation for LspReferences {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(LspReferences::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { LspReferences::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_valid_input() {
        let input = json!({"path": "/src/lib.rs", "line": 15, "column": 8});
        let tool = LspReferences::parse_input(&input).unwrap();
        assert_eq!(tool.path, "/src/lib.rs");
        assert_eq!(tool.line, 15);
        assert_eq!(tool.column, 8);
    }

    #[test]
    fn parse_missing_column_fails() {
        let input = json!({"path": "/src/lib.rs", "line": 15});
        assert!(LspReferences::parse_input(&input).is_err());
    }
}
