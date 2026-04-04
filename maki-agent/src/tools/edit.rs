use std::fs;

use crate::ToolOutput;
use maki_tool_macro::Tool;

use super::fuzzy_replace;
use super::multiedit::build_hunk;
use super::{line_at_offset, relative_path};

#[derive(Tool, Debug, Clone)]
pub struct Edit {
    #[param(description = "Absolute path to the file")]
    path: String,
    #[param(description = "Exact string to find (must match uniquely unless replace_all is true)")]
    old_string: String,
    #[param(description = "Replacement string")]
    new_string: String,
    #[param(description = "Replace all occurrences (default false)")]
    replace_all: Option<bool>,
}

impl Edit {
    pub const NAME: &str = "edit";
    pub const DESCRIPTION: &str = include_str!("edit.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs", "old_string": "fn old_name(", "new_string": "fn new_name("},
  {"path": "/project/src/lib.rs", "old_string": "use std::collections::HashMap;\nuse std::sync::Arc;", "new_string": "use std::collections::HashMap;\nuse std::io::Read;\nuse std::sync::Arc;"}
]"#,
    );

    fn diff_output(&self, resolved_path: &str, lines: &[usize]) -> ToolOutput {
        let rel = relative_path(resolved_path);
        let hunks = lines
            .iter()
            .map(|&line| build_hunk(line, &self.old_string, &self.new_string))
            .collect();
        ToolOutput::Diff {
            hunks,
            summary: format!("edited {rel}"),
            path: resolved_path.to_owned(),
        }
    }

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let old_string = self.old_string.clone();
        let new_string = self.new_string.clone();
        let replace_all = self.replace_all.unwrap_or(false);
        let diff_self = self.clone();
        smol::unblock(move || {
            let content = fs::read_to_string(&path).map_err(|e| format!("read error: {e}"))?;
            let result = fuzzy_replace::replace(&content, &old_string, &new_string, replace_all)?;
            fs::write(&path, &result.content).map_err(|e| format!("write error: {e}"))?;
            let lines: Vec<usize> = result
                .match_offsets
                .iter()
                .map(|&off| line_at_offset(&content, off))
                .collect();
            Ok(diff_self.diff_output(&path, &lines))
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        relative_path(&self.path)
    }
}

impl super::ToolDefaults for Edit {
    fn start_output(&self) -> Option<ToolOutput> {
        let path = super::resolve_path(&self.path).ok()?;
        Some(self.diff_output(&path, &[1]))
    }

    fn mutable_path(&self) -> Option<&str> {
        Some(&self.path)
    }

    fn permission(&self) -> Option<String> {
        Some(crate::permissions::canonicalize_scope_path(&self.path))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    use super::*;

    fn temp_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn edit_reads_replaces_writes() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let ctx = stub_ctx(&AgentMode::Build);

            let path = temp_file(&dir, "f.rs", "fn old() {}\nfn keep() {}");
            Edit {
                path: path.clone(),
                old_string: "fn old() {}".into(),
                new_string: "fn new() {}".into(),
                replace_all: None,
            }
            .execute(&ctx)
            .await
            .unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "fn new() {}\nfn keep() {}"
            );

            let path = temp_file(&dir, "g.rs", "let x = 1;\nlet x = 1;\nlet y = 2;");
            Edit {
                path: path.clone(),
                old_string: "let x = 1;".into(),
                new_string: "let x = 9;".into(),
                replace_all: Some(true),
            }
            .execute(&ctx)
            .await
            .unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "let x = 9;\nlet x = 9;\nlet y = 2;"
            );
        });
    }
}
