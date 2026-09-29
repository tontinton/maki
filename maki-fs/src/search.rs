//! Shared glob/grep search, used by every [`crate::FsBackend`] so the host and
//! the sandbox produce identical results.
//!
//! These are the ripgrep/`ignore`-powered walks behind `FsBackend::glob` and
//! `FsBackend::grep`. They live here, in the leaf crate both sides depend on,
//! next to the [`crate::grep`] types a backend returns, so the sandbox child
//! links the same code the host runs instead of reaching across to maki-agent
//! or receiving it through a registration seam.

use std::cmp::Reverse;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use tracing::debug;

use crate::grep::{GrepFileEntry, GrepLine, GrepMatchGroup, GrepParams};

const INVALID_REGEX: &str = "invalid regex pattern";
const MULTILINE_HEAP_LIMIT: usize = 64 * 1024 * 1024;

static HOME: LazyLock<Option<PathBuf>> = LazyLock::new(|| etcetera::home_dir().ok());

/// Expand a leading `~` and make the result absolute.
pub fn resolve_path(path: &str) -> Result<String, String> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = HOME.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.join(rest).to_string_lossy().into_owned()
    } else if path == "~" {
        let home = HOME.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.to_string_lossy().into_owned()
    } else {
        path.to_string()
    };

    if Path::new(&expanded).is_relative() {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        Ok(cwd.join(&expanded).to_string_lossy().into_owned())
    } else {
        Ok(expanded)
    }
}

/// The search root a glob/grep runs against, defaulting to the cwd.
pub fn resolve_search_path(path: Option<&str>) -> Result<String, String> {
    match path {
        Some(p) => resolve_path(p),
        None => env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("cwd error: {e}")),
    }
}

pub fn walk_builder(root: &str, patterns: &[&str]) -> Result<WalkBuilder, String> {
    walk_builder_opts(root, patterns, true)
}

/// `.git` is always excluded, even when `gitignore` is false.
pub fn walk_builder_opts(
    root: &str,
    patterns: &[&str],
    gitignore: bool,
) -> Result<WalkBuilder, String> {
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    ob.add("!.git").expect("!.git is a valid glob");

    for p in patterns {
        ob.add(p)
            .map_err(|e| format!("invalid glob pattern: {e}"))?;
    }

    let overrides = ob
        .build()
        .map_err(|e| format!("invalid glob pattern: {e}"))?;

    let mut wb = WalkBuilder::new(root);
    wb.hidden(false).overrides(overrides);
    if !gitignore {
        wb.ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    Ok(wb)
}

pub fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

pub fn truncate_bytes(line: &str, max_bytes: usize) -> String {
    if line.len() > max_bytes {
        let mut boundary = max_bytes;
        while !line.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}...", &line[..boundary])
    } else {
        line.to_owned()
    }
}

/// Walk `root` and return every file matching `patterns`, newest-first when
/// `sort_mtime` is set and truncated to `limit` either way.
pub fn glob_walk(
    root: &Path,
    patterns: &[String],
    gitignore: bool,
    sort_mtime: bool,
    limit: Option<usize>,
) -> Result<Vec<PathBuf>, String> {
    let pattern_refs: Vec<&str> = patterns.iter().map(String::as_str).collect();
    let walker = walk_builder_opts(&root.to_string_lossy(), &pattern_refs, gitignore)?.build();
    let iter = walker
        .flatten()
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()));

    if sort_mtime {
        let mut entries: Vec<_> = iter
            .map(|e| {
                let p = e.into_path();
                let mt = mtime(&p);
                (mt, p)
            })
            .collect();
        entries.sort_unstable_by_key(|(mtime, _)| Reverse(*mtime));
        if let Some(lim) = limit {
            entries.truncate(lim);
        }
        return Ok(entries.into_iter().map(|(_, p)| p).collect());
    }

    let bounded: Box<dyn Iterator<Item = _>> = match limit {
        Some(lim) => Box::new(iter.take(lim)),
        None => Box::new(iter),
    };
    Ok(bounded.map(|e| e.into_path()).collect())
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

    let search = Path::new(&search_path);
    let base = if search.is_file() {
        search.parent().unwrap_or(search)
    } else {
        search
    };
    let has_context = params.context_before > 0 || params.context_after > 0;
    let max_line_bytes = params.max_line_bytes;
    let results: Arc<Mutex<Vec<GrepFileEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let base: Arc<Path> = Arc::from(base);

    walker.build_parallel().run({
        let results = Arc::clone(&results);
        let matcher = Arc::new(matcher);
        let base = Arc::clone(&base);
        move || {
            let mut searcher = builder.build();
            let matcher = Arc::clone(&matcher);
            let results = Arc::clone(&results);
            let base = Arc::clone(&base);
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return WalkState::Continue,
                };
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return WalkState::Continue;
                }
                let path = entry.into_path();
                let mut groups = Vec::new();
                let mut sink = GrepSink {
                    groups: &mut groups,
                    current_group: Vec::new(),
                    max_line_bytes,
                    has_context,
                };
                let _ = searcher.search_path(&*matcher, &path, &mut sink);

                if !groups.is_empty() {
                    let rel = path
                        .strip_prefix(&*base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    let mut guard = results.lock().unwrap_or_else(|e| e.into_inner());
                    guard.push(GrepFileEntry { path: rel, groups });
                }
                WalkState::Continue
            })
        }
    });

    let mut entries = std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner()));

    if entries.is_empty() {
        return Ok((base.to_path_buf(), entries));
    }

    entries.sort_by_cached_key(|e| (Reverse(mtime(&base.join(&e.path))), e.path.clone()));

    let mut total_groups = 0;
    for entry in &mut entries {
        let remaining = params.limit.saturating_sub(total_groups);
        entry.groups.truncate(remaining);
        total_groups += entry.groups.len();
    }
    entries.retain(|e| !e.groups.is_empty());

    Ok((base.to_path_buf(), entries))
}

fn needs_multiline(pattern: &str) -> bool {
    pattern.contains("\\n") || pattern.contains("(?s)") || pattern.contains("(?m)")
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
    use std::env;
    use std::fs::{self, File};
    use std::time::Duration;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::grep::GrepParams;

    #[test]
    fn grep_search_finds_filters_and_skips_binary() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world\ngoodbye world").unwrap();
        fs::write(dir.path().join("b.rs"), "hello rust").unwrap();
        fs::write(dir.path().join("bin.dat"), b"hello \x00 binary").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();

        let mut params = GrepParams::new("hello".into());
        params.path = Some(dir_str.clone());
        let (_, entries) = grep_search(params).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"b.rs"));
        assert!(!paths.contains(&"bin.dat"));

        let mut params = GrepParams::new("hello".into());
        params.path = Some(dir_str.clone());
        params.include = Some("*.rs".into());
        let (_, entries) = grep_search(params).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "b.rs");

        let mut params = GrepParams::new("zzzznotfound".into());
        params.path = Some(dir_str);
        let (_, entries) = grep_search(params).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn grep_search_single_file_preserves_filename() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("demo.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let mut params = GrepParams::new("fn main".into());
        params.path = Some(file.to_string_lossy().into());
        let (_, entries) = grep_search(params).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "demo.rs");
    }

    #[test]
    fn grep_search_invalid_regex_returns_error() {
        let dir = TempDir::new().unwrap();
        let mut params = GrepParams::new("[invalid".into());
        params.path = Some(dir.path().to_string_lossy().into());
        let err = grep_search(params).unwrap_err();
        assert!(err.contains(INVALID_REGEX), "got: {err}");
    }

    #[test]
    fn grep_search_multiline_groups_spanning_lines() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("span.rs"), "fn foo() {\n    bar\n}\n").unwrap();

        let mut params = GrepParams::new("(?s)foo.*\\n}".into());
        params.path = Some(dir.path().to_string_lossy().into());
        let (_, entries) = grep_search(params).unwrap();
        assert_eq!(entries.len(), 1);
        let lines = &entries[0].groups[0].lines;
        assert!(lines.iter().any(|l| l.text.contains("foo") && l.is_match));
    }

    #[test]
    fn grep_search_context_lines_surround_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("ctx.rs"),
            "l1\nl2\nA\nl4\nl5\nl6\nl7\nl8\nB\nl10\n",
        )
        .unwrap();

        let mut params = GrepParams::new("A|B".into());
        params.path = Some(dir.path().to_string_lossy().into());
        params.context_before = 1;
        params.context_after = 1;
        let (_, entries) = grep_search(params).unwrap();
        assert_eq!(entries[0].groups.len(), 2);

        let g0 = &entries[0].groups[0].lines;
        assert!(g0.iter().any(|l| l.text == "l2" && !l.is_match));
        assert!(g0.iter().any(|l| l.text == "A" && l.is_match));

        let g1 = &entries[0].groups[1].lines;
        assert!(g1.iter().any(|l| l.text == "B" && l.is_match));
        assert!(g1.iter().any(|l| l.text == "l10" && !l.is_match));
    }

    #[test]
    fn grep_search_parallel_stable_under_repeated_calls() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let tied_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for i in 0..20u32 {
            let path = root.join(format!("f{i:03}.rs"));
            fs::write(&path, format!("needle {i}\n")).unwrap();
            let f = File::options().write(true).open(&path).unwrap();
            f.set_modified(tied_mtime).unwrap();
        }
        let path_str = root.to_string_lossy().to_string();

        let mut reference: Option<Vec<(String, usize, bool)>> = None;
        for _ in 0..20 {
            let mut params = GrepParams::new("needle".into());
            params.path = Some(path_str.clone());
            params.limit = 1000;
            let (_, entries) = grep_search(params).unwrap();

            let flat: Vec<(String, usize, bool)> = entries
                .iter()
                .flat_map(|e| {
                    e.groups.iter().flat_map(|g| {
                        g.lines
                            .iter()
                            .map(|l| (e.path.clone(), l.line_nr, l.is_match))
                    })
                })
                .collect();
            match &reference {
                None => reference = Some(flat),
                Some(prev) => assert_eq!(flat, *prev),
            }
        }
    }

    #[test]
    fn grep_search_limit_truncates_groups_after_sort() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for i in 0..10u32 {
            fs::write(root.join(format!("m_{i}.rs")), "hit\n").unwrap();
        }

        let mut params = GrepParams::new("hit".into());
        params.path = Some(root.to_string_lossy().into());
        params.limit = 3;
        let (_, entries) = grep_search(params).unwrap();

        let total_groups: usize = entries.iter().map(|e| e.groups.len()).sum();
        assert_eq!(total_groups, 3);
    }

    #[test]
    fn glob_walk_filters_and_sorts_by_mtime() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        fs::write(root.join("b.py"), "print('hi')").unwrap();

        let all = glob_walk(root, &[], true, false, None).unwrap();
        assert_eq!(all.len(), 2, "got: {all:?}");

        let rs_only = glob_walk(root, &["*.rs".into()], true, false, None).unwrap();
        assert_eq!(rs_only.len(), 1, "got: {rs_only:?}");
        assert!(rs_only[0].ends_with("a.rs"), "got: {rs_only:?}");

        let limited = glob_walk(root, &["*.rs".into()], true, true, Some(1)).unwrap();
        assert_eq!(limited, rs_only, "limit=1 keeps the newest match");
    }

    #[test]
    fn walk_builder_excludes_dot_git_shows_dotfiles_and_filters_globs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join(".git/config"), "stuff").unwrap();
        fs::write(root.join(".git/objects/abc123"), "blob").unwrap();
        fs::write(root.join(".env"), "SECRET=42").unwrap();
        fs::write(root.join("lib.rs"), "pub fn foo() {}").unwrap();
        fs::write(root.join("main.py"), "print('hi')").unwrap();

        let root_str = root.to_string_lossy();
        let collect = |patterns: &[&str]| -> Vec<String> {
            // A developer's global gitignore decides for itself whether a
            // dotfile like `.env` is ignored, and this test is about our
            // filters, not theirs.
            let mut wb = walk_builder(&root_str, patterns).unwrap();
            wb.git_global(false);
            wb.build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .map(|e| {
                    e.path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        };

        let all = collect(&[]);
        assert!(all.contains(&"lib.rs".into()));
        assert!(all.contains(&".env".into()), "dotfiles must be shown");
        assert!(
            !all.iter().any(|p| p.starts_with(".git")),
            ".git must be excluded"
        );

        let rs_only = collect(&["*.rs"]);
        assert!(rs_only.contains(&"lib.rs".into()));
        assert!(!rs_only.contains(&"main.py".into()), "glob must filter");
        assert!(!rs_only.iter().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn resolve_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = HOME.as_deref().unwrap();

        assert_eq!(
            resolve_path("~/foo/bar").unwrap(),
            home.join("foo/bar").to_string_lossy()
        );
        assert_eq!(resolve_path("~").unwrap(), home.to_string_lossy());
        assert_eq!(
            resolve_path("src/main.rs").unwrap(),
            cwd.join("src/main.rs").to_string_lossy()
        );

        // `/etc/hosts` is absolute on Unix (passed through unchanged) but
        // root-relative on Windows (no drive prefix, so `is_relative()` is
        // true and it gets joined with cwd, producing e.g. `C:\etc\hosts`).
        #[cfg(windows)]
        {
            #[allow(clippy::join_absolute_paths)]
            let expected = cwd.join("/etc/hosts");
            assert_eq!(
                resolve_path("/etc/hosts").unwrap(),
                expected.to_string_lossy()
            );
        }
        #[cfg(not(windows))]
        assert_eq!(resolve_path("/etc/hosts").unwrap(), "/etc/hosts");
    }

    #[test]
    fn resolve_search_path_defaults_to_cwd() {
        let cwd = env::current_dir().unwrap();
        assert_eq!(
            resolve_search_path(None).unwrap(),
            cwd.to_string_lossy().into_owned()
        );
    }

    #[test]
    fn walk_builder_opts_gitignore_false_includes_ignored() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("test.log"), "log data").unwrap();
        fs::write(root.join("test.txt"), "text data").unwrap();

        let root_str = root.to_string_lossy();

        let collect = |wb: WalkBuilder| -> Vec<String> {
            wb.build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .map(|e| e.into_path().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };

        let with_ignored = collect(walk_builder_opts(&root_str, &[], false).unwrap());
        assert!(
            with_ignored.iter().any(|p| p.ends_with("test.log")),
            "gitignore=false should include test.log, got: {with_ignored:?}"
        );
        assert!(
            with_ignored.iter().any(|p| p.ends_with("test.txt")),
            "gitignore=false should include test.txt, got: {with_ignored:?}"
        );

        let without_ignored = collect(walk_builder_opts(&root_str, &[], true).unwrap());
        assert!(
            !without_ignored.iter().any(|p| p.ends_with("test.log")),
            "gitignore=true should exclude test.log, got: {without_ignored:?}"
        );
        assert!(
            without_ignored.iter().any(|p| p.ends_with("test.txt")),
            "gitignore=true should include test.txt, got: {without_ignored:?}"
        );

        assert!(
            !with_ignored.iter().any(|p| p.contains(".git/")),
            ".git/ must be excluded even with gitignore=false, got: {with_ignored:?}"
        );
    }

    #[test]
    fn walk_builder_invalid_pattern_returns_error() {
        let tmp = TempDir::new().unwrap();
        let root_str = tmp.path().to_string_lossy();
        let err = walk_builder(&root_str, &["["]).unwrap_err();
        assert!(
            err.contains("invalid glob pattern"),
            "expected 'invalid glob pattern', got: {err}"
        );
    }

    #[test_case("foo",       false ; "simple_pattern")]
    #[test_case("foo\\nbar", true  ; "literal_newline")]
    #[test_case("(?s)foo",   true  ; "dotall_flag")]
    #[test_case("(?m)^foo",  true  ; "multiline_flag")]
    fn needs_multiline_detection(pattern: &str, expected: bool) {
        assert_eq!(needs_multiline(pattern), expected);
    }

    const LINE_LIMIT: usize = 2000;

    #[test_case("short",                            "short"                             ; "short_passthrough")]
    #[test_case(&"x".repeat(LINE_LIMIT),       &"x".repeat(LINE_LIMIT)        ; "exact_boundary")]
    #[test_case(&"x".repeat(LINE_LIMIT + 500), &format!("{}...", "x".repeat(LINE_LIMIT)) ; "long_truncated")]
    #[test_case(&format!("{}\u{1F600}", "a".repeat(LINE_LIMIT - 1)), &format!("{}...", "a".repeat(LINE_LIMIT - 1)) ; "multibyte_char_boundary")]
    #[test_case(&format!("{}\u{0430}tail", "a".repeat(LINE_LIMIT - 1)), &format!("{}...", "a".repeat(LINE_LIMIT - 1)) ; "two_byte_char_boundary")]
    fn truncate_bytes_cases(input: &str, expected: &str) {
        assert_eq!(truncate_bytes(input, LINE_LIMIT), expected);
    }
}
