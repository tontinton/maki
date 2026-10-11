use tree_sitter::Language as TsLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
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
    /// The inline half of markdown: emphasis, code spans, links. Markdown
    /// parses blocks only and leaves each paragraph's text to this grammar.
    MarkdownInline,
    Starlark,
    V,
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
    Clojure,
}

impl Language {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "typescript" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
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
            "markdown_inline" => Some(Self::MarkdownInline),
            "starlark" => Some(Self::Starlark),
            "v" => Some(Self::V),
            "zig" => Some(Self::Zig),
            "nix" => Some(Self::Nix),
            "dart" => Some(Self::Dart),
            "sql" => Some(Self::Sql),
            "toml" => Some(Self::Toml),
            "yaml" => Some(Self::Yaml),
            "containerfile" | "dockerfile" => Some(Self::Containerfile),
            "css" => Some(Self::Css),
            "hcl" => Some(Self::Hcl),
            "json" => Some(Self::Json),
            "make" => Some(Self::Make),
            "clojure" => Some(Self::Clojure),
            _ => None,
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" => Some(Self::TypeScript),
            "tsx" | "jsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
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
            "v" => Some(Self::V),
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
            "clj" | "cljs" | "cljc" | "bb" => Some(Self::Clojure),
            _ => None,
        }
    }

    pub fn ts_language(&self) -> TsLanguage {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
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
            Self::MarkdownInline => tree_sitter_md::INLINE_LANGUAGE.into(),
            Self::Starlark => tree_sitter_starlark::LANGUAGE.into(),
            Self::V => tree_sitter_vlang::LANGUAGE.into(),
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
            Self::Clojure => tree_sitter_clojure_orchard::LANGUAGE.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use tree_sitter::Parser;

    use super::Language;

    const JSX_SOURCE: &str = "export const x = <T a={<B c={d} />} />;";
    const TYPE_ASSERTION_SOURCE: &str = "const x = <T>y;";

    fn parses_cleanly(lang: Language, source: &str) -> bool {
        let mut parser = Parser::new();
        parser.set_language(&lang.ts_language()).unwrap();
        !parser.parse(source, None).unwrap().root_node().has_error()
    }

    #[test_case("tsx")]
    #[test_case("jsx")]
    #[test_case("js")]
    fn extension_parses_jsx(ext: &str) {
        assert!(parses_cleanly(
            Language::from_extension(ext).unwrap(),
            JSX_SOURCE
        ));
    }

    #[test]
    fn tsx_name_parses_jsx() {
        assert!(parses_cleanly(
            Language::from_name("tsx").unwrap(),
            JSX_SOURCE
        ));
    }

    // The TSX grammar reads `<T>` as an unclosed tag, so this also pins down
    // that .ts never gets routed there.
    #[test]
    fn ts_extension_keeps_type_assertions() {
        let ts = Language::from_extension("ts").unwrap();
        assert!(parses_cleanly(ts, TYPE_ASSERTION_SOURCE));
    }

    #[test_case("clj")]
    #[test_case("cljs")]
    #[test_case("cljc")]
    #[test_case("bb")]
    fn clojure_extension_parses_cleanly(ext: &str) {
        assert!(parses_cleanly(
            Language::from_extension(ext).unwrap(),
            "(ns a.b (:require [c.d :as e]))\n(defn f [x] (inc x))"
        ));
    }
}
