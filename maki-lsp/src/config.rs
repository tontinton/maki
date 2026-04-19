use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LspServerConfig {
    pub command: Vec<String>,
    pub languages: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LspConfig {
    #[serde(default)]
    pub lsp: HashMap<String, LspServerConfig>,
}

impl LspConfig {
    pub fn server_for_language(&self, language: &str) -> Option<(&str, &LspServerConfig)> {
        self.lsp
            .iter()
            .find(|(_, cfg)| cfg.languages.iter().any(|l| l == language))
            .map(|(name, cfg)| (name.as_str(), cfg))
    }
}

pub fn language_from_extension(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "lua" => "lua",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        _ => ext,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsp_config() {
        let toml_str = r#"
[lsp.rust-analyzer]
command = ["rust-analyzer"]
languages = ["rust"]
root_markers = ["Cargo.toml"]

[lsp.pyright]
command = ["pyright-langserver", "--stdio"]
languages = ["python"]
root_markers = ["pyproject.toml", "setup.py"]
"#;
        let config: LspConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.lsp.len(), 2);

        let ra = &config.lsp["rust-analyzer"];
        assert_eq!(ra.command, vec!["rust-analyzer"]);
        assert_eq!(ra.languages, vec!["rust"]);
        assert_eq!(ra.root_markers, vec!["Cargo.toml"]);

        let py = &config.lsp["pyright"];
        assert_eq!(py.command, vec!["pyright-langserver", "--stdio"]);
    }

    #[test]
    fn server_for_language_lookup() {
        let toml_str = r#"
[lsp.rust-analyzer]
command = ["rust-analyzer"]
languages = ["rust"]

[lsp.ts]
command = ["typescript-language-server", "--stdio"]
languages = ["typescript", "javascript"]
"#;
        let config: LspConfig = toml::from_str(toml_str).unwrap();
        let (name, _) = config.server_for_language("rust").unwrap();
        assert_eq!(name, "rust-analyzer");

        let (name, _) = config.server_for_language("typescript").unwrap();
        assert_eq!(name, "ts");

        assert!(config.server_for_language("go").is_none());
    }

    #[test]
    fn language_from_extension_common_cases() {
        assert_eq!(language_from_extension("rs"), "rust");
        assert_eq!(language_from_extension("py"), "python");
        assert_eq!(language_from_extension("ts"), "typescript");
        assert_eq!(language_from_extension("go"), "go");
        assert_eq!(language_from_extension("unknown"), "unknown");
    }
}
