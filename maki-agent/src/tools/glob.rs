use crate::ToolOutput;
use maki_tool_macro::Tool;
use tracing::debug;

use super::{mtime, relative_path, resolve_search_path, walk_builder};

#[derive(Tool, Debug, Clone)]
pub struct Glob {
    #[param(description = "Glob pattern (e.g. **/*.rs, src/**/*.ts)")]
    pattern: String,
    #[param(description = "Directory to search in (default: cwd)")]
    path: Option<String>,
}

impl Glob {
    pub const NAME: &str = "glob";
    pub const DESCRIPTION: &str = include_str!("glob.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"pattern": "src/**/*.rs"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let search_limit = ctx.config.search_result_limit;
        let pattern = self.pattern.clone();
        let path = self.path.clone();
        smol::unblock(move || {
            let search_path = resolve_search_path(path.as_deref())?;

            debug!(
                pattern = %pattern,
                pattern_debug = ?pattern,
                path = %search_path,
                "glob executing"
            );

            let mut entries: Vec<_> = walk_builder(&search_path, &[&pattern])?
                .build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .map(|e| {
                    let p = e.into_path();
                    (mtime(&p), relative_path(&p.to_string_lossy()))
                })
                .collect();

            entries.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            entries.truncate(search_limit);

            Ok(ToolOutput::GlobResult {
                files: entries.into_iter().map(|(_, p)| p).collect(),
            })
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        let mut s = self.pattern.clone();
        if let Some(dir) = &self.path {
            s.push_str(" in ");
            s.push_str(&relative_path(dir));
        }
        s
    }
}

impl super::ToolDefaults for Glob {}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("**/*.rs", None,            "**/*.rs"          ; "pattern_only")]
    #[test_case("**/*.rs", Some("src/"),      "**/*.rs in src/"  ; "with_path")]
    fn start_summary_cases(pattern: &str, path: Option<&str>, expected: &str) {
        let g = Glob {
            pattern: pattern.into(),
            path: path.map(Into::into),
        };
        assert_eq!(g.start_summary(), expected);
    }
}
