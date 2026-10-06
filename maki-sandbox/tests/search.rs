#![cfg(all(feature = "sandbox", target_os = "linux"))]

use maki_fs::FsBackend;
use maki_fs::GrepParams;
use maki_sandbox::fs_backend::SandboxFs;

mod common;

/// The child answers `glob`/`grep` itself, straight from `maki_fs::search`.
///
/// Needs `/proc/sys/kernel/unprivileged_userns_clone=1` on most distros, plus a
/// mount namespace. When AppArmor restricts unprivileged user namespaces the
/// `maki-sandbox` profile must be loaded too (see `maki_sandbox::apparmor`).
/// [`common::sandbox_for`] skips the test where the host gives us none of it.
#[test]
fn sandbox_glob_and_grep_round_trip_to_host_paths() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path();

    std::fs::write(root.join("a.rs"), b"fn alpha() {}\nlet beta = 1;\n").unwrap();
    std::fs::write(root.join("b.py"), b"gamma = 2\n").unwrap();

    let Some(sandbox) = common::sandbox_for(root) else {
        return;
    };
    let fs = SandboxFs::new(sandbox);

    let matches = fs
        .glob(root, &["*.rs".into()], true, false, None)
        .expect("glob should succeed");
    assert_eq!(
        matches,
        vec![root.join("a.rs")],
        "the sandboxed glob filters the same way the host one does"
    );

    let mut params = GrepParams::new("beta".into());
    params.path = Some(root.to_string_lossy().into_owned());
    let entries = fs.grep(params).expect("grep should succeed");
    assert_eq!(
        entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec![root.join("a.rs").to_string_lossy()],
        "only a.rs holds the pattern"
    );
    assert_eq!(entries[0].groups[0].lines[0].text, "let beta = 1;");
}

/// A sandbox that constructed successfully must actually be isolated.
///
/// `SandboxFs` maps host paths into the bind-mounted workspace, so an
/// unisolated child would silently answer every lookup against a filesystem
/// that does not contain them. Guarding the contract here keeps that from
/// coming back as a mode.
#[test]
fn a_constructed_sandbox_is_mount_isolated() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path();

    let Some(sandbox) = common::sandbox_for(root) else {
        return;
    };

    let pwd = sandbox.pwd().expect("pwd should succeed");
    let host_root = root.to_string_lossy();
    assert_ne!(
        pwd.trim_end_matches('/'),
        host_root.trim_end_matches('/'),
        "a live sandbox is chdir'd into the bind-mounted workspace, not the host path"
    );
    assert!(
        pwd.starts_with(SANDBOX_WORKSPACE_ROOT),
        "sandbox pwd should live under the sandbox root, got {pwd}"
    );
}

const SANDBOX_WORKSPACE_ROOT: &str = "/home/maki/workspace/";
