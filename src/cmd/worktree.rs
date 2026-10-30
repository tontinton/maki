//! `--worktree`: run the session in its own git worktree, so parallel sessions
//! never trip over each other's edits.
//!
//! We switch into the worktree before anything reads the cwd, so config
//! discovery, session resume and the permissions root all see the worktree as
//! the project without knowing about it.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use maki_storage::StateDir;
use maki_storage::paths::project_dir;

const WORKTREES_SUBDIR: &str = "worktrees";
const BRANCH_PREFIX: &str = "worktree-";
const NAME_ATTEMPTS: usize = 64;
const ADJECTIVES: &[&str] = &[
    "bright", "quiet", "rapid", "calm", "swift", "bold", "fresh", "mellow",
];
const NOUNS: &[&str] = &[
    "fox", "river", "falcon", "meadow", "otter", "raven", "pine", "ember",
];

pub struct EnteredWorktree {
    root: PathBuf,
    dir: PathBuf,
    name: String,
    /// The commit an auto-named worktree started from. Only those are
    /// throwaway: if nothing happened on top of this commit, cleanup deletes
    /// the worktree. A name the user picked, or a reopened worktree, is never
    /// deleted for them.
    throwaway_base: Option<String>,
}

/// Creates the worktree, or reopens a named one from an earlier run, and moves
/// the process into it.
pub fn enter(name: Option<&str>, storage: &StateDir) -> Result<EnteredWorktree> {
    let cwd = env::current_dir().context("resolve current directory")?;
    let root = PathBuf::from(
        git(&cwd, &["rev-parse", "--show-toplevel"]).context("--worktree needs a git checkout")?,
    );
    let worktrees_dir = project_dir(storage, &root).join(WORKTREES_SUBDIR);
    let (name, named) = match name {
        Some(name) => {
            validate_name(name)?;
            (name.to_owned(), true)
        }
        None => (generate_name(&root, &worktrees_dir)?, false),
    };
    let dir = worktrees_dir.join(&name);

    let throwaway_base = if dir.is_dir() {
        None
    } else {
        let branch = format!("{BRANCH_PREFIX}{name}");
        // Reusing the branch means `worktree add` without `-b`, which lands on a
        // detached HEAD. Cleanup could then delete the old branch and its commits.
        if branch_exists(&root, &branch) {
            bail!("branch {branch:?} already exists, delete it or pick another name");
        }
        let base = git(&root, &["rev-parse", "--verify", "HEAD"])
            .context("resolve HEAD to branch the worktree from")?;
        git(
            &root,
            &["worktree", "add", "-b", &branch, path_str(&dir)?, &base],
        )?;
        (!named).then_some(base)
    };

    env::set_current_dir(&dir).with_context(|| format!("enter worktree {}", dir.display()))?;
    eprintln!("Entered worktree {} ({BRANCH_PREFIX}{name})", dir.display());
    Ok(EnteredWorktree {
        root,
        dir,
        name,
        throwaway_base,
    })
}

impl EnteredWorktree {
    pub fn clean_up(&self) {
        let dir = self.dir.display();
        if let Some(base) = &self.throwaway_base
            && !has_leftover_work(&self.dir, base)
        {
            match self.remove() {
                Ok(()) => eprintln!("Removed clean worktree {dir}"),
                Err(e) => eprintln!("could not remove worktree {dir}: {e}"),
            }
            return;
        }
        let root = self.root.display();
        let branch = format!("{BRANCH_PREFIX}{}", self.name);
        eprintln!(
            "Kept worktree {dir} (branch {branch}). Remove it with:\n  git -C {root} worktree remove {dir}\n  git -C {root} branch -d {branch}"
        );
    }

    fn remove(&self) -> Result<()> {
        // Run from the main checkout, git refuses to remove the worktree it runs in.
        git(
            &self.root,
            &["worktree", "remove", "--force", path_str(&self.dir)?],
        )?;
        // `-d` keeps the branch if it has commits nobody merged. A leftover
        // branch costs nothing, lost commits do.
        let _ = git(
            &self.root,
            &["branch", "-d", &format!("{BRANCH_PREFIX}{}", self.name)],
        );
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
    // A leading dot hides the dir, and git rejects `..` anywhere in a branch name.
    if name.is_empty() || !name.chars().all(allowed) || name.starts_with('.') || name.contains("..")
    {
        bail!(
            "worktree name may only use letters, digits, '-', '_' and '.', and cannot start with '.' or contain '..': got {name:?}"
        );
    }
    Ok(())
}

fn generate_name(root: &Path, worktrees_dir: &Path) -> Result<String> {
    (0..NAME_ATTEMPTS)
        .map(|_| {
            format!(
                "{}-{}",
                ADJECTIVES[fastrand::usize(..ADJECTIVES.len())],
                NOUNS[fastrand::usize(..NOUNS.len())]
            )
        })
        .find(|name| {
            !worktrees_dir.join(name).exists()
                && !branch_exists(root, &format!("{BRANCH_PREFIX}{name}"))
        })
        .ok_or_else(|| eyre!("could not find a free worktree name, pass --worktree-name"))
}

/// Anything git can't confirm counts as work. Deleting by mistake loses
/// someone's changes, keeping by mistake only leaves a dir behind.
fn has_leftover_work(dir: &Path, base: &str) -> bool {
    let clean = git(dir, &["status", "--porcelain"]).is_ok_and(|changes| changes.is_empty());
    let no_commits =
        || git(dir, &["rev-list", "--count", &format!("{base}..HEAD")]).is_ok_and(|n| n == "0");
    !(clean && no_commits())
}

fn branch_exists(root: &Path, branch: &str) -> bool {
    git(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre!("worktree path {} is not valid UTF-8", path.display()))
}

/// Returns trimmed stdout, or an error with git's stderr on a non-zero exit.
fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::*;

    const NAME: &str = "feat";

    #[test_case("fix-login_2.0", true ; "allowed_chars")]
    #[test_case("", false ; "empty")]
    #[test_case("a/b", false ; "slash")]
    #[test_case(".hidden", false ; "leading_dot")]
    #[test_case("a..b", false ; "double_dot")]
    fn validate_name_cases(name: &str, valid: bool) {
        assert_eq!(validate_name(name).is_ok(), valid);
    }

    fn commit(dir: &Path) {
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        )
        .unwrap();
    }

    #[test]
    fn throwaway_worktree_counts_files_and_commits_as_work() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        git(root, &["init", "-q"]).unwrap();
        commit(root);
        let base = git(root, &["rev-parse", "HEAD"]).unwrap();
        let worktrees = tempfile::tempdir().unwrap();
        let dir = worktrees.path().join(NAME);
        let branch = format!("{BRANCH_PREFIX}{NAME}");
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                path_str(&dir).unwrap(),
            ],
        )
        .unwrap();

        assert!(!has_leftover_work(&dir, &base));
        fs::write(dir.join("new.txt"), "x").unwrap();
        assert!(has_leftover_work(&dir, &base));
        git(&dir, &["add", "."]).unwrap();
        commit(&dir);
        assert!(has_leftover_work(&dir, &base));

        let worktree = EnteredWorktree {
            root: root.to_path_buf(),
            dir: dir.clone(),
            name: NAME.into(),
            throwaway_base: Some(base),
        };
        worktree.remove().unwrap();
        assert!(!dir.exists());
        assert!(
            branch_exists(root, &branch),
            "-d must keep unmerged commits"
        );
    }
}
