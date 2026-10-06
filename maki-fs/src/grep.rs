//! Shared grep types. Owned here so both the host and the sandbox can
//! exchange results without depending on each other.

use serde::{Deserialize, Serialize};

const DEFAULT_MAX_LINE_BYTES: usize = 500;

/// Search parameters. `path` is a host path; each backend resolves it in its
/// own address space (defaults to cwd when `None`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepParams {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
    pub context_before: usize,
    pub context_after: usize,
    pub limit: usize,
    pub max_line_bytes: usize,
}

impl GrepParams {
    #[must_use]
    pub fn new(pattern: String) -> Self {
        Self {
            pattern,
            path: None,
            include: None,
            context_before: 0,
            context_after: 0,
            limit: 100,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileEntry {
    pub path: String,
    pub groups: Vec<GrepMatchGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatchGroup {
    pub lines: Vec<GrepLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLine {
    pub line_nr: usize,
    pub text: String,
    pub is_match: bool,
}

impl GrepLine {
    pub fn matched(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: true,
        }
    }

    pub fn context(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: false,
        }
    }
}

impl GrepMatchGroup {
    pub fn single(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            lines: vec![GrepLine::matched(line_nr, text)],
        }
    }

    pub fn match_count(&self) -> usize {
        self.lines.iter().filter(|l| l.is_match).count()
    }
}

impl GrepFileEntry {
    pub fn match_count(&self) -> usize {
        self.groups.iter().map(|g| g.match_count()).sum()
    }
}
