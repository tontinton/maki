//! Regression test for tontinton/maki#909.
//!
//! Re-exec this test binary as a child; the child spawns /bin/sleep via the
//! posix_spawn fast path (process_group(0), resolved program path) while
//! MallocStackLogging=1 is in ITS environment. On the fork+exec path libmalloc's
//! atfork child handler would print "MallocStackLogging: turning off stack
//! logging"; on the posix_spawn path it must not.

#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn read_to_end_capped<R: std::io::Read + Send + 'static>(
    mut rx: R,
) -> std::io::Result<String> {
    let (tx, rrx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = String::new();
        let _ = rx.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    match rrx.recv_timeout(Duration::from_secs(30)) {
        Ok(s) => Ok(s),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "read timed out",
        )),
    }
}

#[test]
fn spawn_does_not_trigger_msl_atfork_warning() {
    if std::env::var("MAKI_MSL_FORK_CHILD").as_deref() == Ok("1") {
        child_side();
        return;
    }
    parent_side();
}

fn parent_side() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(&exe);
    cmd.arg("--exact").arg("spawn_does_not_trigger_msl_atfork_warning").arg("--nocapture");
    cmd.env("MAKI_MSL_FORK_CHILD", "1");
    cmd.env("MallocStackLogging", "1");
    cmd.env("MallocDebugReport", "stderr");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    let mut child = cmd.spawn().expect("re-exec self");
    let so = child.stdout.take().unwrap();
    let se = child.stderr.take().unwrap();
    let (out_rx, err_rx) = (read_to_end_capped(so), read_to_end_capped(se));
    let status = child.wait().expect("wait");
    let out = out_rx.unwrap_or_default();
    let err = err_rx.unwrap_or_default();
    assert!(status.success(), "child failed: {status}\nstdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("GRANDCHILD_STDERR_CLEAN"),
        "missing sentinel; stdout:\n{out}\nstderr:\n{err}"
    );
    let combined = format!("{out}{err}");
    // child's own MSL init banner ("recording ... lite mode") is expected;
    // the atfork "turning off stack logging" warning is the regression signal
    assert!(
        !combined.contains("turning off stack logging") && !combined.contains("had been recording"),
        "MSL atfork warning leaked; combined:\n{combined}"
    );
}

fn child_side() {
    let mut cmd = Command::new(maki_agent::child_env::resolve_program("sleep", &{
        let mut m = std::collections::HashMap::new();
        if let Some(p) = std::env::var_os("PATH") {
            m.insert("PATH".to_string(), p.to_string_lossy().into_owned());
        }
        m
    }));
    cmd.arg("0");
    // strip inherited MSL so sleep prints no init banner; the "turning off"
    // atfork warning is what we assert absent, and it can only fire if the
    // spawn took the fork+exec path (pre_exec) instead of posix_spawn.
    maki_agent::child_env::strip_inherited_malloc_stack_logging(&mut cmd);
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn sleep");
    let se = child.stderr.take().unwrap();
    let err = read_to_end_capped(se).unwrap_or_default();
    let _ = child.wait();
    let clean = !err.contains("MallocStackLogging");
    if !clean {
        eprintln!("GRANDCHILD_STDERR_DIRTY: {err}");
    } else {
        println!("GRANDCHILD_STDERR_CLEAN");
    }
    assert!(clean, "libmalloc MSL warning captured on stderr: {err}");
}
