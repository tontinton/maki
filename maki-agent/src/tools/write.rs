use std::fs;
use std::path::Path;

use crate::ToolOutput;
use maki_tool_macro::Tool;

use super::relative_path;

#[derive(Tool, Debug, Clone)]
pub struct Write {
    #[param(description = "Absolute path to the file")]
    path: String,
    #[param(description = "The complete file content to write")]
    content: String,
}

impl Write {
    pub const NAME: &str = "write";
    pub const DESCRIPTION: &str = include_str!("write.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"path": "/project/src/config.rs", "content": "pub const PORT: u16 = 8080;\n"}]"#);

    fn write_output(&self, resolved_path: &str, max_lines: usize) -> ToolOutput {
        ToolOutput::WriteCode {
            path: resolved_path.to_owned(),
            byte_count: self.content.len(),
            lines: self
                .content
                .lines()
                .take(max_lines)
                .map(ToOwned::to_owned)
                .collect(),
        }
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let content = self.content.clone();
        let output = self.write_output(&path, ctx.config.max_output_lines);
        smol::unblock(move || {
            if let Some(parent) = Path::new(&path).parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir error: {e}"))?;
            }
            fs::write(&path, &content).map_err(|e| format!("write error: {e}"))?;
            Ok(output)
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        relative_path(&self.path)
    }
}

impl super::ToolDefaults for Write {
    fn start_output(&self) -> Option<ToolOutput> {
        let path = super::resolve_path(&self.path).ok()?;
        Some(self.write_output(&path, maki_config::DEFAULT_MAX_OUTPUT_LINES))
    }

    fn mutable_path(&self) -> Option<&str> {
        Some(&self.path)
    }

    fn permission(&self) -> Option<String> {
        Some(crate::permissions::canonicalize_scope_path(&self.path))
    }
}
