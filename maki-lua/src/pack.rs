//! Discovery of external packages installed under the site directory.
//!
//! This is the manual half of the package model: directories a user cloned
//! themselves, laid out the way Neovim lays packages out. Packages that maki
//! installs are resolved from recorded state instead, and never appear here.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::error::PluginError;
use crate::loader::is_bundled;
use crate::plugin_permissions::{Requested, load_requested_permissions};

pub(crate) fn sanitize_message(message: &str) -> String {
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
/// itself permissions would make the request self-certifying: an update could
/// add `run = true` and gain the ability to start subprocesses without anyone
/// agreeing to it.
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
}

/// `pack-lock.json`, beside the user's configuration so it can be committed.
///
/// `global_config_dirs` returns several candidates; the last is the one
/// `append_permission_rule` already treats as writable, so the lockfile uses
/// the same directory rather than inventing its own rule.
pub fn lockfile_path() -> Option<PathBuf> {
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
pub fn approvals_path() -> Option<PathBuf> {
    site_dir().ok().map(|dir| dir.join("pack-approvals.json"))
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
pub fn install_declared(
    specs: &[crate::api::pack::Declared],
    interaction: Interaction,
) -> InstallReport {
    let mut report = InstallReport::default();
    if specs.is_empty() {
        return report;
    }
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
                .push("the pack lockfile is unreadable, so no package was installed".to_owned());
            return report;
        }
    };

    let manager = maki_pack::manager::Manager::new(&site);
    let manual = discover(&site).packages;
    let mut changed = false;

    // Asked once for the whole batch, before anything is cloned. A package
    // already recorded at the revision it asks for is not a new trust
    // decision, so only a fresh source prompts. Credentials are redacted,
    // because the prompt is the one place a source is shown in full.
    let new_sources: Vec<String> = specs
        .iter()
        .filter(|declared| {
            declared.confirm && manager.resolve(&lock, &declared.spec.name).is_none()
        })
        .map(|declared| {
            format!(
                "{} from {}",
                declared.spec.name,
                maki_pack::git::redact(&declared.spec.src)
            )
        })
        .collect();
    if !new_sources.is_empty()
        && !interaction.confirm(&format!(
            "Install these packages?\n  {}",
            new_sources.join("\n  ")
        ))
    {
        report.failures.push(format!(
            "installation was not confirmed, so {} package(s) were not installed",
            new_sources.len()
        ));
        return report;
    }

    for declared in specs {
        let spec = &declared.spec;
        if let Some(error) = owner_conflict(&spec.name) {
            report.failures.push(sanitize_message(&error));
            continue;
        }
        if let Some(package) = manual.iter().find(|package| package.name == spec.name) {
            report.failures.push(sanitize_message(&format!(
                "{}: a manual package at {} already has this name",
                spec.name,
                package.dir.display()
            )));
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
        // A manifest that exists but cannot be parsed is not the same fact as
        // one that is absent, so it stops this package rather than loading it
        // with a request nobody wrote.
        let requested = match load_requested_permissions(&result.dir) {
            Ok(requested) => requested,
            Err(problem) => {
                report
                    .failures
                    .push(sanitize_message(&format!("{}: {problem}", spec.name)));
                continue;
            }
        };
        changed |= result.changed;
        report.packages.push(DiscoveredPackage {
            name: spec.name.clone(),
            requested,
            origin: Origin::Fetched {
                src: spec.src.clone(),
            },
            dir: result.dir,
            // `load = false` installs the package and leaves it for
            // `maki.packadd`, which is the state a lazy trigger delays.
            eager: matches!(declared.load, crate::api::pack::LoadMode::Eager),
        });
    }

    // Written once, after the installs, and only when something moved.
    if changed {
        let recorded = match lock_path {
            Some(path) => write_lockfile(&path, &lock),
            None => false,
        };
        if !recorded {
            // The packages are on disk but nothing records where. The next
            // start reads the old lockfile and would not find them, so they
            // must not be reported as installed either.
            report.packages.clear();
            report
                .failures
                .push("the pack lockfile could not be written, so no package was used".to_owned());
        }
    }

    report
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
pub fn apply_pack_ops(
    host: &crate::loader::PluginHost,
    ops: &[crate::api::pack::PackOp],
    declared: &[crate::api::pack::Declared],
    packages: &[DiscoveredPackage],
    config: &maki_config::PluginsConfig,
) -> PackReport {
    use crate::api::pack::{PackOp, UpdateTarget};

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
    // Updated as the batch proceeds. Read once and left alone, a delete
    // followed by an activate in the same batch would skip the activate as
    // "already loaded" for a package the delete had just unloaded.
    let mut active = match host.active_packages() {
        Ok(active) => active,
        Err(error) => {
            report
                .failures
                .push(format!("could not read active packages: {error}"));
            return report;
        }
    };

    let mut changed = false;
    let manual = discover(&site).packages;
    for op in ops {
        let name = match op {
            PackOp::Update { name, .. } | PackOp::Delete { name } | PackOp::Activate { name } => {
                name
            }
        };
        if let Some(error) = owner_conflict(name) {
            report.failures.push(error);
            continue;
        }
        if manual.iter().any(|package| package.name == *name) && lock.get(name).is_some() {
            report.failures.push(format!(
                "{name}: managed package name conflicts with a manual package"
            ));
            continue;
        }
        match op {
            PackOp::Delete { name } => {
                // Checked before anything is unloaded. `unload` takes an owner
                // name, and every plugin is an owner, so deleting a name that
                // is not a managed package would tear down whatever else
                // answers to it: `del("bash")` would unregister the bundled
                // bash tool, its keymaps, and its hints.
                if lock.get(name).is_none() {
                    tracing::error!(package = %name, "not a managed package; refusing to remove");
                    report.failures.push(format!(
                        "{name}: not a managed package, so it was not removed"
                    ));
                    continue;
                }
                // Unloaded before the files go, so nothing keeps running from a
                // directory that no longer exists.
                if let Err(e) = host.unload(name) {
                    tracing::error!(package = %name, error = %e, "failed to unload package");
                    report.failures.push(format!(
                        "{name}: owner cleanup failed, so it was not removed: {e}"
                    ));
                    continue;
                }
                active.remove(name);
                match manager.remove(name, &mut lock) {
                    Ok(()) => {
                        changed = true;
                        report.removed.push(name.clone());
                        // Last, so a step that fails earlier leaves the
                        // approval exactly as it was. Revoking first would
                        // strip the grant from a package that is still
                        // installed, and every guarded call it makes would
                        // then report a permission problem instead of the
                        // removal that actually failed.
                        if !revoke_approval(name) {
                            report.failures.push(format!(
                                "{name}: removed, but its approval could not be revoked; \
                                 reinstalling the same source would not ask again"
                            ));
                        }
                    }
                    Err(e) => {
                        let msg = redact_error(&e);
                        tracing::error!(package = %name, error = %msg, "failed to remove");
                        report.failures.push(format!("{name}: {msg}"));
                    }
                }
            }
            PackOp::Update { name, options } => {
                let Some(spec) = declared
                    .iter()
                    .find(|d| &d.spec.name == name)
                    .map(|d| &d.spec)
                else {
                    tracing::error!(package = %name, "not a declared package; cannot update");
                    report.failures.push(format!(
                        "{name}: not declared with maki.pack.add, so it cannot be updated"
                    ));
                    continue;
                };
                // Kept so a failure can put it back. Dropping the pin is what
                // makes `version` be resolved again, but a failed update must
                // not leave the package unpinned: a later successful operation
                // writes the lockfile, and the pin would be gone from it.
                let previous = lock.get(name).cloned();
                let was_active = active.contains(name);
                let refresh = match options.target {
                    UpdateTarget::Version => {
                        lock.remove(name);
                        options.remote(maki_pack::manager::Refresh::Always)
                    }
                    // Restoring the recorded revision needs no fetch: either it
                    // is already materialized, or it has to be obtained, and
                    // `IfMissing` covers both.
                    UpdateTarget::Lockfile => {
                        options.remote(maki_pack::manager::Refresh::IfMissing)
                    }
                };
                match smol::block_on(manager.install(spec, &mut lock, refresh)) {
                    Ok(result) => {
                        if was_active {
                            if let Err(e) = host.unload(name) {
                                tracing::error!(package = %name, error = %e, "failed to unload");
                                report.failures.push(format!("{name}: {e}"));
                                match previous {
                                    Some(entry) => lock.restore(name, entry),
                                    None => lock.remove(name),
                                }
                                continue;
                            }
                            // Only reload what was running. Updating a package
                            // that was dormant, or waiting on a lazy trigger,
                            // must not be the thing that starts it.
                            active.remove(name);
                            match load_one(
                                host,
                                name,
                                &result.dir,
                                Origin::Fetched {
                                    src: spec.src.clone(),
                                },
                                config,
                            ) {
                                Ok(()) => {
                                    active.insert(name.clone());
                                }
                                // The old owner is already gone, so this is a
                                // package that is now installed and not
                                // running. Saying so beats reporting success.
                                Err(msg) => report
                                    .failures
                                    .push(format!("{name}: updated but failed to load: {msg}")),
                            }
                        }
                        changed |= result.changed;
                        report.updated.push((name.clone(), result.rev));
                    }
                    Err(e) => {
                        let msg = redact_error(&e);
                        tracing::error!(package = %name, error = %msg, "failed to update");
                        if let Some(entry) = previous {
                            lock.restore(name, entry);
                        }
                        report.failures.push(format!("{name}: {msg}"));
                    }
                }
            }
            PackOp::Activate { name } => {
                if !config.packages.iter().any(|package| package == name) {
                    report.failures.push(format!(
                        "{name}: package is disabled, so it was not activated"
                    ));
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
                // case needs no managed state at all.
                let found = packages
                    .iter()
                    .find(|package| package.name == *name)
                    .map(|package| (package.dir.clone(), package.origin.clone()))
                    .or_else(|| resolve_for_activation(&manager, &lock, &site, name));
                match found {
                    Some((dir, origin)) => match load_one(host, name, &dir, origin, config) {
                        Ok(()) => {
                            active.insert(name.clone());
                            report.activated.push(name.clone());
                        }
                        Err(message) => report
                            .failures
                            .push(format!("{name}: failed to activate: {message}")),
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

    if changed
        && let Some(path) = lock_path
        && !write_lockfile(&path, &lock)
    {
        // The packages moved on disk but nothing recorded where. The next
        // start reads the old lockfile and loads the old revisions, so these
        // updates did not stick and must not be counted as done.
        for (name, _) in report.updated.drain(..) {
            report.failures.push(format!(
                "{name}: the new revision could not be recorded, so the old one \
                comes back on the next start"
            ));
        }
        report
            .failures
            .push("packages changed but the lockfile could not be written".to_owned());
    }
    report
}

fn owner_conflict(name: &str) -> Option<String> {
    is_bundled(name)
        .then(|| format!("{name}: managed package name conflicts with a builtin plugin"))
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
        /// `++all`. Without a bang, active packages are kept.
        all: bool,
    },
}

impl PackCommand {
    pub fn parse(name: &str, args: &str) -> Result<Self, String> {
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
                other => names.push(other.to_owned()),
            }
        }

        match name {
            "/packupdate" => {
                if all {
                    return Err("/packupdate: ++all is not an option; omit the name instead".into());
                }
                Ok(PackCommand::Update { names, options })
            }
            "/packdel" => {
                if options != UpdateOptions::default() {
                    return Err("/packdel: ++offline and ++lockfile apply to /packupdate".into());
                }
                if all != names.is_empty() {
                    return Err("/packdel: name a package, or pass ++all".into());
                }
                Ok(PackCommand::Delete { names, all })
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
    bang: bool,
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
            let names = if names.is_empty() {
                known.iter().map(|n| (*n).to_owned()).collect()
            } else {
                resolve(names)?
            };
            Ok(names
                .into_iter()
                .map(|name| PackOp::Update {
                    name,
                    options: *options,
                })
                .collect())
        }
        PackCommand::Delete { names, all } => {
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
                    .filter(|name| bang || !active.contains(*name))
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
                    if active.contains(name) && !bang {
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
                .map(|name| PackOp::Delete { name })
                .collect())
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
pub fn drain_pack_ops(
    host: &crate::loader::PluginHost,
    declared: &[crate::api::pack::Declared],
    packages: &[DiscoveredPackage],
    config: &maki_config::PluginsConfig,
) -> PackReport {
    let ops = match host.take_pending_pack_ops() {
        Ok(ops) => ops,
        Err(e) => {
            tracing::error!(error = %e, "could not read pending package operations");
            return PackReport::default();
        }
    };
    apply_pack_ops(host, &ops, declared, packages, config)
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
        let src = lock.get(name).map(|e| e.src.clone()).unwrap_or_default();
        return Some((dir, Origin::Fetched { src }));
    }
    discover(site)
        .packages
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| (p.dir, p.origin))
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
    host.load_package(name, dir, effective_permissions(&package), opts)
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
/// Removal stops when this fails, so a deleted package never leaves a grant
/// that can silently return with a later install.
fn revoke_approval(name: &str) -> bool {
    let Some(path) = approvals_path() else {
        return true;
    };
    let Some(mut approvals) = read_approvals_file(&path) else {
        return false;
    };
    approvals.revoke(name);
    let text = if approvals.is_empty() {
        "{}".to_owned()
    } else {
        match serde_json::to_string_pretty(&approvals) {
            Ok(text) => text,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize pack approvals");
                return false;
            }
        }
    };
    match write_atomically(&path, &text) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "failed to write pack approvals");
            false
        }
    }
}

/// Writes through a temporary file and renames, so a crash mid-write cannot
/// truncate a lockfile that a user has committed.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.incoming");
    // Flushed before the rename. Without this the rename can reach the disk
    // first, so a crash leaves the real name pointing at a file whose contents
    // never got there, and the lockfile reads as truncated on the next start.
    let mut file = fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
}

/// What a discovery walk found, and what it had to refuse.
///
/// Problems are collected rather than returned as one error, because one
/// unusable package must not stop the others from loading.
#[derive(Debug, Default)]
pub struct Discovery {
    pub packages: Vec<DiscoveredPackage>,
    pub problems: Vec<PluginError>,
}

fn sorted_paths(dir: &Path, problems: &mut Vec<PluginError>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(source) => {
            problems.push(PluginError::Io {
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
            Err(source) => problems.push(PluginError::Io {
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
            return Discovery {
                packages: Vec::new(),
                problems: vec![PluginError::PackageSiteUnavailable { source }],
            };
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
    for group in sorted_paths(&site.join("pack"), &mut out.problems) {
        if group.file_name().and_then(|n| n.to_str()) == Some(MANAGED_GROUP) {
            continue;
        }
        for (sub, eager) in [("start", true), ("opt", false)] {
            for dir in sorted_paths(&group.join(sub), &mut out.problems) {
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
                        out.problems.push(PluginError::Io { path: dir, source });
                        continue;
                    }
                };
                if !root.is_dir() {
                    continue;
                }

                if is_bundled(&name) {
                    out.problems
                        .push(PluginError::PackageNameConflict { name, path: root });
                    continue;
                }
                if let Some(prev) = out.packages.iter().find(|p| p.name == name) {
                    out.problems.push(PluginError::DuplicatePackage {
                        name,
                        first: prev.dir.clone(),
                        second: root,
                    });
                    continue;
                }

                let requested = match load_requested_permissions(&root) {
                    Ok(requested) => requested,
                    Err(problem) => {
                        out.problems.push(problem);
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
    use super::*;
    use crate::plugin_permissions::Permission;

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

    fn declared_named(names: &[&str]) -> Vec<crate::api::pack::Declared> {
        names
            .iter()
            .map(|n| crate::api::pack::Declared {
                spec: maki_pack::Spec::new(format!("https://example.com/{n}")).with_name(*n),
                load: crate::api::pack::LoadMode::Eager,
                confirm: true,
            })
            .collect()
    }

    fn active_set(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn packupdate_flags_map_onto_the_same_options_lua_uses() {
        use crate::api::pack::{UpdateOptions, UpdateTarget};

        let cmd = PackCommand::parse("/packupdate", "++offline ++lockfile").unwrap();
        assert_eq!(
            cmd,
            PackCommand::Update {
                names: vec![],
                options: UpdateOptions {
                    target: UpdateTarget::Lockfile,
                    offline: true,
                },
            }
        );
    }

    /// A mistyped flag must not become a package name, or the error would name
    /// the wrong problem.
    #[test]
    fn an_unknown_flag_is_refused_rather_than_read_as_a_name() {
        let err = PackCommand::parse("/packupdate", "++ofline")
            .expect_err("a misspelled flag is not a package");
        assert!(err.contains("++ofline"), "{err}");
    }

    /// No name means every declared package, which is what the command's own
    /// help says and what `:packupdate` does.
    #[test]
    fn packupdate_without_a_name_plans_every_declared_package() {
        let declared = declared_named(&["alpha", "beta"]);
        let cmd = PackCommand::parse("/packupdate", "").unwrap();
        let ops = plan_command(&cmd, false, &declared, &[], &active_set(&[])).unwrap();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn packupdate_refuses_a_package_that_was_never_declared() {
        let declared = declared_named(&["alpha"]);
        let cmd = PackCommand::parse("/packupdate", "ghost").unwrap();
        let err = plan_command(&cmd, false, &declared, &[], &active_set(&[]))
            .expect_err("an undeclared package cannot be updated");
        assert!(err.contains("ghost"), "{err}");
    }

    /// `++all` without the bang removes only what is not running, and with the
    /// bang removes everything. This is the confirmation the bang stands in
    /// for: the destructive half has to be asked for explicitly.
    #[test]
    fn packdel_all_skips_loaded_packages_unless_the_bang_is_given() {
        let declared = Vec::new();
        let installed = vec!["alpha".to_owned(), "beta".to_owned()];
        let active = active_set(&["alpha"]);
        let cmd = PackCommand::parse("/packdel", "++all").unwrap();

        let ops = plan_command(&cmd, false, &declared, &installed, &active).unwrap();
        assert_eq!(
            ops,
            vec![crate::api::pack::PackOp::Delete {
                name: "beta".to_owned()
            }],
            "without the bang, the loaded package stays"
        );

        let forced = plan_command(&cmd, true, &declared, &installed, &active).unwrap();
        assert_eq!(forced.len(), 2, "the bang removes the loaded one too");
    }

    /// Deleting a package is how you get rid of one you no longer declare, so
    /// it has to work from the installed set alone. Asking the declarations
    /// would make the ordinary "remove the line, then remove the files"
    /// sequence impossible, and would break the command under `--no-plugins`,
    /// where nothing is declared at all.
    #[test]
    fn packdel_removes_an_installed_package_that_is_no_longer_declared() {
        let installed = vec!["orphan".to_owned()];
        let cmd = PackCommand::parse("/packdel", "orphan").unwrap();

        let ops = plan_command(&cmd, false, &[], &installed, &active_set(&[]))
            .expect("an installed package can be removed without a declaration");
        assert_eq!(
            ops,
            vec![crate::api::pack::PackOp::Delete {
                name: "orphan".to_owned()
            }]
        );
    }

    #[test]
    fn packdel_refuses_a_package_that_startup_would_reinstall() {
        let declared = declared_named(&["alpha"]);
        let installed = vec!["alpha".to_owned()];
        let cmd = PackCommand::parse("/packdel", "alpha").unwrap();

        let error = plan_command(&cmd, true, &declared, &installed, &active_set(&[]))
            .expect_err("a declared package would be reinstalled during the reload");

        assert!(error.contains("remove it from maki.pack.add"), "{error}");
    }

    #[test]
    fn packdel_requires_bang_for_an_active_named_package() {
        let installed = vec!["alpha".to_owned()];
        let active = active_set(&["alpha"]);
        let cmd = PackCommand::parse("/packdel", "alpha").unwrap();

        assert!(plan_command(&cmd, false, &[], &installed, &active).is_err());
        assert_eq!(
            plan_command(&cmd, true, &[], &installed, &active).unwrap(),
            vec![crate::api::pack::PackOp::Delete {
                name: "alpha".to_owned()
            }]
        );
    }

    #[test]
    fn packdel_refuses_a_package_that_is_not_installed() {
        let cmd = PackCommand::parse("/packdel", "ghost").unwrap();
        let err = plan_command(&cmd, false, &[], &[], &active_set(&[]))
            .expect_err("nothing to remove is an error, not a silent success");
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn packdel_needs_either_a_name_or_all_but_not_both() {
        assert!(PackCommand::parse("/packdel", "").is_err());
        assert!(PackCommand::parse("/packdel", "++all alpha").is_err());
        assert!(PackCommand::parse("/packdel", "alpha").is_ok());
    }

    /// The update flags mean nothing to a delete, so accepting them silently
    /// would let a user believe an offline delete did something different.
    #[test]
    fn packdel_refuses_the_update_flags() {
        assert!(PackCommand::parse("/packdel", "++all ++offline").is_err());
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
            [PluginError::Io { .. }]
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
            [PluginError::PackageNameConflict { .. }]
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
            [PluginError::PackageNameConflict { .. }]
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
            [PluginError::DuplicatePackage { .. }]
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
