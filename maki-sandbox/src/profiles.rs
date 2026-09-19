use std::path::{Path, PathBuf};

use crate::namespace::NamespaceConfig;

/// How a profile directory is mounted inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountUsage {
    /// Read-write access to the directory.
    Write,
    /// Read-only access to the directory.
    ReadOnly,
    /// Add the directory to $PATH.
    OnlyPath,
    /// Recreate the host symlink inside the sandbox (e.g. /etc/localtime).
    SymLink,
}

impl MountUsage {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Write => "rw",
            Self::ReadOnly => "ro",
            Self::OnlyPath => "on $PATH",
            Self::SymLink => "symlink",
        }
    }
}

/// A single directory mount within a profile.
#[derive(Debug, Clone)]
pub struct ProfileMount {
    pub path: String,
    pub usage: MountUsage,
}

impl ProfileMount {
    #[must_use]
    pub fn rw(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::Write,
        }
    }
    #[must_use]
    pub fn only_path(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::OnlyPath,
        }
    }
    #[must_use]
    pub fn read_only(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::ReadOnly,
        }
    }

    #[must_use]
    pub fn sym_link(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::SymLink,
        }
    }

    /// Resolve tilde path to an absolute host path.
    #[must_use]
    pub fn resolved_host_path(&self) -> PathBuf {
        match std::env::var("HOME") {
            Ok(home) => self.resolved_host_path_under(Path::new(&home)),
            Err(_) => PathBuf::from(&self.path),
        }
    }

    /// Resolve tilde path against an explicit home directory.
    pub(crate) fn resolved_host_path_under(&self, home: &Path) -> PathBuf {
        if let Some(rest) = self.path.strip_prefix("~/") {
            return home.join(rest);
        }
        PathBuf::from(&self.path)
    }

    /// Map tilde path to sandbox-internal path (`~/.cargo` → `/home/maki/.cargo`).
    #[must_use]
    pub fn sandbox_internal_path(&self) -> String {
        if let Some(rest) = self.path.strip_prefix("~/") {
            return format!("/home/maki/{rest}");
        }
        self.path.clone()
    }

    /// Derive directory name by stripping `~/` prefix (`~/.cargo` → `.cargo`).
    #[must_use]
    pub fn dir_name(&self) -> String {
        if let Some(rest) = self.path.strip_prefix("~/") {
            return rest.to_string();
        }
        self.path.clone()
    }
}

/// A named collection of directory mounts that can be toggled on/off.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    pub name: String,
    pub mounts: Vec<ProfileMount>,
}

/// Returns the built-in profiles.
#[must_use]
pub fn builtin_profiles() -> Vec<SandboxProfile> {
    vec![
        SandboxProfile {
            name: "rust".into(),
            mounts: vec![
                ProfileMount::rw("~/.cargo"),
                ProfileMount::only_path("~/.cargo/bin"),
                ProfileMount::read_only("~/.rustup"),
                // cargo shells out to `cc` for C builds. On Debian/Ubuntu
                // /usr/bin/cc is a symlink through /etc/alternatives, which
                // dangles inside the sandbox because /etc is a fresh tmpfs.
                // Recreating the link lets it resolve against the bound /usr.
                ProfileMount::sym_link("/etc/alternatives/cc"),
            ],
        },
        SandboxProfile {
            name: "java".into(),
            mounts: vec![ProfileMount::rw("~/.m2"), ProfileMount::rw("~/.gradle")],
        },
        SandboxProfile {
            name: "node".into(),
            mounts: vec![
                ProfileMount::rw("~/.npm"),
                ProfileMount::rw("~/.yarn"),
                ProfileMount::only_path("~/.npm/bin"),
            ],
        },
        SandboxProfile {
            name: "go".into(),
            mounts: vec![
                ProfileMount::rw("~/go"),
                ProfileMount::only_path("~/go/bin"),
            ],
        },
        // Custom Maki plugins: the active config directory (XDG layout), or
        // the legacy plugins subdir for `~/.maki` setups. Read-only: plugins
        // are code to load, not data to mutate. Missing entries are pruned
        // before spawn.
        SandboxProfile {
            name: "plugins".into(),
            mounts: vec![
                ProfileMount::read_only("~/.config/maki"),
                ProfileMount::read_only("~/.maki/plugins"),
            ],
        },
    ]
}

/// The built-in profiles whose names appear in `names`, in built-in order.
/// Unknown names are ignored.
#[must_use]
pub fn select_profiles(names: &[String]) -> Vec<SandboxProfile> {
    builtin_profiles()
        .into_iter()
        .filter(|p| names.iter().any(|n| n == &p.name))
        .collect()
}

/// Flattened profile mounts ready to merge into a [`NamespaceConfig`].
pub(crate) struct FlatMounts {
    pub home: Vec<(PathBuf, String)>,
    pub readonly: Vec<(PathBuf, String)>,
    pub path_dirs: Vec<String>,
    pub symlinks: Vec<(PathBuf, String)>,
}

impl FlatMounts {
    pub(crate) fn from_profiles(enabled: &[SandboxProfile]) -> Self {
        match std::env::var("HOME") {
            Ok(home) => Self::from_profiles_under(enabled, Path::new(&home)),
            Err(_) => Self::from_profiles_under(enabled, Path::new("/")),
        }
    }

    pub(crate) fn from_profiles_under(enabled: &[SandboxProfile], home: &Path) -> Self {
        let mut out = Self {
            home: Vec::new(),
            readonly: Vec::new(),
            path_dirs: Vec::new(),
            symlinks: Vec::new(),
        };
        for profile in enabled {
            for mount in &profile.mounts {
                let path = mount.resolved_host_path_under(home);
                let name = mount.dir_name();
                match mount.usage {
                    MountUsage::Write => out.home.push((path, name)),
                    MountUsage::ReadOnly => out.readonly.push((path, name)),
                    MountUsage::OnlyPath => out.path_dirs.push(mount.sandbox_internal_path()),
                    MountUsage::SymLink => out.symlinks.push((path, name)),
                }
            }
        }
        out
    }
}

/// Build a [`NamespaceConfig`] from profiles.
///
/// Each profile contributes mounts and PATH entries. `extra_home_mounts`
/// are additional host paths to bind-mount (e.g. from the UI info struct).
#[must_use]
pub fn build_namespace_config(
    profiles: &[SandboxProfile],
    workspace_dir: PathBuf,
    workspace_name: String,
    extra_home_mounts: Vec<(PathBuf, String)>,
    extra_workspace_dirs: Vec<(PathBuf, String)>,
) -> NamespaceConfig {
    let flat = FlatMounts::from_profiles(profiles);
    let mut home_mounts = extra_home_mounts;
    home_mounts.extend(flat.home);
    NamespaceConfig::new(
        vec![],
        vec![],
        workspace_dir,
        workspace_name,
        home_mounts,
        flat.readonly,
        flat.path_dirs,
        extra_workspace_dirs,
        flat.symlinks,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SANDBOX_HOME: &str = "/home/maki";

    #[test_case("~/.cargo", ".cargo" ; "tilde cargo")]
    #[test_case("~/go", "go" ; "tilde go")]
    #[test_case("~/.npm/bin", ".npm/bin" ; "tilde npm bin")]
    fn dir_name_strips_tilde_prefix(input: &str, expected: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.dir_name(), expected);
    }

    #[test_case("/usr/local" ; "absolute path")]
    #[test_case("relative/path" ; "relative path")]
    fn dir_name_passthrough_without_tilde(input: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.dir_name(), input);
    }

    #[test_case("~/.cargo", "/home/maki/.cargo" ; "tilde cargo")]
    #[test_case("~/go/bin", "/home/maki/go/bin" ; "tilde go bin")]
    #[test_case("~/.npm/bin", "/home/maki/.npm/bin" ; "tilde npm bin")]
    fn sandbox_internal_path_maps_tilde(input: &str, expected: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.sandbox_internal_path(), expected);
    }

    #[test_case("/usr/local" ; "absolute")]
    #[test_case("relative" ; "relative")]
    fn sandbox_internal_path_passthrough_without_tilde(input: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.sandbox_internal_path(), input);
    }

    #[test]
    fn resolved_host_path_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        let home = PathBuf::from(home);
        assert_eq!(
            ProfileMount::rw("~/.cargo").resolved_host_path(),
            home.join(".cargo")
        );
        assert_eq!(
            ProfileMount::rw("~/go/bin").resolved_host_path(),
            home.join("go/bin")
        );
    }

    #[test]
    fn resolved_host_path_passthrough_without_tilde() {
        assert_eq!(
            ProfileMount::rw("/usr/local").resolved_host_path(),
            PathBuf::from("/usr/local")
        );
        assert_eq!(
            ProfileMount::rw("relative").resolved_host_path(),
            PathBuf::from("relative")
        );
    }

    #[test]
    fn builtin_profiles_have_expected_names() {
        let profiles = builtin_profiles();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["rust", "java", "node", "go", "plugins"]);
    }

    #[test]
    fn select_profiles_keeps_builtin_order_and_ignores_unknown() {
        let selected = select_profiles(&["go".into(), "nope".into(), "rust".into(), "rust".into()]);
        let names: Vec<&str> = selected.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["rust", "go"],
            "built-in order wins, unknowns and duplicates dropped"
        );
    }

    #[test]
    fn plugins_profile_mounts_config_dirs_read_only() {
        let plugins = builtin_profiles()
            .into_iter()
            .find(|p| p.name == "plugins")
            .unwrap();
        assert_eq!(plugins.mounts[0].path, "~/.config/maki");
        assert_eq!(plugins.mounts[0].usage, MountUsage::ReadOnly);
        assert_eq!(plugins.mounts[1].path, "~/.maki/plugins");
        assert_eq!(plugins.mounts[1].usage, MountUsage::ReadOnly);
    }

    #[test]
    fn builtin_profiles_rust_has_cargo_mounts() {
        let profiles = builtin_profiles();
        let rust = profiles.into_iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(rust.mounts.len(), 4);
        assert_eq!(rust.mounts[0].path, "~/.cargo");
        assert_eq!(rust.mounts[0].usage, MountUsage::Write);
        assert_eq!(rust.mounts[1].path, "~/.cargo/bin");
        assert_eq!(rust.mounts[1].usage, MountUsage::OnlyPath);
        assert_eq!(rust.mounts[2].path, "~/.rustup");
        assert_eq!(rust.mounts[2].usage, MountUsage::ReadOnly);
        assert_eq!(rust.mounts[3].path, "/etc/alternatives/cc");
        assert_eq!(rust.mounts[3].usage, MountUsage::SymLink);
    }

    #[test]
    fn rust_profile_symlinks_cc_into_etc_alternatives() {
        let rust: Vec<SandboxProfile> = builtin_profiles()
            .into_iter()
            .filter(|p| p.name == "rust")
            .collect();
        let config = build_namespace_config(&rust, "/ws".into(), "ws".into(), vec![], vec![]);
        // Non-tilde path passes through unchanged: same location in the sandbox.
        assert!(config.symlinks.iter().any(|(host, sandbox)| host
            == &PathBuf::from("/etc/alternatives/cc")
            && sandbox == "/etc/alternatives/cc"));
    }

    #[test]
    fn mount_usage_label() {
        assert_eq!(MountUsage::Write.label(), "rw");
        assert_eq!(MountUsage::ReadOnly.label(), "ro");
        assert_eq!(MountUsage::OnlyPath.label(), "on $PATH");
    }

    #[test]
    fn build_namespace_config_enabled_profiles() {
        let profiles = builtin_profiles();
        let rust: Vec<SandboxProfile> = profiles.into_iter().filter(|p| p.name == "rust").collect();
        let config =
            build_namespace_config(&rust, "/workspace".into(), "test".into(), vec![], vec![]);
        assert_eq!(config.home_mounts.len(), 1);
        assert_eq!(config.home_mounts[0].1, ".cargo");
        assert_eq!(config.readonly_mounts.len(), 1);
        assert_eq!(config.readonly_mounts[0].1, ".rustup");
        assert_eq!(config.path_dirs.len(), 1);
        assert_eq!(config.path_dirs[0], format!("{SANDBOX_HOME}/.cargo/bin"));
        assert_eq!(config.workspace_dir, PathBuf::from("/workspace"));
    }

    #[test]
    fn build_namespace_config_multiple_profiles() {
        let profiles = builtin_profiles();
        let selected: Vec<SandboxProfile> = profiles
            .into_iter()
            .filter(|p| p.name == "rust" || p.name == "node")
            .collect();
        let config = build_namespace_config(&selected, "/ws".into(), "ws".into(), vec![], vec![]);
        // Rust: ~/.cargo (Write), ~/.cargo/bin (OnlyPath), ~/.rustup (ReadOnly)
        // Node: ~/.npm (Write), ~/.yarn (Write), ~/.npm/bin (OnlyPath)
        assert_eq!(config.home_mounts.len(), 3); // .cargo, .npm, .yarn
        assert_eq!(config.readonly_mounts.len(), 1); // .rustup
        assert_eq!(config.path_dirs.len(), 2); // .cargo/bin, .npm/bin
    }

    #[test]
    fn build_namespace_config_with_extra_mounts() {
        let config = build_namespace_config(
            &[],
            "/ws".into(),
            "ws".into(),
            vec![(PathBuf::from("/host/dir"), "dir".into())],
            vec![],
        );
        assert_eq!(config.home_mounts.len(), 1);
        assert_eq!(config.home_mounts[0].0, PathBuf::from("/host/dir"));
        assert_eq!(config.home_mounts[0].1, "dir");
    }
}
