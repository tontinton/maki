use std::ops::Range;
use std::path::{Path, PathBuf};

use tree_sitter::{Query, Tree};

#[derive(Debug, Clone)]
pub enum SearchScope {
    Local {
        file: PathBuf,
        range: Option<Range<usize>>,
    },
    Project {
        subtree: Option<PathBuf>,
    },
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub kind: ReferenceKind,
    pub context: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    Definition,
    Call,
    TypeRef,
    FieldRef,
    MacroUse,
    Read,
    Write,
    Unknown,
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Definition => write!(f, "def"),
            Self::Call => write!(f, "call"),
            Self::TypeRef => write!(f, "type_ref"),
            Self::FieldRef => write!(f, "field_ref"),
            Self::MacroUse => write!(f, "macro_use"),
            Self::Read => write!(f, "read"),
            Self::Write => write!(f, "write"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureResult {
    pub kind: ReferenceKind,
    pub text_search: bool,
}

pub struct ScopeContext<'a> {
    pub file: &'a Path,
    pub source: &'a [u8],
    pub project_root: &'a Path,
}

pub trait RefLanguage: Send + Sync {
    fn extensions(&self) -> &'static [&'static str];
    fn reference_query(&self) -> &Query;
    fn identifier_kinds(&self) -> &'static [&'static str];

    fn analyze_scope(
        &self,
        tree: &Tree,
        symbol_byte_offset: usize,
        ctx: &ScopeContext,
    ) -> SearchScope;

    fn classify_capture(
        &self,
        capture_name: &str,
        node: tree_sitter::Node,
        source: &[u8],
        symbol: &str,
    ) -> Option<CaptureResult>;
}

impl std::fmt::Display for SearchScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local {
                file,
                range: Some(r),
            } => write!(f, "byte range {}[{}..{}]", file.display(), r.start, r.end),
            Self::Local { file, range: None } => write!(f, "file {}", file.display()),
            Self::Project { subtree: None } => write!(f, "project-wide"),
            Self::Project {
                subtree: Some(path),
            } => write!(f, "subtree {}", path.display()),
        }
    }
}

impl Reference {
    pub fn format_relative(&self, root: &Path) -> String {
        let rel = self.path.strip_prefix(root).unwrap_or(&self.path);
        let text = truncate_context(&self.context);
        format!(
            "{}:{}:{} ({}) {}",
            rel.display(),
            self.line,
            self.col,
            self.kind,
            text,
        )
    }
}

const MAX_CONTEXT_LEN: usize = 120;

fn truncate_context(ctx: &str) -> &str {
    let trimmed = ctx.trim();
    if trimmed.len() <= MAX_CONTEXT_LEN {
        return trimmed;
    }
    let mut end = MAX_CONTEXT_LEN;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    &trimmed[..end]
}
