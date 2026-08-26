//! Installing and pruning managed packages.
//!
//! The lockfile is the single source of truth for which revision of a package
//! is active. Nothing else records it, so nothing else can disagree with it,
//! and a package resolves straight to `core/<name>/<sha>` without the tree
//! having to be searched.

use std::fs;
use std::path::{Path, PathBuf};

use crate::git::{self, GitError};
use crate::lock::{Lock, LockError};
use crate::lockfile::Lockfile;
use crate::paths;
use crate::spec::Spec;

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("package name {name:?} (from {src:?}) is not a usable directory name")]
    UnsafeName { name: String, src: String },
    #[error("revision {rev:?} would be read by git as an option")]
    UnsafeRevision { rev: String },
    #[error(
        "package source {src:?} contains HTTP credentials; use a Git credential helper instead"
    )]
    UnsafeSource { src: String },
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A package that is on disk and ready to load.
#[derive(Debug, Clone)]
pub struct Installed {
    pub rev: String,
    /// Absolute path of the revision directory this session should load.
    pub dir: PathBuf,
    /// True when this call installed or moved the package.
    pub changed: bool,
}

/// Where a clone keeps the refs that `git fetch` actually moves.
const REMOTE_PREFIX: &str = "refs/remotes/origin/";
/// What the default branch resolves against. See `resolve_revision`.
const REMOTE_HEAD: &str = "refs/remotes/origin/HEAD";

pub struct Manager {
    site: PathBuf,
}

impl Manager {
    pub fn new(site: impl Into<PathBuf>) -> Self {
        Self { site: site.into() }
    }

    /// Where a package's revision lives, without touching the network.
    ///
    /// Startup uses this: a package already recorded in the lockfile resolves
    /// to a directory with no git call at all, which is what keeps a steady
    /// state start offline.
    pub fn resolve(&self, lock: &Lockfile, name: &str) -> Option<PathBuf> {
        // The lockfile is a file on disk that a user edits and commits, so its
        // contents are input, not fact. Both halves become path components, and
        // an absolute or traversing value would point this outside the site
        // directory entirely.
        if !crate::spec::name_is_safe(name) {
            return None;
        }
        let entry = lock.get(name)?;
        if !revision_is_safe_component(&entry.rev) {
            return None;
        }
        let dir = paths::revision_dir(&self.site, name, &entry.rev);
        dir.is_dir().then_some(dir)
    }

    /// Deletes stale immutable revisions that no running process is reading.
    pub fn prune(&self, lock: &Lockfile) -> Vec<ManagerError> {
        let mut failures = Vec::new();
        for name in lock.install_order() {
            if !crate::spec::name_is_safe(name) {
                continue;
            }
            let Some(current) = lock.get(name).map(|entry| entry.rev.as_str()) else {
                continue;
            };
            if !revision_is_prunable(current) {
                continue;
            }
            let root = paths::package_root(&self.site, name);
            let entries = match fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    failures.push(ManagerError::Io { path: root, source });
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(source) => {
                        failures.push(ManagerError::Io {
                            path: root.clone(),
                            source,
                        });
                        continue;
                    }
                };
                let revision = entry.file_name().to_string_lossy().into_owned();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(source) => {
                        failures.push(ManagerError::Io {
                            path: entry.path(),
                            source,
                        });
                        continue;
                    }
                };
                if !file_type.is_dir() || revision == current || !revision_is_prunable(&revision) {
                    continue;
                }
                let guard = match Lock::acquire(&paths::revision_lock(&self.site, name, &revision))
                {
                    Ok(guard) => guard,
                    Err(LockError::Held { .. }) => {
                        tracing::debug!(package = name, %revision, "stale package revision is in use");
                        continue;
                    }
                    Err(error) => {
                        failures.push(error.into());
                        continue;
                    }
                };
                if let Err(source) = fs::remove_dir_all(entry.path()) {
                    failures.push(ManagerError::Io {
                        path: entry.path(),
                        source,
                    });
                }
                drop(guard);
            }
        }
        failures
    }

    /// Makes sure a package is on disk, cloning and checking out if needed.
    ///
    /// When the lockfile already records a revision, that revision is used and
    /// `version` is not consulted. That is what makes a committed lockfile
    /// reproduce a package set rather than re-resolving to something newer.
    pub async fn ensure_installed(
        &self,
        spec: &Spec,
        lock: &mut Lockfile,
    ) -> Result<Installed, ManagerError> {
        if git::http_source_has_userinfo(&spec.src) {
            return Err(ManagerError::UnsafeSource {
                src: git::redact(&spec.src),
            });
        }
        self.check_name(&spec.name, &spec.src)?;

        // The recorded source must match before an installed directory counts.
        // Pointing a name at a different repository has to fetch that
        // repository, not keep serving whatever was installed under the name.
        let source_matches = lock
            .get(&spec.name)
            .is_some_and(|entry| entry.src == spec.src);

        if source_matches && let Some(dir) = self.resolve(lock, &spec.name) {
            let rev = lock
                .get(&spec.name)
                .expect("resolved from this entry")
                .rev
                .clone();
            return Ok(Installed {
                rev,
                dir,
                changed: false,
            });
        }

        let _guard = Lock::acquire(&paths::package_lock(&self.site, &spec.name))?;
        let hooks = self.hooks_dir()?;
        let work = self.work_dir(&spec.name);

        // A cached working copy is only reusable if it is a copy of the source
        // being asked for. Reusing one by name alone would materialize the old
        // repository's code while recording the new source.
        if work.join(".git").is_dir() && !self.work_matches_source(&work, &spec.src).await {
            fs::remove_dir_all(&work).map_err(|source| ManagerError::Io {
                path: work.clone(),
                source,
            })?;
        }
        if !work.join(".git").is_dir() {
            self.clone_into(&hooks, &spec.src, &work).await?;
        }

        // A recorded revision wins over `version`, even when nothing is on disk
        // yet. That is the case a lockfile exists for: a fresh machine must get
        // the commit that was committed, not whatever `version` resolves to
        // today. Only a matching source counts, since a changed source makes
        // the old revision meaningless.
        let recorded = lock
            .get(&spec.name)
            .filter(|entry| entry.src == spec.src)
            .map(|entry| {
                if revision_is_safe_component(&entry.rev) {
                    Ok(entry.rev.clone())
                } else {
                    Err(ManagerError::UnsafeRevision {
                        rev: entry.rev.clone(),
                    })
                }
            })
            .transpose()?;

        let rev = match recorded {
            Some(rev) => rev,
            None => self.resolve_revision(&hooks, &work, spec).await?,
        };
        let dest = paths::revision_dir(&self.site, &spec.name, &rev);
        if !dest.is_dir() {
            self.materialize(&hooks, &work, &rev, &dest).await?;
        }

        lock.record(&spec.name, &spec.src, &rev);
        Ok(Installed {
            rev,
            dir: dest,
            changed: true,
        })
    }

    /// Rejects a name that would not stay inside the package root.
    ///
    /// Every path this type builds is `<site>/pack/core/<name>/...`, and an
    /// absolute or traversing name escapes that entirely, so this is checked
    /// before any directory is created or removed.
    fn check_name(&self, name: &str, src: &str) -> Result<(), ManagerError> {
        if crate::spec::name_is_safe(name) {
            Ok(())
        } else {
            Err(ManagerError::UnsafeName {
                name: name.to_owned(),
                src: git::redact(src),
            })
        }
    }

    /// Whether a cached working copy points at the source being asked for.
    ///
    /// A copy whose remote cannot be read is treated as not matching, so the
    /// safe outcome is a fresh clone rather than code from an unknown origin.
    async fn work_matches_source(&self, work: &Path, src: &str) -> bool {
        let args = vec![
            "remote".to_owned(),
            "get-url".to_owned(),
            "origin".to_owned(),
        ];
        match git::run(args, Some(work.to_path_buf())).await {
            Ok(out) => out.stdout.trim() == src.trim(),
            Err(_) => false,
        }
    }

    /// The bare working copy git operates on. Revisions are copied out of it,
    /// so no session ever reads a directory git is mutating.
    fn work_dir(&self, name: &str) -> PathBuf {
        paths::package_root(&self.site, name).join(".work")
    }

    fn hooks_dir(&self) -> Result<PathBuf, ManagerError> {
        let dir = paths::empty_hooks_dir(&self.site);
        fs::create_dir_all(&dir).map_err(|source| ManagerError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(dir)
    }

    async fn clone_into(&self, hooks: &Path, src: &str, work: &Path) -> Result<(), ManagerError> {
        if let Some(parent) = work.parent() {
            fs::create_dir_all(parent).map_err(|source| ManagerError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        // A blobless clone is much smaller, but an older server refuses the
        // filter outright, so fall back to a full clone rather than failing.
        match git::run(git::clone_args(hooks, src, work, true), None).await {
            Ok(_) => Ok(()),
            Err(_) => {
                let _ = fs::remove_dir_all(work);
                git::run(git::clone_args(hooks, src, work, false), None).await?;
                Ok(())
            }
        }
    }

    async fn resolve_revision(
        &self,
        hooks: &Path,
        work: &Path,
        spec: &Spec,
    ) -> Result<String, ManagerError> {
        // Tried in order, first one that resolves wins.
        //
        // A branch name has the same problem the default branch has: `git
        // fetch` moves `refs/remotes/origin/<branch>` and leaves the local
        // branch where it was, so `version = "main"` would pin the commit that
        // was current at clone time for ever. A tag or a commit is not a
        // remote-tracking ref, and falls through to the literal name.
        let candidates: Vec<String> = match spec.version.as_deref() {
            // A clone with no `origin/HEAD` is unusual but legal, and then the
            // local head is the only answer there is.
            None => vec![REMOTE_HEAD.to_owned(), "HEAD".to_owned()],
            Some(revision) => remote_branch_ref(revision)
                .into_iter()
                .chain(std::iter::once(revision.to_owned()))
                .collect(),
        };

        let mut last = None;
        for candidate in &candidates {
            if !git::revision_is_safe(candidate) {
                return Err(ManagerError::UnsafeRevision {
                    rev: candidate.clone(),
                });
            }
            match git::run(
                git::rev_parse_args(hooks, candidate),
                Some(work.to_path_buf()),
            )
            .await
            {
                Ok(out) => return Ok(out.stdout.trim().to_owned()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("at least one candidate is always tried").into())
    }

    /// Builds the revision directory. It is written under a temporary name and
    /// renamed, so a session never sees a half-populated revision.
    async fn materialize(
        &self,
        hooks: &Path,
        work: &Path,
        rev: &str,
        dest: &Path,
    ) -> Result<(), ManagerError> {
        git::run(git::checkout_args(hooks, rev), Some(work.to_path_buf())).await?;

        let staging = dest.with_extension("incoming");
        let _ = fs::remove_dir_all(&staging);
        copy_tree(work, &staging).map_err(|source| ManagerError::Io {
            path: staging.clone(),
            source,
        })?;
        fs::rename(&staging, dest).map_err(|source| ManagerError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        Ok(())
    }
}

fn remote_branch_ref(rev: &str) -> Option<String> {
    if let Some(name) = rev.strip_prefix("refs/heads/") {
        return Some(format!("{REMOTE_PREFIX}{name}"));
    }
    (!rev.starts_with("refs/") && rev != "HEAD").then(|| format!("{REMOTE_PREFIX}{rev}"))
}

/// A revision directory name. Revisions are recorded from `git rev-parse`, so a
/// real one is a hex object id; anything else came from a hand-edited lockfile.
fn revision_is_safe_component(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn revision_is_prunable(rev: &str) -> bool {
    revision_is_safe_component(rev)
}

/// Copies a checkout without its `.git` directory, so a revision holds only the
/// package's files and nothing that git would later mutate.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let root = from.canonicalize()?;
    copy_tree_with_root(from, to, &root)
}

fn copy_tree_with_root(from: &Path, to: &Path, root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        let meta = fs::symlink_metadata(&src)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&src)?;
            let Some(parent) = src.parent() else {
                continue;
            };
            let Ok(resolved) = parent.join(&target).canonicalize() else {
                continue;
            };
            if target.is_absolute() || !resolved.starts_with(root) {
                // Dropped, not followed: a link out of the checkout would make
                // the package read or shadow a file the user never installed.
                // Reported, because a package silently missing a file is far
                // harder to diagnose than one that says what it left behind.
                tracing::warn!(
                    link = %src.display(),
                    target = %target.display(),
                    "package symlink leaves the package; it was not copied"
                );
                continue;
            }
            copy_symlink(&target, &dst, resolved.is_dir())?;
            continue;
        }
        if meta.is_dir() {
            copy_tree_with_root(&src, &dst, root)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(target: &Path, dest: &Path, _is_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}

#[cfg(windows)]
fn copy_symlink(target: &Path, dest: &Path, is_dir: bool) -> std::io::Result<()> {
    if is_dir {
        std::os::windows::fs::symlink_dir(target, dest)
    } else {
        std::os::windows::fs::symlink_file(target, dest)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_target: &Path, _dest: &Path, _is_dir: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "copying symbolic links is unsupported",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TEST_REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_REV: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn site() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn unnamed_source_is_rejected_rather_than_guessed() {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let spec = Spec::new("").with_name("");
        let mut lock = Lockfile::default();

        let err = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect_err("a package with no name cannot be installed");
        assert!(matches!(err, ManagerError::UnsafeName { .. }), "got: {err}");
    }

    #[test]
    fn http_credentials_are_rejected_before_clone() {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let spec = Spec::new("https://user:secret@example.com/repo");
        let mut lock = Lockfile::default();

        let error = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect_err("credentials must not reach Git or the lockfile");

        assert!(matches!(error, ManagerError::UnsafeSource { .. }));
        assert!(
            !error.to_string().contains("secret"),
            "credential leaked: {error}"
        );
        assert!(lock.get(&spec.name).is_none());
    }

    /// Every path is built as `<site>/pack/core/<name>/...`, and `Path::join`
    /// replaces the base when the name is absolute, so an unchecked name puts
    /// `remove_dir_all` anywhere on the filesystem.
    #[test_case("/etc" ; "absolute")]
    #[test_case("../../etc" ; "traversal")]
    #[test_case(".." ; "parent")]
    #[test_case("a/b" ; "separator")]
    #[test_case(".hidden" ; "leading_dot_collides_with_work_dir")]
    #[test_case("" ; "empty")]
    fn unsafe_package_names_are_refused(name: &str) {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        let spec = Spec::new("https://x/demo").with_name(name);

        assert!(
            matches!(
                smol::block_on(mgr.ensure_installed(&spec, &mut lock)),
                Err(ManagerError::UnsafeName { .. })
            ),
            "{name:?} must not become a package directory"
        );
    }

    /// The name check has to run before anything is created or deleted, so a
    /// refused name leaves no trace outside the site directory.
    #[test]
    fn a_refused_name_creates_nothing() {
        let dir = site();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();

        let site_dir = dir.path().join("site");
        let mgr = Manager::new(&site_dir);
        let mut lock = Lockfile::default();
        let spec = Spec::new("https://x/demo").with_name("../outside");

        let _ = smol::block_on(mgr.ensure_installed(&spec, &mut lock));
        assert!(outside.is_dir(), "a refused name must not touch anything");
    }

    #[test]
    fn a_revision_that_looks_like_an_option_is_refused() {
        let dir = site();
        let origin = origin_repo(dir.path(), None);
        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let spec = Spec::new(origin.display().to_string())
            .with_name("demo")
            .with_version("--upload-pack=evil");

        let err = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect_err("a revision starting with a dash must not reach git");
        assert!(
            matches!(err, ManagerError::UnsafeRevision { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn an_invalid_recorded_revision_is_not_re_resolved() {
        let dir = site();
        let origin = origin_repo(dir.path(), None);
        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let src = origin.display().to_string();
        lock.record("demo", &src, "../invalid");
        let spec = Spec::new(src).with_name("demo");

        let err = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect_err("an invalid lock revision must not resolve a replacement");

        assert!(matches!(err, ManagerError::UnsafeRevision { .. }));
        assert_eq!(lock.get("demo").unwrap().rev, "../invalid");
    }

    /// A recorded revision that exists on disk resolves with no git call, which
    /// is what keeps a steady state start offline.
    #[test]
    fn resolve_finds_a_recorded_revision_without_touching_git() {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("demo", "https://x/demo", TEST_REV);

        assert!(mgr.resolve(&lock, "demo").is_none(), "not on disk yet");

        fs::create_dir_all(paths::revision_dir(dir.path(), "demo", TEST_REV)).unwrap();
        assert_eq!(
            mgr.resolve(&lock, "demo").unwrap(),
            paths::revision_dir(dir.path(), "demo", TEST_REV)
        );
    }

    #[test]
    fn an_already_installed_package_reports_no_change() {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("demo", "https://x/demo", TEST_REV);
        fs::create_dir_all(paths::revision_dir(dir.path(), "demo", TEST_REV)).unwrap();

        let spec = Spec::new("https://x/demo").with_name("demo");
        let installed = smol::block_on(mgr.ensure_installed(&spec, &mut lock)).unwrap();
        assert!(!installed.changed);
        assert_eq!(installed.rev, TEST_REV);
    }

    /// Builds a real repository to install from, so the whole path runs
    /// without the network.
    ///
    /// Failures panic rather than skipping. git is a hard requirement of this
    /// feature and the repository under test is itself a git checkout, so a
    /// missing git is a broken environment, not a reason to pass silently.
    fn origin_repo(dir: &Path, tag: Option<&str>) -> PathBuf {
        let repo = dir.join("origin");
        fs::create_dir_all(repo.join("plugin")).unwrap();
        fs::write(repo.join("plugin").join("init.lua"), "-- demo\n").unwrap();

        let run = |args: Vec<&str>| {
            smol::block_on(git::run(
                args.iter().map(|a| (*a).to_owned()).collect(),
                Some(repo.clone()),
            ))
            .unwrap_or_else(|e| panic!("git {args:?} failed while building the fixture: {e}"))
        };
        run(vec!["init", "--quiet"]);
        run(vec!["config", "user.email", "t@example.com"]);
        run(vec!["config", "user.name", "test"]);
        run(vec!["add", "."]);
        run(vec!["commit", "--quiet", "-m", "initial"]);
        if let Some(tag) = tag {
            run(vec!["tag", tag]);
        }
        repo
    }

    #[test]
    fn installs_a_package_from_a_local_repository() {
        let dir = site();
        let origin = origin_repo(dir.path(), None);

        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let spec = Spec::new(origin.display().to_string()).with_name("demo");

        let installed = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect("a local repository should install");

        assert!(installed.changed);
        assert!(
            installed.dir.join("plugin").join("init.lua").is_file(),
            "the package files should be in the revision directory"
        );
        assert!(
            !installed.dir.join(".git").exists(),
            "a revision must not carry a git directory"
        );
        assert_eq!(
            lock.get("demo").map(|e| e.rev.as_str()),
            Some(installed.rev.as_str()),
            "the installed revision is recorded"
        );
        assert!(
            installed.dir.ends_with(&installed.rev),
            "the directory is named for its revision"
        );
    }

    /// A second call must not re-clone or move the package, which is what
    /// makes a steady state start free.
    #[test]
    fn installing_twice_is_idempotent() {
        let dir = site();
        let origin = origin_repo(dir.path(), None);

        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let spec = Spec::new(origin.display().to_string()).with_name("demo");

        let first = smol::block_on(mgr.ensure_installed(&spec, &mut lock)).unwrap();
        let second = smol::block_on(mgr.ensure_installed(&spec, &mut lock)).unwrap();

        assert!(!second.changed, "the second call changes nothing");
        assert_eq!(first.dir, second.dir);
        assert_eq!(first.rev, second.rev);
    }

    /// A tag is not a remote-tracking ref, so it has to resolve literally
    /// rather than being lost to the branch lookup that runs first.
    #[test]
    fn an_explicit_tag_still_resolves() {
        let dir = site();
        let origin = origin_repo(dir.path(), Some("v1.0.0"));

        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let spec = Spec::new(origin.display().to_string())
            .with_name("demo")
            .with_version("v1.0.0");

        smol::block_on(mgr.ensure_installed(&spec, &mut lock)).expect("a tag should install");
    }

    /// An annotated tag is its own object. Recording that object id would put
    /// something in the lockfile that is not a commit, and `resolve` would then
    /// look for a revision directory no checkout ever produces.
    #[test]
    fn an_annotated_tag_records_the_commit_it_points_at() {
        let dir = site();
        let origin = origin_repo(dir.path(), None);
        let git = |args: Vec<&str>| {
            smol::block_on(git::run(
                args.iter().map(|a| (*a).to_owned()).collect(),
                Some(origin.clone()),
            ))
            .unwrap()
        };
        git(vec!["tag", "-a", "v2.0.0", "-m", "release"]);
        let commit = git(vec!["rev-parse", "HEAD"]).stdout.trim().to_owned();
        let tag_object = git(vec!["rev-parse", "v2.0.0"]).stdout.trim().to_owned();
        assert_ne!(tag_object, commit, "the fixture tag must be annotated");

        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let spec = Spec::new(origin.display().to_string())
            .with_name("demo")
            .with_version("v2.0.0");

        let installed = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect("an annotated tag should install");
        assert_eq!(installed.rev, commit);
    }

    /// The point of committing a lockfile: another machine, with nothing
    /// installed, gets the recorded commit rather than re-resolving `version`.
    #[test]
    fn a_committed_lockfile_reproduces_the_recorded_commit_on_a_fresh_machine() {
        let dir = site();
        let origin = origin_repo(dir.path(), Some("v1.0.0"));
        let src = origin.display().to_string();

        // Machine one installs and records a commit.
        let first_site = dir.path().join("site-one");
        let mgr = Manager::new(&first_site);
        let mut lock = Lockfile::default();
        let spec = Spec::new(src.clone()).with_name("demo");
        let first = smol::block_on(mgr.ensure_installed(&spec, &mut lock)).unwrap();

        // A later commit lands upstream, so `version` would now resolve higher.
        fs::write(origin.join("plugin").join("extra.lua"), "-- later\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "--quiet", "-m", "later"]] {
            smol::block_on(git::run(
                args.iter().map(|a| (*a).to_owned()).collect(),
                Some(origin.clone()),
            ))
            .unwrap();
        }

        // Machine two starts empty but carries the committed lockfile.
        let second_site = dir.path().join("site-two");
        let mgr2 = Manager::new(&second_site);
        let second = smol::block_on(mgr2.ensure_installed(&spec, &mut lock)).unwrap();

        assert_eq!(
            second.rev, first.rev,
            "a committed lockfile must reproduce its recorded commit"
        );
        assert!(
            !second.dir.join("plugin").join("extra.lua").exists(),
            "the later commit must not be installed"
        );
    }

    /// A changed source makes the old revision meaningless, so it must not be
    /// reused for a different repository.
    #[test]
    fn a_changed_source_ignores_the_recorded_revision() {
        let dir = site();
        let origin = origin_repo(dir.path(), None);
        let mgr = Manager::new(dir.path().join("site"));

        let mut lock = Lockfile::default();
        lock.record("demo", "https://elsewhere/other", OTHER_REV);

        let spec = Spec::new(origin.display().to_string()).with_name("demo");
        let installed = smol::block_on(mgr.ensure_installed(&spec, &mut lock))
            .expect("a new source should resolve afresh");
        assert_ne!(installed.rev, OTHER_REV);
        assert_eq!(lock.get("demo").unwrap().src, spec.src);
    }

    /// Repointing a name at a different repository must install that
    /// repository. Reusing the cached checkout would serve the old code while
    /// recording the new source.
    #[test]
    fn repointing_a_name_at_another_repository_installs_the_new_one() {
        let dir = site();
        let first = origin_repo(dir.path(), None);

        let site_dir = dir.path().join("site");
        let mgr = Manager::new(&site_dir);
        let mut lock = Lockfile::default();

        let spec = Spec::new(first.display().to_string()).with_name("demo");
        smol::block_on(mgr.ensure_installed(&spec, &mut lock)).unwrap();

        // A second, genuinely different repository under the same name.
        let second_root = dir.path().join("second");
        fs::create_dir_all(&second_root).unwrap();
        let second = origin_repo(&second_root, None);
        fs::write(second.join("plugin").join("only_here.lua"), "-- new\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "--quiet", "-m", "second"]] {
            smol::block_on(git::run(
                args.iter().map(|a| (*a).to_owned()).collect(),
                Some(second.clone()),
            ))
            .unwrap();
        }

        let respec = Spec::new(second.display().to_string()).with_name("demo");
        let installed = smol::block_on(mgr.ensure_installed(&respec, &mut lock))
            .expect("a new source should install");

        assert!(
            installed.dir.join("plugin").join("only_here.lua").is_file(),
            "the new repository's code must be installed"
        );
        assert_eq!(lock.get("demo").unwrap().src, respec.src);
    }

    /// The lockfile wins over `version`, which is what makes a committed
    /// lockfile reproduce a package set instead of resolving something newer.
    #[test]
    fn a_recorded_revision_is_used_instead_of_resolving_version() {
        let dir = site();
        let origin = origin_repo(dir.path(), Some("v1.2.0"));

        let mgr = Manager::new(dir.path().join("site"));
        let mut lock = Lockfile::default();
        let spec = Spec::new(origin.display().to_string()).with_name("demo");
        let first = smol::block_on(mgr.ensure_installed(&spec, &mut lock)).unwrap();

        let pinned = Spec::new(origin.display().to_string())
            .with_name("demo")
            .with_version("v1.2.0");
        let second = smol::block_on(mgr.ensure_installed(&pinned, &mut lock)).unwrap();

        assert_eq!(second.rev, first.rev, "the recorded revision must win");
        assert!(!second.changed);
    }

    /// The lockfile is committed and hand-edited, so its values are input. Both
    /// halves become path components and must not escape the site directory.
    #[test_case("/tmp/evil" ; "absolute_revision")]
    #[test_case("../../evil" ; "traversing_revision")]
    #[test_case("has/slash" ; "revision_with_separator")]
    #[test_case("abc123" ; "abbreviated_revision")]
    fn a_crafted_lockfile_revision_does_not_resolve(rev: &str) {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("demo", "https://x/demo", rev);

        // Make the escaped target exist, so only the guard can reject it.
        let escaped = paths::revision_dir(dir.path(), "demo", rev);
        let _ = fs::create_dir_all(&escaped);

        assert!(
            mgr.resolve(&lock, "demo").is_none(),
            "revision {rev:?} must not resolve to a directory outside the package"
        );
    }

    #[test]
    fn a_crafted_lockfile_package_name_does_not_resolve() {
        let dir = site();
        let mgr = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("../escape", "https://x/demo", TEST_REV);
        assert!(mgr.resolve(&lock, "../escape").is_none());
    }

    /// A repository can commit a symlink to any host file. `fs::copy` follows
    /// one, which would copy that file's contents into the installed package
    /// where the package's own Lua can read it.
    #[cfg(unix)]
    #[test]
    fn copy_tree_does_not_follow_symlinks_out_of_the_tree() {
        let dir = site();
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, "sensitive").unwrap();

        let from = dir.path().join("from");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("real.lua"), "-- ok").unwrap();
        std::os::unix::fs::symlink(&secret, from.join("leak.txt")).unwrap();

        let to = dir.path().join("to");
        copy_tree(&from, &to).unwrap();

        assert!(to.join("real.lua").is_file(), "real files still copy");
        assert!(
            !to.join("leak.txt").exists(),
            "a symlink must not be followed into the installed revision"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_preserves_relative_symlinks_inside_the_tree() {
        let dir = site();
        let from = dir.path().join("from");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("real.lua"), "-- ok").unwrap();
        std::os::unix::fs::symlink("real.lua", from.join("linked.lua")).unwrap();

        let to = dir.path().join("to");
        copy_tree(&from, &to).unwrap();

        assert_eq!(
            fs::read_link(to.join("linked.lua")).unwrap(),
            Path::new("real.lua")
        );
        assert_eq!(fs::read_to_string(to.join("linked.lua")).unwrap(), "-- ok");
    }

    #[test]
    fn copy_tree_omits_the_git_directory() {
        let dir = site();
        let from = dir.path().join("from");
        fs::create_dir_all(from.join(".git")).unwrap();
        fs::create_dir_all(from.join("plugin")).unwrap();
        fs::write(from.join(".git").join("HEAD"), "ref").unwrap();
        fs::write(from.join("plugin").join("init.lua"), "-- x").unwrap();

        let to = dir.path().join("to");
        copy_tree(&from, &to).unwrap();

        assert!(to.join("plugin").join("init.lua").is_file());
        assert!(!to.join(".git").exists(), "a revision must not carry .git");
    }

    #[test]
    fn prune_removes_only_unlocked_stale_revisions() {
        const CURRENT: &str = "1111111111111111111111111111111111111111";
        const STALE: &str = "2222222222222222222222222222222222222222";
        const HELD: &str = "3333333333333333333333333333333333333333";

        let dir = site();
        let manager = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("demo", "https://x/demo", CURRENT);
        for revision in [CURRENT, STALE, HELD] {
            fs::create_dir_all(paths::revision_dir(dir.path(), "demo", revision)).unwrap();
        }
        fs::create_dir_all(paths::package_root(dir.path(), "demo").join(".work")).unwrap();
        let _held = Lock::acquire_shared(&paths::revision_lock(dir.path(), "demo", HELD)).unwrap();

        assert!(manager.prune(&lock).is_empty());

        assert!(paths::revision_dir(dir.path(), "demo", CURRENT).is_dir());
        assert!(!paths::revision_dir(dir.path(), "demo", STALE).exists());
        assert!(paths::revision_dir(dir.path(), "demo", HELD).is_dir());
        assert!(
            paths::package_root(dir.path(), "demo")
                .join(".work")
                .is_dir()
        );
    }

    #[cfg(unix)]
    #[test]
    fn prune_ignores_symlinks_and_packages_without_a_lockfile_owner() {
        const CURRENT: &str = "1111111111111111111111111111111111111111";
        const STALE: &str = "2222222222222222222222222222222222222222";

        let dir = site();
        let manager = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("owned", "https://x/owned", CURRENT);
        fs::create_dir_all(paths::revision_dir(dir.path(), "owned", CURRENT)).unwrap();
        let target = dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, paths::revision_dir(dir.path(), "owned", STALE))
            .unwrap();
        let unknown = paths::revision_dir(dir.path(), "unknown", STALE);
        fs::create_dir_all(&unknown).unwrap();

        assert!(manager.prune(&lock).is_empty());

        assert!(target.is_dir());
        assert!(paths::revision_dir(dir.path(), "owned", STALE).is_symlink());
        assert!(unknown.is_dir());
    }

    #[test]
    fn prune_does_not_mutate_a_package_with_an_invalid_current_revision() {
        const RECOVERABLE: &str = "2222222222222222222222222222222222222222";

        let dir = site();
        let manager = Manager::new(dir.path());
        let mut lock = Lockfile::default();
        lock.record("demo", "https://x/demo", "../invalid");
        let recoverable = paths::revision_dir(dir.path(), "demo", RECOVERABLE);
        fs::create_dir_all(&recoverable).unwrap();

        assert!(manager.prune(&lock).is_empty());
        assert!(recoverable.is_dir());
    }
}
