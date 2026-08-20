//! Discovery of external packages installed under the site directory.
//!
//! This is the manual half of the package model: directories a user cloned
//! themselves, laid out the way Neovim lays packages out. Packages that maki
//! installs are resolved from recorded state instead, and never appear here.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::api::pack::{Dormant, PackState};
use crate::error::PluginError;
use crate::loader::is_bundled;
use crate::plugin_permissions::{Requested, load_requested_permissions};
use maki_pack::Spec;

use maki_pack::paths::MANAGED_GROUP;

/// `<data>/site`, the root Neovim would call a package path.
pub(crate) fn site_dir() -> Result<PathBuf, std::io::Error> {
    maki_storage::paths::data_dir().map(|d| d.join("site"))
}

/// How a package reached the disk, which decides how permissions are granted.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Manual,
    Fetched { src: String },
}

#[derive(Debug, Clone)]
pub struct DiscoveredPackage {
    pub name: String,
    /// Canonical package root. Resolved once, here, so the manifest, the
    /// entrypoints, and every later `require` agree on one directory.
    pub(crate) dir: PathBuf,
    /// `start/` packages load at startup; `opt/` packages wait to be activated.
    pub(crate) eager: bool,
    /// What the package's manifest asks for. Never a grant on its own for a
    /// fetched package; see `Origin`.
    pub(crate) requested: Requested,
    origin: Origin,
}

/// `pack-lock.json`, beside the user's configuration so it can be committed.
///
/// `global_config_dirs` returns several candidates; the last is the one
/// `append_permission_rule` already treats as writable, so the lockfile uses
/// the same directory rather than inventing its own rule.
pub(crate) fn lockfile_path() -> Option<PathBuf> {
    maki_config::global_config_dirs()
        .into_iter()
        .next_back()
        .map(|dir| dir.join("pack-lock.json"))
}

/// `pack-approvals.json`, in the state directory beside the checkouts.
///
/// Deliberately not beside the lockfile. A lockfile is meant to be committed,
/// so a package set reproduces on another machine. An approval is the opposite
/// kind of fact: one person's decision to trust one repository on one machine,
/// which must not travel with a repository into someone else's checkout.
fn approvals_path() -> Option<PathBuf> {
    site_dir().ok().map(|dir| dir.join("pack-approvals.json"))
}

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

#[derive(Debug, Default)]
pub struct InstallReport {
    pub packages: Vec<DiscoveredPackage>,
    pub failures: Vec<String>,
}

struct InstallCandidate {
    declared: crate::api::pack::Declared,
    spec: Spec,
    confirm: bool,
    source_changed: bool,
}

fn install_candidates(
    specs: &[crate::api::pack::Declared],
    lock: &maki_pack::lockfile::Lockfile,
    manager: &maki_pack::manager::Manager,
    lock_confirm: Option<bool>,
) -> Vec<InstallCandidate> {
    let mut candidates = Vec::new();
    for name in lock.install_order() {
        if manager.resolve(lock, name).is_some() {
            continue;
        }
        let Some(entry) = lock.get(name) else {
            continue;
        };
        let current = specs.iter().find(|declared| declared.spec.name == name);
        if current.is_some_and(|declared| declared.spec.src != entry.src) {
            continue;
        }
        let spec = Spec::new(entry.src.clone()).with_name(name.to_owned());
        let declared = current
            .cloned()
            .map(|mut declared| {
                declared.spec = spec.clone();
                declared
            })
            .unwrap_or_else(|| synthetic_declared(spec.clone()));
        candidates.push(InstallCandidate {
            declared,
            spec,
            confirm: lock_confirm.unwrap_or(true),
            source_changed: false,
        });
    }
    for declared in specs {
        let source_changed = lock
            .get(&declared.spec.name)
            .is_some_and(|entry| entry.src != declared.spec.src);
        let needs_install = source_changed || lock.get(&declared.spec.name).is_none();
        if needs_install {
            candidates.push(InstallCandidate {
                declared: declared.clone(),
                spec: declared.spec.clone(),
                confirm: declared.confirm,
                source_changed,
            });
        }
    }
    candidates
}

/// Reads the approval store.
///
/// An unreadable store yields no approvals rather than a default-open one. The
/// failure mode is then a package that loads with nothing granted, which is
/// visible and recoverable, instead of one that loads with everything granted.
pub fn read_approvals() -> maki_pack::approvals::Approvals {
    let Some(path) = approvals_path() else {
        return maki_pack::approvals::Approvals::default();
    };
    read_approvals_file(&path).unwrap_or_default()
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

fn read_approvals_for_write() -> Option<maki_pack::approvals::Approvals> {
    approvals_path()
        .map(|path| read_approvals_file(&path))
        .unwrap_or_else(|| Some(maki_pack::approvals::Approvals::default()))
}

/// Effective permissions for a package about to load.
///
/// The whole point of `Origin`: a fetched package's own manifest is a request,
/// and a request is not a grant.
pub(crate) fn effective_permissions(
    pkg: &DiscoveredPackage,
) -> crate::plugin_permissions::PluginPermissions {
    granted(pkg, &read_approvals())
}

/// The rule itself, with the store passed in.
///
/// Separated from the disk read so it can be tested against a store built in
/// the test rather than against whatever the person running the tests happens
/// to have approved.
fn granted(
    pkg: &DiscoveredPackage,
    approvals: &maki_pack::approvals::Approvals,
) -> crate::plugin_permissions::PluginPermissions {
    match &pkg.origin {
        Origin::Manual => pkg.requested.clone().granted_for_manual_install(),
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

/// The declared packages that are already installed at the source and revision
/// the lockfile records.
///
/// Reads only: no git, no lock, no network. This is what a session still has
/// when it cannot install, so one held lock does not take away the packages
/// the user already had.
fn resolved_on_disk(
    specs: &[crate::api::pack::Declared],
    site: &Path,
    lock: &maki_pack::lockfile::Lockfile,
    failures: &mut Vec<String>,
) -> Vec<DiscoveredPackage> {
    let manager = maki_pack::manager::Manager::new(site);
    let mut found = Vec::new();
    for declared in specs {
        let spec = &declared.spec;
        if !installed_from_declared_source(declared, lock, &manager) {
            continue;
        }
        let Some(dir) = manager.resolve(lock, &spec.name) else {
            continue;
        };
        let requested = match load_requested_permissions(&dir) {
            Ok(requested) => requested,
            Err(problem) => {
                // Named here, because the caller only reports why installing
                // stopped. Dropping this would make the package vanish with
                // the lock error as the only clue.
                failures.push(sanitize_message(&format!("{}: {problem}", spec.name)));
                continue;
            }
        };
        found.push(DiscoveredPackage {
            name: spec.name.clone(),
            requested,
            origin: Origin::Fetched {
                src: spec.src.clone(),
            },
            dir,
            eager: matches!(declared.load, crate::api::pack::LoadMode::Eager),
        });
    }
    found
}

/// Installs the packages `init.lua` declared, and reports where each one
/// landed.
///
/// Runs on the caller's thread, never the Lua thread: installing clones, and
/// loading blocks on a reply from the runtime, so doing either from inside a
/// Lua call would wait on a message that cannot be processed until it returns.
///
/// A package that fails to install is reported and skipped. One unreachable
/// repository must not stop maki from starting, or a network problem would
/// make the editor unusable.
/// What the approval pass reads while it decides what each package may have.
struct Grant<'a> {
    manager: &'a maki_pack::manager::Manager,
    manual: &'a [DiscoveredPackage],
    lock: &'a maki_pack::lockfile::Lockfile,
    interaction: Interaction,
    delivers_agent_events: bool,
    source_changes: &'a std::collections::BTreeSet<String>,
}

/// Narrows each installed package to what its request and a stored approval
/// agree on, asking about anything new.
///
/// Kept apart from installing, because a manifest arrives with the code it
/// describes. Deciding what a package may have has to be a separate step from
/// putting it on disk, or the download would be deciding its own access.
fn grant_installed(
    specs: &[crate::api::pack::Declared],
    ctx: &Grant<'_>,
    report: &mut InstallReport,
) {
    let Grant {
        manager,
        manual,
        lock,
        interaction,
        delivers_agent_events,
        source_changes,
    } = ctx;
    let interaction = *interaction;
    let delivers_agent_events = *delivers_agent_events;

    let Some(mut approvals) = read_approvals_for_write() else {
        report
            .failures
            .push("the package approval store is unreadable, so no package was loaded".to_owned());
        return;
    };
    let mut approvals_changed = false;
    let mut revoked_sources = std::collections::BTreeSet::new();
    let mut newly_approved = std::collections::BTreeSet::new();
    for name in source_changes.iter() {
        let declared = specs
            .iter()
            .find(|declared| declared.spec.name == *name)
            .expect("source changes come from declarations");
        if revoke_mismatched_approval(&mut approvals, name, &declared.spec.src) {
            approvals_changed = true;
            revoked_sources.insert(name.clone());
        }
    }
    for declared in specs {
        if let Some(error) = owner_conflict(&declared.spec.name) {
            if !report
                .failures
                .iter()
                .any(|failure| failure.starts_with(&format!("{}:", declared.spec.name)))
            {
                report.failures.push(error);
            }
            continue;
        }
        if let Some(manual) = manual.iter().find(|p| p.name == declared.spec.name) {
            if !report
                .failures
                .iter()
                .any(|failure| failure.starts_with(&format!("{}:", declared.spec.name)))
            {
                report.failures.push(format!(
                    "{}: managed package name conflicts with manual package at {}",
                    declared.spec.name,
                    manual.dir.display()
                ));
            }
            continue;
        }
        let Some(dir) = manager.resolve(lock, &declared.spec.name) else {
            continue;
        };
        if lock
            .get(&declared.spec.name)
            .is_none_or(|entry| entry.src != declared.spec.src)
        {
            continue;
        }
        // A manifest that exists but cannot be read is not the same fact as
        // an absent one, so it stops this package rather than approving an
        // empty request nobody wrote.
        let requested = match load_requested_permissions(&dir) {
            Ok(requested) => requested,
            Err(problem) => {
                report.failures.push(sanitize_message(&format!(
                    "{}: {problem}",
                    declared.spec.name
                )));
                continue;
            }
        };
        let names = requested.names();
        let key =
            maki_pack::approvals::ApprovalKey::new(declared.spec.name.clone(), &declared.spec.src);
        let approved = approvals.get(&key).unwrap_or(&[]);
        let missing: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|name| !approved.iter().any(|approved| approved == name))
            .collect();
        if !missing.is_empty() {
            let accepted = interaction.confirm(&format!(
                "Allow package {} these permissions: {}?",
                declared.spec.name,
                missing.join(", ")
            ));
            if !accepted {
                report.failures.push(format!(
                    "{}: permission approval is required for {}",
                    declared.spec.name,
                    missing.join(", ")
                ));
                continue;
            }
            approvals.approve(&key, names);
            approvals_changed = true;
            newly_approved.insert(declared.spec.name.clone());
        }
        report.packages.push(DiscoveredPackage {
            name: declared.spec.name.clone(),
            requested,
            origin: Origin::Fetched {
                src: declared.spec.src.clone(),
            },
            dir,
            eager: declared.loads_at_start(delivers_agent_events),
        });
    }
    if approvals_changed && !write_approvals(&approvals) {
        for package in std::mem::take(&mut report.packages) {
            if newly_approved.contains(&package.name) {
                report.failures.push(format!(
                    "{}: permission approval could not be saved",
                    package.name
                ));
            } else {
                report.packages.push(package);
            }
        }
        for name in revoked_sources {
            report.failures.push(format!(
                "{name}: the old source permission approval could not be revoked"
            ));
        }
    }
}

pub fn install_declared(
    host: &crate::loader::PluginHost,
    specs: &[crate::api::pack::Declared],
    lock_confirm: Option<bool>,
    interaction: Interaction,
    delivers_agent_events: bool,
) -> InstallReport {
    let mut report = InstallReport::default();
    if specs.is_empty() && lock_confirm.is_none() {
        return report;
    }
    let site = match site_dir() {
        Ok(site) => site,
        Err(error) => {
            report.failures.push(format!(
                "no data directory, so packages cannot be installed: {error}"
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
                // The error names the lock file and the process holding it,
                // and says to delete it if that process is gone. Reporting a
                // bare "another process" would be wrong after a crash.
                tracing::error!(error = %e, "could not take the package lock");
                report.failures.push(sanitize_message(&e.to_string()));
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
                .push("pack lockfile is unreadable; no package was installed".to_owned());
            return report;
        }
    };
    let original_lock = lock.clone();
    let manager = maki_pack::manager::Manager::new(&site);
    let manual = discover(&site).packages;
    let collides = |name: &str| manual.iter().find(|package| package.name == name);
    let mut source_changes = std::collections::BTreeSet::new();
    let candidates = install_candidates(specs, &lock, &manager, lock_confirm);

    let requiring_confirmation: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.confirm)
        .map(|candidate| {
            format!(
                "{} from {}",
                candidate.spec.name,
                maki_pack::git::redact(&candidate.spec.src)
            )
        })
        .collect();
    let confirmed = requiring_confirmation.is_empty()
        || interaction.confirm(&format!(
            "Install these packages?\n  {}",
            requiring_confirmation.join("\n  ")
        ));
    let mut accepted = Vec::new();
    for candidate in candidates {
        if let Some(error) = owner_conflict(&candidate.spec.name) {
            report.failures.push(error);
            continue;
        }
        if let Some(manual) = collides(&candidate.spec.name) {
            report.failures.push(format!(
                "{}: managed package name conflicts with manual package at {}",
                candidate.spec.name,
                manual.dir.display()
            ));
            continue;
        }
        if candidate.confirm && !confirmed {
            report.failures.push(format!(
                "{}: installation requires confirmation; set confirm = false for a non-interactive install",
                candidate.spec.name
            ));
            continue;
        }
        accepted.push(candidate);
    }

    for candidate in &accepted {
        let change = crate::api::pack::PackChange {
            declared: candidate.declared.clone(),
            active: false,
            kind: crate::api::pack::PackChangeKind::Install,
            path: maki_pack::paths::package_root(&site, &candidate.spec.name),
        };
        let _ = host.fire_pack_changed("PackChangedPre", change);
    }

    let mut installed_changes = Vec::new();
    for candidate in accepted {
        let InstallCandidate {
            declared,
            spec,
            source_changed,
            ..
        } = candidate;
        match smol::block_on(manager.ensure_installed(&spec, &mut lock)) {
            Ok(result) => {
                if result.changed {
                    if source_changed {
                        source_changes.insert(spec.name.clone());
                    }
                    installed_changes.push((declared, result.dir));
                }
            }
            Err(e) => {
                let message = redact_error(&e);
                tracing::error!(
                    package = %spec.name,
                    error = %message,
                    "failed to install package"
                );
                report.failures.push(format!("{}: {message}", spec.name));
            }
        }
    }

    if lock != original_lock {
        let Some(path) = lock_path.as_deref() else {
            report
                .failures
                .push("packages changed but no lockfile path is available".to_owned());
            return report;
        };
        if !write_lockfile(path, &lock) {
            report
                .failures
                .push("packages changed but the lockfile could not be written".to_owned());
            return report;
        }
    }
    for (declared, path) in installed_changes {
        let _ = host.fire_pack_changed(
            "PackChanged",
            crate::api::pack::PackChange {
                declared,
                active: false,
                kind: crate::api::pack::PackChangeKind::Install,
                path,
            },
        );
    }

    grant_installed(
        specs,
        &Grant {
            manager: &manager,
            manual: &manual,
            lock: &lock,
            interaction,
            delivers_agent_events,
            source_changes: &source_changes,
        },
        &mut report,
    );
    report
}

fn synthetic_declared(spec: Spec) -> crate::api::pack::Declared {
    crate::api::pack::Declared {
        spec,
        load: crate::api::pack::LoadMode::Dormant,
        confirm: true,
        data: None,
    }
}

fn owner_conflict(name: &str) -> Option<String> {
    is_bundled(name)
        .then(|| format!("{name}: managed package name conflicts with a builtin plugin"))
}

fn activation_refusal(name: &str, config: &maki_config::PluginsConfig) -> Option<String> {
    owner_conflict(name).or_else(|| {
        (!config.packages.iter().any(|package| package == name))
            .then(|| format!("{name}: package is disabled, so it was not activated"))
    })
}

/// Applies the package operations Lua recorded.
///
/// Called by the host once the requesting task has exited. Nothing here may run
/// inside a Lua call: unloading an owner blocks on a reply from the runtime
/// thread, so a Lua handler that waited for its own package to be removed would
/// wait on a message that cannot be processed until it returns.
///
/// Each operation is independent, so one failure is reported and the rest still
/// run.
/// An update that resolved to a revision, waiting on review and approval.
struct PreparedUpdate {
    declared: crate::api::pack::Declared,
    installed: maki_pack::manager::Installed,
    was_active: bool,
    force: bool,
    source_changed: bool,
    review: String,
}

/// What `commit_batch` needs to record a batch, or to undo it.
struct CommitBatch<'a> {
    host: &'a crate::loader::PluginHost,
    config: &'a maki_config::PluginsConfig,
    manager: &'a maki_pack::manager::Manager,
    lock: &'a maki_pack::lockfile::Lockfile,
    lock_path: Option<&'a Path>,
    original_lock: &'a maki_pack::lockfile::Lockfile,
    approvals_before: Option<&'a maki_pack::approvals::Approvals>,
    approvals_changed: bool,
    completed_changes: Vec<crate::api::pack::PackChange>,
}

/// Records what the batch did, or puts back what it can when it cannot.
///
/// The lockfile is the only durable record of where a checkout went. If it
/// cannot be written the packages have still moved on disk, so every change
/// has to be undone and no operation may be reported as done: the next start
/// reads the old lockfile and would not find the new revisions.
fn commit_batch(batch: CommitBatch<'_>, report: &mut PackReport) {
    let CommitBatch {
        host,
        config,
        manager,
        lock,
        lock_path,
        original_lock,
        approvals_before,
        approvals_changed,
        completed_changes,
    } = batch;

    // The lockfile is the durable record, so it decides whether anything moved.
    // A flag carried beside it could only ever disagree with it.
    let changed = lock != original_lock;
    let lock_written = !changed || lock_path.is_some_and(|path| write_lockfile(path, lock));
    if !lock_written {
        if approvals_changed
            && approvals_before
                .as_ref()
                .is_none_or(|approvals| !write_approvals(approvals))
        {
            report.failures.push(
                    "the previous package approvals could not be restored after the lockfile write failed"
                        .to_owned(),
                );
        }
        for change in &completed_changes {
            let name = &change.declared.spec.name;
            let Some(previous) = original_lock.get(name) else {
                continue;
            };
            let restored = match change.kind {
                crate::api::pack::PackChangeKind::Update => {
                    if change.active
                        && let Err(error) = host.unload(name)
                    {
                        report.failures.push(format!(
                            "{name}: the unrecorded revision could not be unloaded: {error}"
                        ));
                        continue;
                    }
                    manager.resolve(original_lock, name)
                }
                crate::api::pack::PackChangeKind::Delete => {
                    let spec = Spec::new(previous.src.clone()).with_name(name.clone());
                    let mut restore_lock = original_lock.clone();
                    match smol::block_on(manager.ensure_installed(&spec, &mut restore_lock)) {
                        Ok(installed) => Some(installed.dir),
                        Err(error) => {
                            let message = redact_error(&error);
                            report.failures.push(format!(
                                "{name}: the removed checkout could not be restored: {message}"
                            ));
                            None
                        }
                    }
                }
                crate::api::pack::PackChangeKind::Install => None,
            };
            if change.active
                && let Some(path) = restored
                && let Err(error) =
                    load_declared_one(host, &change.declared, &path, &previous.src, config)
            {
                report.failures.push(format!(
                    "{name}: the previous revision failed to reload: {error}"
                ));
            }
        }
        // The packages moved on disk but nothing recorded where. The next
        // catalog reads the old lockfile, so an unrecorded revision must not
        // remain active or be counted as done.
        for (name, _) in report.updated.drain(..) {
            report.failures.push(format!(
                "{name}: the new revision could not be recorded, so the old one \
                     comes back on the next start"
            ));
        }
        for name in report.removed.drain(..) {
            report.failures.push(format!(
                    "{name}: the removal could not be recorded, so the package comes back on the next start"
                ));
        }
        report
            .failures
            .push("packages changed but the lockfile could not be written".to_owned());
    } else {
        for name in &report.removed {
            if !revoke_approval(name) {
                report.failures.push(format!(
                    "{name}: removed, but its approval could not be revoked; \
                        reinstalling the same source restores the old grants"
                ));
            }
        }
        for change in completed_changes {
            let _ = host.fire_pack_changed("PackChanged", change);
        }
    }
}

/// The operations in a batch that may run, in request order.
///
/// A name given more than one operation in one batch is refused outright:
/// which of them won would depend on the order the passes below happen to run
/// in. A name that belongs to a builtin owner is refused for the same reason
/// deleting one is refused, because every plugin is an owner.
fn runnable_ops<'a>(
    ops: &'a [crate::api::pack::PackOp],
    manual: &Discovery,
    lock: &maki_pack::lockfile::Lockfile,
    report: &mut PackReport,
) -> Vec<&'a crate::api::pack::PackOp> {
    use crate::api::pack::PackOp;

    let name_of = |op: &'a PackOp| match op {
        PackOp::Update { name, .. } | PackOp::Delete { name, .. } | PackOp::Activate { name } => {
            name.as_str()
        }
    };

    // Every name the walk saw, not only the ones that loaded. A package whose
    // manifest became unreadable after it loaded is still the live owner of
    // its name, and reading the loadable set alone would let this batch tear
    // that owner down.
    let manual_names = manual.known_names();
    let mut seen = std::collections::BTreeSet::new();
    let mut conflicted = std::collections::BTreeSet::new();
    for op in ops {
        let name = name_of(op);
        if !seen.insert(name) {
            conflicted.insert(name);
        }
    }
    for name in &conflicted {
        report.failures.push(format!(
            "{name}: several package operations were requested; nothing was changed for it"
        ));
    }

    ops.iter()
        .filter(|op| {
            let name = name_of(op);
            if conflicted.contains(name) {
                return false;
            }
            if let Some(error) = owner_conflict(name) {
                report.failures.push(error);
                return false;
            }
            // A lock entry proves a managed package exists under this name, not
            // that the loaded owner is that one. With an orphan entry beside a
            // hand-installed package of the same name, deleting unloads the
            // manual package's registrations and removes the managed checkout,
            // and nothing brings the manual one back. Refused here so every
            // pass agrees, since not being declared is exactly what gets an
            // operation this far.
            if manual_names.iter().any(|manual| manual == name) && lock.get(name).is_some() {
                let where_it_is = manual
                    .packages
                    .iter()
                    .find(|package| package.name == name)
                    .map(|package| format!(" at {}", package.dir.display()))
                    .unwrap_or_default();
                report.failures.push(format!(
                    "{name}: managed package name conflicts with manual package{where_it_is}"
                ));
                return false;
            }
            true
        })
        .collect()
}

/// What the update pass reads while it resolves each requested revision.
struct Prepare<'a> {
    declared: &'a [crate::api::pack::Declared],
    manager: &'a maki_pack::manager::Manager,
    active: &'a std::collections::BTreeSet<String>,
}

/// Resolves every requested update to a revision, without moving anything.
///
/// Separate from applying them, because Neovim shows the whole set for review
/// before the first checkout moves, and a declined review must leave the
/// lockfile exactly as it was.
fn prepare_updates(
    runnable: &[&crate::api::pack::PackOp],
    ctx: &Prepare<'_>,
    lock: &mut maki_pack::lockfile::Lockfile,
    report: &mut PackReport,
) -> Vec<PreparedUpdate> {
    use crate::api::pack::{PackOp, UpdateTarget};

    let Prepare {
        declared,
        manager,
        active,
    } = ctx;
    let mut prepared = Vec::new();
    for op in runnable {
        let PackOp::Update { name, options } = op else {
            continue;
        };
        let Some(declaration) = declared
            .iter()
            .find(|declaration| declaration.spec.name == *name)
        else {
            report.failures.push(format!(
                "{name}: not declared with maki.pack.add, so it cannot be updated"
            ));
            continue;
        };
        let Some(previous) = lock.get(name).cloned() else {
            report
                .failures
                .push(format!("{name}: not installed, so it cannot be updated"));
            continue;
        };
        // A declaration that now names a different repository is a new trust
        // decision, and the prompt for it belongs to the install path.
        // Updating here would fetch and record the very source the user had
        // the chance to refuse, on the strength of the lock entry the old
        // source left behind.
        if previous.src != declaration.spec.src {
            report.failures.push(format!(
                "{name}: the declared source changed, so it cannot be updated; \
                 reload to install it from the new source"
            ));
            continue;
        }
        let existed = manager.resolve(lock, name).is_some();
        let mut preview = lock.clone();
        let refresh = match options.target {
            UpdateTarget::Version => {
                preview.remove(name);
                options.remote(maki_pack::manager::Refresh::Always)
            }
            UpdateTarget::Lockfile => options.remote(maki_pack::manager::Refresh::IfMissing),
        };
        let installed =
            match smol::block_on(manager.install(&declaration.spec, &mut preview, refresh)) {
                Ok(installed) => installed,
                Err(error) => {
                    let message = redact_error(&error);
                    tracing::error!(package = %name, error = %message, "failed to prepare update");
                    report.failures.push(format!("{name}: {message}"));
                    continue;
                }
            };
        if existed && previous.src == declaration.spec.src && previous.rev == installed.rev {
            continue;
        }
        let subjects = if previous.src == declaration.spec.src {
            match smol::block_on(manager.revision_log(name, &previous.rev, &installed.rev)) {
                Ok(subjects) => subjects,
                Err(error) => {
                    let message = redact_error(&error);
                    tracing::error!(package = %name, error = %message, "failed to review update");
                    report
                        .failures
                        .push(format!("{name}: could not review update: {message}"));
                    continue;
                }
            }
        } else {
            Vec::new()
        };
        let mut review = format!("{name}: {} -> {}", previous.rev, installed.rev);
        for subject in subjects {
            review.push_str("\n    ");
            review.push_str(&subject);
        }
        prepared.push(PreparedUpdate {
            declared: declaration.clone(),
            installed,
            was_active: active.contains(name),
            force: options.force,
            source_changed: previous.src != declaration.spec.src,
            review,
        });
    }
    prepared
}

/// What the delete pass reads while it decides what may be removed.
struct Remove<'a> {
    declared: &'a [crate::api::pack::Declared],
    site: &'a Path,
    manager: &'a maki_pack::manager::Manager,
    lock: &'a maki_pack::lockfile::Lockfile,
    active: &'a std::collections::BTreeSet<String>,
}

/// Works out which removals may go ahead, without removing anything.
///
/// Separate from applying them for the same reason updates are: every
/// pre-event fires before the first file moves, so the whole batch has to be
/// known first.
fn prepare_deletes(
    runnable: &[&crate::api::pack::PackOp],
    ctx: &Remove<'_>,
    report: &mut PackReport,
) -> std::collections::BTreeMap<String, crate::api::pack::PackChange> {
    use crate::api::pack::PackOp;

    let Remove {
        declared,
        site,
        manager,
        lock,
        active,
    } = ctx;
    let mut prepared_deletes = std::collections::BTreeMap::new();
    for op in runnable {
        let PackOp::Delete { name, force } = op else {
            continue;
        };
        // `/packdel` refuses a package that is still declared, because the
        // next start would install it again. `maki.pack.del` reaches this
        // point without that check, so it is made here as well and both
        // callers get the same rule.
        if declared.iter().any(|d| d.spec.name == *name) {
            report.failures.push(format!(
                "{name}: still declared with maki.pack.add, so it would be \
                 installed again; remove the declaration first"
            ));
            continue;
        }
        // Checked before anything is unloaded. `unload` takes an owner name,
        // and every plugin is an owner, so a crafted lock entry must not make
        // deleting a package clear a builtin with the same name.
        let Some(entry) = lock.get(name) else {
            tracing::error!(package = %name, "not a managed package; refusing to remove");
            report.failures.push(format!(
                "{name}: not a managed package, so it was not removed"
            ));
            continue;
        };
        let was_active = active.contains(name);
        if was_active && !force {
            report
                .failures
                .push(format!("{name}: package is active; use force to remove it"));
            continue;
        }
        let declaration = declared
            .iter()
            .find(|declaration| declaration.spec.name == *name)
            .cloned()
            .unwrap_or_else(|| {
                synthetic_declared(Spec::new(entry.src.clone()).with_name(name.clone()))
            });
        let path = manager
            .resolve(lock, name)
            .unwrap_or_else(|| maki_pack::paths::package_root(site, name));
        prepared_deletes.insert(
            name.clone(),
            crate::api::pack::PackChange {
                declared: declaration,
                active: was_active,
                kind: crate::api::pack::PackChangeKind::Delete,
                path,
            },
        );
    }
    prepared_deletes
}

pub fn apply_pack_ops(
    host: &crate::loader::PluginHost,
    ops: &[crate::api::pack::PackOp],
    declared: &[crate::api::pack::Declared],
    packages: &[DiscoveredPackage],
    config: &maki_config::PluginsConfig,
    interaction: Interaction,
) -> PackReport {
    use crate::api::pack::PackOp;

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
    let manager = maki_pack::manager::Manager::new(&site);

    // An activation loads code that is already installed at a revision the
    // lockfile already records, so it changes no shared state. Taking the
    // cross-process lock for it would block a real install for no reason.
    let changes_state = ops
        .iter()
        .any(|op| !matches!(op, crate::api::pack::PackOp::Activate { .. }));
    let lock_path = lockfile_path();
    let _guard = match lock_path
        .as_deref()
        .filter(|_| changes_state)
        .map(maki_pack::paths::sidecar_lock)
    {
        Some(path) => match maki_pack::lock::Lock::acquire(&path) {
            Ok(guard) => Some(guard),
            Err(e) => {
                // The error names the lock file and the process holding it,
                // and says to delete it if that process is gone. Reporting a
                // bare "another process" would be wrong after a crash.
                tracing::error!(error = %e, "could not take the package lock");
                report.failures.push(sanitize_message(&e.to_string()));
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
                .push("the pack lockfile is unreadable, so nothing was changed".to_owned());
            return report;
        }
    };
    let original_lock = lock.clone();
    let mut active = match host.active_packages() {
        Ok(active) => active,
        Err(error) => {
            report
                .failures
                .push(format!("could not read active packages: {error}"));
            return report;
        }
    };
    // Decided once. Four later passes walk this batch, and each repeating the
    // same two guards is how one of them ends up disagreeing with the others
    // about whether an operation may run.
    let manual = discover(&site);
    let runnable = runnable_ops(ops, &manual, &lock, &mut report);

    let prepared = prepare_updates(
        &runnable,
        &Prepare {
            declared,
            manager: &manager,
            active: &active,
        },
        &mut lock,
        &mut report,
    );

    let review_items: Vec<&str> = prepared
        .iter()
        .filter(|update| !update.force)
        .map(|update| update.review.as_str())
        .collect();
    let review_accepted = review_items.is_empty()
        || interaction.confirm(&format!(
            "Apply these package updates?\n  {}",
            review_items.join("\n  ")
        ));
    let mut approvals = read_approvals_for_write();
    let approvals_before = approvals.clone();
    let mut approvals_changed = false;
    let mut revoked_sources = std::collections::BTreeSet::new();
    let mut approved = Vec::new();
    for update in prepared {
        let name = &update.declared.spec.name;
        let Some(approvals) = approvals.as_mut() else {
            report.failures.push(format!(
                "{name}: the package approval store is unreadable, so it was not updated"
            ));
            continue;
        };
        if !update.force && !review_accepted {
            report.failures.push(format!(
                "{name}: update requires confirmation; use force for a non-interactive update"
            ));
            continue;
        }
        match approve_permissions(
            interaction,
            name,
            &update.declared.spec.src,
            &update.installed.dir,
            approvals,
        ) {
            Ok(changed) => {
                let revoked = update.source_changed
                    && !changed
                    && revoke_mismatched_approval(approvals, name, &update.declared.spec.src);
                if revoked {
                    revoked_sources.insert(name.clone());
                }
                approvals_changed |= changed || revoked;
                approved.push((update, changed));
            }
            Err(message) => report.failures.push(message),
        }
    }
    if approvals_changed
        && approvals
            .as_ref()
            .is_none_or(|approvals| !write_approvals(approvals))
    {
        approved.retain(|(update, newly_approved)| {
            if *newly_approved {
                report.failures.push(format!(
                    "{}: permission approval could not be saved",
                    update.declared.spec.name
                ));
                false
            } else {
                true
            }
        });
        for name in revoked_sources {
            report.failures.push(format!(
                "{name}: the old source permission approval could not be revoked"
            ));
        }
    }

    let mut approved: std::collections::BTreeMap<String, PreparedUpdate> = approved
        .into_iter()
        .map(|(update, _)| (update.declared.spec.name.clone(), update))
        .collect();
    let mut prepared_deletes = prepare_deletes(
        &runnable,
        &Remove {
            declared,
            site: &site,
            manager: &manager,
            lock: &lock,
            active: &active,
        },
        &mut report,
    );

    // Neovim sends every pre-event in request order before it applies any
    // change. A dependency observer can therefore see the complete batch
    // before the first checkout moves.
    for op in &runnable {
        let change = match op {
            PackOp::Delete { name, .. } => prepared_deletes.get(name).cloned(),
            PackOp::Update { name, .. } => {
                approved
                    .get(name)
                    .map(|update| crate::api::pack::PackChange {
                        declared: update.declared.clone(),
                        active: update.was_active,
                        kind: crate::api::pack::PackChangeKind::Update,
                        path: update.installed.dir.clone(),
                    })
            }
            PackOp::Activate { .. } => None,
        };
        if let Some(change) = change {
            let _ = host.fire_pack_changed("PackChangedPre", change);
        }
    }

    active = match host.active_packages() {
        Ok(active) => active,
        Err(error) => {
            report.failures.push(format!(
                "package state changed during PackChangedPre, but could not be read: {error}"
            ));
            return report;
        }
    };
    for (name, update) in &mut approved {
        update.was_active = active.contains(name);
    }
    for (name, change) in &mut prepared_deletes {
        change.active = active.contains(name);
    }
    for operation in &runnable {
        let PackOp::Delete { name, force: false } = operation else {
            continue;
        };
        if prepared_deletes
            .get(name)
            .is_some_and(|change| change.active)
        {
            prepared_deletes.remove(name);
            report.failures.push(format!(
                "{name}: package became active during PackChangedPre; use force to remove it"
            ));
        }
    }

    let mut completed_changes = Vec::new();
    for op in &runnable {
        match op {
            PackOp::Delete { name, .. } => {
                let Some(change) = prepared_deletes.remove(name) else {
                    continue;
                };
                if let Err(error) = host.unload(name) {
                    tracing::error!(package = %name, %error, "failed to unload package");
                    report.failures.push(format!(
                        "{name}: owner cleanup failed, so it was not removed: {error}"
                    ));
                    continue;
                }
                if change.active {
                    active.remove(name);
                }
                match manager.remove(name, &mut lock) {
                    Ok(()) => {
                        report.removed.push(name.clone());
                        completed_changes.push(change);
                    }
                    Err(e) => {
                        let msg = redact_error(&e);
                        tracing::error!(package = %name, error = %msg, "failed to remove");
                        report.failures.push(format!("{name}: {msg}"));
                        if change.active {
                            match load_declared_one(
                                host,
                                &change.declared,
                                &change.path,
                                &change.declared.spec.src,
                                config,
                            ) {
                                Ok(()) => {
                                    active.insert(name.clone());
                                }
                                Err(error) => report.failures.push(format!(
                                    "{name}: the old package also failed to reload: {error}"
                                )),
                            }
                        }
                    }
                }
            }
            PackOp::Update { name, .. } => {
                let Some(update) = approved.remove(name) else {
                    continue;
                };
                if update.was_active
                    && let Err(error) = host.unload(name)
                {
                    tracing::error!(package = %name, %error, "failed to unload package");
                    report.failures.push(format!(
                        "{name}: owner cleanup failed, so it was not updated: {error}"
                    ));
                    continue;
                }
                if update.was_active {
                    active.remove(name);
                }
                lock.record(name, &update.declared.spec.src, &update.installed.rev);
                report
                    .updated
                    .push((name.clone(), update.installed.rev.clone()));
                if update.was_active {
                    let loaded = load_declared_one(
                        host,
                        &update.declared,
                        &update.installed.dir,
                        &update.declared.spec.src,
                        config,
                    );
                    match loaded {
                        Ok(()) => {
                            active.insert(name.clone());
                        }
                        Err(message) => report
                            .failures
                            .push(format!("{name}: updated but failed to load: {message}")),
                    }
                }
                completed_changes.push(crate::api::pack::PackChange {
                    declared: update.declared,
                    active: update.was_active,
                    kind: crate::api::pack::PackChangeKind::Update,
                    path: update.installed.dir,
                });
            }
            PackOp::Activate { name } => {
                if let Some(error) = activation_refusal(name, config) {
                    report.failures.push(error);
                    continue;
                }
                // Already loaded is success, not an error. Loading again would
                // run the package's top level a second time under an owner that
                // is already registered.
                if active.contains(name) {
                    continue;
                }
                // The caller's set is tried first. It already holds every
                // package this startup discovered or installed, so the common
                // case reads no managed state at all.
                let found = packages
                    .iter()
                    .find(|package| package.name == *name)
                    .map(|package| (package.dir.clone(), package.origin.clone()))
                    .or_else(|| resolve_for_activation(&manager, &lock, &site, name));
                match found {
                    Some((dir, origin)) => match load_one(host, name, &dir, origin, config) {
                        Ok(()) => {
                            report.activated.push(name.clone());
                            active.insert(name.clone());
                        }
                        Err(msg) => report.failures.push(format!("{name}: {msg}")),
                    },
                    None => {
                        tracing::error!(package = %name, "not installed; cannot activate");
                        report
                            .failures
                            .push(format!("{name}: not installed, so it cannot be activated"));
                    }
                }
            }
        }
    }

    commit_batch(
        CommitBatch {
            host,
            config,
            manager: &manager,
            lock: &lock,
            lock_path: lock_path.as_deref(),
            original_lock: &original_lock,
            approvals_before: approvals_before.as_ref(),
            approvals_changed,
            completed_changes,
        },
        &mut report,
    );
    report
}

fn approve_permissions(
    interaction: Interaction,
    name: &str,
    src: &str,
    dir: &Path,
    approvals: &mut maki_pack::approvals::Approvals,
) -> Result<bool, String> {
    let requested = load_requested_permissions(dir)
        .map_err(|e| redact_error(&e))?
        .names();
    let key = maki_pack::approvals::ApprovalKey::new(name, src);
    let approved = approvals.get(&key).unwrap_or(&[]);
    let missing: Vec<&str> = requested
        .iter()
        .map(String::as_str)
        .filter(|permission| !approved.iter().any(|item| item == permission))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    if !interaction.confirm(&format!(
        "Allow package {name} these permissions: {}?",
        missing.join(", ")
    )) {
        return Err(format!(
            "{name}: permission approval is required for {}",
            missing.join(", ")
        ));
    }
    approvals.approve(&key, requested);
    Ok(true)
}

fn revoke_mismatched_approval(
    approvals: &mut maki_pack::approvals::Approvals,
    name: &str,
    src: &str,
) -> bool {
    let key = maki_pack::approvals::ApprovalKey::new(name, src);
    if approvals.get(&key).is_some() {
        return false;
    }
    approvals.revoke(name)
}

/// Every package name the lockfile records, whether or not `init.lua` still
/// declares it.
///
/// Removal is a property of what is on disk, not of what is declared. A user
/// who deletes the `maki.pack.add` line and then wants the files gone is the
/// ordinary case, and asking the declarations would answer "unknown package".
pub fn installed_names() -> Option<Vec<String>> {
    let lock = read_lockfile(lockfile_path().as_deref())?;
    Some(lock.install_order().map(str::to_owned).collect())
}

/// Where one installed package lives, if the lockfile records it.
pub fn installed_package(name: &str) -> Option<DiscoveredPackage> {
    let site = site_dir().ok()?;
    let lock = read_lockfile(lockfile_path().as_deref())?;
    let manager = maki_pack::manager::Manager::new(&site);
    let dir = manager.resolve(&lock, name)?;
    let src = lock.get(name)?.src.clone();
    Some(DiscoveredPackage {
        name: name.to_owned(),
        requested: load_requested_permissions(&dir).ok()?,
        dir,
        eager: true,
        origin: Origin::Fetched { src },
    })
}

/// What `/packupdate` or `/packdel` asked for.
///
/// Parsed here rather than in the UI so the flags mean exactly what the Lua
/// options mean; `++lockfile` and `target = "lockfile"` reach the same field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackCommand {
    Update {
        /// Empty means every declared package.
        names: Vec<String>,
        options: crate::api::pack::UpdateOptions,
    },
    Delete {
        names: Vec<String>,
        all: bool,
        force: bool,
    },
}

impl PackCommand {
    /// Reads one command line. `bang` is the trailing `!`, which the palette
    /// has already taken off the name.
    pub fn parse(name: &str, args: &str, bang: bool) -> Result<Self, String> {
        use crate::api::pack::{UpdateOptions, UpdateTarget};

        let mut names = Vec::new();
        let mut options = UpdateOptions::default();
        let mut all = false;
        for word in args.split_whitespace() {
            match word {
                "++offline" => options.offline = true,
                "++lockfile" => options.target = UpdateTarget::Lockfile,
                "++all" => all = true,
                // Refused rather than treated as a package name. A flag maki
                // does not know is a mistake, and silently reading `++ofline`
                // as a package would report the wrong problem.
                flag if flag.starts_with('+') || flag.starts_with('-') => {
                    return Err(format!("{name}: unknown option {flag:?}"));
                }
                other if !names.iter().any(|name| name == other) => names.push(other.to_owned()),
                _ => {}
            }
        }

        match name {
            "/packupdate" => {
                if all {
                    return Err("/packupdate: ++all is not an option; omit the name instead".into());
                }
                options.force = bang;
                Ok(PackCommand::Update { names, options })
            }
            "/packdel" => {
                if options != UpdateOptions::default() {
                    return Err("/packdel: ++offline and ++lockfile apply to /packupdate".into());
                }
                if all != names.is_empty() {
                    return Err("/packdel: name a package, or pass ++all".into());
                }
                Ok(PackCommand::Delete {
                    names,
                    all,
                    force: bang,
                })
            }
            other => Err(format!("{other}: not a package command")),
        }
    }
}

/// Turns a command into the operations that carry it out.
///
/// `declared` names what `init.lua` asked for, `installed` what the lockfile
/// records, and `active` what is loaded. All three are passed in rather than
/// read here, so this stays a pure decision that a test can drive.
pub fn plan_command(
    cmd: &PackCommand,
    declared: &[crate::api::pack::Declared],
    installed: &[String],
    active: &std::collections::BTreeSet<String>,
) -> Result<Vec<crate::api::pack::PackOp>, String> {
    use crate::api::pack::PackOp;

    let known: Vec<&str> = declared.iter().map(|d| d.spec.name.as_str()).collect();
    let resolve = |names: &[String]| -> Result<Vec<String>, String> {
        for name in names {
            if !known.contains(&name.as_str()) {
                return Err(format!("{name}: not a package declared with maki.pack.add"));
            }
        }
        Ok(names.to_vec())
    };

    match cmd {
        PackCommand::Update { names, options } => {
            // Updating asks what is installed as well as what is declared, for
            // the same reason deletion does. Applying an update installs when
            // there is no lock entry, so a package whose install was declined
            // at startup would be cloned and recorded by a later bare
            // `/packupdate`, and the next start would see it as installed and
            // never ask again.
            let names: Vec<String> = if names.is_empty() {
                known
                    .iter()
                    .filter(|name| installed.iter().any(|i| i == *name))
                    .map(|n| (*n).to_owned())
                    .collect()
            } else {
                let names = resolve(names)?;
                for name in &names {
                    if !installed.contains(name) {
                        return Err(format!(
                            "{name}: not installed, so there is nothing to update; \
                             reload to install it"
                        ));
                    }
                }
                names
            };
            if names.is_empty() {
                return Err("no declared package is installed; nothing was updated".into());
            }
            Ok(names
                .into_iter()
                .map(|name| PackOp::Update {
                    name,
                    options: *options,
                })
                .collect())
        }
        PackCommand::Delete { names, all, force } => {
            // Deletion asks what is installed, not what is declared. Removing
            // the `maki.pack.add` line and then removing the files is the
            // normal way to get rid of a package, and it is the only way that
            // works under `--no-plugins`, where nothing is declared at all.
            let names = if *all {
                // Without the bang this removes only what is not running,
                // matching `:packdel ++all`. Removing a loaded package is the
                // destructive half, so it needs the bang to say so.
                installed
                    .iter()
                    .filter(|name| !known.contains(&name.as_str()))
                    .filter(|name| *force || !active.contains(*name))
                    .cloned()
                    .collect()
            } else {
                for name in names {
                    if !installed.contains(name) {
                        return Err(format!("{name}: not an installed package"));
                    }
                    if known.contains(&name.as_str()) {
                        return Err(format!(
                            "{name}: package is still declared; remove it from maki.pack.add first"
                        ));
                    }
                    if active.contains(name) && !force {
                        return Err(format!(
                            "{name}: package is active; use /packdel! to remove it"
                        ));
                    }
                }
                names.to_vec()
            };
            if names.is_empty() {
                return Err("no package matched; nothing was removed".into());
            }
            Ok(names
                .into_iter()
                .map(|name| PackOp::Delete {
                    name,
                    force: *force,
                })
                .collect())
        }
    }
}

/// Hands the runtime every installed package that can be activated later.
///
/// This includes manual `opt/` packages, managed packages declared with
/// `load = false`, and packages with lazy triggers. The same runtime table
/// therefore serves explicit `maki.packadd` calls and automatic activation.
///
/// `reserved` carries the names the caller resolves *before* Lua commands:
/// builtins and discovered custom commands. A trigger on one of those could
/// never fire, because the palette would dispatch the other one first, so it
/// is refused rather than left as a command that silently does nothing.
pub fn arm_packages(
    host: &crate::loader::PluginHost,
    packages: &[DiscoveredPackage],
    declared: &[crate::api::pack::Declared],
    reserved: &[String],
    active: &std::collections::BTreeSet<String>,
    config: &maki_config::PluginsConfig,
) -> Result<(), PluginError> {
    let mut names = std::collections::BTreeSet::new();
    let mut dormant = Vec::new();
    for package in packages {
        if owner_conflict(&package.name).is_some() {
            continue;
        }
        if !names.insert(package.name.clone()) {
            continue;
        }
        let Some(triggers) = activation_triggers(package, declared) else {
            continue;
        };
        // A package the config disabled stays disabled. Waking it on a
        // trigger would be a way around `plugins.<name>.enabled = false`.
        if !config.packages.iter().any(|name| name == &package.name) {
            continue;
        }
        // Already loaded, because an eager load or `maki.packadd` named it.
        // Cataloging it again would let a later trigger run its top level a
        // second time under an owner that is already registered.
        if active.contains(&package.name) {
            continue;
        }
        let package = match &package.origin {
            // Re-resolve managed packages from the lockfile. An update or
            // deletion may have changed the path since installation returned.
            Origin::Fetched { .. } => match installed_package(&package.name) {
                Some(package) => package,
                None => continue,
            },
            Origin::Manual => package.clone(),
        };
        let permissions = effective_permissions(&package);
        if !package.requested.is_granted_by(&permissions) {
            continue;
        }
        dormant.push(Dormant {
            // Narrowed here, once, by the same rule a startup load uses.
            // Activation must not be a way to get more than the package
            // would have been granted at startup.
            permissions,
            name: package.name.clone(),
            dir: package.dir,
            opts: config
                .opts
                .get(&package.name)
                .cloned()
                .map(std::sync::Arc::new)
                .unwrap_or_default(),
            triggers,
            state: PackState::Inactive,
        });
    }

    let dormant = filter_trigger_collisions(host, dormant, reserved);

    host.set_dormant_packages(dormant)
}

fn filter_trigger_collisions(
    host: &crate::loader::PluginHost,
    dormant: Vec<Dormant>,
    reserved: &[String],
) -> Vec<Dormant> {
    // A trigger that claims a name something already answers to would shadow
    // it until the package loads, and then lose the name anyway once the real
    // registration wins. Refusing the trigger keeps the working command
    // working; the package can still be woken with `maki.packadd`.
    //
    // The published snapshot mixes real registrations with the pending entries
    // a previous arming put there for these same packages. Startup arms twice,
    // once before loading and once after, so without this the second pass
    // would see its own pending entries and drop every trigger it had just
    // published.
    let arming: std::collections::BTreeSet<&str> =
        dormant.iter().map(|pkg| pkg.name.as_str()).collect();
    let mut taken: Vec<String> = host
        .command_reader()
        .load()
        .commands
        .iter()
        .filter(|command| !arming.contains(command.plugin.as_ref()))
        .map(|command| command.name.to_string())
        .collect();
    taken.extend(reserved.iter().cloned());
    let mut seen: Vec<String> = Vec::new();
    let mut seen_keys: Vec<(KeyCode, KeyModifiers)> = Vec::new();
    // Keys a loaded plugin already answers. The snapshot suppresses a pending
    // entry for one of these, so arming it would leave a package with a
    // trigger that can never fire and no warning saying why.
    let bound: Vec<(KeyCode, KeyModifiers)> = host
        .keymap_reader()
        .load()
        .entries
        .iter()
        .filter(|e| !arming.contains(e.plugin.as_ref()))
        .map(|e| (e.key, e.modifiers))
        .collect();
    dormant
        .into_iter()
        .map(|mut pkg| {
            pkg.triggers.cmd.retain(|cmd| {
                if taken.iter().any(|t| t == cmd) {
                    tracing::warn!(
                        package = %pkg.name,
                        command = %cmd,
                        "a command with this name already exists; the trigger is ignored"
                    );
                    return false;
                }
                // Two packages claiming one command would race, and which one
                // won would depend on declaration order.
                if seen.iter().any(|t| t == cmd) {
                    tracing::warn!(
                        package = %pkg.name,
                        command = %cmd,
                        "another package already claims this command; the trigger is ignored"
                    );
                    return false;
                }
                seen.push(cmd.clone());
                true
            });
            // Keys need the same rule. `with_dormant` already refuses a key a
            // loaded plugin holds, but nothing there stops two dormant
            // packages claiming one key, and which of them a press woke would
            // then depend on declaration order.
            pkg.triggers.keys.retain(|notation| {
                // Parsed once, before either test. Parsing inside the bound
                // check ran it again for every key a plugin already holds, and
                // reported an unusable notation as "already bound" whenever
                // some other binding happened to exist.
                let Ok(parsed) = crate::api::keymap::parse_key_notation(notation) else {
                    tracing::warn!(
                        package = %pkg.name,
                        key = %notation,
                        "not a usable key notation; the trigger is ignored"
                    );
                    return false;
                };
                if bound.contains(&parsed) {
                    tracing::warn!(
                        package = %pkg.name,
                        key = %notation,
                        "a plugin already binds this key; the trigger is ignored"
                    );
                    return false;
                }
                if seen_keys.contains(&parsed) {
                    tracing::warn!(
                        package = %pkg.name,
                        key = %notation,
                        "another package already claims this key; the trigger is ignored"
                    );
                    return false;
                }
                seen_keys.push(parsed);
                true
            });
            pkg
        })
        .collect()
}

/// Automatic triggers for one package, or an empty set when only an explicit
/// `maki.packadd` may activate it.
fn activation_triggers(
    package: &DiscoveredPackage,
    declared: &[crate::api::pack::Declared],
) -> Option<crate::api::pack::Triggers> {
    use crate::api::pack::LoadMode;

    match &package.origin {
        Origin::Manual if !package.eager => Some(crate::api::pack::Triggers::default()),
        Origin::Manual => None,
        Origin::Fetched { .. } => {
            let declaration = declared
                .iter()
                .find(|declaration| declaration.spec.name == package.name)?;
            match &declaration.load {
                LoadMode::Dormant => Some(crate::api::pack::Triggers::default()),
                LoadMode::Triggered(triggers) => Some(triggers.clone()),
                LoadMode::Eager | LoadMode::Custom(_) => None,
            }
        }
    }
}

/// Applies whatever `init.lua` asked for, once everything it could refer to is
/// loaded.
///
/// This is the only drain point, and it is deliberately here rather than
/// somewhere inside the running UI. `maki.pack.update` and `del` are attached
/// only where a config store is, which is the `init.lua` context, so the
/// pending queue is filled during startup and nowhere else. Draining on the
/// startup thread, after the packages are loaded, means the work happens off
/// the Lua thread (so unloading cannot wait on itself) and before the terminal
/// is taken over (so a clone cannot freeze a redraw).
///
pub fn drain_pack_ops(
    host: &crate::loader::PluginHost,
    declared: &[crate::api::pack::Declared],
    packages: &[DiscoveredPackage],
    config: &maki_config::PluginsConfig,
    interaction: Interaction,
) -> PackReport {
    // Read and closed in one message. `maki.packadd` records here whenever
    // the runtime cannot serve it directly, and a read followed by a separate
    // close leaves a window where a spawned Lua task records an activation
    // that is then sealed away unread.
    let ops = match host.seal_pack_ops() {
        Ok(ops) => ops,
        Err(e) => {
            tracing::error!(error = %e, "could not read pending package operations");
            return PackReport::default();
        }
    };
    apply_pack_ops(host, &ops, declared, packages, config, interaction)
}

/// What a batch of package operations did.
///
/// Returned rather than only logged, because `/packupdate` has to tell the user
/// what moved, and "check the log" is not an answer for a command they just
/// typed. Every string here is already redacted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackReport {
    /// Package name, and the revision it now sits at.
    pub updated: Vec<(String, String)>,
    pub removed: Vec<String>,
    pub activated: Vec<String>,
    pub failures: Vec<String>,
}

impl PackReport {
    /// One line summarising the batch, for the status area.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.updated.is_empty() {
            parts.push(format!("{} updated", self.updated.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!("{} removed", self.removed.len()));
        }
        if !self.activated.is_empty() {
            parts.push(format!("{} activated", self.activated.len()));
        }
        if !self.failures.is_empty() {
            parts.push(format!("{} failed", self.failures.len()));
        }
        if parts.is_empty() {
            return "no package changed".to_owned();
        }
        format!("packages: {}", parts.join(", "))
    }
}

/// Strips credentials out of an error before it is logged.
///
/// `ManagerError` embeds the source URI in several variants, and a source may
/// carry a password. The git runner redacts its own output, but an error built
/// outside it (an unsatisfied version range, for one) never passed through
/// that, so the redaction is applied at the point of logging instead.
pub(crate) fn redact_error(e: &impl std::fmt::Display) -> String {
    sanitize_message(&maki_pack::git::redact(&e.to_string()))
}

/// Finds where an activatable package lives, and how far to trust it.
///
/// A managed package resolves through the lockfile. A package the user placed
/// under `pack/<group>/opt/` by hand has no lock entry, and refusing it here
/// would contradict the documented behaviour of `maki.packadd`.
fn resolve_for_activation(
    manager: &maki_pack::manager::Manager,
    lock: &maki_pack::lockfile::Lockfile,
    site: &Path,
    name: &str,
) -> Option<(PathBuf, Origin)> {
    if let Some(dir) = manager.resolve(lock, name) {
        let src = lock.get(name)?.src.clone();
        return Some((dir, Origin::Fetched { src }));
    }
    discover(site)
        .packages
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| (p.dir, p.origin))
}

/// What one package load should be given.
///
/// Both load paths go through this, so a change to how a grant is narrowed or
/// where options come from cannot reach one of them and miss the other.
fn load_inputs(
    name: &str,
    dir: &Path,
    origin: Origin,
    config: &maki_config::PluginsConfig,
) -> Result<
    (
        crate::plugin_permissions::PluginPermissions,
        crate::api::options::PluginOpts,
    ),
    String,
> {
    let package = DiscoveredPackage {
        name: name.to_owned(),
        requested: load_requested_permissions(dir).map_err(|e| redact_error(&e))?,
        dir: dir.to_path_buf(),
        eager: true,
        origin,
    };
    let opts = config
        .opts
        .get(name)
        .cloned()
        .map(std::sync::Arc::new)
        .unwrap_or_default();
    Ok((effective_permissions(&package), opts))
}

/// Loads one package and says whether it worked.
///
/// Deliberately not `load_packages`, which only logs a failure. A caller that
/// has just unloaded the old owner has to know the new one did not arrive, or
/// it reports an update that left the package gone.
fn load_one(
    host: &crate::loader::PluginHost,
    name: &str,
    dir: &Path,
    origin: Origin,
    config: &maki_config::PluginsConfig,
) -> Result<(), String> {
    let (permissions, opts) = load_inputs(name, dir, origin, config)?;
    host.load_package(name, dir, permissions, opts)
        .map_err(|error| redact_error(&error))
}

fn load_declared_one(
    host: &crate::loader::PluginHost,
    declared: &crate::api::pack::Declared,
    dir: &Path,
    src: &str,
    config: &maki_config::PluginsConfig,
) -> Result<(), String> {
    let origin = Origin::Fetched {
        src: src.to_owned(),
    };
    if !matches!(declared.load, crate::api::pack::LoadMode::Custom(_)) {
        return load_one(host, &declared.spec.name, dir, origin, config);
    }
    let (permissions, opts) = load_inputs(&declared.spec.name, dir, origin, config)?;
    let mut declared = declared.clone();
    declared.spec.src = src.to_owned();
    host.run_pack_loader(declared, dir, permissions, opts)
        .map_err(|error| redact_error(&error))
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
    match maki_pack::lockfile::Lockfile::from_json(&text) {
        Ok(lock) => Some(lock),
        Err(e) => {
            tracing::error!(error = %e, "pack lockfile is unreadable; refusing to change packages");
            None
        }
    }
}

fn write_approvals(approvals: &maki_pack::approvals::Approvals) -> bool {
    let Some(path) = approvals_path() else {
        return false;
    };
    match serde_json::to_string_pretty(approvals) {
        Ok(text) => match write_atomically(&path, &text) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "failed to write pack approvals");
                false
            }
        },
        Err(error) => {
            tracing::error!(%error, "failed to serialize pack approvals");
            false
        }
    }
}

/// Writes the lockfile, and says whether it landed.
fn write_lockfile(path: &Path, lock: &maki_pack::lockfile::Lockfile) -> bool {
    match lock.to_json() {
        Ok(text) => match write_atomically(path, &text) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "failed to write pack lockfile");
                false
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize pack lockfile");
            false
        }
    }
}

/// Drops a package's recorded approval.
///
/// A best-effort write: failing to revoke leaves a stale grant that only takes
/// effect if the same name and source are installed again, so it is logged
/// rather than allowed to abort the removal that already happened.
fn revoke_approval(name: &str) -> bool {
    let Some(path) = approvals_path() else {
        return true;
    };
    let Some(mut approvals) = read_approvals_file(&path) else {
        return false;
    };
    approvals.revoke(name);
    write_approvals(&approvals)
}

/// Writes through a temporary file and renames, so a crash mid-write cannot
/// truncate a lockfile that a user has committed.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    maki_storage::atomic_write(path, text.as_bytes()).map_err(std::io::Error::other)
}

/// What a discovery walk found, and what it had to refuse.
///
/// Problems are collected rather than returned as one error, because one
/// unusable package must not stop the others from loading.
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
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::loader::PluginHost;
    use crate::plugin_permissions::{Permission, PluginPermissions};
    use maki_agent::tools::ToolRegistry;

    const EMPTY_PACK_SUMMARY: &str = "no package changed";
    const FULL_PACK_SUMMARY: &str = "packages: 1 updated, 2 removed, 1 activated, 2 failed";
    const SAFE_PROMPT: &str = "update [31m name\nnext";

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
        }
    }

    /// A package the user placed by hand is trusted like `init.lua`: they put
    /// the files there, so its manifest is their own statement.
    #[test]
    fn a_manual_package_is_granted_what_its_manifest_asks_for() {
        let pkg = greedy("demo", Origin::Manual);
        let effective = granted(&pkg, &maki_pack::approvals::Approvals::default());

        for perm in Permission::ALL {
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

        for perm in Permission::ALL {
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

    fn declared_pack(name: &str, src: &str) -> crate::api::pack::Declared {
        crate::api::pack::Declared {
            spec: maki_pack::Spec::new(src).with_name(name),
            load: crate::api::pack::LoadMode::Eager,
            confirm: true,
            data: None,
        }
    }

    fn declared_named(names: &[&str]) -> Vec<crate::api::pack::Declared> {
        names
            .iter()
            .map(|n| declared_pack(n, &format!("https://example.com/{n}")))
            .collect()
    }

    /// Sets up a site where `name` is installed from `src` at one revision.
    fn installed_site(name: &str, src: &str) -> (tempfile::TempDir, maki_pack::lockfile::Lockfile) {
        let site = tempfile::TempDir::new().unwrap();
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record(name, src, "abc123");
        fs::create_dir_all(maki_pack::paths::revision_dir(site.path(), name, "abc123")).unwrap();
        (site, lock)
    }

    /// Every pass over a batch has to agree about which operations may run.
    /// A lock entry proves a managed package exists under a name, not that the
    /// loaded owner is that one: with an orphan entry beside a hand-installed
    /// package of the same name, `/packdel!` unloaded the manual package's
    /// registrations, removed the managed checkout and revoked its approval,
    /// and nothing brought the manual package back. Not being declared is
    /// exactly what gets an operation this far, so no later pass can catch it.
    #[test]
    fn a_name_held_by_a_manual_package_runs_no_operation() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "demo");
        let manual = discover(tmp.path());
        assert_eq!(
            manual.packages.len(),
            1,
            "the hand-installed package is there"
        );

        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/demo", "abc123");

        // A manifest that stopped parsing does not stop the package from being
        // the live owner of its name, so the guard has to hold for it too.
        fs::write(
            tmp.path()
                .join("pack/vendor/start/demo")
                .join("plugin.toml"),
            "not = valid = toml",
        )
        .unwrap();
        let refused = discover(tmp.path());
        assert!(refused.packages.is_empty(), "it can no longer be loaded");
        let mut report = PackReport::default();
        assert!(
            runnable_ops(
                &[crate::api::pack::PackOp::Delete {
                    name: "demo".to_owned(),
                    force: true,
                }],
                &refused,
                &lock,
                &mut report,
            )
            .is_empty(),
            "a package that stopped loading still holds its name"
        );

        // One at a time: two operations naming one package are refused as a
        // pair, which would hide whether this guard fired at all.
        for op in [
            crate::api::pack::PackOp::Delete {
                name: "demo".to_owned(),
                force: true,
            },
            crate::api::pack::PackOp::Activate {
                name: "demo".to_owned(),
            },
            crate::api::pack::PackOp::Update {
                name: "demo".to_owned(),
                options: Default::default(),
            },
        ] {
            let mut report = PackReport::default();
            let ops = vec![op.clone()];
            let runnable = runnable_ops(&ops, &manual, &lock, &mut report);

            assert!(runnable.is_empty(), "{op:?} must not run");
            assert_eq!(report.failures.len(), 1, "got: {:?}", report.failures);
            assert!(
                report.failures[0].contains("conflicts with manual package"),
                "{op:?}: {}",
                report.failures[0]
            );
        }
    }

    /// The rule that decides whether installing is a fresh trust decision.
    /// `.maki/init.lua` is project local, so a repository maki opens can point
    /// a name the user already trusts somewhere else.
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
            maki_pack::paths::revision_dir(site.path(), "demo", "abc123")
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

    fn active_set(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn only_manual_opt_packages_are_explicit_activation_targets() {
        let mut package = greedy("demo", Origin::Manual);
        package.eager = false;
        assert!(activation_triggers(&package, &[]).is_some());

        package.eager = true;
        assert!(activation_triggers(&package, &[]).is_none());
    }

    #[test]
    fn managed_activation_uses_the_declaration_with_the_same_name() {
        let package = greedy(
            "demo",
            Origin::Fetched {
                src: "https://example.com/demo".to_owned(),
            },
        );
        let mut declared = declared_named(&["other", "demo"]);
        declared[1].load = crate::api::pack::LoadMode::Triggered(crate::api::pack::Triggers {
            cmd: vec!["/demo".to_owned()],
            ..Default::default()
        });

        let triggers = activation_triggers(&package, &declared).expect("demo is activatable");

        assert_eq!(triggers.cmd, ["/demo"]);
    }

    #[test]
    fn builtin_owners_and_disabled_packages_cannot_be_activated() {
        let config = maki_config::PluginsConfig::from_plugins_and_packages(
            Default::default(),
            &["bash".to_owned(), "demo".to_owned()],
        );

        assert!(owner_conflict("bash").is_some());
        assert!(activation_refusal("bash", &config).is_some());
        assert!(activation_refusal("disabled", &config).is_some());
        assert!(activation_refusal("demo", &config).is_none());

        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let mut package = greedy("bash", Origin::Manual);
        package.eager = false;
        arm_packages(&host, &[package], &[], &[], &Default::default(), &config).unwrap();

        let error = host
            .load_source("caller", r#"maki.packadd("bash")"#)
            .expect_err("a catalog refresh must not reintroduce a builtin owner");
        assert!(error.to_string().contains("not installed"), "got: {error}");
    }

    #[test]
    fn trigger_collisions_keep_only_unclaimed_commands_and_keys() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "loaded",
            r#"
            maki.api.register_command({ name = "/taken", handler = function() end })
            maki.keymap.set("n", "<C-g>", function() end)
            "#,
        )
        .unwrap();
        let package = |name: &str, commands: &[&str], keys: &[&str]| Dormant {
            name: name.to_owned(),
            dir: PathBuf::from("/nowhere"),
            permissions: PluginPermissions::denied(),
            opts: Default::default(),
            triggers: crate::api::pack::Triggers {
                cmd: commands
                    .iter()
                    .map(|command| (*command).to_owned())
                    .collect(),
                keys: keys.iter().map(|key| (*key).to_owned()).collect(),
                ..Default::default()
            },
            state: PackState::Inactive,
        };
        let dormant = vec![
            package(
                "first",
                &["/taken", "/reserved", "/shared"],
                &["<C-g>", "<C-x>"],
            ),
            package("second", &["/shared", "/second"], &["<C-x>", "<C-y>"]),
        ];

        let filtered = filter_trigger_collisions(&host, dormant, &["/reserved".to_owned()]);

        assert_eq!(filtered[0].triggers.cmd, ["/shared"]);
        assert_eq!(filtered[0].triggers.keys, ["<C-x>"]);
        assert_eq!(filtered[1].triggers.cmd, ["/second"]);
        assert_eq!(filtered[1].triggers.keys, ["<C-y>"]);
    }

    /// Startup arms twice: once before the packages load, and once after. The
    /// first pass publishes a pending palette entry for each trigger so the
    /// user can type the command that wakes the package. The second pass must
    /// not read those entries as names that are already taken, or every
    /// trigger it just published would be dropped and no lazy package could
    /// ever be woken by its command or key.
    #[test]
    fn a_second_arming_keeps_the_triggers_the_first_published() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let package = || Dormant {
            name: "lazy".to_owned(),
            dir: PathBuf::from("/nowhere"),
            permissions: PluginPermissions::denied(),
            opts: Default::default(),
            triggers: crate::api::pack::Triggers {
                cmd: vec!["/lazy".to_owned()],
                keys: vec!["<C-l>".to_owned()],
                ..Default::default()
            },
            state: PackState::Inactive,
        };

        let first = filter_trigger_collisions(&host, vec![package()], &[]);
        assert_eq!(first[0].triggers.cmd, ["/lazy"]);
        host.set_dormant_packages(first).unwrap();

        let second = filter_trigger_collisions(&host, vec![package()], &[]);

        assert_eq!(
            second[0].triggers.cmd,
            ["/lazy"],
            "the pending entry the first arming published is not a collision"
        );
        assert_eq!(second[0].triggers.keys, ["<C-l>"]);
    }

    #[test]
    fn terminal_prompts_keep_layout_but_remove_control_characters() {
        assert_eq!(
            sanitize_message("update\u{1b}[31m\rname\nnext"),
            SAFE_PROMPT
        );
    }

    #[test]
    fn a_changed_source_does_not_fetch_the_old_lock_entry() {
        let site = tempfile::TempDir::new().unwrap();
        let manager = maki_pack::manager::Manager::new(site.path());
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/old", "abc123");
        let mut declared = declared_named(&["demo"]);
        declared[0].spec.src = "https://example.com/new".to_owned();

        let candidates = install_candidates(&declared, &lock, &manager, Some(false));

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].spec.src, "https://example.com/new");
        assert!(candidates[0].confirm);
        assert!(candidates[0].source_changed);
    }

    #[test]
    fn an_installed_revision_does_not_block_a_source_change() {
        let site = tempfile::TempDir::new().unwrap();
        let manager = maki_pack::manager::Manager::new(site.path());
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/old", "abc123");
        fs::create_dir_all(maki_pack::paths::revision_dir(
            site.path(),
            "demo",
            "abc123",
        ))
        .unwrap();
        let mut declared = declared_named(&["demo"]);
        declared[0].spec.src = "https://example.com/new".to_owned();

        let candidates = install_candidates(&declared, &lock, &manager, Some(true));

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].spec.src, "https://example.com/new");
        assert!(candidates[0].source_changed);
    }

    #[test]
    fn an_installed_matching_revision_needs_no_install() {
        let site = tempfile::TempDir::new().unwrap();
        let manager = maki_pack::manager::Manager::new(site.path());
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/demo", "abc123");
        fs::create_dir_all(maki_pack::paths::revision_dir(
            site.path(),
            "demo",
            "abc123",
        ))
        .unwrap();
        let declared = declared_named(&["demo"]);

        assert!(install_candidates(&declared, &lock, &manager, Some(true)).is_empty());
    }

    #[test]
    fn a_missing_matching_lock_entry_is_planned_once() {
        let site = tempfile::TempDir::new().unwrap();
        let manager = maki_pack::manager::Manager::new(site.path());
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/demo", "abc123");
        let declared = declared_named(&["demo"]);

        let candidates = install_candidates(&declared, &lock, &manager, Some(true));

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].declared.load,
            crate::api::pack::LoadMode::Eager
        );
    }

    #[test]
    fn permission_check_accepts_no_request_and_rejects_an_unapproved_request() {
        let package = tempfile::TempDir::new().unwrap();
        let mut approvals = maki_pack::approvals::Approvals::default();
        assert_eq!(
            approve_permissions(
                Interaction::None,
                "demo",
                "https://example.com/demo",
                package.path(),
                &mut approvals,
            ),
            Ok(false)
        );

        fs::write(
            package.path().join("plugin.toml"),
            "[permissions]\nrun = true\n",
        )
        .unwrap();
        let error = approve_permissions(
            Interaction::None,
            "demo",
            "https://example.com/demo",
            package.path(),
            &mut approvals,
        )
        .expect_err("an unapproved request must not pass in headless mode");
        assert!(error.contains("run"), "{error}");
    }

    #[test]
    fn permission_check_reuses_an_exact_approval() {
        let package = tempfile::TempDir::new().unwrap();
        fs::write(
            package.path().join("plugin.toml"),
            "[permissions]\nrun = true\n",
        )
        .unwrap();
        let src = "https://example.com/demo";
        let mut approvals = maki_pack::approvals::Approvals::default();
        approvals.approve(
            &maki_pack::approvals::ApprovalKey::new("demo", src),
            vec!["run".to_owned()],
        );

        assert_eq!(
            approve_permissions(
                Interaction::None,
                "demo",
                src,
                package.path(),
                &mut approvals,
            ),
            Ok(false)
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

        assert_eq!(
            read_lockfile(Some(&path)).unwrap().install_order().count(),
            0
        );
    }

    #[test]
    fn an_invalid_approval_store_is_not_safe_to_overwrite() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("approvals.json");
        fs::write(&path, "not json").unwrap();

        assert!(read_approvals_file(&path).is_none());
    }

    #[test]
    fn a_missing_approval_store_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("missing.json");

        assert!(read_approvals_file(&path).unwrap().is_empty());
    }

    #[test]
    fn atomic_writes_replace_an_existing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pack-lock.json");
        fs::write(&path, "old").unwrap();

        write_atomically(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "new");
    }

    #[test]
    fn a_source_change_keeps_an_approval_for_the_new_source() {
        let name = "demo";
        let old_src = "https://example.com/old";
        let new_src = "https://example.com/new";
        let mut approvals = maki_pack::approvals::Approvals::default();
        approvals.approve(
            &maki_pack::approvals::ApprovalKey::new(name, new_src),
            vec!["run".to_owned()],
        );

        assert!(!revoke_mismatched_approval(&mut approvals, name, new_src));
        assert!(
            approvals
                .get(&maki_pack::approvals::ApprovalKey::new(name, new_src))
                .is_some()
        );

        approvals.approve(
            &maki_pack::approvals::ApprovalKey::new(name, old_src),
            vec!["run".to_owned()],
        );
        assert!(revoke_mismatched_approval(&mut approvals, name, new_src));
        assert!(approvals.is_empty());
    }

    #[test]
    fn pack_report_summary_lists_each_result_count() {
        assert_eq!(PackReport::default().summary(), EMPTY_PACK_SUMMARY);
        let report = PackReport {
            updated: vec![("updated".to_owned(), "revision".to_owned())],
            removed: vec!["removed-one".to_owned(), "removed-two".to_owned()],
            activated: vec!["activated".to_owned()],
            failures: vec!["failed-one".to_owned(), "failed-two".to_owned()],
        };

        assert_eq!(report.summary(), FULL_PACK_SUMMARY);
    }

    #[test]
    fn packupdate_flags_map_onto_the_same_options_lua_uses() {
        use crate::api::pack::{UpdateOptions, UpdateTarget};

        let cmd = PackCommand::parse("/packupdate", "++offline ++lockfile", false).unwrap();
        assert_eq!(
            cmd,
            PackCommand::Update {
                names: vec![],
                options: UpdateOptions {
                    target: UpdateTarget::Lockfile,
                    offline: true,
                    force: false,
                },
            }
        );
    }

    #[test]
    fn packupdate_bang_sets_force() {
        let PackCommand::Update { options, .. } =
            PackCommand::parse("/packupdate", "", true).unwrap()
        else {
            panic!("expected update command");
        };
        assert!(options.force);
    }

    /// A mistyped flag must not become a package name, or the error would name
    /// the wrong problem.
    #[test]
    fn an_unknown_flag_is_refused_rather_than_read_as_a_name() {
        let err = PackCommand::parse("/packupdate", "++ofline", false)
            .expect_err("a misspelled flag is not a package");
        assert!(err.contains("++ofline"), "{err}");
    }

    /// No name means every declared package that is installed, which is what
    /// the command's own help says and what `:packupdate` does.
    #[test]
    fn packupdate_without_a_name_plans_every_installed_declared_package() {
        let declared = declared_named(&["alpha", "beta"]);
        let installed = vec!["alpha".to_owned(), "beta".to_owned()];
        let cmd = PackCommand::parse("/packupdate", "", false).unwrap();
        let ops = plan_command(&cmd, &declared, &installed, &active_set(&[])).unwrap();
        assert_eq!(ops.len(), 2);
    }

    /// Declining the install prompt has to stick. Applying an update installs
    /// when there is no lock entry, so planning one for a declared package
    /// that is not installed let a bare `/packupdate` clone and record the
    /// source the user had just refused, after which startup saw it as
    /// installed and never asked again.
    #[test]
    fn packupdate_leaves_a_declared_package_that_is_not_installed() {
        let declared = declared_named(&["alpha", "beta"]);
        let installed = vec!["alpha".to_owned()];
        let active = active_set(&[]);

        let ops = plan_command(
            &PackCommand::parse("/packupdate", "", false).unwrap(),
            &declared,
            &installed,
            &active,
        )
        .unwrap();
        assert_eq!(ops.len(), 1, "only the installed package is updated");

        let err = plan_command(
            &PackCommand::parse("/packupdate", "beta", false).unwrap(),
            &declared,
            &installed,
            &active,
        )
        .expect_err("naming it explicitly is still not an install");
        assert!(err.contains("beta"), "{err}");
    }

    #[test]
    fn packupdate_refuses_a_package_that_was_never_declared() {
        let declared = declared_named(&["alpha"]);
        let cmd = PackCommand::parse("/packupdate", "ghost", false).unwrap();
        let err = plan_command(&cmd, &declared, &[], &active_set(&[]))
            .expect_err("an undeclared package cannot be updated");
        assert!(err.contains("ghost"), "{err}");
    }

    /// `++all` without the bang removes only what is not running, and with the
    /// bang removes everything. This is the confirmation the bang stands in
    /// for: the destructive half has to be asked for explicitly.
    #[test]
    fn packdel_all_skips_loaded_packages_unless_the_bang_is_given() {
        let installed = vec!["alpha".to_owned(), "beta".to_owned()];
        let active = active_set(&["alpha"]);
        let cmd = PackCommand::parse("/packdel", "++all", false).unwrap();

        let ops = plan_command(&cmd, &[], &installed, &active).unwrap();
        assert_eq!(
            ops,
            vec![crate::api::pack::PackOp::Delete {
                name: "beta".to_owned(),
                force: false,
            }],
            "without the bang, the loaded package stays"
        );

        let cmd = PackCommand::parse("/packdel", "++all", true).unwrap();
        let forced = plan_command(&cmd, &[], &installed, &active).unwrap();
        assert_eq!(forced.len(), 2, "the bang removes the loaded one too");
        assert!(forced.into_iter().all(|operation| {
            matches!(
                operation,
                crate::api::pack::PackOp::Delete { force: true, .. }
            )
        }));
    }

    /// Deleting a package is how you get rid of one you no longer declare, so
    /// it has to work from the installed set alone. Asking the declarations
    /// would make the ordinary "remove the line, then remove the files"
    /// sequence impossible, and would break the command under `--no-plugins`,
    /// where nothing is declared at all.
    #[test]
    fn packdel_removes_an_installed_package_that_is_no_longer_declared() {
        let installed = vec!["orphan".to_owned()];
        let cmd = PackCommand::parse("/packdel", "orphan", false).unwrap();

        let ops = plan_command(&cmd, &[], &installed, &active_set(&[]))
            .expect("an installed package can be removed without a declaration");
        assert_eq!(
            ops,
            vec![crate::api::pack::PackOp::Delete {
                name: "orphan".to_owned(),
                force: false,
            }]
        );
    }

    #[test]
    fn packdel_refuses_a_package_that_startup_would_reinstall() {
        let declared = declared_named(&["alpha"]);
        let installed = vec!["alpha".to_owned()];
        let cmd = PackCommand::parse("/packdel", "alpha", true).unwrap();

        let error = plan_command(&cmd, &declared, &installed, &active_set(&[]))
            .expect_err("a declared package would be reinstalled during the reload");

        assert!(error.contains("remove it from maki.pack.add"), "{error}");
    }

    #[test]
    fn packdel_requires_bang_for_an_active_named_package() {
        let installed = vec!["alpha".to_owned()];
        let active = active_set(&["alpha"]);
        let cmd = PackCommand::parse("/packdel", "alpha", false).unwrap();

        assert!(plan_command(&cmd, &[], &installed, &active).is_err());
        let cmd = PackCommand::parse("/packdel", "alpha", true).unwrap();
        assert_eq!(
            plan_command(&cmd, &[], &installed, &active).unwrap(),
            vec![crate::api::pack::PackOp::Delete {
                name: "alpha".to_owned(),
                force: true,
            }]
        );
    }

    #[test]
    fn packdel_refuses_a_package_that_is_not_installed() {
        let cmd = PackCommand::parse("/packdel", "ghost", false).unwrap();
        let err = plan_command(&cmd, &[], &[], &active_set(&[]))
            .expect_err("nothing to remove is an error, not a silent success");
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn packdel_needs_either_a_name_or_all_but_not_both() {
        assert!(PackCommand::parse("/packdel", "", false).is_err());
        assert!(PackCommand::parse("/packdel", "++all alpha", false).is_err());
        assert!(PackCommand::parse("/packdel", "alpha", false).is_ok());
    }

    /// The update flags mean nothing to a delete, so accepting them silently
    /// would let a user believe an offline delete did something different.
    #[test]
    fn packdel_refuses_the_update_flags() {
        assert!(PackCommand::parse("/packdel", "++all ++offline", false).is_err());
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
        let found = discover(&PathBuf::from("/definitely/not/here"));
        assert!(found.packages.is_empty());
        assert!(found.problems.is_empty());
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
