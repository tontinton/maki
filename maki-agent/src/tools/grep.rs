use std::io;
use std::path::{Path, PathBuf};

use crate::{GrepFileEntry, GrepLine, GrepMatchGroup};
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::{Sink, SinkContext, SinkFinish, SinkMatch};
use tracing::debug;

use super::{mtime, resolve_search_path, truncate_bytes, walk_builder};

pub(super) const INVALID_REGEX: &str = "invalid regex pattern";
const MULTILINE_HEAP_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_LINE_BYTES: usize = 500;

fn needs_multiline(pattern: &str) -> bool {
    pattern.contains("\\n") || pattern.contains("(?s)") || pattern.contains("(?m)")
}

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

/// Core grep logic. Blocking — caller must run on a thread pool.
/// Returns `(base_path, entries)` where entries have paths relative to base.
pub fn grep_search(params: GrepParams) -> Result<(PathBuf, Vec<GrepFileEntry>), String> {
    let search_path = resolve_search_path(params.path.as_deref())?;
    let is_multiline = needs_multiline(&params.pattern);
    debug!(
        pattern = %params.pattern,
        include = ?params.include,
        path = %search_path,
        context_before = params.context_before,
        context_after = params.context_after,
        is_multiline,
        "grep executing"
    );

    let matcher = if is_multiline {
        RegexMatcher::new(&params.pattern).map_err(|e| format!("{INVALID_REGEX}: {e}"))?
    } else {
        RegexMatcher::new_line_matcher(&params.pattern)
            .or_else(|_| RegexMatcher::new(&params.pattern))
            .map_err(|e| format!("{INVALID_REGEX}: {e}"))?
    };

    let patterns: Vec<&str> = params.include.as_deref().into_iter().collect();
    let walker = walk_builder(&search_path, &patterns)?;

    let mut builder = SearcherBuilder::new();
    builder
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(params.context_before)
        .after_context(params.context_after)
        .multi_line(is_multiline);

    if is_multiline {
        builder.heap_limit(Some(MULTILINE_HEAP_LIMIT));
    }

    let mut searcher = builder.build();

    let search = Path::new(&search_path);
    let base = if search.is_file() {
        search.parent().unwrap_or(search)
    } else {
        search
    };
    let has_context = params.context_before > 0 || params.context_after > 0;
    let mut entries: Vec<GrepFileEntry> = Vec::new();

    for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let mut groups = Vec::new();

        let mut sink = GrepSink {
            groups: &mut groups,
            current_group: Vec::new(),
            max_line_bytes: params.max_line_bytes,
            has_context,
        };
        let _ = searcher.search_path(&matcher, &path, &mut sink);

        if !groups.is_empty() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            entries.push(GrepFileEntry { path: rel, groups });
        }
    }

    if entries.is_empty() {
        return Ok((base.to_path_buf(), entries));
    }

    entries.sort_by(|a, b| {
        let a_abs = base.join(&a.path);
        let b_abs = base.join(&b.path);
        mtime(&b_abs).cmp(&mtime(&a_abs))
    });

    let mut total_groups = 0;
    for entry in &mut entries {
        let remaining = params.limit.saturating_sub(total_groups);
        entry.groups.truncate(remaining);
        total_groups += entry.groups.len();
    }
    entries.retain(|e| !e.groups.is_empty());

    Ok((base.to_path_buf(), entries))
}

struct GrepSink<'a> {
    groups: &'a mut Vec<GrepMatchGroup>,
    current_group: Vec<GrepLine>,
    max_line_bytes: usize,
    has_context: bool,
}

impl GrepSink<'_> {
    fn flush(&mut self) {
        if !self.current_group.is_empty() {
            self.groups.push(GrepMatchGroup {
                lines: std::mem::take(&mut self.current_group),
            });
        }
    }

    fn push_line(&mut self, bytes: &[u8], line_nr: u64, is_match: bool) {
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        self.current_group.push(GrepLine {
            line_nr: line_nr as usize,
            text: truncate_bytes(text, self.max_line_bytes),
            is_match,
        });
    }
}

impl Sink for GrepSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if !self.has_context {
            self.flush();
        }
        let start_line = mat.line_number().unwrap_or(1);
        for (i, line) in mat.lines().enumerate() {
            self.push_line(line, start_line + i as u64, true);
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        let line_nr = context.line_number().unwrap_or(1);
        self.push_line(context.bytes(), line_nr, false);
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        self.flush();
        Ok(true)
    }

    fn finish(&mut self, _searcher: &Searcher, _: &SinkFinish) -> Result<(), io::Error> {
        self.flush();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("foo",       false ; "simple_pattern")]
    #[test_case("foo\\nbar", true  ; "literal_newline")]
    #[test_case("(?s)foo",   true  ; "dotall_flag")]
    #[test_case("(?m)^foo",  true  ; "multiline_flag")]
    fn needs_multiline_detection(pattern: &str, expected: bool) {
        assert_eq!(needs_multiline(pattern), expected);
    }
}
