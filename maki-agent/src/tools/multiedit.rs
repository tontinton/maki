use std::fs;

use crate::{DiffHunk, DiffLine, DiffSpan, ToolOutput};
use serde::Deserialize;
use similar::ChangeTag;

use maki_tool_macro::{Args, Tool};

use super::fuzzy_replace;
use super::{line_at_offset, relative_path};

#[derive(Args, Debug, Clone, Deserialize)]
struct EditEntry {
    #[param(description = "Exact string to find")]
    old_string: String,
    #[param(description = "Replacement string")]
    new_string: String,
    #[param(description = "Replace all occurrences (default false)")]
    replace_all: Option<bool>,
}

#[derive(Tool, Debug, Clone)]
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

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let edits = self.edits.clone();
        let edit_count_label = self.edit_count_label();
        smol::unblock(move || {
            if edits.is_empty() {
                return Err("provide at least one edit".into());
            }
            let mut content = fs::read_to_string(&path).map_err(|e| format!("read error: {e}"))?;

            let mut hunks = Vec::with_capacity(edits.len());
            for (i, edit) in edits.iter().enumerate() {
                let replace_all = edit.replace_all.unwrap_or(false);
                let result = fuzzy_replace::replace(
                    &content,
                    &edit.old_string,
                    &edit.new_string,
                    replace_all,
                )
                .map_err(|e| format!("edit {i}: {e}"))?;
                for &off in &result.match_offsets {
                    let line = line_at_offset(&content, off);
                    hunks.push(build_hunk(line, &edit.old_string, &edit.new_string));
                }
                content = result.content;
            }

            fs::write(&path, &content).map_err(|e| format!("write error: {e}"))?;
            let rel = relative_path(&path);
            Ok(ToolOutput::Diff {
                hunks,
                summary: format!("applied {edit_count_label} to {rel}"),
                path,
            })
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        relative_path(&self.path)
    }
}

impl super::ToolDefaults for MultiEdit {
    fn start_output(&self) -> Option<ToolOutput> {
        let hunks: Vec<DiffHunk> = self
            .edits
            .iter()
            .map(|e| build_hunk(1, &e.old_string, &e.new_string))
            .collect();
        let path = super::resolve_path(&self.path).ok()?;
        let rel = relative_path(&path);
        Some(ToolOutput::Diff {
            hunks,
            summary: format!("applied {} to {rel}", self.edit_count_label()),
            path,
        })
    }

    fn start_annotation(&self) -> Option<String> {
        Some(self.edit_count_label())
    }

    fn mutable_path(&self) -> Option<&str> {
        Some(&self.path)
    }

    fn permission(&self) -> Option<String> {
        Some(crate::permissions::canonicalize_scope_path(&self.path))
    }
}
pub(super) fn build_hunk(start_line: usize, old: &str, new: &str) -> DiffHunk {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut lines = Vec::new();
    for op in diff.ops() {
        for change in diff.iter_inline_changes(op) {
            match change.tag() {
                ChangeTag::Equal => {
                    let text: String = change
                        .iter_strings_lossy()
                        .map(|(_, t)| t.trim_end_matches('\n').to_owned())
                        .collect();
                    lines.push(DiffLine::Unchanged(text));
                }
                tag => {
                    let spans: Vec<DiffSpan> = change
                        .iter_strings_lossy()
                        .map(|(emphasized, text)| DiffSpan {
                            text: text.trim_end_matches('\n').to_owned(),
                            emphasized,
                        })
                        .collect();
                    if tag == ChangeTag::Delete {
                        lines.push(DiffLine::Removed(spans));
                    } else {
                        lines.push(DiffLine::Added(spans));
                    }
                }
            }
        }
    }
    DiffHunk { start_line, lines }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use crate::AgentMode;
    use crate::tools::ToolDefaults;
    use crate::tools::test_support::stub_ctx;

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
            let tool = MultiEdit::parse_input(&json!({ "path": path, "edits": [] })).unwrap();
            assert_eq!(tool.execute(&ctx).await.unwrap_err(), EMPTY_ERR);
        });
    }

    fn tags(hunk: &DiffHunk) -> Vec<char> {
        hunk.lines
            .iter()
            .map(|l| match l {
                DiffLine::Unchanged(_) => '=',
                DiffLine::Removed(_) => '-',
                DiffLine::Added(_) => '+',
            })
            .collect()
    }

    #[test_case(1, "keep\n",  "keep\nadded", &['=', '+']           ; "append")]
    #[test_case(5, "a\nb\nc", "a\nB\nc",     &['=', '-', '+', '='] ; "middle_change")]
    fn build_hunk_tags(line: usize, old: &str, new: &str, expected: &[char]) {
        let hunk = build_hunk(line, old, new);
        assert_eq!(hunk.start_line, line);
        assert_eq!(tags(&hunk), expected);
    }

    #[test]
    fn start_summary_is_path_only() {
        let tool = MultiEdit {
            path: "/x.rs".into(),
            edits: vec![
                EditEntry {
                    old_string: "a".into(),
                    new_string: "b".into(),
                    replace_all: None,
                };
                2
            ],
        };
        assert_eq!(tool.start_summary(), "/x.rs");
    }

    #[test_case(1, "1 edit"  ; "singular")]
    #[test_case(2, "2 edits" ; "plural")]
    fn start_annotation_edit_count(n: usize, expected: &str) {
        let tool = MultiEdit {
            path: "/x.rs".into(),
            edits: vec![
                EditEntry {
                    old_string: "a".into(),
                    new_string: "b".into(),
                    replace_all: None,
                };
                n
            ],
        };
        assert_eq!(tool.start_annotation().unwrap(), expected);
    }
}
