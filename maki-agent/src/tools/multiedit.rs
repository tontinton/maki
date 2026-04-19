use std::fs;
use std::path::Path;

use crate::ToolOutput;
use serde::Deserialize;

use maki_tool_macro::{Args, Tool};

use super::fuzzy_replace;
use super::relative_path;

#[derive(Args, Debug, Clone, Deserialize)]
struct EditEntry {
    #[param(description = "Exact string to find")]
    old_string: String,
    #[param(description = "Replacement string")]
    new_string: String,
    #[param(description = "Replace all occurrences (default false)")]
    replace_all: Option<bool>,
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct MultiEdit {
    #[param(description = "Absolute path to the file")]
    path: String,
    #[param(description = "Array of edit operations to apply sequentially")]
    edits: Vec<EditEntry>,
}

impl MultiEdit {
    pub const NAME: &str = "multiedit";
    pub const DESCRIPTION: &str = include_str!("multiedit.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"path": "/project/src/lib.rs", "edits": [
  {"old_string": "use old_crate::Foo;", "new_string": "use new_crate::Foo;"},
  {"old_string": "old_crate::init()", "new_string": "new_crate::init()", "replace_all": true}
]}]"#,
    );

    fn edit_count_label(&self) -> String {
        let n = self.edits.len();
        let s = if n == 1 { "" } else { "s" };
        format!("{n} edit{s}")
    }

    fn apply_edits(&self, before: &str) -> Result<String, String> {
        if self.edits.is_empty() {
            return Err("provide at least one edit".into());
        }
        let mut content = before.to_owned();
        for (i, edit) in self.edits.iter().enumerate() {
            content = fuzzy_replace::replace(
                &content,
                &edit.old_string,
                &edit.new_string,
                edit.replace_all.unwrap_or(false),
            )
            .map_err(|e| format!("edit {i}: {e}"))?;
        }
        Ok(content)
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let this = self.clone();
        let file_tracker = ctx.file_tracker.clone();
        smol::unblock(move || {
            let p = Path::new(&path);
            file_tracker.check_before_edit(p)?;

            let before = fs::read_to_string(p).map_err(|e| format!("read error: {e}"))?;
            let after = this.apply_edits(&before)?;
            fs::write(p, &after).map_err(|e| format!("write error: {e}"))?;

            file_tracker.record_read(p);

            Ok(ToolOutput::Diff {
                summary: format!(
                    "applied {} to {}",
                    this.edit_count_label(),
                    relative_path(&path)
                ),
                path,
                before,
                after,
            })
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(
    MultiEdit,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
);

impl super::ToolInvocation for MultiEdit {
    fn start_summary(&self) -> super::SummaryFuture {
        super::SummaryFuture::Ready(MultiEdit::start_summary(self))
    }
    fn start_annotation(&self) -> Option<String> {
        Some(self.edit_count_label())
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
    fn permission_scope(&self) -> Option<String> {
        Some(crate::permissions::canonicalize_scope_path(&self.path))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { MultiEdit::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use crate::AgentMode;
    use crate::tools::test_support::{pre_read, stub_ctx};

    use super::*;

    const EMPTY_ERR: &str = "provide at least one edit";

    fn temp_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn sequential_edits_compose() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let path = temp_file(&dir, "f.rs", "fn alpha() {}\nfn beta() {}");
            pre_read(&ctx, &path);
            let tool = MultiEdit::parse_input(&json!({
                "path": path,
                "edits": [
                    { "old_string": "fn alpha() {}", "new_string": "fn one() {}" },
                    { "old_string": "fn beta() {}", "new_string": "fn two() {}" }
                ]
            }))
            .unwrap();
            let msg = tool.execute(&ctx).await.unwrap().as_text().to_string();
            assert!(msg.contains("2 edits"));
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "fn one() {}\nfn two() {}"
            );
        });
    }

    #[test]
    fn failure_leaves_file_unchanged() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let original = "let a = 1;\nlet b = 2;";
            let path = temp_file(&dir, "f.rs", original);
            pre_read(&ctx, &path);
            let tool = MultiEdit::parse_input(&json!({
                "path": path,
                "edits": [
                    { "old_string": "let a = 1;", "new_string": "let a = 9;" },
                    { "old_string": "MISSING", "new_string": "x" }
                ]
            }))
            .unwrap();
            let err = tool.execute(&ctx).await.unwrap_err();
            assert!(err.contains("edit 1"));
            assert!(err.contains(fuzzy_replace::NO_MATCH));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        });
    }

    #[test]
    fn empty_edits_rejected() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let path = temp_file(&dir, "f.rs", "content");
            pre_read(&ctx, &path);
            let tool = MultiEdit::parse_input(&json!({ "path": path, "edits": [] })).unwrap();
            assert_eq!(tool.execute(&ctx).await.unwrap_err(), EMPTY_ERR);
        });
    }
}
