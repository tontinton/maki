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
use maki_storage::StateDir;
use maki_storage::paths::project_dir;

/// Worktree checkouts nest under this directory in the project's state dir.
const WORKTREES_SUBDIR: &str = "worktrees";

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
#[derive(Clone, Default)]
pub struct WorktreeOptions {
    pub base_ref: BaseRef,
}

/// What a worktree session entered. `base` is the commit the worktree branched
/// from, captured at creation and used by cleanup to prove the worktree holds
/// no work before auto-removing it. It is `None` when reusing an existing
/// worktree, whose history was not created by this session.
pub struct EnteredWorktree {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub name: String,
    /// Whether the name came from the user (`--worktree-name`) instead of
    /// being auto-generated. An unnamed, empty worktree is auto-removed.
    pub named: bool,
    pub base: Option<String>,
}

/// Create a worktree from the process's current directory, chdir into it, and
/// return what it entered. `name` may be `None` to auto-generate one.
///
/// `git worktree add` runs synchronously; the launch path has no runtime yet.
pub fn enter(
    name: Option<&str>,
    opts: &WorktreeOptions,
    storage: &StateDir,
) -> Result<EnteredWorktree> {
    let cwd = env::current_dir().context("worktree: resolve current directory")?;
    enter_at(&cwd, name, opts, storage)
}

/// Core of [`enter`], parameterized by the starting directory so tests can
/// drive it without mutating the process-global cwd.
fn enter_at(
    cwd: &Path,
    name: Option<&str>,
    opts: &WorktreeOptions,
    storage: &StateDir,
) -> Result<EnteredWorktree> {
    let root = git_root(cwd).ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "not inside a git repository (from {}); --worktree needs a git checkout",
            cwd.display()
        )
    })?;

    let worktrees_dir = project_dir(storage, &root).join(WORKTREES_SUBDIR);
    let named = matches!(name, Some(n) if !n.is_empty());
    let name = match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => generate_name(&root, &worktrees_dir)?,
    };
    enter_named(&root, &name, opts, &worktrees_dir, named)
}

fn enter_named(
    root: &Path,
    name: &str,
    opts: &WorktreeOptions,
    worktrees_dir: &Path,
    named: bool,
) -> Result<EnteredWorktree> {
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

    let dir = worktrees_dir.join(name);

    // Reuse an existing worktree of the same name instead of failing.
    if dir.is_dir() {
        env::set_current_dir(&dir).with_context(|| format!("enter worktree {}", dir.display()))?;
        return Ok(EnteredWorktree {
            root: root.to_path_buf(),
            dir,
            name: name.to_string(),
            named,
            base: None,
        });
    }

    let dir_str = dir.to_str().unwrap_or(name);
    let branch = format!("worktree-{name}");
    // Fail rather than reuse the branch: `git worktree add` without `-b` would
    // land on a detached HEAD, and cleanup could then force-delete the branch.
    if git_status(
        true,
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )? == 0
    {
        bail!("branch {branch:?} already exists; delete it or pick another name");
    }
    let start = match opts.base_ref {
        BaseRef::Head => "HEAD".to_string(),
        BaseRef::Fresh => fresh_base(root)?,
    };
    // Resolve the branch point to a commit so cleanup can prove the worktree
    // has not diverged from it before auto-removing the tree.
    let base_commit = resolve_commit(root, &start)?;
    let add_args = ["worktree", "add", "-b", &branch, dir_str, start.as_str()];
    git_status(false, root, &add_args)?;

    env::set_current_dir(&dir).with_context(|| format!("enter worktree {}", dir.display()))?;
    Ok(EnteredWorktree {
        root: root.to_path_buf(),
        dir,
        name: name.to_string(),
        named,
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
fn generate_name(root: &Path, worktrees_dir: &Path) -> Result<String> {
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
        if !worktrees_dir.join(&name).exists()
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
    // Best-effort: `-d` refuses if the branch has unmerged commits, keeping
    // them; a leftover branch is harmless anyway.
    let _ = git_status(true, root, &["branch", "-d", &format!("worktree-{name}")]);
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

    fn state(tmp: &Path) -> StateDir {
        StateDir::from_path(tmp.join("state"))
    }

    #[test]
    fn rejects_unsafe_names() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        for bad in ["a/b", "..", ".hidden", "a..b"] {
            assert!(
                enter_at(
                    tmp.path(),
                    Some(bad),
                    &WorktreeOptions::default(),
                    &state(tmp.path())
                )
                .is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn requires_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            enter_at(
                tmp.path(),
                Some("ok"),
                &WorktreeOptions::default(),
                &state(tmp.path())
            )
            .is_err()
        );
    }

    #[test]
    fn generates_valid_free_name() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let name = generate_name(tmp.path(), &tmp.path().join("wts")).unwrap();
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

        let wts = tempfile::tempdir().unwrap();
        let wt = wts.path().join("feat");
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
