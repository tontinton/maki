use std::path::Path;

use tree_sitter::Parser;

use crate::Language;
use language::ScopeContext;
use search::find_symbol_on_line;

pub mod lang;
pub mod language;
pub(crate) mod search;

pub use language::{CaptureResult, Reference, ReferenceKind, SearchScope};
pub use search::{SearchParams, SearchStats};

#[derive(Debug)]
pub struct FindSymbolResult {
    pub references: Vec<Reference>,
    pub scope: SearchScope,
    pub stats: SearchStats,
}

#[derive(Debug, thiserror::Error)]
pub enum FindSymbolError {
    #[error("unsupported language: .{0}")]
    UnsupportedLanguage(String),
    #[error("no reference language support for {0:?}")]
    NoRefLanguageSupport(Language),
    #[error("failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse file")]
    ParseFailed,
    #[error("symbol '{symbol}' not found at {file}:{line} (occurrence {occurrence})")]
    SymbolNotFound {
        symbol: String,
        file: String,
        line: usize,
        occurrence: usize,
    },
}

pub fn find_symbol(
    project_root: &Path,
    file: &Path,
    line: usize,
    symbol: &str,
    occurrence: usize,
    max_results: Option<usize>,
) -> Result<FindSymbolResult, FindSymbolError> {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    let lang_enum = Language::from_extension(ext)
        .ok_or_else(|| FindSymbolError::UnsupportedLanguage(ext.to_string()))?;
    let ref_lang = lang::ref_language_for(lang_enum)
        .ok_or(FindSymbolError::NoRefLanguageSupport(lang_enum))?;

    let ts_lang = lang_enum.ts_language();
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|_| FindSymbolError::ParseFailed)?;

    let source = std::fs::read(file)?;
    let tree = parser
        .parse(&source, None)
        .ok_or(FindSymbolError::ParseFailed)?;

    let symbol_offset = find_symbol_on_line(
        &tree,
        line,
        symbol,
        occurrence,
        &source,
        ref_lang.identifier_kinds(),
    )
    .ok_or_else(|| FindSymbolError::SymbolNotFound {
        symbol: symbol.to_string(),
        file: file.display().to_string(),
        line,
        occurrence,
    })?;

    let ctx = ScopeContext {
        file,
        source: &source,
        project_root,
    };

    let scope = ref_lang.analyze_scope(&tree, symbol_offset, &ctx);

    let (references, stats) = search::search_scope(&search::SearchParams {
        lang: ref_lang,
        ts_lang: &ts_lang,
        scope: &scope,
        symbol,
        source: Some(&source),
        existing_tree: Some(&tree),
        project_root,
        max_results,
    });

    Ok(FindSymbolResult {
        references,
        scope,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    #[cfg(feature = "lang-python")]
    fn unsupported_ref_language_returns_error() {
        let result = find_symbol(Path::new("/"), Path::new("file.py"), 1, "foo", 1, None);
        assert!(matches!(
            result,
            Err(FindSymbolError::NoRefLanguageSupport(_))
        ));
    }

    #[test]
    fn unknown_extension_returns_error() {
        let result = find_symbol(Path::new("/"), Path::new("file.yaml"), 1, "foo", 1, None);
        assert!(matches!(
            result,
            Err(FindSymbolError::UnsupportedLanguage(_))
        ));
    }
}
