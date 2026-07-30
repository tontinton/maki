use tree_sitter::Language as TsLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Gleam,
    Go,
    Html,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Swift,
    Kotlin,
    Scala,
    Bash,
    Lua,
    Elixir,
    Markdown,
    Starlark,
    Zig,
    Nix,
    Dart,
    Sql,
    Toml,
    Yaml,
    Containerfile,
    Css,
    Hcl,
    Json,
    Make,
    Scss,
}

impl Language {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "typescript" => Some(Self::TypeScript),
            "javascript" => Some(Self::JavaScript),
            "gleam" => Some(Self::Gleam),
            "go" => Some(Self::Go),
            "html" => Some(Self::Html),
            "java" => Some(Self::Java),
            "c" => Some(Self::C),
            "cpp" => Some(Self::Cpp),
            "c_sharp" => Some(Self::CSharp),
            "ruby" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "swift" => Some(Self::Swift),
            "kotlin" => Some(Self::Kotlin),
            "scala" => Some(Self::Scala),
            "bash" => Some(Self::Bash),
            "lua" => Some(Self::Lua),
            "elixir" => Some(Self::Elixir),
            "markdown" => Some(Self::Markdown),
            "starlark" => Some(Self::Starlark),
            "zig" => Some(Self::Zig),
            "nix" => Some(Self::Nix),
            "dart" => Some(Self::Dart),
            "sql" => Some(Self::Sql),
            "toml" => Some(Self::Toml),
            "yaml" => Some(Self::Yaml),
            "containerfile" => Some(Self::Containerfile),
            "css" => Some(Self::Css),
            "hcl" => Some(Self::Hcl),
            "json" => Some(Self::Json),
            "make" => Some(Self::Make),
            "scss" => Some(Self::Scss),
            _ => None,
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "gleam" => Some(Self::Gleam),
            "go" => Some(Self::Go),
            "html" | "htm" => Some(Self::Html),
            "java" => Some(Self::Java),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" | "ixx" => Some(Self::Cpp),
            "cs" => Some(Self::CSharp),
            "rb" | "rake" | "gemspec" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "swift" => Some(Self::Swift),
            "kt" | "kts" => Some(Self::Kotlin),
            "scala" | "sc" => Some(Self::Scala),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "lua" => Some(Self::Lua),
            "ex" | "exs" => Some(Self::Elixir),
            "md" | "markdown" => Some(Self::Markdown),
            "bzl" => Some(Self::Starlark),
            "zig" => Some(Self::Zig),
            "nix" => Some(Self::Nix),
            "dart" => Some(Self::Dart),
            "sql" => Some(Self::Sql),
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            "dockerfile" => Some(Self::Containerfile),
            "css" => Some(Self::Css),
            "hcl" | "tf" | "tfvars" => Some(Self::Hcl),
            "json" => Some(Self::Json),
            "mk" => Some(Self::Make),
            "scss" => Some(Self::Scss),
            _ => None,
        }
    }

    pub fn ts_language(&self) -> TsLanguage {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Gleam => tree_sitter_gleam::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Elixir => tree_sitter_elixir::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::Starlark => tree_sitter_starlark::LANGUAGE.into(),
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::Nix => tree_sitter_nix::LANGUAGE.into(),
            Self::Dart => tree_sitter_dart::LANGUAGE.into(),
            Self::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Self::Containerfile => tree_sitter_containerfile::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Hcl => tree_sitter_hcl::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Make => tree_sitter_make::LANGUAGE.into(),
            Self::Scss => tree_sitter_scss::language(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Language;

    #[test]
    fn recognizes_new_index_languages_by_name_and_extension() {
        let cases = [
            ("css", "css", Language::Css),
            ("scss", "scss", Language::Scss),
            ("json", "json", Language::Json),
            ("hcl", "tf", Language::Hcl),
            ("containerfile", "dockerfile", Language::Containerfile),
            ("make", "mk", Language::Make),
        ];

        for (name, extension, expected) in cases {
            assert_eq!(Language::from_name(name), Some(expected));
            assert_eq!(Language::from_extension(extension), Some(expected));
        }
    }

    #[test]
    fn new_index_languages_load_tree_sitter_grammars() {
        let languages = [
            Language::Css,
            Language::Scss,
            Language::Json,
            Language::Hcl,
            Language::Containerfile,
            Language::Make,
        ];

        let mut parser = tree_sitter::Parser::new();
        for language in languages {
            parser
                .set_language(&language.ts_language())
                .expect("grammar should load");
        }
    }

    #[test]
    fn existing_html_grammar_still_parses_elements() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&Language::Html.ts_language())
            .expect("HTML grammar should load");
        let tree = parser
            .parse("<main><div>text</div></main>", None)
            .expect("HTML source should parse");

        let syntax = tree.root_node().to_sexp();
        assert!(
            syntax.contains("element"),
            "unexpected syntax tree: {syntax}"
        );
    }
}
