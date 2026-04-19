use crate::ToolOutput;
use maki_tool_macro::Tool;
use serde::Deserialize;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct LspDiagnostics {
    #[param(description = "Absolute path to the file")]
    path: String,
}

impl LspDiagnostics {
    pub const NAME: &str = "lsp_diagnostics";
    pub const DESCRIPTION: &str = include_str!("lsp_diagnostics.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"path": "/project/src/main.rs"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let handle = ctx.lsp_handle.as_ref().ok_or("no LSP servers configured")?;
        let path = super::resolve_path(&self.path)?;
        let result = handle.diagnostics(&path).await.map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(result))
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(LspDiagnostics);

impl super::ToolInvocation for LspDiagnostics {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(LspDiagnostics::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { LspDiagnostics::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_valid_input() {
        let input = json!({"path": "/src/main.rs"});
        let tool = LspDiagnostics::parse_input(&input).unwrap();
        assert_eq!(tool.path, "/src/main.rs");
    }

    #[test]
    fn parse_missing_path_fails() {
        let input = json!({});
        assert!(LspDiagnostics::parse_input(&input).is_err());
    }
}
