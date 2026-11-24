use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::{GrepFileEntry, GrepLine, GrepMatchGroup};
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::{Sink, SinkContext, SinkFinish, SinkMatch};
use ignore::WalkState;
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

/// What `limit` cut. `files` counts every file that lost a group, including
/// one that still shows some.
#[derive(Debug, PartialEq, Eq)]
pub struct Omitted {
    pub groups: usize,
    pub files: usize,
}

#[derive(Debug)]
pub struct GrepResults {
    pub base: PathBuf,
    /// Paths are relative to `base`.
    pub entries: Vec<GrepFileEntry>,
    pub omitted: Option<Omitted>,
}

/// A file that has matching lines, and how many.
#[derive(Debug, PartialEq, Eq)]
pub struct GrepFileCount {
    pub path: PathBuf,
    pub matches: usize,
}

/// Runs `search_file` on every file the walk finds and keeps what it returns.
/// The hits come back newest file first, with paths relative to the base.
fn search_files<T: Send>(
    params: &GrepParams,
    search_file: impl Fn(&mut Searcher, &RegexMatcher, &Path) -> Option<T> + Sync,
) -> Result<(PathBuf, Vec<(String, T)>), String> {
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

    let search = Path::new(&search_path);
    let base = if search.is_file() {
        search.parent().unwrap_or(search)
    } else {
        search
    };
    let hits = Mutex::new(Vec::new());

    walker.build_parallel().run({
        let (hits, matcher, search_file) = (&hits, &matcher, &search_file);
        move || {
            let mut searcher = builder.build();
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return WalkState::Continue,
                };
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return WalkState::Continue;
                }
                let path = entry.into_path();
                if let Some(hit) = search_file(&mut searcher, matcher, &path) {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    hits.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((rel, hit));
                }
                WalkState::Continue
            })
        }
    });

    let mut hits = hits.into_inner().unwrap_or_else(PoisonError::into_inner);
    hits.sort_by_cached_key(|(path, _)| (std::cmp::Reverse(mtime(&base.join(path))), path.clone()));
    Ok((base.to_path_buf(), hits))
}

/// Counts the matching lines of each file instead of collecting them.
/// Blocking — caller must run on a thread pool.
pub fn grep_count(params: &GrepParams) -> Result<Vec<GrepFileCount>, String> {
    let (base, hits) = search_files(params, |searcher, matcher, path| {
        let mut sink = CountSink(0);
        let _ = searcher.search_path(matcher, path, &mut sink);
        (sink.0 > 0).then_some(sink.0)
    })?;
    Ok(hits
        .into_iter()
        .map(|(path, matches)| GrepFileCount {
            path: base.join(path),
            matches,
        })
        .collect())
}

/// Core grep logic. Blocking — caller must run on a thread pool.
pub fn grep_search(params: GrepParams) -> Result<GrepResults, String> {
    let has_context = params.context_before > 0 || params.context_after > 0;
    let max_line_bytes = params.max_line_bytes;
    let (base, hits) = search_files(&params, |searcher, matcher, path| {
        let mut groups = Vec::new();
        let mut sink = GrepSink {
            groups: &mut groups,
            current_group: Vec::new(),
            max_line_bytes,
            has_context,
        };
        let _ = searcher.search_path(matcher, path, &mut sink);
        (!groups.is_empty()).then_some(groups)
    })?;
    let mut entries: Vec<_> = hits
        .into_iter()
        .map(|(path, groups)| GrepFileEntry { path, groups })
        .collect();

    let mut kept = 0;
    let mut omitted = Omitted {
        groups: 0,
        files: 0,
    };
    for entry in &mut entries {
        let keep = entry.groups.len().min(params.limit.saturating_sub(kept));
        if keep < entry.groups.len() {
            omitted.groups += entry.groups.len() - keep;
            omitted.files += 1;
        }
        entry.groups.truncate(keep);
        kept += keep;
    }
    entries.retain(|e| !e.groups.is_empty());

    Ok(GrepResults {
        base,
        entries,
        omitted: (omitted.groups > 0).then_some(omitted),
    })
}

struct CountSink(usize);

impl Sink for CountSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.0 += mat.lines().count();
        Ok(true)
    }
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
            text: if self.max_line_bytes == 0 {
                text.to_owned()
            } else {
                truncate_bytes(text, self.max_line_bytes)
            },
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

    const TEST_LINE: &[u8] = "match é tail\r\n".as_bytes();
    const NEEDLE: &str = "needle";

    #[test_case("foo",       false ; "simple_pattern")]
    #[test_case("foo\\nbar", true  ; "literal_newline")]
    #[test_case("(?s)foo",   true  ; "dotall_flag")]
    #[test_case("(?m)^foo",  true  ; "multiline_flag")]
    fn needs_multiline_detection(pattern: &str, expected: bool) {
        assert_eq!(needs_multiline(pattern), expected);
    }

    #[test_case(0, "match é tail" ; "unlimited")]
    #[test_case(7, "match ..." ; "bounded_utf8")]
    fn sink_line_byte_limit(max_line_bytes: usize, expected: &str) {
        let mut groups = Vec::new();
        let mut sink = GrepSink {
            groups: &mut groups,
            current_group: Vec::new(),
            max_line_bytes,
            has_context: true,
        };
        sink.push_line(TEST_LINE, 2, true);
        sink.push_line(TEST_LINE, 3, false);
        sink.flush();

        let lines = &groups[0].lines;
        assert_eq!(lines[0].text, expected);
        assert_eq!(lines[1].text, expected);
    }

    #[test_case(None, &[("one.rs", 1), ("two.txt", 2)] ; "every_file_with_a_match")]
    #[test_case(Some("*.txt"), &[("two.txt", 2)] ; "only_included_files")]
    fn count_lists_matching_lines_per_file(include: Option<&str>, expected: &[(&str, usize)]) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.rs"), "needle\nhay\n").unwrap();
        std::fs::write(dir.path().join("two.txt"), "needle needle\nhay\nneedle\n").unwrap();
        std::fs::write(dir.path().join("none.txt"), "hay\n").unwrap();
        let params = GrepParams {
            path: Some(dir.path().to_string_lossy().into_owned()),
            include: include.map(str::to_owned),
            ..GrepParams::new(NEEDLE.into())
        };

        let mut counts: Vec<_> = grep_count(&params)
            .unwrap()
            .into_iter()
            .map(|file| (file.path.file_name().unwrap().to_owned(), file.matches))
            .collect();
        counts.sort();

        let expected: Vec<_> = expected
            .iter()
            .map(|&(name, matches)| (name.into(), matches))
            .collect();
        assert_eq!(counts, expected);
    }
}
