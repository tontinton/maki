//! Discovery of external packages installed under the site directory.
//!
//! This is the manual half of the package model: directories a user cloned
//! themselves, laid out the way Neovim lays packages out. Packages that maki
//! installs are resolved from recorded state instead, and never appear here.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::PluginError;
use crate::loader::is_bundled;
use crate::plugin_permissions::{Requested, load_requested_permissions};

pub fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                ' '
            }
        })
        .collect()
}

/// The group name reserved for packages maki installs itself. Manual discovery
/// skips it, so one package can never be found twice, once from disk and once
/// from recorded state, with the two disagreeing about its revision.
///
/// Re-exported rather than repeated: `maki-pack` decides where it puts a
/// checkout, and a second copy here that drifted would stop discovery skipping
/// the directory it writes.
pub use maki_pack::paths::MANAGED_GROUP;

/// `<data>/site`, the root Neovim would call a package path.
pub fn site_dir() -> Result<PathBuf, std::io::Error> {
    maki_storage::paths::data_dir().map(|d| d.join("site"))
}

/// How a package reached the disk, which is what decides whose word grants its
/// permissions.
///
/// This is a distinction the code needs and did not have. A manifest is written
/// by whoever wrote the package. For a package the user placed by hand that is
/// effectively the user, so the manifest is their own statement of intent. For
/// a package maki fetched it is a stranger, and letting the manifest grant
/// itself permissions would make the request self-certifying: a later revision
/// could add `run = true` and start subprocesses without anyone agreeing to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The user put the files under `pack/<group>/`. Trusted like `init.lua`.
    Manual,
    /// Maki cloned it from this source. The source is part of the approval key,
    /// so re-pointing a name at another repository does not inherit its grants.
    Fetched { src: String },
}

#[derive(Debug, Clone)]
pub struct DiscoveredPackage {
    pub name: String,
    /// Canonical package root. Resolved once, here, so the manifest, the
    /// entrypoints, and every later `require` agree on one directory.
    pub dir: PathBuf,
    /// `start/` packages load at startup; `opt/` packages wait to be activated.
    pub eager: bool,
    /// What the package's manifest asks for. Never a grant on its own for a
    /// fetched package; see `Origin`.
    pub requested: Requested,
    pub origin: Origin,
    pub revision_guard: Option<Arc<maki_pack::lock::Lock>>,
}

/// `pack-lock.json`, beside the user's configuration so it can be committed.
///
/// `global_config_dirs` returns several candidates in the order `init.lua` is
/// searched for, so the first that exists is the one whose `maki.pack.add`
/// declared these packages, and a custom config directory keeps its lockfile
/// rather than leaving it behind in XDG. With none of them created yet there is
/// nothing to sit beside, and the last candidate is the one
/// `append_permission_rule` already treats as writable.
pub fn lockfile_path() -> Option<PathBuf> {
    let dirs = maki_config::global_config_dirs();
    dirs.iter()
        .find(|dir| dir.is_dir())
        .cloned()
        .or_else(|| dirs.into_iter().next_back())
        .map(|dir| dir.join("pack-lock.json"))
}

/// `pack-approvals.json`, in the state directory beside the checkouts.
///
/// Deliberately not beside the lockfile. A lockfile is meant to be committed,
/// so a package set reproduces on another machine. An approval is the opposite
/// kind of fact: one person's decision to trust one repository on one machine,
/// which must not travel with a repository into someone else's checkout.
pub fn approvals_path() -> Option<PathBuf> {
    site_dir().ok().map(|dir| dir.join("pack-approvals.json"))
}

/// Reads the approval store.
///
/// An unreadable store yields no approvals rather than a default-open one. The
/// failure mode is then a package that loads with nothing granted, which is
/// visible and recoverable, instead of one that loads with everything granted.
pub fn read_approvals() -> maki_pack::approvals::Approvals {
    try_read_approvals().unwrap_or_default()
}

fn read_approvals_file(path: &Path) -> Option<maki_pack::approvals::Approvals> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(maki_pack::approvals::Approvals::default());
        }
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                %error,
                "pack approval store could not be read; granting nothing"
            );
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(approvals) => Some(approvals),
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "pack approval store is unreadable; granting nothing"
            );
            None
        }
    }
}

/// The approval store, or `None` when one exists but cannot be trusted.
///
/// Granting is the caller's decision to make: approving on top of a store that
/// failed to parse would write back a file missing everyone else's entries.
fn try_read_approvals() -> Option<maki_pack::approvals::Approvals> {
    match approvals_path() {
        Some(path) => read_approvals_file(&path),
        None => Some(maki_pack::approvals::Approvals::default()),
    }
}

/// Effective permissions for a package about to load.
///
/// The whole point of `Origin`: a fetched package's own manifest is a request,
/// and a request is not a grant.
pub fn effective_permissions(
    pkg: &DiscoveredPackage,
) -> crate::plugin_permissions::PluginPermissions {
    granted(pkg, &read_approvals())
}

/// The rule itself, with the store passed in.
///
/// Separated from the disk read so it can be tested against a store built in
/// the test rather than against whatever the person running the tests happens
/// to have approved.
pub fn granted(
    pkg: &DiscoveredPackage,
    approvals: &maki_pack::approvals::Approvals,
) -> crate::plugin_permissions::PluginPermissions {
    match &pkg.origin {
        Origin::Manual => pkg.requested.clone().granted(),
        Origin::Fetched { src } => {
            let key = maki_pack::approvals::ApprovalKey::new(pkg.name.clone(), src);
            let approved = crate::plugin_permissions::PluginPermissions::from_approved(
                approvals
                    .get(&key)
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str),
            );
            pkg.requested.intersect(&approved)
        }
    }
}

/// Whether this entry point can ask the user a question.
///
/// An install runs downloaded code, so it is a trust decision. A run with no
/// terminal cannot take one, and must refuse rather than assume consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interaction {
    Tty,
    None,
}

impl Interaction {
    fn confirm(self, prompt: &str) -> bool {
        if self != Self::Tty || !io::stdin().is_terminal() {
            return false;
        }
        eprint!("{} [y/N] ", sanitize_message(prompt));
        let _ = io::stderr().flush();
        let mut input = String::new();
        io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
    }
}

/// What an install pass produced, and what it could not.
///
/// Failures are returned rather than only logged: installation runs before
/// logging is set up, so a message written there reaches nobody.
#[derive(Debug, Default)]
pub struct InstallReport {
    pub packages: Vec<DiscoveredPackage>,
    pub failures: Vec<String>,
}

/// Whether a declared package is already installed from the source it names.
///
/// Both halves matter. A recorded revision only describes the source it was
/// recorded for, so a name pointed at a different repository is neither
/// installed nor covered by the decision that trusted the old one.
fn installed_from_declared_source(
    declared: &crate::api::pack::Declared,
    lock: &maki_pack::lockfile::Lockfile,
    manager: &maki_pack::manager::Manager,
) -> bool {
    let spec = &declared.spec;
    lock.get(&spec.name)
        .is_some_and(|entry| entry.src == spec.src)
        && manager.resolve(lock, &spec.name).is_some()
}

/// A declared package that recorded state says is on disk, together with the
/// approval facts needed to decide whether it may load.
struct Resolved {
    package: DiscoveredPackage,
    key: maki_pack::approvals::ApprovalKey,
    /// Permissions the manifest asks for that the store has not approved.
    missing: Vec<String>,
}

/// Resolves one declared package from recorded state alone: no git, no lock on
/// the lockfile, no network.
///
/// `None` without a failure means the package is simply not installed from the
/// source it names. A package that is installed but unusable pushes a reason,
/// because otherwise it would vanish with nothing said about why.
///
/// Shared by the install path and the read-only fallback so the two cannot
/// drift: both must take the same revision guard and read the same manifest,
/// and a guard missed on one side would let a live revision be pruned.
fn resolve_declared(
    declared: &crate::api::pack::Declared,
    site: &Path,
    manager: &maki_pack::manager::Manager,
    lock: &maki_pack::lockfile::Lockfile,
    approvals: &maki_pack::approvals::Approvals,
    failures: &mut Vec<String>,
) -> Option<Resolved> {
    let spec = &declared.spec;
    let entry = lock.get(&spec.name).filter(|entry| entry.src == spec.src)?;
    let dir = manager.resolve(lock, &spec.name)?;
    let revision_guard = match maki_pack::lock::Lock::acquire_shared(
        &maki_pack::paths::revision_lock(site, &spec.name, &entry.rev),
    ) {
        Ok(guard) => Arc::new(guard),
        Err(error) => {
            failures.push(format!("{}: {}", spec.name, redact_error(&error)));
            return None;
        }
    };
    let requested = match load_requested_permissions(&dir) {
        Ok(requested) => requested,
        Err(problem) => {
            failures.push(format!("{}: {problem}", spec.name));
            return None;
        }
    };
    let key = maki_pack::approvals::ApprovalKey::new(spec.name.clone(), &spec.src);
    let missing = missing_permissions(&requested, approvals.get(&key).unwrap_or(&[]));
    Some(Resolved {
        package: DiscoveredPackage {
            name: spec.name.clone(),
            dir,
            eager: declared.load.is_eager(),
            requested,
            origin: Origin::Fetched {
                src: spec.src.clone(),
            },
            revision_guard: Some(revision_guard),
        },
        key,
        missing,
    })
}

/// The declared packages that are already installed, at the source and
/// revision the lockfile records, with everything they ask for approved.
///
/// This is what a session still has when it cannot install, so one held lock
/// does not take away the packages the user already had.
fn resolved_on_disk(
    specs: &[crate::api::pack::Declared],
    site: &Path,
    lock: &maki_pack::lockfile::Lockfile,
    failures: &mut Vec<String>,
) -> Vec<DiscoveredPackage> {
    let manager = maki_pack::manager::Manager::new(site);
    let approvals = read_approvals();
    let mut found = Vec::new();
    for declared in specs {
        let Some(resolved) = resolve_declared(declared, site, &manager, lock, &approvals, failures)
        else {
            continue;
        };
        if resolved.missing.is_empty() {
            found.push(resolved.package);
        } else {
            failures.push(approval_required(&resolved));
        }
    }
    found
}

fn approval_required(resolved: &Resolved) -> String {
    format!(
        "{}: permission approval is required for {}",
        resolved.package.name,
        resolved.missing.join(", ")
    )
}

fn missing_permissions(requested: &Requested, approved: &[String]) -> Vec<String> {
    requested
        .names()
        .into_iter()
        .filter(|name| !approved.iter().any(|approved| approved == name))
        .collect()
}

/// The declarations that may be installed: the rest name something that
/// already has an owner, and two owners for one name is not resolvable.
fn runnable_declarations<'a>(
    specs: &'a [crate::api::pack::Declared],
    manual: &Discovery,
    report: &mut InstallReport,
) -> Vec<&'a crate::api::pack::Declared> {
    // Includes names discovery refused, so a manual package maki could not
    // read still owns its name rather than being silently overwritten.
    let manual_names = manual.known_names();
    specs
        .iter()
        .filter(|declared| {
            let name = &declared.spec.name;
            if is_bundled(name) {
                report.failures.push(format!(
                    "{name}: managed package name conflicts with a builtin plugin"
                ));
                return false;
            }
            if manual_names.iter().any(|manual| manual == name) {
                let path = manual
                    .packages
                    .iter()
                    .find(|package| package.name == *name)
                    .map(|package| format!(" at {}", package.dir.display()))
                    .unwrap_or_default();
                report.failures.push(format!(
                    "{name}: managed package name conflicts with manual package{path}"
                ));
                return false;
            }
            true
        })
        .collect()
}

/// Decides which installed packages may load, prompting for any permission the
/// store has not already approved.
fn grant_installed(
    specs: &[&crate::api::pack::Declared],
    site: &Path,
    manager: &maki_pack::manager::Manager,
    lock: &maki_pack::lockfile::Lockfile,
    interaction: Interaction,
    report: &mut InstallReport,
) {
    let Some(mut approvals) = try_read_approvals() else {
        report
            .failures
            .push("the package approval store is unreadable, so no package was loaded".to_owned());
        return;
    };
    let mut approvals_changed = false;
    let mut newly_approved = BTreeSet::new();

    for declared in specs.iter().copied() {
        let Some(resolved) = resolve_declared(
            declared,
            site,
            manager,
            lock,
            &approvals,
            &mut report.failures,
        ) else {
            continue;
        };
        let name = &resolved.package.name;
        // An entry under this name that the key did not match belongs to some
        // other source, so it is dropped rather than left to be inherited.
        if approvals.get(&resolved.key).is_none() {
            approvals_changed |= approvals.revoke(name);
        }
        if !resolved.missing.is_empty() {
            if !interaction.confirm(&format!(
                "Allow package {name} these permissions: {}?",
                resolved.missing.join(", ")
            )) {
                report.failures.push(approval_required(&resolved));
                continue;
            }
            approvals.approve(&resolved.key, resolved.package.requested.names());
            approvals_changed = true;
            newly_approved.insert(name.clone());
        }
        report.packages.push(resolved.package);
    }

    if approvals_changed && !write_approvals(&approvals) {
        let packages = std::mem::take(&mut report.packages);
        for package in packages {
            if newly_approved.contains(&package.name) {
                report.failures.push(format!(
                    "{}: permission approval could not be saved",
                    package.name
                ));
            } else {
                report.packages.push(package);
            }
        }
    }
}

/// Installs the packages the global `init.lua` declared and reports where each
/// one landed.
///
/// Runs on the caller's thread, never the Lua thread: installing clones, and
/// loading blocks on a reply from the runtime, so doing either from inside a
/// Lua call would wait on a message that cannot be processed until it returns.
/// One unreachable repository is reported and skipped so a network problem
/// does not stop Maki from starting.
pub fn install_declared(
    specs: &[crate::api::pack::Declared],
    interaction: Interaction,
) -> InstallReport {
    let mut report = InstallReport::default();
    let site = match site_dir() {
        Ok(site) => site,
        Err(error) => {
            report.failures.push(format!(
                "no data directory, so no package was installed: {error}"
            ));
            return report;
        }
    };

    let lock_path = lockfile_path();

    // Held across the whole read, install, and write. Atomic rename gives
    // durability but not isolation: without this, two processes could each read
    // the same lockfile and the second write would discard the first's entries.
    let _guard = match lock_path.as_deref().map(maki_pack::paths::sidecar_lock) {
        Some(path) => match maki_pack::lock::Lock::acquire(&path) {
            Ok(guard) => Some(guard),
            Err(e) => {
                // The kernel releases the lock when its process exits,
                // including after a crash.
                tracing::error!(error = %e, "could not take the package lock");
                report.failures.push(redact_error(&e));
                // Installing needs the lock; reading what is already there
                // does not. Returning nothing would tell the session it has
                // no managed packages at all, because discovery skips the
                // managed group, so a second maki started during a clone
                // would come up with every package the user has missing.
                if let Some(lock) = read_lockfile(lock_path.as_deref()) {
                    report.packages = resolved_on_disk(specs, &site, &lock, &mut report.failures);
                }
                return report;
            }
        },
        None => None,
    };

    let mut lock = match read_lockfile(lock_path.as_deref()) {
        Some(lock) => lock,
        None => {
            report
                .failures
                .push("the pack lockfile is unreadable, so no package was installed".to_owned());
            return report;
        }
    };

    let manager = maki_pack::manager::Manager::new(&site);
    for error in manager.prune(&lock) {
        tracing::warn!(error = %error, "could not prune a stale package revision");
        report.failures.push(redact_error(&error));
    }
    if specs.is_empty() {
        return report;
    }
    let manual = discover(&site);
    let runnable = runnable_declarations(specs, &manual, &mut report);
    let mut changed = false;

    // Asked once for the whole batch, before anything is cloned. A package
    // already recorded at the revision it asks for is not a new trust
    // decision, so only a fresh source prompts. Credentials are redacted,
    // because the prompt is the one place a source is shown in full.
    let new_sources: Vec<String> = runnable
        .iter()
        .copied()
        .filter(|declared| {
            declared.confirm && !installed_from_declared_source(declared, &lock, &manager)
        })
        .map(|declared| {
            format!(
                "{} from {}",
                declared.spec.name,
                maki_pack::git::redact(&declared.spec.src)
            )
        })
        .collect();
    let confirmed = new_sources.is_empty()
        || interaction.confirm(&format!(
            "Install these packages?\n  {}",
            new_sources.join("\n  ")
        ));

    for declared in runnable.iter().copied() {
        let spec = &declared.spec;
        if declared.confirm
            && !installed_from_declared_source(declared, &lock, &manager)
            && !confirmed
        {
            report.failures.push(format!(
                "{}: installation requires confirmation; set confirm = false for a non-interactive install",
                spec.name
            ));
            continue;
        }
        let result = match smol::block_on(manager.ensure_installed(spec, &mut lock)) {
            Ok(result) => result,
            Err(e) => {
                let message = redact_error(&e);
                tracing::error!(package = %spec.name, error = %message, "failed to install package");
                report
                    .failures
                    .push(format!("{}: failed to install: {message}", spec.name));
                continue;
            }
        };
        changed |= result.changed;
    }

    // Written once, after the installs, and only when something moved.
    if changed {
        let recorded = lock_path.is_some_and(|path| write_json(&path, &lock, "pack lockfile"));
        if !recorded {
            // The packages are on disk but nothing records where. The next
            // start reads the old lockfile and would not find them, so they
            // must not be reported as installed either.
            report.packages.clear();
            report
                .failures
                .push("the pack lockfile could not be written, so no package was used".to_owned());
            return report;
        }
    }

    grant_installed(&runnable, &site, &manager, &lock, interaction, &mut report);
    report
}

fn redact_error(error: &impl std::fmt::Display) -> String {
    sanitize_message(&maki_pack::git::redact(&error.to_string()))
}

pub(crate) fn read_lockfile(path: Option<&Path>) -> Option<maki_pack::lockfile::Lockfile> {
    let Some(path) = path else {
        return Some(maki_pack::lockfile::Lockfile::default());
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(maki_pack::lockfile::Lockfile::default());
        }
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "pack lockfile could not be read");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(lock) => Some(lock),
        Err(error) => {
            tracing::error!(%error, "pack lockfile is unreadable; refusing to change packages");
            None
        }
    }
}

fn write_approvals(approvals: &maki_pack::approvals::Approvals) -> bool {
    approvals_path().is_some_and(|path| write_json(&path, approvals, "pack approvals"))
}

/// Replaces a shared JSON file in one step.
///
/// Returns whether the write landed rather than an error: every caller has to
/// keep going either way, and the reason belongs in the log, not in a message
/// each of them would have to format again.
fn write_json(path: &Path, value: &impl serde::Serialize, what: &str) -> bool {
    let written = serde_json::to_string_pretty(value)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            maki_storage::atomic_write(path, text.as_bytes()).map_err(|error| error.to_string())
        });
    match written {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "failed to write {}", what);
            false
        }
    }
}

/// Something the walk could not use, and the name it had for it.
///
/// One record and not two lists, because the name and the reason are two
/// halves of one fact and the config layer needs the name: a `plugins.<name>`
/// table naming a package that failed to load still names something real.
#[derive(Debug)]
pub struct Problem {
    /// `None` when the failure belongs to no single package, such as a group
    /// directory that could not be read at all.
    pub name: Option<String>,
    pub error: PluginError,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

/// What a discovery walk found, and what it had to refuse.
///
/// Problems are collected rather than returned as one error, because one
/// unusable package must not stop the others from loading.
#[derive(Debug, Default)]
pub struct Discovery {
    pub packages: Vec<DiscoveredPackage>,
    pub problems: Vec<Problem>,
}

impl Discovery {
    /// Records a package that was found but cannot load.
    fn refuse(&mut self, name: String, error: PluginError) {
        self.problems.push(Problem {
            name: Some(name),
            error,
        });
    }

    /// Records a failure that names no package.
    fn note(&mut self, error: PluginError) {
        self.problems.push(Problem { name: None, error });
    }

    /// Every package name the walk saw, loadable or not.
    ///
    /// This is what a `plugins.<name>` table is validated against. Validating
    /// against the loadable set instead would turn a package maki itself
    /// refused into a config error, and blame the user's config for it.
    pub fn known_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .packages
            .iter()
            .map(|package| package.name.clone())
            .chain(self.problems.iter().filter_map(|p| p.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

fn sorted_paths(dir: &Path, out: &mut Discovery) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(source) => {
            out.note(PluginError::Io {
                path: dir.to_path_buf(),
                source,
            });
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(source) => out.note(PluginError::Io {
                path: dir.to_path_buf(),
                source,
            }),
        }
    }
    paths.sort();
    paths
}

/// Discovers installed packages, or nothing when `--no-plugins` is set.
pub fn discover_installed(no_plugins: bool) -> Discovery {
    if no_plugins {
        return Discovery::default();
    }
    // An unresolvable data directory is not the same fact as an empty one.
    // Reported, because otherwise every installed package silently disappears.
    let site = match site_dir() {
        Ok(site) => site,
        Err(source) => {
            let mut out = Discovery::default();
            out.note(PluginError::PackageSiteUnavailable { source });
            return out;
        }
    };
    discover(&site)
}

/// Finds every manually installed package under `site`.
///
/// Returns them in a deterministic order, so two machines with the same
/// packages load them the same way. A missing site directory is not a problem;
/// it just means no packages are installed.
pub fn discover(site: &Path) -> Discovery {
    let mut out = Discovery::default();
    for group in sorted_paths(&site.join("pack"), &mut out) {
        if group.file_name().and_then(|n| n.to_str()) == Some(MANAGED_GROUP) {
            continue;
        }
        for (sub, eager) in [("start", true), ("opt", false)] {
            for dir in sorted_paths(&group.join(sub), &mut out) {
                let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.is_empty() || name.starts_with('.') {
                    continue;
                }
                let name = name.to_owned();
                let root = match dir.canonicalize() {
                    Ok(root) => root,
                    Err(source) => {
                        // A package that cannot even be resolved is reported
                        // rather than silently vanishing, since an unreadable
                        // directory looks identical to one that is not there.
                        out.refuse(name, PluginError::Io { path: dir, source });
                        continue;
                    }
                };
                if !root.is_dir() {
                    continue;
                }

                if is_bundled(&name) {
                    out.refuse(
                        name.clone(),
                        PluginError::PackageNameConflict { name, path: root },
                    );
                    continue;
                }
                let first = out
                    .packages
                    .iter()
                    .find(|p| p.name == name)
                    .map(|p| p.dir.clone());
                if let Some(first) = first {
                    out.refuse(
                        name.clone(),
                        PluginError::DuplicatePackage {
                            name,
                            first,
                            second: root,
                        },
                    );
                    continue;
                }

                let requested = match load_requested_permissions(&root) {
                    Ok(requested) => requested,
                    Err(problem) => {
                        out.refuse(name, problem);
                        continue;
                    }
                };
                out.packages.push(DiscoveredPackage {
                    name,
                    dir: root,
                    eager,
                    requested,
                    // Found by walking `pack/<group>/`, which is where a user
                    // puts files by hand. Maki's own checkouts are resolved
                    // through the lockfile, never discovered this way.
                    origin: Origin::Manual,
                    revision_guard: None,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::error::PluginError;
    use crate::plugin_permissions::{Permission, Requested};

    use super::{
        DiscoveredPackage, MANAGED_GROUP, Origin, Problem, discover, granted,
        installed_from_declared_source, missing_permissions, read_approvals_file, read_lockfile,
        resolved_on_disk, sanitize_message,
    };

    const TEST_REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn terminal_messages_keep_layout_but_remove_control_characters() {
        assert_eq!(
            sanitize_message("package\u{1b}[31m\rname\nnext"),
            "package [31m name\nnext"
        );
    }

    #[test]
    fn a_lockfile_read_error_is_not_treated_as_a_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();

        assert!(read_lockfile(Some(dir.path())).is_none());
    }

    #[test]
    fn a_missing_lockfile_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("missing.json");

        assert!(read_lockfile(Some(&path)).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_approval_store_is_not_treated_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("approvals.json");
        fs::write(&path, "not json").unwrap();

        assert!(read_approvals_file(&path).is_none());
    }

    #[test]
    fn a_missing_approval_store_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("approvals.json");

        assert!(read_approvals_file(&path).unwrap().is_empty());
    }

    fn make_package(site: &Path, group: &str, sub: &str, name: &str) -> PathBuf {
        let dir = site.join("pack").join(group).join(sub).join(name);
        fs::create_dir_all(dir.join("plugin")).unwrap();
        fs::write(dir.join("plugin").join("init.lua"), "").unwrap();
        dir
    }

    /// Builds a package whose manifest asks for everything, which is the
    /// interesting case: the question is never what it asked for, but whose
    /// word turns the request into a grant.
    fn greedy(name: &str, origin: Origin) -> DiscoveredPackage {
        let manifest = toml::from_str::<toml::Value>(
            "[permissions]\nfs_read = true\nfs_write = true\nnet = true\nrun = true\nenv = true\n",
        )
        .unwrap();
        DiscoveredPackage {
            name: name.to_owned(),
            dir: PathBuf::from("/nowhere"),
            eager: true,
            requested: Requested::from_manifest(&manifest),
            origin,
            revision_guard: None,
        }
    }

    /// A package the user placed by hand is trusted like `init.lua`: they put
    /// the files there, so its manifest is their own statement.
    #[test]
    fn a_manual_package_is_granted_what_its_manifest_asks_for() {
        let pkg = greedy("demo", Origin::Manual);
        let effective = granted(&pkg, &maki_pack::approvals::Approvals::default());

        for &perm in Permission::ALL {
            assert!(
                effective.is_allowed(perm),
                "{perm} should be granted to a manually installed package"
            );
        }
    }

    /// The negative half of the pair above, and the defect this exists to
    /// prevent: downloaded code must not be able to certify its own
    /// permissions by writing them into the manifest it ships.
    #[test]
    fn a_fetched_package_is_granted_nothing_without_an_approval() {
        let pkg = greedy(
            "demo",
            Origin::Fetched {
                src: "https://example.com/demo".to_owned(),
            },
        );
        let effective = granted(&pkg, &maki_pack::approvals::Approvals::default());

        for &perm in Permission::ALL {
            assert!(
                !effective.is_allowed(perm),
                "{perm} must not be granted to fetched code with no approval"
            );
        }
    }

    /// An approval grants only what it names, and only where the package also
    /// asked for it. Both halves have to agree.
    #[test]
    fn a_fetched_package_is_granted_the_intersection_of_request_and_approval() {
        let src = "https://example.com/demo";
        let pkg = greedy(
            "demo",
            Origin::Fetched {
                src: src.to_owned(),
            },
        );

        let mut approvals = maki_pack::approvals::Approvals::default();
        approvals.approve(
            &maki_pack::approvals::ApprovalKey::new("demo", src),
            vec!["run".to_owned()],
        );
        let effective = granted(&pkg, &approvals);

        assert!(effective.is_allowed(Permission::Run), "run was approved");
        assert!(
            !effective.is_allowed(Permission::Net),
            "net was requested but never approved"
        );
    }

    /// The reason an approval is keyed by source as well as name. Pointing a
    /// name at another repository is a new trust decision, so the grants
    /// recorded for the old one must not carry over.
    #[test]
    fn an_approval_does_not_survive_a_changed_source() {
        let mut approvals = maki_pack::approvals::Approvals::default();
        approvals.approve(
            &maki_pack::approvals::ApprovalKey::new("demo", "https://example.com/demo"),
            vec!["run".to_owned()],
        );

        let moved = greedy(
            "demo",
            Origin::Fetched {
                src: "https://elsewhere.example/demo".to_owned(),
            },
        );
        assert!(
            !granted(&moved, &approvals).is_allowed(Permission::Run),
            "an approval for one repository must not grant another"
        );
    }

    #[test]
    fn missing_permissions_reports_each_unapproved_request() {
        let manifest = toml::from_str::<toml::Value>(
            "[permissions]\nfs_read = true\nnet = true\nrun = true\n",
        )
        .unwrap();
        let requested = Requested::from_manifest(&manifest);

        assert_eq!(
            missing_permissions(&requested, &["net".to_owned()]),
            ["fs_read".to_owned(), "run".to_owned()]
        );
    }

    fn declared_pack(name: &str, src: &str) -> crate::api::pack::Declared {
        crate::api::pack::Declared {
            spec: maki_pack::Spec::new(src).with_name(name),
            load: crate::api::pack::LoadMode::Eager,
            confirm: true,
            data: None,
        }
    }

    /// Sets up a site where `name` is installed from `src` at one revision.
    fn installed_site(name: &str, src: &str) -> (tempfile::TempDir, maki_pack::lockfile::Lockfile) {
        let site = tempfile::TempDir::new().unwrap();
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record(name, src, TEST_REV);
        fs::create_dir_all(maki_pack::paths::revision_dir(site.path(), name, TEST_REV)).unwrap();
        (site, lock)
    }

    /// The rule that decides whether installing is a fresh trust decision.
    /// `.maki/init.lua` is project local, so a repository maki opens can point
    /// a name the user already trusts somewhere else: matching on the name
    /// alone skipped the prompt and cloned the new source on the strength of
    /// the old decision.
    #[test]
    fn a_package_is_only_installed_when_the_recorded_source_matches() {
        let src = "https://example.com/demo";
        let (site, lock) = installed_site("demo", src);
        let manager = maki_pack::manager::Manager::new(site.path());

        assert!(
            installed_from_declared_source(&declared_pack("demo", src), &lock, &manager),
            "same name, same source, and on disk"
        );
        assert!(
            !installed_from_declared_source(
                &declared_pack("demo", "https://elsewhere.example/demo"),
                &lock,
                &manager
            ),
            "a name pointed at another repository is a new trust decision"
        );
    }

    /// A held lock stops an install, but it does not make the packages already
    /// on disk disappear. Discovery skips the managed group, so reporting none
    /// left the session with every managed package missing.
    #[test]
    fn packages_already_on_disk_resolve_without_git_or_a_lock() {
        let src = "https://example.com/demo";
        let (site, lock) = installed_site("demo", src);

        let mut failures = Vec::new();
        let found = resolved_on_disk(
            &[declared_pack("demo", src)],
            site.path(),
            &lock,
            &mut failures,
        );
        assert_eq!(found.len(), 1, "the installed package is still usable");
        assert_eq!(found[0].name, "demo");
        assert_eq!(
            found[0].dir,
            maki_pack::paths::revision_dir(site.path(), "demo", TEST_REV)
        );

        let moved = resolved_on_disk(
            &[declared_pack("demo", "https://elsewhere.example/demo")],
            site.path(),
            &lock,
            &mut failures,
        );
        assert!(
            moved.is_empty(),
            "the recorded revision describes the old source only"
        );
    }

    /// A package maki itself refused is still a name the user can write in
    /// `plugins.<name>`. Validating a config against the loadable set alone
    /// turned one bad manifest into a startup failure that blamed the config.
    #[test]
    fn a_package_that_cannot_be_read_is_still_a_known_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = make_package(tmp.path(), "vendor", "start", "broken");
        fs::write(dir.join("plugin.toml"), "not = valid = toml").unwrap();

        let found = discover(tmp.path());
        assert!(found.packages.is_empty(), "the package must not load");
        assert_eq!(found.problems.len(), 1, "and the reason must be reported");
        assert_eq!(found.known_names(), vec!["broken".to_owned()]);
    }

    #[test]
    fn missing_site_dir_is_not_a_problem() {
        let tmp = tempfile::TempDir::new().unwrap();
        let found = discover(&tmp.path().join("absent"));
        assert!(found.packages.is_empty());
        assert!(found.problems.is_empty());
    }

    #[test]
    fn unreadable_package_root_is_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pack = tmp.path().join("pack");
        fs::write(&pack, "not a directory").unwrap();

        let found = discover(tmp.path());

        assert!(found.packages.is_empty());
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::Io { .. },
                ..
            }]
        ));
    }

    #[test]
    fn finds_start_and_opt_and_marks_eagerness() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "eager_one");
        make_package(tmp.path(), "vendor", "opt", "lazy_one");

        let found = discover(tmp.path());
        assert_eq!(found.packages.len(), 2);

        let eager = found
            .packages
            .iter()
            .find(|p| p.name == "eager_one")
            .unwrap();
        let lazy = found
            .packages
            .iter()
            .find(|p| p.name == "lazy_one")
            .unwrap();
        assert!(eager.eager, "start/ packages load at startup");
        assert!(!lazy.eager, "opt/ packages wait to be activated");
    }

    /// Managed packages are resolved from recorded state, so finding them here
    /// too would give one package two identities.
    #[test]
    fn managed_group_is_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), MANAGED_GROUP, "opt", "managed_one");
        make_package(tmp.path(), "vendor", "start", "manual_one");

        let names: Vec<String> = discover(tmp.path())
            .packages
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["manual_one"]);
    }

    #[test]
    fn bundled_name_collision_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "bash");

        let found = discover(tmp.path());
        assert!(found.packages.is_empty());
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::PackageNameConflict { .. },
                ..
            }]
        ));
    }

    /// `lib` is bundled without being enabled by default, so a name check
    /// against the default set alone would let it through.
    #[test]
    fn non_default_bundled_name_is_also_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "lib");

        let found = discover(tmp.path());
        assert!(found.packages.is_empty());
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::PackageNameConflict { .. },
                ..
            }]
        ));
    }

    #[test]
    fn duplicate_names_across_groups_are_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "alpha", "start", "twice");
        make_package(tmp.path(), "beta", "start", "twice");

        let found = discover(tmp.path());
        assert_eq!(found.packages.len(), 1, "the first one still loads");
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::DuplicatePackage { .. },
                ..
            }]
        ));
    }

    /// One unusable package must not take the others down with it.
    #[test]
    fn a_refused_package_does_not_stop_the_others() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "bash");
        make_package(tmp.path(), "vendor", "start", "fine_one");

        let found = discover(tmp.path());
        let names: Vec<&str> = found.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["fine_one"]);
        assert_eq!(found.problems.len(), 1);
    }

    #[test]
    fn manifest_permissions_are_read_and_deny_by_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = make_package(tmp.path(), "vendor", "start", "asks");
        fs::write(dir.join("plugin.toml"), "[permissions]\nnet = true\n").unwrap();

        let found = discover(tmp.path());
        let pkg = &found.packages[0];
        assert!(pkg.requested.is_requested(Permission::Net));
        assert!(!pkg.requested.is_requested(Permission::Run));
    }

    #[test]
    fn package_without_manifest_requests_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "silent");

        let found = discover(tmp.path());
        for perm in [
            Permission::FsRead,
            Permission::FsWrite,
            Permission::Net,
            Permission::Run,
            Permission::Env,
        ] {
            assert!(!found.packages[0].requested.is_requested(perm));
        }
    }
}
