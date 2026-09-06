//! File browsing, editing, and git status/diff for the web terminal's file
//! panel. Every path this module touches is resolved and jailed to the
//! session's cwd before it reaches disk: this is a new, browser-reachable
//! filesystem surface, and it gets the same discipline as everything else
//! that crossed the wire in the last security pass on this fork.

use std::path::{Path, PathBuf};
use std::process::Command;

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde_json::{Value, json};

use super::App;

/// A human browsing a file in a panel, not a tool call with a token budget —
/// generous, but bounded so a huge log file can't wedge the request thread
/// (it blocks the whole event loop for up to `REQUEST_REPLY_TIMEOUT_SECS`).
const MAX_FILE_BYTES: u64 = 1_500_000;
/// One level of a directory listing; a flat directory with more than this
/// is unusual enough that showing the first slice and stopping beats
/// building a multi-megabyte JSON response nobody will scroll through.
const MAX_ENTRIES: usize = 4000;

/// Resolves `rel` (a path relative to `cwd`, as the browser sent it) to a
/// real, existing path inside `cwd` — refusing anything that canonicalizes
/// outside it, which covers `..`, an absolute path in disguise, and a
/// symlink whose target escapes. Existence is required: this jail is not in
/// the business of deciding where a *new* file would be allowed to go.
fn resolve_in_cwd(cwd: &str, rel: &str) -> Result<PathBuf, String> {
    let root = Path::new(cwd)
        .canonicalize()
        .map_err(|e| format!("session cwd unavailable: {e}"))?;
    let rel = rel.trim_start_matches(['/', '\\']);
    let joined = if rel.is_empty() {
        root.clone()
    } else {
        root.join(rel)
    };
    let resolved = joined.canonicalize().map_err(|_| "not found".to_owned())?;
    if resolved != root && !resolved.starts_with(&root) {
        return Err("path escapes the project directory".to_owned());
    }
    Ok(resolved)
}

/// `git`'s own two-letter porcelain status codes take a plain path with no
/// options special-cased, but the working directory has to be inside the
/// repo (or `-C` pointed at it) for pathspecs to resolve the way a user
/// looking at that directory would expect.
fn git(cwd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Local, read-only plumbing only (status/diff/rev-parse) — nothing
        // here should ever prompt, but this closes that door regardless of
        // what a repo's credential config says.
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("git failed to start: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git diff --no-index` exits 1 (not 0) when it found differences — its way
/// of saying "the files differ", not a failure — so unlike every other git
/// call here, stdout is what's wanted regardless of the exit code; only a
/// spawn failure is a real error.
fn git_diff_no_index(cwd: &str, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Parses `git status --porcelain=v1` into (repo-relative path, XY code).
/// A rename's porcelain line is `XY old -> new`; the new path is what the
/// tree and the diff view both key on, so that's what's kept.
fn parse_porcelain(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let code = line[..2].trim().to_owned();
            let rest = &line[3..];
            let path = rest.rsplit_once(" -> ").map_or(rest, |(_, new)| new);
            Some((path.to_owned(), code))
        })
        .collect()
}

impl App {
    /// `git status --porcelain`, parsed, or `None` when `cwd` isn't inside a
    /// git repo at all (a plain project, not an error the caller needs to
    /// see — the panel just shows no badges).
    fn git_status_entries(&self) -> Option<Vec<(String, String)>> {
        let cwd = &self.state.session.cwd;
        let raw = git(
            cwd,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                ".",
            ],
        )
        .ok()?;
        Some(parse_porcelain(&raw))
    }

    /// One level of a directory under the session's cwd, gitignore-aware
    /// (matching the TUI's own fuzzy file picker), with a git status badge
    /// per entry — a file's own code, or, for a directory, `"*"` when
    /// anything underneath it has one, so a collapsed tree still hints at
    /// where the changes are.
    pub(crate) fn remote_files_list(&self, rel: &str) -> Result<Value, String> {
        let cwd = self.state.session.cwd.clone();
        let dir = resolve_in_cwd(&cwd, rel)?;
        if !dir.is_dir() {
            return Err("not a directory".to_owned());
        }
        let status = self.git_status_entries();
        let root = Path::new(&cwd)
            .canonicalize()
            .map_err(|e| format!("session cwd unavailable: {e}"))?;

        let overrides = OverrideBuilder::new(&dir)
            .add("!.git")
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?;
        let mut entries: Vec<Value> = WalkBuilder::new(&dir)
            .hidden(false)
            // Not `min_depth(Some(1))`: combined with `max_depth`, that
            // silently drops gitignore filtering (verified empirically —
            // depth 0 still gets *walked* to build the ignore stack, but
            // something in how `ignore` prunes below `min_depth` skips
            // applying it). Depth 0 is excluded by hand below instead.
            .max_depth(Some(1))
            .overrides(overrides)
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.depth() != 0)
            .filter(|entry| {
                entry
                    .file_type()
                    .is_some_and(|ft| ft.is_file() || ft.is_dir())
            })
            .take(MAX_ENTRIES)
            .map(|entry| {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                let name = entry.file_name().to_string_lossy().into_owned();
                let size = entry.metadata().ok().map(|m| m.len());
                let entry_rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let git = status.as_ref().and_then(|rows| {
                    if is_dir {
                        let prefix = format!("{entry_rel}/");
                        rows.iter()
                            .any(|(p, _)| p.starts_with(&prefix))
                            .then(|| "*".to_owned())
                    } else {
                        rows.iter()
                            .find(|(p, _)| p == &entry_rel)
                            .map(|(_, code)| code.clone())
                    }
                });
                json!({
                    "name": name,
                    "is_dir": is_dir,
                    "size": size,
                    "git": git,
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            let a_dir = a["is_dir"].as_bool().unwrap_or(false);
            let b_dir = b["is_dir"].as_bool().unwrap_or(false);
            b_dir
                .cmp(&a_dir)
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        Ok(json!({ "path": rel, "entries": entries }))
    }

    /// A file's content for the panel's viewer/editor. Binary files (or ones
    /// too large to be worth showing inline) come back flagged rather than
    /// with their bytes, since raw bytes may not even be valid UTF-8 and
    /// this rides inside a JSON string either way.
    pub(crate) fn remote_file_read(&self, rel: &str) -> Result<Value, String> {
        let cwd = self.state.session.cwd.clone();
        let path = resolve_in_cwd(&cwd, rel)?;
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a file".to_owned());
        }
        let size = meta.len();
        if size > MAX_FILE_BYTES {
            return Ok(json!({ "path": rel, "binary": false, "size": size, "too_large": true }));
        }
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        match String::from_utf8(bytes) {
            Ok(content) => Ok(json!({
                "path": rel,
                "content": content,
                "size": size,
                "binary": false,
                "too_large": false,
            })),
            Err(_) => Ok(json!({ "path": rel, "binary": true, "size": size, "too_large": false })),
        }
    }

    /// Overwrites an existing file's content from the panel's editor. Never
    /// creates a new file or directory — the jail in `resolve_in_cwd`
    /// requires the target to already exist, which keeps this to "edit what
    /// the tree already shows you" rather than opening up arbitrary path
    /// creation from a browser.
    pub(crate) fn remote_file_write(&self, rel: &str, content: &str) -> Result<(), String> {
        let cwd = self.state.session.cwd.clone();
        let path = resolve_in_cwd(&cwd, rel)?;
        if !path.is_file() {
            return Err("not a file".to_owned());
        }
        std::fs::write(&path, content).map_err(|e| e.to_string())
    }

    /// The full `git status`, plus the current branch, for the panel's own
    /// status view (as opposed to the per-entry badges `remote_files_list`
    /// computes from the same data).
    pub(crate) fn remote_git_status(&self) -> Result<Value, String> {
        let cwd = self.state.session.cwd.clone();
        let Some(rows) = self.git_status_entries() else {
            return Ok(json!({ "is_repo": false }));
        };
        let branch = git(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
            .ok()
            .map(|s| s.trim().to_owned());
        let entries: Vec<Value> = rows
            .into_iter()
            .map(|(path, status)| json!({ "path": path, "status": status }))
            .collect();
        Ok(json!({ "is_repo": true, "branch": branch, "entries": entries }))
    }

    /// A unified diff for one file against `HEAD` (covers both staged and
    /// unstaged changes in one view). An untracked file has nothing to diff
    /// against, so it falls back to a `/dev/null` diff, which renders the
    /// whole file as added — same view, same code path either side.
    pub(crate) fn remote_git_diff(&self, rel: &str) -> Result<Value, String> {
        let cwd = self.state.session.cwd.clone();
        // Resolved and jailed the same as a read, but the pathspec handed to
        // git is the caller's own relative string — git resolves that
        // against `-C cwd` itself, and does not need (or want) an absolute
        // path here.
        resolve_in_cwd(&cwd, rel)?;
        let tracked = git(&cwd, &["diff", "--no-color", "HEAD", "--", rel]).unwrap_or_default();
        if !tracked.trim().is_empty() {
            return Ok(json!({ "path": rel, "diff": tracked, "untracked": false }));
        }
        let untracked = git_diff_no_index(
            &cwd,
            &["diff", "--no-color", "--no-index", "--", "/dev/null", rel],
        );
        Ok(json!({ "path": rel, "diff": untracked, "untracked": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_app;
    use std::process::Command as StdCommand;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        StdCommand::new("git")
            .args(["init", "--quiet", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "t@example.com"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        dir
    }

    fn app_at(cwd: &Path) -> App {
        let mut app = test_app();
        app.state
            .session_mut()
            .set_cwd(cwd.to_string_lossy().into_owned());
        app
    }

    #[test]
    fn resolve_in_cwd_refuses_escapes_and_accepts_the_root_and_children() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.txt"), "hi").unwrap();
        let cwd = root.to_string_lossy();

        assert_eq!(resolve_in_cwd(&cwd, "").unwrap(), root);
        assert_eq!(
            resolve_in_cwd(&cwd, "sub/a.txt").unwrap(),
            root.join("sub/a.txt")
        );
        assert!(resolve_in_cwd(&cwd, "../../etc/passwd").is_err());
        assert!(resolve_in_cwd(&cwd, "sub/../../etc/passwd").is_err());
        assert!(resolve_in_cwd(&cwd, "does-not-exist").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn resolve_in_cwd_refuses_a_symlink_that_points_outside() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "nope").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link"))
            .unwrap();
        assert!(resolve_in_cwd(&dir.path().to_string_lossy(), "link").is_err());
    }

    #[test]
    fn remote_files_list_reports_entries_gitignore_and_status_badges() {
        let dir = git_repo();
        std::fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(dir.path().join("ignored.log"), "x").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "one").unwrap();
        StdCommand::new("git")
            .args(["add", "tracked.txt", ".gitignore"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "changed").unwrap();
        std::fs::write(dir.path().join("new.txt"), "brand new").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.txt"), "x").unwrap();

        let app = app_at(dir.path());
        let listing = app.remote_files_list("").unwrap();
        let names: Vec<String> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            !names.contains(&"ignored.log".to_owned()),
            "gitignored file must not appear: {names:?}"
        );
        assert!(names.contains(&"tracked.txt".to_owned()));
        assert!(names.contains(&"new.txt".to_owned()));
        assert!(names.contains(&"sub".to_owned()));

        let by_name = |n: &str| {
            listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("tracked.txt")["git"], "M");
        assert_eq!(by_name("new.txt")["git"], "??");
        assert_eq!(
            by_name("sub")["git"],
            "*",
            "a directory badges as changed when a descendant does"
        );
    }

    #[test]
    fn remote_file_read_detects_binary_and_caps_large_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello world").unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        std::fs::write(
            dir.path().join("huge.txt"),
            vec![b'a'; (MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        let app = app_at(dir.path());

        let text = app.remote_file_read("text.txt").unwrap();
        assert_eq!(text["content"], "hello world");
        assert_eq!(text["binary"], false);

        let bin = app.remote_file_read("bin.dat").unwrap();
        assert_eq!(bin["binary"], true);
        assert!(bin.get("content").is_none());

        let huge = app.remote_file_read("huge.txt").unwrap();
        assert_eq!(huge["too_large"], true);
        assert!(huge.get("content").is_none());

        assert!(app.remote_file_read("../outside").is_err());
    }

    #[test]
    fn remote_file_write_overwrites_existing_but_never_creates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        let app = app_at(dir.path());

        app.remote_file_write("a.txt", "new content").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new content"
        );

        assert!(
            app.remote_file_write("brand-new.txt", "x").is_err(),
            "writing a path that doesn't exist yet must be refused"
        );
        assert!(!dir.path().join("brand-new.txt").exists());
    }

    #[test]
    fn remote_git_status_reports_not_a_repo_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path());
        let status = app.remote_git_status().unwrap();
        assert_eq!(status["is_repo"], false);
    }

    #[test]
    fn remote_git_status_and_diff_reflect_working_tree_changes() {
        let dir = git_repo();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        StdCommand::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let app = app_at(dir.path());

        let status = app.remote_git_status().unwrap();
        assert_eq!(status["is_repo"], true);
        let entries = status["entries"].as_array().unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e["path"] == "a.txt" && e["status"] == "M")
        );

        let diff = app.remote_git_diff("a.txt").unwrap();
        assert_eq!(diff["untracked"], false);
        assert!(diff["diff"].as_str().unwrap().contains("+two"));

        std::fs::write(dir.path().join("b.txt"), "brand new\n").unwrap();
        let untracked_diff = app.remote_git_diff("b.txt").unwrap();
        assert_eq!(untracked_diff["untracked"], true);
        assert!(
            untracked_diff["diff"]
                .as_str()
                .unwrap()
                .contains("brand new")
        );
    }
}
