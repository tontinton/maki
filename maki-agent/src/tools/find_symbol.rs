use std::path::Path;

use maki_code_index::find_symbol::{FindSymbolError, find_symbol as do_find_symbol};
use maki_tool_macro::Tool;

use super::relative_path;
use crate::ToolOutput;

const DEFAULT_MAX_RESULTS: usize = 50;

#[derive(Tool, Debug, Clone)]
pub struct FindSymbol {
    #[param(description = "Symbol name")]
    symbol: String,
    #[param(description = "Absolute path to file containing the symbol")]
    file: String,
    #[param(description = "Line number (1-indexed)")]
    line: usize,
    #[param(description = "Nth occurrence on line (default 1)")]
    occurrence: Option<usize>,
    #[param(description = "Max results (default 50)")]
    max_results: Option<usize>,
}

impl FindSymbol {
    pub const NAME: &str = "find_symbol";
    pub const DESCRIPTION: &str = include_str!("find_symbol.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"symbol": "spawn", "file": "/project/src/main.rs", "line": 42}]"#);

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let file = super::resolve_path(&self.file)?;
        let symbol = self.symbol.clone();
        let line = self.line;
        let occurrence = self.occurrence.unwrap_or(1);
        let max_results = self.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let project_root = super::resolve_search_path(None)?;

        smol::unblock(move || {
            let file_path = Path::new(&file);
            let project_root = Path::new(&project_root);

            match do_find_symbol(
                project_root,
                file_path,
                line,
                &symbol,
                occurrence,
                Some(max_results),
            ) {
                Ok(result) => {
                    let mut out = String::new();
                    out.push_str(&format!(
                        "Scope: {}\n{} references found",
                        result.scope,
                        result.references.len()
                    ));
                    if result.stats.files_grepped > 1 {
                        out.push_str(&format!(
                            " ({} files searched, {} parsed)",
                            result.stats.files_grepped, result.stats.files_parsed
                        ));
                    }
                    out.push('\n');

                    for r in &result.references {
                        out.push_str(&r.format_relative(project_root));
                        out.push('\n');
                    }

                    Ok(ToolOutput::Plain(out))
                }
                Err(FindSymbolError::UnsupportedLanguage(ext)) => {
                    Err(format!("Unsupported language: .{ext}"))
                }
                Err(FindSymbolError::NoRefLanguageSupport(lang)) => Err(format!(
                    "No reference support for {lang:?}. Only Rust, C, and C++ are supported."
                )),
                Err(e) => Err(e.to_string()),
            }
        })
        .await
    }

    pub fn start_summary(&self) -> String {
        format!("{} in {}", self.symbol, relative_path(&self.file))
    }
}

impl super::ToolDefaults for FindSymbol {}
