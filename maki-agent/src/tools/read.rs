use std::fmt::Write;
use std::fs;
use std::path::Path;

use crate::agent::{self, LoadedInstructions};
use crate::{InstructionBlock, ToolOutput};
use maki_tool_macro::Tool;

use super::{relative_path, truncate_bytes};

#[derive(Tool, Debug, Clone)]
pub struct Read {
    #[param(description = "Absolute path to the file or directory")]
    path: String,
    #[param(description = "Line number to start from (1-indexed)")]
    offset: Option<usize>,
    #[param(description = "Max number of lines to read")]
    limit: Option<usize>,
}

fn to_instruction_blocks(found: Vec<(String, String)>) -> Option<Vec<InstructionBlock>> {
    if found.is_empty() {
        return None;
    }
    Some(
        found
            .into_iter()
            .map(|(path, content)| InstructionBlock { path, content })
            .collect(),
    )
}

impl Read {
    pub const NAME: &str = "read";
    pub const DESCRIPTION: &str = include_str!("read.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"path": "/project/src/main.rs", "offset": 10, "limit": 20}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let offset = self.offset;
        let limit = self.limit;
        let loaded = ctx.loaded_instructions.clone();
        let max_output_lines = ctx.config.max_output_lines;
        let max_line_bytes = ctx.config.max_line_bytes;
        smol::unblock(move || {
            let cwd = std::env::current_dir().ok();
            if Path::new(&path).is_dir() {
                return Self::list_dir(&path, cwd.as_deref(), &loaded);
            }

            let raw = fs::read_to_string(&path).map_err(|e| format!("read error: {e}"))?;
            let total_lines = raw.lines().count();

            let start = offset.unwrap_or(1).saturating_sub(1);
            let limit = limit.unwrap_or(max_output_lines);

            let lines: Vec<String> = raw
                .lines()
                .skip(start)
                .take(limit)
                .map(|l| truncate_bytes(l, max_line_bytes))
                .collect();

            let instructions = cwd.as_deref().and_then(|cwd| {
                let p = Path::new(&path);
                if agent::is_instruction_file(p.file_name()?.to_str()?) {
                    return None;
                }
                to_instruction_blocks(agent::find_subdirectory_instructions(
                    p.parent()?,
                    cwd,
                    &loaded,
                ))
            });

            Ok(ToolOutput::ReadCode {
                path,
                start_line: start + 1,
                lines,
                total_lines,
                instructions,
            })
        })
        .await
    }

    fn list_dir(
        path: &str,
        cwd: Option<&Path>,
        loaded: &LoadedInstructions,
    ) -> Result<ToolOutput, String> {
        let entries = fs::read_dir(path).map_err(|e| format!("read error: {e}"))?;

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                dirs.push(format!("{name}/"));
            } else {
                files.push(name);
            }
        }

        dirs.sort_unstable();
        files.sort_unstable();
        files.retain(|name| !agent::is_instruction_file(name));
        dirs.append(&mut files);
        let text = dirs.join("\n");

        let instructions = cwd.and_then(|cwd| {
            to_instruction_blocks(agent::find_subdirectory_instructions(
                Path::new(path),
                cwd,
                loaded,
            ))
        });

        Ok(ToolOutput::ReadDir { text, instructions })
    }

    pub fn start_summary(&self) -> String {
        let mut s = relative_path(&self.path);
        let start = self.offset.unwrap_or(1);
        match (self.offset.is_some(), self.limit) {
            (_, Some(l)) => {
                let _ = write!(s, ":{start}-{}", start + l - 1);
            }
            (true, None) => {
                let _ = write!(s, ":{start}");
            }
            _ => {}
        }
        s
    }
}

impl super::ToolDefaults for Read {}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    #[test_case(None,      None,      "/a/b.rs"       ; "path_only")]
    #[test_case(Some(10),  None,      "/a/b.rs:10"    ; "offset_only")]
    #[test_case(None,      Some(25),  "/a/b.rs:1-25"  ; "limit_only")]
    #[test_case(Some(50),  Some(51),  "/a/b.rs:50-100" ; "offset_and_limit")]
    fn start_summary_cases(offset: Option<usize>, limit: Option<usize>, expected: &str) {
        let r = Read {
            path: "/a/b.rs".into(),
            offset,
            limit,
        };
        assert_eq!(r.start_summary(), expected);
    }

    #[test]
    fn list_dir_sorts_dirs_first_and_hides_instruction_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().to_string_lossy().to_string();

        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();

        let result =
            Read::list_dir(&dir_path, None, &crate::agent::LoadedInstructions::new()).unwrap();
        match &result {
            ToolOutput::ReadDir { text, instructions } => {
                let entries: Vec<&str> = text.lines().collect();
                assert_eq!(entries, vec!["adir/", "zdir/", "a.rs", "b.txt"]);
                assert!(instructions.is_none());
            }
            other => panic!("expected ReadDir, got {other:?}"),
        }
    }

    #[test]
    fn list_dir_discovers_subdirectory_instructions() {
        let root = tempfile::TempDir::new().unwrap();
        let sub = root.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "sub rules").unwrap();
        std::fs::write(sub.join("lib.rs"), "").unwrap();

        let sub_path = sub.to_string_lossy().to_string();
        let loaded = crate::agent::LoadedInstructions::new();
        let result = Read::list_dir(&sub_path, Some(root.path()), &loaded).unwrap();
        match &result {
            ToolOutput::ReadDir { instructions, .. } => {
                let blocks = instructions.as_ref().expect("should have instructions");
                assert_eq!(blocks.len(), 1);
                assert!(blocks[0].path.ends_with("AGENTS.md"));
                assert_eq!(blocks[0].content, "sub rules");
            }
            other => panic!("expected ReadDir, got {other:?}"),
        }
    }

    #[test]
    fn parse_input_bad_coercion_returns_error() {
        let err = Read::parse_input(&json!({"path": "x", "limit": "not_a_number"})).unwrap_err();
        assert!(
            err.contains("limit"),
            "error should mention field name: {err}"
        );
    }

    #[test]
    fn parse_input_error_includes_schema_hint() {
        let err = Read::parse_input(&json!({})).unwrap_err();
        assert!(
            err.contains("path") && err.contains("Expected:"),
            "error should mention missing field and schema: {err}"
        );
    }
}
