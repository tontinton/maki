#![cfg(all(feature = "sandbox", target_os = "linux"))]

use maki_sandbox::namespace::NamespaceConfig;
use std::path::PathBuf;

/// Test that a sandbox browser can be spawned, list files, and navigate
/// directories. This requires user namespace support (`CLONE_NEWUSER`).
///
/// Needs `/proc/sys/kernel/unprivileged_userns_clone=1` on most distros,
/// or `kernel.apparmor_restrict_unprivileged_userns=0` on Ubuntu.
#[test]
fn sandbox_browser_listing() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path();

    std::fs::write(root.join("hello.txt"), b"hello").unwrap();
    std::fs::write(root.join("world.txt"), b"world").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("nested.txt"), b"nested").unwrap();
    std::fs::write(root.join(".hidden"), b"hidden").unwrap();

    let config = NamespaceConfig::new(
        vec![],
        vec![],
        PathBuf::from(root),
        "test".into(),
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
    );

    let sandbox = maki_sandbox::Sandbox::new(config)
        .expect("Sandbox::new should succeed (may need user namespace support)");

    let pwd = sandbox.pwd().expect("pwd should succeed");
    assert!(
        pwd.ends_with("test") || pwd == root.to_string_lossy().as_ref(),
        "pwd should point to workspace: got {pwd}"
    );

    let entries = sandbox.ls(&pwd).expect("ls root");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "should list hello.txt");
    assert!(names.contains(&"world.txt"), "should list world.txt");
    assert!(names.contains(&"sub"), "should list sub dir");
    assert!(names.contains(&".hidden"), "should list hidden files");

    let sub_path = format!("{}/sub", pwd.trim_end_matches('/'));
    sandbox.cd(&sub_path).expect("cd sub");

    let entries = sandbox.ls(&sub_path).expect("ls sub");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"nested.txt"), "should list nested.txt");
    assert!(
        !names.contains(&"hello.txt"),
        "should not list parent files"
    );
    // sandbox dropped here — Drop sends Exit and waits for child
}
