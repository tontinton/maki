//! Discovery of external packages installed under the site directory.
//!
//! This is the manual half of the package model: directories a user cloned
//! themselves, laid out the way Neovim lays packages out. Packages that maki
//! installs are resolved from recorded state instead, and never appear here.

use std::fs;
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
pub const MANAGED_GROUP: &str = "core";

/// `<data>/site`, the root Neovim would call a package path.
pub fn site_dir() -> Result<PathBuf, std::io::Error> {
    maki_storage::paths::data_dir().map(|d| d.join("site"))
}

#[derive(Debug, Clone)]
pub struct DiscoveredPackage {
    pub name: String,
    /// Canonical package root. Resolved once, here, so the manifest, the
    /// entrypoints, and every later `require` agree on one directory.
    pub dir: PathBuf,
    /// `start/` packages load at startup; `opt/` packages wait to be activated.
    pub eager: bool,
    /// What the package's manifest asks for. A manually installed package is
    /// granted this directly: the user placed the files.
    pub requested: Requested,
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

    fn make_package(site: &Path, group: &str, sub: &str, name: &str) -> PathBuf {
        let dir = site.join("pack").join(group).join(sub).join(name);
        fs::create_dir_all(dir.join("plugin")).unwrap();
        fs::write(dir.join("plugin").join("init.lua"), "").unwrap();
        dir
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
