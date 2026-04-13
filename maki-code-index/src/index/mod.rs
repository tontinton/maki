use std::path::Path;

use tree_sitter::Parser;

use crate::Language;
use common::{detect_module_doc, doc_comment_start_line, format_skeleton};

#[cfg(test)]
pub(crate) const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;

#[cfg(feature = "lang-bash")]
pub(crate) mod bash;
#[cfg(feature = "lang-c")]
pub(crate) mod c;
pub(crate) mod common;
#[cfg(feature = "lang-cpp")]
pub(crate) mod cpp;
#[cfg(feature = "lang-c-sharp")]
pub(crate) mod csharp;
#[cfg(feature = "lang-elixir")]
pub(crate) mod elixir;
#[cfg(feature = "lang-go")]
pub(crate) mod go;
#[cfg(feature = "lang-java")]
pub(crate) mod java;
#[cfg(feature = "lang-kotlin")]
pub(crate) mod kotlin;
#[cfg(feature = "lang-lua")]
pub(crate) mod lua;
#[cfg(feature = "lang-php")]
pub(crate) mod php;
#[cfg(feature = "lang-python")]
pub(crate) mod python;
#[cfg(feature = "lang-ruby")]
pub(crate) mod ruby;
#[cfg(feature = "lang-rust")]
pub(crate) mod rust;
#[cfg(feature = "lang-scala")]
pub(crate) mod scala;
#[cfg(feature = "lang-swift")]
pub(crate) mod swift;
#[cfg(feature = "lang-typescript")]
pub(crate) mod typescript;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("unsupported file type: {0}")]
    UnsupportedLanguage(String),
    #[error("file too large ({size} bytes, max {max})")]
    FileTooLarge { size: u64, max: u64 },
    #[error("read error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: tree-sitter failed to parse file")]
    ParseFailed,
}

pub fn index_file(path: &Path, max_file_size: u64) -> Result<String, IndexError> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let lang = Language::from_extension(ext)
        .ok_or_else(|| IndexError::UnsupportedLanguage(format!(".{ext}")))?;

    let meta = std::fs::metadata(path)?;
    if meta.len() > max_file_size {
        return Err(IndexError::FileTooLarge {
            size: meta.len(),
            max: max_file_size,
        });
    }

    let source = std::fs::read(path)?;
    index_source(&source, lang)
}

pub fn index_source(source: &[u8], lang: Language) -> Result<String, IndexError> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.ts_language())
        .map_err(|_| IndexError::ParseFailed)?;

    let tree = parser.parse(source, None).ok_or(IndexError::ParseFailed)?;
    let root = tree.root_node();
    let extractor = lang.extractor();

    let module_doc = detect_module_doc(root, source, extractor);
    let mut entries = Vec::new();
    let mut test_lines: Vec<usize> = Vec::new();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if extractor.is_attr(child) || extractor.is_doc_comment(child, source) {
            continue;
        }
        let attrs = extractor.collect_preceding_attrs(child);
        if extractor.is_test_node(child, source, &attrs) {
            test_lines.push(child.start_position().row + 1);
            continue;
        }
        for (i, mut entry) in extractor
            .extract_nodes(child, source, &attrs)
            .into_iter()
            .enumerate()
        {
            if i == 0
                && let Some(doc_start) = doc_comment_start_line(child, source, extractor)
            {
                entry.line_start = entry.line_start.min(doc_start);
            }
            entries.push(entry);
        }
    }

    Ok(format_skeleton(
        &entries,
        &test_lines,
        module_doc,
        extractor.import_separator(),
    ))
}
