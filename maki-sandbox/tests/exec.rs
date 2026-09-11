#![cfg(all(feature = "sandbox", target_os = "linux"))]

use maki_sandbox::namespace::NamespaceConfig;
use std::path::PathBuf;

const EXEC_FAILED: &str = "exec failed";

/// Test that a sandbox browser can execute shell commands via Exec IPC.
/// This requires user namespace support (`CLONE_NEWUSER`).
#[test]
fn sandbox_shell_exec() {
    let dir = tempfile::TempDir::new().expect("temp dir");

    let config = NamespaceConfig::new(
        vec![],
        vec![],
        PathBuf::from(dir.path()),
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
    assert!(!pwd.is_empty(), "pwd should not be empty");

    let entries = sandbox.ls("/usr/bin").expect("ls /usr/bin should succeed");
    assert!(
        !entries.is_empty(),
        "/usr/bin should have entries in sandbox"
    );

    let (output, is_error) = sandbox.exec("echo hello").expect(EXEC_FAILED);
    assert!(!is_error, "echo should succeed");
    assert_eq!(output.trim(), "hello", "echo should output 'hello'");

    let (output, is_error) = sandbox.exec("echo $PATH").expect(EXEC_FAILED);
    assert!(!is_error, "echo $PATH should succeed");
    assert!(
        output.contains("/usr/bin"),
        "PATH should contain /usr/bin, got: {output}"
    );

    // Regression: opening /dev/null used to fail with EACCES because devices
    // were bound over placeholders on a tmpfs mounted inside the user ns.
    let (output, is_error) = sandbox.exec("echo visible 2>/dev/null").expect(EXEC_FAILED);
    assert!(!is_error, "redirect to /dev/null should succeed");
    assert_eq!(
        output.trim(),
        "visible",
        "stdout must survive a stderr redirect to /dev/null"
    );
    // sandbox dropped here — Drop sends Exit and waits for child
}
