//! `--worktree`: create a git worktree and launch maki inside it, so parallel
//! sessions work on isolated file trees that cannot collide on edits.
//!
//! Runs in the synchronous launch path, before the async runtime and the TUI
//! start, so everything downstream (cwd, config discovery, session resume,
//! the permissions root) naturally binds to the worktree.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

/// Worktree checkouts nest under this directory at the git root.
const WORKTREES_REL: &str = ".maki/worktrees";
/// Files listed here are copied into a fresh worktree.
const WORKTREE_INCLUDE: &str = ".worktreeinclude";

/// Where a new worktree branches from.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum BaseRef {
    /// Branch from the current local HEAD, carrying unpushed work. Default.
    #[default]
    Head,
    /// Branch from the repository's default branch fetched from the remote.
    Fresh,
}

/// Options controlling a worktree session.
#[derive(Clone)]
pub struct WorktreeOptions {
    pub base_ref: BaseRef,
    /// Override where the worktree checkout lands. Defaults to
    /// `<root>/.maki/worktrees/<name>`.
    pub dir_override: Option<PathBuf>,
}

impl Default for WorktreeOptions {
    fn default() -> Self {
        Self {
            base_ref: BaseRef::Head,
            dir_override: None,
        }
    }
}

/// What a worktree session entered. `base` is the commit the worktree branched
/// from, captured at creation and used by cleanup to prove the worktree holds
/// no work before auto-removing it. It is `None` when reusing an existing
/// worktree, whose history was not created by this session.
pub struct EnteredWorktree {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub name: String,
    pub base: Option<String>,
}

/// Create a worktree from the process's current directory, chdir into it, and
/// return what it entered. `name` may be `None` to auto-generate one.
///
/// `git worktree add` runs synchronously; the launch path has no runtime yet.
pub fn enter(name: Option<&str>, opts: &WorktreeOptions) -> Result<EnteredWorktree> {
    let cwd = env::current_dir().context("worktree: resolve current directory")?;
    enter_at(&cwd, name, opts)
}

/// Core of [`enter`], parameterized by the starting directory so tests can
/// drive it without mutating the process-global cwd.
fn enter_at(cwd: &Path, name: Option<&str>, opts: &WorktreeOptions) -> Result<EnteredWorktree> {
    let root = git_root(cwd).ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "not inside a git repository (from {}); --worktree needs a git checkout",
            cwd.display()
        )
    })?;

    let name = match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => generate_name(&root)?,
    };
    enter_named(&root, &name, opts)
}

fn enter_named(root: &Path, name: &str, opts: &WorktreeOptions) -> Result<EnteredWorktree> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!("worktree name must be alnum, '-', '_' or '.': got {name:?}");
    }
    if name.starts_with('.') || name.contains("..") {
        bail!("worktree name cannot start with '.' or contain '..': got {name:?}");
    }

    let dir = opts
        .dir_override
        .clone()
        .unwrap_or_else(|| root.join(WORKTREES_REL).join(name));
    if opts.dir_override.is_none() {
        ensure_ignored(root);
    }

    // Reuse an existing worktree of the same name instead of failing.
    if dir.is_dir() {
        env::set_current_dir(&dir).with_context(|| format!("enter worktree {}", dir.display()))?;
        return Ok(EnteredWorktree {
            root: root.to_path_buf(),
            dir,
            name: name.to_string(),
            base: None,
        });
    }

    let dir_str = dir.to_str().unwrap_or(name);
    let branch = format!("worktree-{name}");
    // Attach to an existing `worktree-<name>` branch instead of erroring when a
    // previous worktree was deleted but its branch was left behind.
    let branch_exists = git_status(
        true,
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )? == 0;
    let start = match opts.base_ref {
        BaseRef::Head => "HEAD".to_string(),
        BaseRef::Fresh => fresh_base(root)?,
    };
    // Resolve the branch point to a commit so cleanup can prove the worktree
    // has not diverged from it before auto-removing the tree.
    let base_commit = resolve_commit(root, &start)?;
    let mut add_args = vec!["worktree", "add"];
    if !branch_exists {
        add_args.extend(["-b", branch.as_str()]);
    }
    add_args.push(dir_str);
    add_args.push(start.as_str());
    let status = git_status(false, root, &add_args)?;
    if status != 0 {
        bail!("git worktree add failed for {name:?}");
    }

    copy_included(root, &dir);
    env::set_current_dir(&dir).with_context(|| format!("enter worktree {}", dir.display()))?;
    Ok(EnteredWorktree {
        root: root.to_path_buf(),
        dir,
        name: name.to_string(),
        base: Some(base_commit),
    })
}

/// Resolve a ref (e.g. `HEAD`, `main`) to a commit SHA in the source checkout.
fn resolve_commit(root: &Path, r: &str) -> Result<String> {
    let output = git_output(root, &["rev-parse", "--verify", r])
        .map_err(|e| color_eyre::eyre::eyre!("resolve base {r:?} for worktree: {e}"))?;
    if !output.status.success() {
        bail!("could not resolve base {r:?} for worktree");
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        bail!("could not resolve base {r:?} for worktree");
    }
    Ok(sha)
}

/// The base to branch a "fresh" worktree from: the repository's default branch,
/// falling back to the current HEAD when none can be determined.
fn fresh_base(root: &Path) -> Result<String> {
    Ok(default_base(root).unwrap_or_else(|| "HEAD".into()))
}

/// Auto-generate a free `adjective-noun` name for an unnamed worktree.
fn generate_name(root: &Path) -> Result<String> {
    const ADJ: &[&str] = &[
        "bright", "quiet", "rapid", "calm", "swift", "bold", "fresh", "mellow",
    ];
    const NOUN: &[&str] = &[
        "fox", "river", "falcon", "meadow", "otter", "raven", "pine", "ember",
    ];
    for _ in 0..64 {
        let name = format!(
            "{}-{}",
            ADJ[rand_index(ADJ.len())],
            NOUN[rand_index(NOUN.len())]
        );
        if !root.join(WORKTREES_REL).join(&name).exists()
            && git_status(
                true,
                root,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/worktree-{name}"),
                ],
            )? != 0
        {
            return Ok(name);
        }
    }
    bail!("could not generate a free worktree name")
}

/// Walk up from `start` for a directory containing a `.git` directory or a
/// `.git` file that names a linked worktree (what `git worktree` itself uses).
fn git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| is_repo_root(dir))
        .map(Path::to_path_buf)
}

fn is_repo_root(dir: &Path) -> bool {
    let dotgit = dir.join(".git");
    dotgit.is_dir()
        || (dotgit.is_file()
            && fs::read_to_string(&dotgit)
                .map(|s| s.starts_with("gitdir:"))
                .unwrap_or(false))
}

/// Make sure `<root>/.maki/worktrees/` is ignored so parallel checkouts do not
/// show up as untracked files in the main checkout. Written to
/// `.git/info/exclude` rather than the tracked `.gitignore`, which would dirty
/// the tree and invite accidental commits.
fn ensure_ignored(root: &Path) {
    let entry = format!("{WORKTREES_REL}/");
    let Some(exclude) = git_dir(root).map(|g| g.join("info/exclude")) else {
        return;
    };
    let ok = fs::read_to_string(&exclude)
        .map(|s| s.lines().any(|l| l.trim() == entry))
        .unwrap_or(false);
    if !ok {
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(exclude)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(format!("\n{entry}\n").as_bytes())
            });
    }
}

/// The git metadata dir for `root`: `<root>/.git`, or the dir a `.git` file
/// points at (a linked worktree).
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dotgit = root.join(".git");
    if dotgit.is_dir() {
        return Some(dotgit);
    }
    if dotgit.is_file() {
        let pointer = fs::read_to_string(&dotgit).ok()?;
        let p = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        return Some(if p.is_absolute() { p } else { root.join(p) });
    }
    None
}

/// Copy files matched by `WORKTREE_INCLUDE` (gitignore syntax) if they exist in
/// the main checkout and are gitignored, mirroring Claude Code's worktree setup.
fn copy_included(root: &Path, worktree: &Path) {
    let list = match fs::read_to_string(root.join(WORKTREE_INCLUDE)) {
        Ok(s) => s,
        Err(_) => return,
    };
    for pat in list.lines() {
        let pat = pat.trim().trim_end_matches('/');
        if pat.is_empty() || pat.starts_with('#') {
            continue;
        }
        let src = root.join(pat);
        if src.is_dir() {
            copy_tree_ignored(root, &src, &worktree.join(pat));
        } else if src.is_file() && is_ignored(root, &src) {
            copy_file(root, &src, &worktree.join(pat));
        }
    }
}

fn copy_file(root: &Path, src: &Path, dst: &Path) {
    if let Some(parent) = dst.parent()
        && let Ok(()) = fs::create_dir_all(parent)
        && is_ignored(root, src)
    {
        let _ = fs::copy(src, dst);
    }
}

/// Recursively copy the gitignored files under a `.worktreeinclude` directory.
fn copy_tree_ignored(root: &Path, src: &Path, dst: &Path) {
    let Ok(entries) = fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree_ignored(root, &from, &to);
        } else if is_ignored(root, &from) {
            copy_file(root, &from, &to);
        }
    }
}

fn is_ignored(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    git_status(
        true,
        root,
        &["check-ignore", "-q", "--", rel.to_string_lossy().as_ref()],
    )
    .map(|status| status == 0)
    .unwrap_or(false)
}

/// Run git and return its exit code. Only spawn failures are errors; a
/// non-zero exit is returned as-is so callers can interpret it. Pass `check`
/// for commands whose non-zero exit is a meaningful answer (e.g.
/// `check-ignore`) rather than a failure.
fn git_status(check: bool, cwd: &Path, args: &[&str]) -> Result<i32> {
    let output = git_output(cwd, args).with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !output.status.success() && !check {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(output.status.code().unwrap_or(1))
}

fn git_output(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .output()
}

/// True when removing `dir` would delete work: changed/untracked files or
/// commits not on the base this worktree was created from. Fails closed: git
/// unavailable or an unknown base counts as having work, so an unnamed clean
/// worktree is only auto-removed when its emptiness is provable.
pub fn has_leftover_work(dir: &Path, base: Option<&str>) -> bool {
    let clean = git_output(dir, &["status", "--porcelain"])
        .map(|o| o.status.success() && o.stdout.is_empty())
        .unwrap_or(false);
    if !clean {
        return true;
    }
    let Some(base) = base else {
        return true;
    };
    git_output(dir, &["rev-list", "--count", &format!("{base}..HEAD")])
        .map(|o| !o.status.success() || has_nonzero(&o.stdout))
        .unwrap_or(true)
}

fn has_nonzero(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| *b != b'0' && !b.is_ascii_whitespace())
}

/// A ref the worktree can be measured against: the repository's default branch
/// (remote `origin/HEAD`/`main`/`master`, else local `main`/`master`).
fn default_base(dir: &Path) -> Option<String> {
    for cand in [
        &["symbolic-ref", "refs/remotes/origin/HEAD"][..],
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/main",
        ][..],
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/master",
        ][..],
        &["rev-parse", "--verify", "--quiet", "main"][..],
        &["rev-parse", "--verify", "--quiet", "master"][..],
    ] {
        if let Ok(o) = git_output(dir, cand)
            && o.status.success()
        {
            return Some(String::from_utf8_lossy(&o.stdout).trim().to_string());
        }
    }
    None
}

/// Remove a worktree and its branch. Runs git from `root` (a working tree
/// other than `dir`, since git refuses to remove the current one).
pub fn remove(root: &Path, dir: &Path, name: &str) -> Result<()> {
    let dir_str = dir.to_str().unwrap_or(name);
    git_status(false, root, &["worktree", "remove", "--force", dir_str])?;
    // Branch deletion is best-effort cleanup; a leftover branch is harmless.
    let _ = git_status(false, root, &["branch", "-D", &format!("worktree-{name}")]);
    Ok(())
}

fn rand_index(len: usize) -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_nanos() as usize) % len)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/config"), "[core]\n").unwrap();
    }

    #[test]
    fn rejects_unsafe_names() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        for bad in ["a/b", "..", ".hidden", "a..b"] {
            assert!(
                enter_at(tmp.path(), Some(bad), &WorktreeOptions::default()).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn requires_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(enter_at(tmp.path(), Some("ok"), &WorktreeOptions::default()).is_err());
    }

    #[test]
    fn generates_valid_free_name() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let name = generate_name(tmp.path()).unwrap();
        assert!(!name.is_empty());
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn git_root_walks_up() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_root(&nested), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn git_root_accepts_linked_worktree_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_root(&nested), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn git_root_rejects_plain_file_named_git() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "hello\n").unwrap();
        assert_eq!(git_root(tmp.path()), None);
    }

    #[test]
    fn ensure_ignored_writes_info_exclude_once() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let exclude = tmp.path().join(".git/info/exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "target/\n").unwrap();
        ensure_ignored(tmp.path());
        ensure_ignored(tmp.path());
        let s = fs::read_to_string(&exclude).unwrap();
        assert_eq!(s.matches(WORKTREES_REL).count(), 1);
        let tracked = fs::read_to_string(tmp.path().join(".gitignore")).unwrap_or_default();
        assert!(
            !tracked.contains(WORKTREES_REL),
            "must not touch the tracked .gitignore"
        );
    }

    #[test]
    fn cleanup_detects_work_and_removes() {
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.co")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.co")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q", "-b", "main"]) {
            return; // git unavailable; skip
        }
        fs::write(root.join("README.md"), "hi").unwrap();
        assert!(git(&["add", "."]));
        assert!(git(&["commit", "-qm", "init"]));

        let wt = root.join(WORKTREES_REL).join("feat");
        assert!(git(&[
            "worktree",
            "add",
            "-b",
            "worktree-feat",
            &wt.to_string_lossy()
        ]));
        assert!(
            !has_leftover_work(&wt, Some("HEAD")),
            "fresh worktree should be clean"
        );
        // An unknown base must fail closed (count as work), never auto-remove.
        assert!(
            has_leftover_work(&wt, None),
            "unknown base should fail closed"
        );

        fs::write(wt.join("change.txt"), "x").unwrap();
        assert!(
            has_leftover_work(&wt, Some("HEAD")),
            "untracked change should count as work"
        );

        assert!(remove(root, &wt, "feat").is_ok());
        assert!(!wt.exists(), "worktree should be removed");
    }
}
