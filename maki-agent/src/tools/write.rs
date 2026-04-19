use std::fs;
use std::path::Path;

use crate::ToolOutput;
use maki_tool_macro::Tool;
use serde::Deserialize;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
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
        let file_tracker = ctx.file_tracker.clone();
        smol::unblock(move || {
            let p = Path::new(&path);
            if p.exists() {
                file_tracker.check_before_edit(p)?;
            }
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir error: {e}"))?;
            }
            fs::write(&path, &content).map_err(|e| format!("write error: {e}"))?;
            file_tracker.record_read(p);
            Ok(output)
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(
    Write,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
);

impl super::ToolInvocation for Write {
    fn start_summary(&self) -> super::SummaryFuture {
        super::SummaryFuture::Ready(Write::start_summary(self))
    }
    fn start_output(&self) -> Option<ToolOutput> {
        let path = super::resolve_path(&self.path).ok()?;
        Some(self.write_output(&path, maki_config::DEFAULT_MAX_OUTPUT_LINES))
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
    fn permission_scope(&self) -> Option<String> {
        Some(crate::permissions::canonicalize_scope_path(&self.path))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Write::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::AgentMode;
    use crate::tools::test_support::{pre_read, stub_ctx};

    use super::*;

    const ERR_NOT_READ: &str = "file must be read before editing";

    #[test]
    fn write_new_file_succeeds() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let path = dir.path().join("new.txt").to_string_lossy().to_string();

            Write {
                path: path.clone(),
                content: "hello".into(),
            }
            .execute(&ctx)
            .await
            .unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        });
    }

    #[test]
    fn write_existing_without_read_fails() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let path = dir.path().join("existing.txt");
            fs::write(&path, "original").unwrap();

            let err = Write {
                path: path.to_string_lossy().to_string(),
                content: "overwrite".into(),
            }
            .execute(&ctx)
            .await
            .unwrap_err();

            assert!(err.contains(ERR_NOT_READ));
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        });
    }

    #[test]
    fn write_existing_after_read_succeeds() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let path = dir.path().join("existing.txt");
            fs::write(&path, "original").unwrap();
            pre_read(&ctx, &path.to_string_lossy());

            Write {
                path: path.to_string_lossy().to_string(),
                content: "overwrite".into(),
            }
            .execute(&ctx)
            .await
            .unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), "overwrite");
        });
    }
}
