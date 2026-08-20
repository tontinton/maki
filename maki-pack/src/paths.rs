//! Where managed packages live on disk.
//!
//! Every revision gets its own directory and is never rewritten, so a session
//! that resolved one revision keeps reading it even while an update installs
//! another. There is no symlink to swap, which also means no Windows junction
//! or privilege question: the layout is identical on every platform.

use std::path::{Path, PathBuf};

/// Group name reserved for packages maki installs. Manual discovery skips it,
/// so a managed package is never also found by walking the tree.
pub const MANAGED_GROUP: &str = "core";

/// `<site>/pack/core/<name>`, holding one directory per installed revision.
pub fn package_root(site: &Path, name: &str) -> PathBuf {
    site.join("pack").join(MANAGED_GROUP).join(name)
}

/// `<site>/pack/core/<name>/<sha>`, the directory a session actually loads.
pub fn revision_dir(site: &Path, name: &str, sha: &str) -> PathBuf {
    package_root(site, name).join(sha)
}

/// An empty directory pointed at by `core.hooksPath`, so no repository hook can
/// run during a clone, a fetch, or a checkout.
pub fn empty_hooks_dir(site: &Path) -> PathBuf {
    site.join(".nohooks")
}

/// Lock covering one package's checkouts.
pub fn package_lock(site: &Path, name: &str) -> PathBuf {
    site.join("pack")
        .join(MANAGED_GROUP)
        .join(format!("{name}.lock"))
}

/// Sidecar lock for a shared file, held across its whole read-modify-write.
pub fn sidecar_lock(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    file.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> PathBuf {
        PathBuf::from("/data/site")
    }

    #[test]
    fn revisions_are_siblings_under_one_package_root() {
        let a = revision_dir(&site(), "demo", "aaa");
        let b = revision_dir(&site(), "demo", "bbb");
        assert_eq!(a.parent(), b.parent());
        assert_eq!(a.parent().unwrap(), package_root(&site(), "demo"));
        assert_ne!(a, b, "two revisions must not share a directory");
    }

    /// Managed packages live under the reserved group, which manual discovery
    /// skips. If this ever changed, one package could get two identities.
    #[test]
    fn managed_packages_live_under_the_reserved_group() {
        let root = package_root(&site(), "demo");
        assert!(root.starts_with(site().join("pack").join(MANAGED_GROUP)));
    }

    #[test]
    fn sidecar_lock_sits_beside_its_file() {
        let lock = sidecar_lock(Path::new("/cfg/pack-lock.json"));
        assert_eq!(lock, PathBuf::from("/cfg/pack-lock.json.lock"));
    }

    #[test]
    fn package_lock_is_not_inside_the_package_root() {
        let name = "demo";
        let lock = package_lock(&site(), name);
        assert!(
            !lock.starts_with(package_root(&site(), name)),
            "a lock inside the directory it guards would be removed with it"
        );
    }
}
