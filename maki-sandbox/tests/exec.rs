#![cfg(all(feature = "sandbox", target_os = "linux"))]

use std::fs::{create_dir_all, read_dir, read_to_string, write};
use std::path::Path;

mod common;

const EXEC_FAILED: &str = "exec failed";
const SECRET: &str = "top secret";
const MASKED_DIR: &str = ".maki";
const HOST_PROC: &str = "/proc";

/// The PIDs a process belongs to, from the procfs mount's namespace inward.
fn nspids(status: &str) -> Option<Vec<u32>> {
    let line = status.lines().find(|line| line.starts_with("NSpid:"))?;
    line["NSpid:".len()..]
        .split_whitespace()
        .map(|pid| pid.parse().ok())
        .collect()
}

/// The host pid of the sandbox's PID 1: the only child of the supervisor
/// whose last `NSpid` entry is 1.
fn pid_namespace_init(supervisor: u32) -> Option<u32> {
    for entry in read_dir(HOST_PROC).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(status) = read_to_string(format!("{HOST_PROC}/{pid}/status")) else {
            continue;
        };
        let ppid = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        });
        if ppid == Some(supervisor) && nspids(&status).is_some_and(|pids| pids.last() == Some(&1)) {
            return Some(pid);
        }
    }
    None
}

#[test]
fn sandbox_hides_host_processes_and_masked_paths() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let masked = dir.path().join(MASKED_DIR);
    create_dir_all(&masked).expect("create masked dir");
    write(masked.join("secret"), SECRET).expect("write secret");
    write(dir.path().join("visible.txt"), "visible").expect("write visible file");

    let Some(sandbox) = common::sandbox_for(dir.path()) else {
        return;
    };

    let supervisor = sandbox.pid().expect("sandbox pid").as_raw().unsigned_abs();
    assert!(
        pid_namespace_init(supervisor).is_some(),
        "sandbox must run its own pid namespace"
    );

    let (status, exit_code) = sandbox
        .exec("cat /proc/self/status", None, None)
        .expect(EXEC_FAILED);
    assert_eq!(exit_code, 0);
    assert_eq!(
        nspids(&status).expect("NSpid in /proc/self/status").len(),
        1,
        "sandbox /proc must belong to its own pid namespace, got: {status}"
    );

    let (output, exit_code) = sandbox
        .exec("cat .maki/secret", None, None)
        .expect(EXEC_FAILED);
    assert_ne!(exit_code, 0, "masked file must not be readable");
    assert!(!output.contains(SECRET), "masked file leaked: {output}");

    let (output, exit_code) = sandbox.exec("ls -A .maki", None, None).expect(EXEC_FAILED);
    assert_eq!(exit_code, 0);
    assert!(
        output.trim().is_empty(),
        "masked dir must be empty, got: {output}"
    );

    let (output, exit_code) = sandbox
        .exec("cat visible.txt", None, None)
        .expect(EXEC_FAILED);
    assert_eq!(exit_code, 0);
    assert_eq!(output.trim(), "visible");
}

/// A workspace without `.maki` still gets a mask. Otherwise sandboxed code
/// could create the folder and plant config the host reads on a later start.
#[test]
fn sandbox_cannot_create_the_masked_project_dir() {
    let dir = tempfile::TempDir::new().expect("temp dir");

    let Some(sandbox) = common::sandbox_for(dir.path()) else {
        return;
    };

    let (output, exit_code) = sandbox
        .exec(
            "mkdir -p .maki && echo planted > .maki/init.lua",
            None,
            None,
        )
        .expect(EXEC_FAILED);
    assert_eq!(exit_code, 0, "{output}");

    let masked = dir.path().join(MASKED_DIR);
    assert!(masked.is_dir(), "setup must create the mask mountpoint");
    assert_eq!(
        read_dir(&masked).expect("read masked dir").count(),
        0,
        "writes to the mask must not reach the host"
    );
}

/// Test that a sandbox browser can execute shell commands via Exec IPC.
///
/// Needs `/proc/sys/kernel/unprivileged_userns_clone=1` on most distros, plus a
/// mount namespace. When AppArmor restricts unprivileged user namespaces the
/// `maki-sandbox` profile must be loaded too (see `maki_sandbox::apparmor`).
/// [`common::sandbox_for`] skips the test where the host gives us none of it.
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
