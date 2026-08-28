//! Discovery of external packages installed under the site directory.
//!
//! This is the manual half of the package model: directories a user cloned
//! themselves, laid out the way Neovim lays packages out. Packages that maki
//! installs are resolved from recorded state instead, and never appear here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::PluginError;
use crate::loader::is_bundled;
use crate::plugin_permissions::{
    Permission, Requested, load_requested_permissions, requested_permissions_from_text,
};

const REVIEW_REVISION_LEN: usize = 12;
const UPDATE_REVIEW_DECLINED: &str = "update review was declined";
const UPDATE_REVIEW_UNAVAILABLE: &str =
    "update review needs a terminal; pass force = true to skip it";
const ACTIVE_DELETE_REFUSAL: &str = "package is active and was not removed";
const OWNER_CONFLICT_FAILURE: &str = "package name conflicts with another owner";
const DECLARED_DELETE_REFUSAL: &str = "still declared; remove it from maki.pack.add first";
const PLUGIN_MANIFEST: &str = "plugin.toml";

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
    try_read_approvals_at(approvals_path().as_deref())
}

fn try_read_approvals_at(path: Option<&Path>) -> Option<maki_pack::approvals::Approvals> {
    path.map(read_approvals_file)
        .unwrap_or_else(|| Some(maki_pack::approvals::Approvals::default()))
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
    fn can_confirm(self) -> bool {
        self == Self::Tty && io::stdin().is_terminal()
    }

    fn confirm(self, prompt: &str) -> bool {
        if !self.can_confirm() {
            return false;
        }
        eprint!("{} [y/N] ", sanitize_message(prompt));
        let _ = io::stderr().flush();
        let mut input = String::new();
        io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
    }

    fn review(self, prompt: &str) -> ReviewDecision {
        if !self.can_confirm() {
            ReviewDecision::Unavailable
        } else if self.confirm(prompt) {
            ReviewDecision::Accepted
        } else {
            ReviewDecision::Declined
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewDecision {
    Accepted,
    Declined,
    Unavailable,
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
    specs: &[&crate::api::pack::Declared],
    site: &Path,
    lock: &maki_pack::lockfile::Lockfile,
    failures: &mut Vec<String>,
) -> Vec<DiscoveredPackage> {
    let manager = maki_pack::manager::Manager::new(site);
    let approvals = read_approvals();
    let mut found = Vec::new();
    for declared in specs.iter().copied() {
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
            if let Some(failure) = owner_conflict(name) {
                report.failures.push(failure);
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
                    let manual = discover(&site);
                    let runnable = runnable_declarations(specs, &manual, &mut report);
                    report.packages =
                        resolved_on_disk(&runnable, &site, &lock, &mut report.failures);
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
        let recorded = lock_path.is_some_and(|path| write_lockfile(&path, &lock));
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackReport {
    pub updated: Vec<(String, String)>,
    pub removed: Vec<String>,
    pub failures: Vec<String>,
}

impl PackReport {
    pub fn changed(&self) -> bool {
        !self.updated.is_empty() || !self.removed.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.updated.is_empty() {
            parts.push(format!("{} updated", self.updated.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!("{} removed", self.removed.len()));
        }
        if parts.is_empty() {
            "No package changes".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

pub fn installed_names() -> Option<Vec<String>> {
    read_lockfile(lockfile_path().as_deref())
        .map(|lock| lock.install_order().map(str::to_owned).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackCommand {
    Update {
        names: Vec<String>,
        options: crate::api::pack::UpdateOptions,
    },
    Delete {
        names: Vec<String>,
        all: bool,
    },
}

impl PackCommand {
    pub fn parse(name: &str, args: &str, bang: bool) -> Result<Self, String> {
        use crate::api::pack::{UpdateOptions, UpdateTarget};

        let mut names = Vec::new();
        let mut lockfile = false;
        let mut all = false;
        for word in args.split_whitespace() {
            match word {
                "++lockfile" => lockfile = true,
                "++all" => all = true,
                flag if flag.starts_with('+') || flag.starts_with('-') => {
                    return Err(format!("{name}: unknown option {flag:?}"));
                }
                other => names.push(other.to_owned()),
            }
        }

        match name {
            "/packupdate" => {
                if all {
                    return Err("/packupdate: ++all is not an option".to_owned());
                }
                if names.len() > 1 {
                    return Err("/packupdate: name at most one package".to_owned());
                }
                Ok(Self::Update {
                    names,
                    options: UpdateOptions {
                        force: bang,
                        target: if lockfile {
                            UpdateTarget::Lockfile
                        } else {
                            UpdateTarget::Version
                        },
                    },
                })
            }
            "/packdel" => {
                if bang {
                    return Err("/packdel does not accept !".to_owned());
                }
                if lockfile {
                    return Err("/packdel: ++lockfile applies only to /packupdate".to_owned());
                }
                if (all && !names.is_empty()) || (!all && names.len() != 1) {
                    return Err("/packdel: name a package, or pass ++all".to_owned());
                }
                Ok(Self::Delete { names, all })
            }
            other => Err(format!("{other}: not a package command")),
        }
    }
}

pub fn plan_command(
    command: &PackCommand,
    declared: &[crate::api::pack::Declared],
    installed: &[String],
) -> Result<Vec<crate::api::pack::PackOp>, String> {
    use crate::api::pack::PackOp;

    let declared_names: Vec<&str> = declared
        .iter()
        .map(|declared| declared.spec.name.as_str())
        .collect();
    match command {
        PackCommand::Update { names, options } => {
            if names.len() > 1 {
                return Err("/packupdate: name at most one package".to_owned());
            }
            let names = if names.is_empty() {
                declared_names
                    .iter()
                    .filter(|name| installed.iter().any(|installed| installed == **name))
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>()
            } else {
                let Some(name) = names.first() else {
                    return Err("/packupdate: name a package".to_owned());
                };
                if !declared_names.contains(&name.as_str()) {
                    return Err(format!("{name}: not a package declared with maki.pack.add"));
                }
                if !installed.contains(name) {
                    return Err(format!("{name}: not installed, so it cannot be updated"));
                }
                names.clone()
            };
            if names.is_empty() {
                return Err("no declared package is installed; nothing was updated".to_owned());
            }
            Ok(names
                .into_iter()
                .map(|name| PackOp::Update {
                    name,
                    options: *options,
                })
                .collect())
        }
        PackCommand::Delete { names, all } => {
            let names = if *all {
                if !names.is_empty() {
                    return Err("/packdel: name a package, or pass ++all".to_owned());
                }
                installed
                    .iter()
                    .filter(|name| !declared_names.contains(&name.as_str()))
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                let Some(name) = names.first() else {
                    return Err("/packdel: name a package, or pass ++all".to_owned());
                };
                if names.len() != 1 {
                    return Err("/packdel: name one package".to_owned());
                }
                if !installed.contains(name) {
                    return Err(format!("{name}: not an installed package"));
                }
                if declared_names.contains(&name.as_str()) {
                    return Err(format!("{name}: {DECLARED_DELETE_REFUSAL}"));
                }
                names.clone()
            };
            if names.is_empty() {
                return Err("no undeclared package matched; nothing was removed".to_owned());
            }
            Ok(names
                .into_iter()
                .map(|name| PackOp::Delete { name })
                .collect())
        }
    }
}

pub fn apply_pack_ops(
    ops: &[crate::api::pack::PackOp],
    declared: &[crate::api::pack::Declared],
    active: &BTreeSet<String>,
    interaction: Interaction,
) -> PackReport {
    let mut report = PackReport::default();
    if ops.is_empty() {
        return report;
    }
    let site = match site_dir() {
        Ok(site) => site,
        Err(error) => {
            report.failures.push(format!(
                "no data directory, so packages cannot be changed: {error}"
            ));
            return report;
        }
    };
    let lock_path = match lockfile_path() {
        Some(path) => path,
        None => {
            report
                .failures
                .push("no config directory, so packages cannot be changed".to_owned());
            return report;
        }
    };
    apply_pack_ops_at(
        ops,
        declared,
        active,
        &site,
        &lock_path,
        interaction.can_confirm(),
        |prompt| interaction.review(prompt),
    )
}

fn apply_pack_ops_at(
    ops: &[crate::api::pack::PackOp],
    declared: &[crate::api::pack::Declared],
    active: &BTreeSet<String>,
    site: &Path,
    lock_path: &Path,
    review_available: bool,
    review_updates: impl FnOnce(&str) -> ReviewDecision,
) -> PackReport {
    use crate::api::pack::{PackOp, UpdateTarget};

    let mut report = PackReport::default();
    let _guard = match maki_pack::lock::Lock::acquire(&maki_pack::paths::sidecar_lock(lock_path)) {
        Ok(guard) => guard,
        Err(error) => {
            report.failures.push(redact_error(&error));
            return report;
        }
    };
    let mut lock = match read_lockfile(Some(lock_path)) {
        Some(lock) => lock,
        None => {
            report
                .failures
                .push("the pack lockfile is unreadable, so nothing was changed".to_owned());
            return report;
        }
    };
    let manager = maki_pack::manager::Manager::new(site);
    let approval_path = site.join("pack-approvals.json");
    let manual_names = discover(site).known_names();
    let mut prepared = BTreeMap::new();
    let mut review = Vec::new();
    let mut approvals = None;

    for (index, op) in ops.iter().enumerate() {
        let PackOp::Update { name, options } = op else {
            continue;
        };
        if owner_conflict(name).is_some() || manual_names.contains(name) {
            report
                .failures
                .push(format!("{name}: {OWNER_CONFLICT_FAILURE}"));
            continue;
        }
        let Some(spec) = declared
            .iter()
            .find(|declared| declared.spec.name == *name)
            .map(|declared| &declared.spec)
        else {
            report.failures.push(format!(
                "{name}: not declared with maki.pack.add, so it cannot be updated"
            ));
            continue;
        };
        let approved = if options.force {
            None
        } else {
            if !review_available {
                report
                    .failures
                    .push(format!("{name}: {UPDATE_REVIEW_UNAVAILABLE}"));
                continue;
            }
            let store =
                approvals.get_or_insert_with(|| try_read_approvals_at(Some(&approval_path)));
            let Some(store) = store else {
                report.failures.push(format!(
                    "{name}: the pack approval store is unreadable, so the update cannot be reviewed"
                ));
                continue;
            };
            Some(
                store
                    .get(&maki_pack::approvals::ApprovalKey::new(name, &spec.src))
                    .unwrap_or(&[])
                    .to_vec(),
            )
        };
        let proposal = match smol::block_on(manager.prepare_update(
            spec,
            &lock,
            options.target == UpdateTarget::Lockfile,
        )) {
            Ok(proposal) => proposal,
            Err(error) => {
                report
                    .failures
                    .push(format!("{name}: {}", redact_error(&error)));
                continue;
            }
        };
        let requested = match proposed_permissions(name, &proposal) {
            Ok(requested) => requested,
            Err(error) => {
                report
                    .failures
                    .push(format!("{name}: {}", redact_error(&error)));
                continue;
            }
        };
        if proposal.changed && !options.force {
            let Some(approved) = approved.as_deref() else {
                report
                    .failures
                    .push(format!("{name}: the update review baseline is unavailable"));
                continue;
            };
            let permission_diff = update_permission_diff(&requested, approved);
            let previous = if matches!(
                proposal.old_manifest,
                maki_pack::manager::PreviousManifest::Unavailable
            ) {
                "\n  previous manifest unavailable"
            } else {
                ""
            };
            review.push(format!(
                "{name}: {} -> {}\n  permissions: {permission_diff}{previous}",
                short_revision(&proposal.old_rev),
                short_revision(&proposal.new_rev)
            ));
        }
        prepared.insert(index, proposal);
    }

    let review_decision = if review.is_empty() {
        ReviewDecision::Accepted
    } else {
        review_updates(&format!(
            "Apply these package updates?\n  {}",
            review.join("\n  ")
        ))
    };
    for (index, op) in ops.iter().enumerate() {
        match op {
            PackOp::Activate { .. } => {}
            PackOp::Update { name, options } => {
                let Some(proposal) = prepared.get(&index) else {
                    continue;
                };
                if !proposal.changed {
                    continue;
                }
                if !options.force && review_decision != ReviewDecision::Accepted {
                    report.failures.push(match review_decision {
                        ReviewDecision::Declined => format!("{name}: {UPDATE_REVIEW_DECLINED}"),
                        ReviewDecision::Unavailable => {
                            format!("{name}: {UPDATE_REVIEW_UNAVAILABLE}")
                        }
                        ReviewDecision::Accepted => unreachable!("accepted above"),
                    });
                    continue;
                }
                match smol::block_on(manager.apply_update(proposal, &mut lock)) {
                    Ok(installed) => report.updated.push((name.clone(), installed.rev)),
                    Err(error) => report
                        .failures
                        .push(format!("{name}: {}", redact_error(&error))),
                }
            }
            PackOp::Delete { name } => {
                if declared.iter().any(|declared| declared.spec.name == *name) {
                    report
                        .failures
                        .push(format!("{name}: {DECLARED_DELETE_REFUSAL}"));
                    continue;
                }
                if active.contains(name) {
                    report
                        .failures
                        .push(format!("{name}: {ACTIVE_DELETE_REFUSAL}"));
                    continue;
                }
                if lock.get(name).is_none() {
                    report
                        .failures
                        .push(format!("{name}: not an installed managed package"));
                    continue;
                }
                match manager.remove(name, &mut lock) {
                    Ok(()) => {
                        report.removed.push(name.clone());
                        if !revoke_approval(name, Some(&approval_path)) {
                            report.failures.push(format!(
                                "{name}: removed, but its approval could not be revoked"
                            ));
                        }
                    }
                    Err(error) => report
                        .failures
                        .push(format!("{name}: {}", redact_error(&error))),
                }
            }
        }
    }

    if report.changed() {
        if !write_lockfile(lock_path, &lock) {
            report
                .failures
                .push("packages changed but the lockfile could not be written".to_owned());
        } else {
            for error in manager.prune(&lock) {
                report.failures.push(redact_error(&error));
            }
        }
    }
    report
}

fn short_revision(revision: &str) -> &str {
    revision.get(..REVIEW_REVISION_LEN).unwrap_or(revision)
}

fn proposed_permissions(
    name: &str,
    update: &maki_pack::manager::PreparedUpdate,
) -> Result<crate::plugin_permissions::Requested, PluginError> {
    let new_path = PathBuf::from(format!("{name}@{}", update.new_rev)).join(PLUGIN_MANIFEST);
    requested_permissions_from_text(update.new_manifest.as_deref(), &new_path)
}

fn update_permission_diff(
    requested: &crate::plugin_permissions::Requested,
    approved: &[String],
) -> String {
    let mut additions = Vec::new();
    let mut removals = Vec::new();
    for permission in Permission::ALL {
        let approved = approved
            .iter()
            .any(|name| name == permission.manifest_key());
        match (approved, requested.is_requested(*permission)) {
            (false, true) => additions.push(format!("+{permission}")),
            (true, false) => removals.push(format!("-{permission}")),
            _ => {}
        }
    }
    additions.extend(removals);
    if additions.is_empty() {
        "unchanged".to_owned()
    } else {
        additions.join(", ")
    }
}

fn revoke_approval(name: &str, path: Option<&Path>) -> bool {
    let Some(mut approvals) = try_read_approvals_at(path) else {
        return false;
    };
    if !approvals.revoke(name) {
        return true;
    }
    write_approvals_at(path, &approvals)
}

/// The reason a managed package may not own a name, if something else does.
fn owner_conflict(name: &str) -> Option<String> {
    is_bundled(name)
        .then(|| format!("{name}: managed package name conflicts with a builtin plugin"))
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

fn write_lockfile(path: &Path, lock: &maki_pack::lockfile::Lockfile) -> bool {
    write_json(path, lock, "pack lockfile")
}

fn write_approvals(approvals: &maki_pack::approvals::Approvals) -> bool {
    write_approvals_at(approvals_path().as_deref(), approvals)
}

fn write_approvals_at(path: Option<&Path>, approvals: &maki_pack::approvals::Approvals) -> bool {
    path.is_some_and(|path| write_json(path, approvals, "pack approvals"))
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
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::error::PluginError;
    use crate::plugin_permissions::{Permission, Requested};
    use test_case::test_case;

    use super::{
        ACTIVE_DELETE_REFUSAL, DECLARED_DELETE_REFUSAL, DiscoveredPackage, MANAGED_GROUP,
        OWNER_CONFLICT_FAILURE, Origin, PLUGIN_MANIFEST, Problem, REVIEW_REVISION_LEN,
        ReviewDecision, UPDATE_REVIEW_DECLINED, UPDATE_REVIEW_UNAVAILABLE, apply_pack_ops_at,
        discover, granted, installed_from_declared_source, missing_permissions,
        read_approvals_file, read_lockfile, resolved_on_disk, sanitize_message, write_approvals_at,
        write_lockfile,
    };

    const TEST_REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_REV: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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
            &[&declared_pack("demo", src)],
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
            &[&declared_pack("demo", "https://elsewhere.example/demo")],
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

    fn declaration(name: &str) -> crate::api::pack::Declared {
        crate::api::pack::Declared {
            spec: maki_pack::Spec::new(format!("https://example.com/{name}")).with_name(name),
            load: crate::api::pack::LoadMode::Eager,
            confirm: true,
            data: None,
        }
    }

    struct UpdateFixture {
        _temp: tempfile::TempDir,
        site: PathBuf,
        lock_path: PathBuf,
        declared: Vec<crate::api::pack::Declared>,
        old_rev: String,
        new_rev: String,
    }

    fn run_git(repo: &Path, args: &[&str]) -> maki_pack::git::GitOutput {
        smol::block_on(maki_pack::git::run(
            args.iter().map(|arg| (*arg).to_owned()).collect(),
            repo.to_path_buf(),
        ))
        .unwrap()
    }

    fn update_fixture() -> UpdateFixture {
        update_fixture_with_manifests(None, None)
    }

    fn update_fixture_with_manifests(
        old_manifest: Option<&str>,
        new_manifest: Option<&str>,
    ) -> UpdateFixture {
        let temp = tempfile::TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        fs::create_dir_all(origin.join("plugin")).unwrap();
        fs::write(origin.join("plugin").join("init.lua"), "-- first\n").unwrap();
        if let Some(manifest) = old_manifest {
            fs::write(origin.join(PLUGIN_MANIFEST), manifest).unwrap();
        }
        run_git(&origin, &["init", "--quiet"]);
        run_git(&origin, &["config", "user.email", "test@example.com"]);
        run_git(&origin, &["config", "user.name", "test"]);
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "--quiet", "-m", "first"]);

        let site = temp.path().join("site");
        let lock_path = temp.path().join("pack-lock.json");
        let spec = maki_pack::Spec::new(origin.display().to_string()).with_name("demo");
        let mut lock = maki_pack::lockfile::Lockfile::default();
        let installed = smol::block_on(
            maki_pack::manager::Manager::new(&site).ensure_installed(&spec, &mut lock),
        )
        .unwrap();
        assert!(write_lockfile(&lock_path, &lock));

        fs::write(origin.join("plugin").join("later.lua"), "-- later\n").unwrap();
        match new_manifest {
            Some(manifest) => fs::write(origin.join(PLUGIN_MANIFEST), manifest).unwrap(),
            None if old_manifest.is_some() => {
                fs::remove_file(origin.join(PLUGIN_MANIFEST)).unwrap()
            }
            None => {}
        }
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "--quiet", "-m", "later"]);
        let new_rev = run_git(&origin, &["rev-parse", "HEAD"])
            .stdout
            .trim()
            .to_owned();
        let declared = vec![crate::api::pack::Declared {
            spec,
            load: crate::api::pack::LoadMode::Eager,
            confirm: true,
            data: None,
        }];

        UpdateFixture {
            _temp: temp,
            site,
            lock_path,
            declared,
            old_rev: installed.rev,
            new_rev,
        }
    }

    fn update_operation(force: bool) -> crate::api::pack::PackOp {
        crate::api::pack::PackOp::Update {
            name: "demo".to_owned(),
            options: crate::api::pack::UpdateOptions {
                force,
                target: crate::api::pack::UpdateTarget::Version,
            },
        }
    }

    fn approve(fixture: &UpdateFixture, names: &[&str]) {
        let path = fixture.site.join("pack-approvals.json");
        let mut approvals = maki_pack::approvals::Approvals::default();
        approvals.approve(
            &maki_pack::approvals::ApprovalKey::new("demo", &fixture.declared[0].spec.src),
            names.iter().map(|name| (*name).to_owned()).collect(),
        );
        assert!(write_approvals_at(Some(&path), &approvals));
    }

    #[test]
    fn accepted_update_review_applies_the_prepared_revision() {
        let fixture = update_fixture();
        let mut prompt = String::new();
        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |value| {
                prompt = value.to_owned();
                ReviewDecision::Accepted
            },
        );

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(
            report.updated,
            vec![("demo".to_owned(), fixture.new_rev.clone())]
        );
        assert!(prompt.contains(&fixture.old_rev[..REVIEW_REVISION_LEN]));
        assert!(prompt.contains(&fixture.new_rev[..REVIEW_REVISION_LEN]));
        assert!(prompt.contains("permissions: unchanged"));
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.new_rev
        );
    }

    #[test_case(
        &[],
        None,
        Some("[permissions]\nnet = true\n"),
        "+net";
        "added_permission"
    )]
    #[test_case(
        &["fs_write"],
        Some("[permissions]\nfs_write = true\n"),
        Some("[permissions]\nnet = true\n"),
        "+net, -fs_write";
        "added_and_removed_permissions"
    )]
    #[test_case(
        &["run"],
        Some("[permissions]\nrun = true\n"),
        None,
        "-run";
        "removed_permission"
    )]
    #[test_case(
        &["net", "run"],
        Some("[permissions]\nnet = true\n"),
        Some("[permissions]\nnet = true\nrun = true\n"),
        "unchanged";
        "new_request_was_already_approved"
    )]
    #[test_case(
        &["net"],
        Some("[permissions]\nnet = true\nrun = true\n"),
        Some("[permissions]\nnet = true\nrun = true\n"),
        "+run";
        "unchanged_manifest_still_needs_missing_approval"
    )]
    fn update_review_shows_permission_changes(
        approved: &[&str],
        old_manifest: Option<&str>,
        new_manifest: Option<&str>,
        expected: &str,
    ) {
        let fixture = update_fixture_with_manifests(old_manifest, new_manifest);
        approve(&fixture, approved);
        let mut prompt = String::new();

        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |value| {
                prompt = value.to_owned();
                ReviewDecision::Accepted
            },
        );

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains(&format!("permissions: {expected}")));
    }

    #[test]
    fn invalid_proposed_manifest_prevents_forced_apply() {
        let fixture = update_fixture_with_manifests(None, Some("permissions = ["));

        let report = apply_pack_ops_at(
            &[update_operation(true)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |_| panic!("an invalid manifest must not reach review"),
        );

        assert!(report.updated.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("is not valid toml"));
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.old_rev
        );
        assert!(!maki_pack::paths::revision_dir(&fixture.site, "demo", &fixture.new_rev).exists());
    }

    #[test]
    fn invalid_previous_manifest_is_display_only() {
        let fixture = update_fixture_with_manifests(
            Some("permissions = ["),
            Some("[permissions]\nnet = true\n"),
        );
        let mut prompt = String::new();

        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |value| {
                prompt = value.to_owned();
                ReviewDecision::Accepted
            },
        );

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains("permissions: +net"));
    }

    #[test]
    fn unavailable_previous_manifest_is_reported_in_review() {
        let fixture = update_fixture_with_manifests(
            Some("[permissions]\nrun = true\n"),
            Some("[permissions]\nnet = true\n"),
        );
        let mut lock = read_lockfile(Some(&fixture.lock_path)).unwrap();
        lock.record("demo", &fixture.declared[0].spec.src, TEST_REV);
        assert!(write_lockfile(&fixture.lock_path, &lock));
        let mut prompt = String::new();

        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |value| {
                prompt = value.to_owned();
                ReviewDecision::Accepted
            },
        );

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains("previous manifest unavailable"));
        assert!(prompt.contains("permissions: +net"));
    }

    #[test]
    fn declined_update_review_changes_no_managed_state() {
        let fixture = update_fixture();
        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |_| ReviewDecision::Declined,
        );

        assert!(report.updated.is_empty());
        assert_eq!(
            report.failures,
            vec![format!("demo: {UPDATE_REVIEW_DECLINED}")]
        );
        assert!(!maki_pack::paths::revision_dir(&fixture.site, "demo", &fixture.new_rev).exists());
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.old_rev
        );
    }

    #[test]
    fn forced_update_skips_review_and_applies_the_revision() {
        let fixture = update_fixture();
        let approvals = fixture.site.join("pack-approvals.json");
        let report = apply_pack_ops_at(
            &[update_operation(true)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |_| panic!("forced updates must not ask for review"),
        );

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.updated, vec![("demo".to_owned(), fixture.new_rev)]);
        assert!(
            !approvals.exists(),
            "force must not write permission approval"
        );
    }

    #[test]
    fn unavailable_update_review_stops_before_git_and_changes_nothing() {
        let fixture = update_fixture();
        let work = maki_pack::paths::package_root(&fixture.site, "demo").join(".work");
        let before = run_git(&work, &["rev-parse", "refs/remotes/origin/HEAD"])
            .stdout
            .trim()
            .to_owned();
        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            false,
            |_| panic!("an unavailable review must be rejected before preparation"),
        );

        assert!(report.updated.is_empty());
        assert_eq!(
            report.failures,
            vec![format!("demo: {UPDATE_REVIEW_UNAVAILABLE}")]
        );
        assert!(!maki_pack::paths::revision_dir(&fixture.site, "demo", &fixture.new_rev).exists());
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.old_rev
        );
        let after = run_git(&work, &["rev-parse", "refs/remotes/origin/HEAD"])
            .stdout
            .trim()
            .to_owned();
        assert_eq!(before, after, "the work clone must not fetch");
    }

    #[test]
    fn unreadable_approval_store_stops_before_git() {
        let fixture = update_fixture();
        let approval_path = fixture.site.join("pack-approvals.json");
        fs::create_dir_all(&fixture.site).unwrap();
        fs::write(&approval_path, "not json").unwrap();
        let work = maki_pack::paths::package_root(&fixture.site, "demo").join(".work");
        let before = run_git(&work, &["rev-parse", "refs/remotes/origin/HEAD"])
            .stdout
            .trim()
            .to_owned();

        let report = apply_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &fixture.site,
            &fixture.lock_path,
            true,
            |_| panic!("an unreadable approval store cannot reach review"),
        );

        assert!(report.updated.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("approval store is unreadable"));
        let after = run_git(&work, &["rev-parse", "refs/remotes/origin/HEAD"])
            .stdout
            .trim()
            .to_owned();
        assert_eq!(before, after, "the work clone must not fetch");
    }

    #[test]
    fn delete_batch_removes_inactive_packages_and_keeps_active_ones() {
        let temp = tempfile::TempDir::new().unwrap();
        let site = temp.path().join("site");
        let lock_path = temp.path().join("pack-lock.json");
        let mut lock = maki_pack::lockfile::Lockfile::default();
        for (name, revision) in [("inactive", TEST_REV), ("active", OTHER_REV)] {
            lock.record(name, format!("https://example.com/{name}"), revision);
            fs::create_dir_all(maki_pack::paths::revision_dir(&site, name, revision)).unwrap();
        }
        assert!(write_lockfile(&lock_path, &lock));
        let active = BTreeSet::from(["active".to_owned()]);
        let operations = [
            crate::api::pack::PackOp::Delete {
                name: "inactive".to_owned(),
            },
            crate::api::pack::PackOp::Delete {
                name: "active".to_owned(),
            },
        ];

        let report = apply_pack_ops_at(&operations, &[], &active, &site, &lock_path, true, |_| {
            panic!("deletion does not use update review")
        });

        assert_eq!(report.removed, vec!["inactive"]);
        assert_eq!(
            report.failures,
            vec![format!("active: {ACTIVE_DELETE_REFUSAL}")]
        );
        assert!(!maki_pack::paths::package_root(&site, "inactive").exists());
        assert!(maki_pack::paths::package_root(&site, "active").is_dir());
        let lock = read_lockfile(Some(&lock_path)).unwrap();
        assert!(lock.get("inactive").is_none());
        assert!(lock.get("active").is_some());
    }

    #[test]
    fn updates_refuse_bundled_and_manual_package_owners() {
        let temp = tempfile::TempDir::new().unwrap();
        let site = temp.path().join("site");
        let lock_path = temp.path().join("pack-lock.json");
        make_package(&site, "vendor", "start", "manual");
        assert!(write_lockfile(
            &lock_path,
            &maki_pack::lockfile::Lockfile::default()
        ));

        for name in ["bash", "manual"] {
            let report = apply_pack_ops_at(
                &[crate::api::pack::PackOp::Update {
                    name: name.to_owned(),
                    options: crate::api::pack::UpdateOptions::default(),
                }],
                &[declaration(name)],
                &Default::default(),
                &site,
                &lock_path,
                true,
                |_| panic!("owner conflicts do not use update review"),
            );

            assert_eq!(
                report.failures,
                vec![format!("{name}: {OWNER_CONFLICT_FAILURE}")]
            );
        }
    }

    #[test]
    fn deletion_refuses_a_declared_package_before_removing_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let site = temp.path().join("site");
        let lock_path = temp.path().join("pack-lock.json");
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/demo", TEST_REV);
        fs::create_dir_all(maki_pack::paths::revision_dir(&site, "demo", TEST_REV)).unwrap();
        assert!(write_lockfile(&lock_path, &lock));

        let report = apply_pack_ops_at(
            &[crate::api::pack::PackOp::Delete {
                name: "demo".to_owned(),
            }],
            &[declaration("demo")],
            &Default::default(),
            &site,
            &lock_path,
            true,
            |_| panic!("deletion does not use update review"),
        );

        assert_eq!(
            report.failures,
            vec![format!("demo: {DECLARED_DELETE_REFUSAL}")]
        );
        assert!(maki_pack::paths::package_root(&site, "demo").is_dir());
        assert!(
            read_lockfile(Some(&lock_path))
                .unwrap()
                .get("demo")
                .is_some()
        );
    }

    #[test]
    fn update_bang_sets_force_and_removed_flags_are_rejected() {
        let command = super::PackCommand::parse("/packupdate", "++lockfile demo", true).unwrap();
        assert_eq!(
            command,
            super::PackCommand::Update {
                names: vec!["demo".to_owned()],
                options: crate::api::pack::UpdateOptions {
                    force: true,
                    target: crate::api::pack::UpdateTarget::Lockfile,
                },
            }
        );
        assert!(super::PackCommand::parse("/packupdate", "++offline", false).is_err());
        assert!(super::PackCommand::parse("/packdel", "demo", true).is_err());
        assert!(super::PackCommand::parse("/packdel", "one two", false).is_err());
    }

    #[test]
    fn command_plan_keeps_update_and_delete_boundaries() {
        let declared = vec![declaration("alpha"), declaration("missing")];
        let installed = vec!["alpha".to_owned(), "orphan".to_owned()];
        let update = super::PackCommand::parse("/packupdate", "", false).unwrap();
        assert_eq!(
            super::plan_command(&update, &declared, &installed).unwrap(),
            vec![crate::api::pack::PackOp::Update {
                name: "alpha".to_owned(),
                options: crate::api::pack::UpdateOptions::default(),
            }]
        );

        let undeclared_update = super::PackCommand::parse("/packupdate", "orphan", false).unwrap();
        assert!(super::plan_command(&undeclared_update, &declared, &installed).is_err());
        let uninstalled_update =
            super::PackCommand::parse("/packupdate", "missing", false).unwrap();
        assert!(super::plan_command(&uninstalled_update, &declared, &installed).is_err());

        let declared_delete = super::PackCommand::parse("/packdel", "alpha", false).unwrap();
        assert!(super::plan_command(&declared_delete, &declared, &installed).is_err());
        let missing_delete = super::PackCommand::parse("/packdel", "absent", false).unwrap();
        assert!(super::plan_command(&missing_delete, &declared, &installed).is_err());
        let delete_all = super::PackCommand::parse("/packdel", "++all", false).unwrap();
        assert_eq!(
            super::plan_command(&delete_all, &declared, &installed).unwrap(),
            vec![crate::api::pack::PackOp::Delete {
                name: "orphan".to_owned(),
            }]
        );

        for invalid in [
            super::PackCommand::Delete {
                names: vec![],
                all: false,
            },
            super::PackCommand::Delete {
                names: vec!["orphan".to_owned(), "other".to_owned()],
                all: false,
            },
            super::PackCommand::Delete {
                names: vec!["orphan".to_owned()],
                all: true,
            },
            super::PackCommand::Update {
                names: vec!["alpha".to_owned(), "missing".to_owned()],
                options: crate::api::pack::UpdateOptions::default(),
            },
        ] {
            assert!(super::plan_command(&invalid, &declared, &installed).is_err());
        }
    }
}
