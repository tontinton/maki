#![cfg(all(feature = "sandbox", target_os = "linux"))]

use std::path::Path;

mod common;

const EXEC_FAILED: &str = "exec failed";

/// Test that a sandbox browser can execute shell commands via Exec IPC.
///
/// Needs `/proc/sys/kernel/unprivileged_userns_clone=1` on most distros, or
/// `kernel.apparmor_restrict_unprivileged_userns=0` on Ubuntu, plus a mount
/// namespace. [`common::sandbox_for`] skips the test where the host gives us
/// neither.
#[test]
fn sandbox_shell_exec() {
    let dir = tempfile::TempDir::new().expect("temp dir");

    let Some(sandbox) = common::sandbox_for(Path::new(dir.path())) else {
        return;
    };

    let pwd = sandbox.pwd().expect("pwd should succeed");
    assert!(!pwd.is_empty(), "pwd should not be empty");

    let entries = sandbox.ls("/usr/bin").expect("ls /usr/bin should succeed");
    assert!(
        !entries.is_empty(),
        "/usr/bin should have entries in sandbox"
    );

    let (output, exit_code) = sandbox.exec("echo hello", None, None).expect(EXEC_FAILED);
    assert_eq!(exit_code, 0, "echo should succeed");
    assert_eq!(output.trim(), "hello", "echo should output 'hello'");

    let (output, exit_code) = sandbox.exec("echo $PATH", None, None).expect(EXEC_FAILED);
    assert_eq!(exit_code, 0, "echo $PATH should succeed");
    assert!(
        output.contains("/usr/bin"),
        "PATH should contain /usr/bin, got: {output}"
    );

    // Regression: opening /dev/null used to fail with EACCES because devices
    // were bound over placeholders on a tmpfs mounted inside the user ns.
    let (output, exit_code) = sandbox
        .exec("echo visible 2>/dev/null", None, None)
        .expect(EXEC_FAILED);
    assert_eq!(exit_code, 0, "redirect to /dev/null should succeed");
    assert_eq!(
        output.trim(),
        "visible",
        "stdout must survive a stderr redirect to /dev/null"
    );
    // sandbox dropped here — Drop sends Exit and waits for child
}
