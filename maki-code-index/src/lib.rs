//! Parses source files into compact skeletons: imports, types, functions, line numbers.
//! Uses tree-sitter for language-specific AST walking. Each language has a `LanguageExtractor`
//! that knows which nodes matter and how to summarize them. Output is ~70-90% smaller than
//! the original file while preserving the structural information an LLM needs.
//! Language support is feature-gated so unused grammars are not compiled in.

use index::common::LanguageExtractor;

pub mod find_symbol;
pub(crate) mod helpers;
pub mod index;

pub use index::{IndexError, index_file, index_source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    #[cfg(feature = "lang-rust")]
    Rust,
    #[cfg(feature = "lang-python")]
    Python,
    #[cfg(feature = "lang-typescript")]
    TypeScript,
    #[cfg(feature = "lang-typescript")]
    JavaScript,
    #[cfg(feature = "lang-go")]
    Go,
    #[cfg(feature = "lang-java")]
    Java,
    #[cfg(feature = "lang-c")]
    C,
    #[cfg(feature = "lang-cpp")]
    Cpp,
    #[cfg(feature = "lang-c-sharp")]
    CSharp,
    #[cfg(feature = "lang-ruby")]
    Ruby,
    #[cfg(feature = "lang-php")]
    Php,
    #[cfg(feature = "lang-swift")]
    Swift,
    #[cfg(feature = "lang-kotlin")]
    Kotlin,
    #[cfg(feature = "lang-scala")]
    Scala,
    #[cfg(feature = "lang-bash")]
    Bash,
    #[cfg(feature = "lang-lua")]
    Lua,
}

impl Language {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            #[cfg(feature = "lang-rust")]
            "rs" => Some(Self::Rust),
            #[cfg(feature = "lang-python")]
            "py" | "pyi" => Some(Self::Python),
            #[cfg(feature = "lang-typescript")]
            "ts" | "tsx" => Some(Self::TypeScript),
            #[cfg(feature = "lang-typescript")]
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            #[cfg(feature = "lang-go")]
            "go" => Some(Self::Go),
            #[cfg(feature = "lang-java")]
            "java" => Some(Self::Java),
            #[cfg(feature = "lang-c")]
            "c" => Some(Self::C),
            #[cfg(feature = "lang-c")]
            "h" => Some(Self::C),
            #[cfg(all(feature = "lang-cpp", not(feature = "lang-c")))]
            "h" => Some(Self::Cpp),
            #[cfg(feature = "lang-cpp")]
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Some(Self::Cpp),
            #[cfg(feature = "lang-c-sharp")]
            "cs" => Some(Self::CSharp),
            #[cfg(feature = "lang-ruby")]
            "rb" | "rake" | "gemspec" => Some(Self::Ruby),
            #[cfg(feature = "lang-php")]
            "php" => Some(Self::Php),
            #[cfg(feature = "lang-swift")]
            "swift" => Some(Self::Swift),
            #[cfg(feature = "lang-kotlin")]
            "kt" | "kts" => Some(Self::Kotlin),
            #[cfg(feature = "lang-scala")]
            "scala" | "sc" => Some(Self::Scala),
            #[cfg(feature = "lang-bash")]
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            #[cfg(feature = "lang-lua")]
            "lua" => Some(Self::Lua),
            _ => None,
        }
    }

    pub fn ts_language(&self) -> tree_sitter::Language {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            #[cfg(feature = "lang-python")]
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            #[cfg(feature = "lang-typescript")]
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            #[cfg(feature = "lang-go")]
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            #[cfg(feature = "lang-java")]
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            #[cfg(feature = "lang-c")]
            Self::C => tree_sitter_c::LANGUAGE.into(),
            #[cfg(feature = "lang-cpp")]
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            #[cfg(feature = "lang-c-sharp")]
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            #[cfg(feature = "lang-ruby")]
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            #[cfg(feature = "lang-php")]
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            #[cfg(feature = "lang-swift")]
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            #[cfg(feature = "lang-scala")]
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            #[cfg(feature = "lang-bash")]
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            #[cfg(feature = "lang-lua")]
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
        }
    }

    fn extractor(&self) -> &dyn LanguageExtractor {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => &index::rust::RustExtractor,
            #[cfg(feature = "lang-python")]
            Self::Python => &index::python::PythonExtractor,
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript => &index::typescript::TsJsExtractor,
            #[cfg(feature = "lang-typescript")]
            Self::JavaScript => &index::typescript::TsJsExtractor,
            #[cfg(feature = "lang-go")]
            Self::Go => &index::go::GoExtractor,
            #[cfg(feature = "lang-java")]
            Self::Java => &index::java::JavaExtractor,
            #[cfg(feature = "lang-c")]
            Self::C => &index::c::CExtractor,
            #[cfg(feature = "lang-cpp")]
            Self::Cpp => &index::cpp::CppExtractor,
            #[cfg(feature = "lang-c-sharp")]
            Self::CSharp => &index::csharp::CSharpExtractor,
            #[cfg(feature = "lang-ruby")]
            Self::Ruby => &index::ruby::RubyExtractor,
            #[cfg(feature = "lang-php")]
            Self::Php => &index::php::PhpExtractor,
            #[cfg(feature = "lang-swift")]
            Self::Swift => &index::swift::SwiftExtractor,
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => &index::kotlin::KotlinExtractor,
            #[cfg(feature = "lang-scala")]
            Self::Scala => &index::scala::ScalaExtractor,
            #[cfg(feature = "lang-bash")]
            Self::Bash => &index::bash::BashExtractor,
            #[cfg(feature = "lang-lua")]
            Self::Lua => &index::lua::LuaExtractor,
        }
    }
}
