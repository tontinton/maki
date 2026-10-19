//! The re-exec trampoline behind `detached_command`: `maki __spawn_detached
//! <program> <args>` must `setsid`+`exec`, so the pid the parent spawned is a
//! session leader with no controlling terminal — the detach semantics every
//! spawn site relies on — while the spawn itself never leaves `posix_spawn`.
//!
//! macOS only: the trampoline only exists there, and CI has no macOS runner,
//! so this runs when a mac dev remembers to (`cargo nextest run -p maki
//! --test detached_spawn`).

#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::{Duration, Instant};

use maki_agent::spawn::DETACHED_MARKER;
use rustix::process::{Pid, getpgid, getsid};

fn raw_pid(pid: i32) -> Pid {
    Pid::from_raw(pid).expect("valid pid")
}

fn raw_id(pid: Pid) -> i32 {
    pid.as_raw_pid()
}

#[test]
fn detached_child_leads_its_own_session() {
    // `argv` forwarding: the program runs and sees its own arguments.
    let out = Command::new(env!("CARGO_BIN_EXE_maki"))
        .arg(DETACHED_MARKER)
        .args(["sh", "-c", "echo got:$1", "_", "hello"])
        .output()
        .expect("failed to spawn maki");
    assert!(out.status.success(), "trampoline failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "got:hello");

    let mut child = Command::new(env!("CARGO_BIN_EXE_maki"))
        .arg(DETACHED_MARKER)
        .args(["sleep", "30"])
        .spawn()
        .expect("failed to spawn maki");
    let pid = child.id() as i32;

    // The trampoline setsids between spawn and exec, so poll instead of
    // assuming it already happened.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sid = raw_id(getsid(Some(raw_pid(pid))).expect("getsid"));
        if sid == pid {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child never became a session leader (sid={sid})"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Session leader means no inherited controlling terminal; group
    // leadership keeps the killpg cleanup working.
    assert_eq!(
        raw_id(getpgid(Some(raw_pid(pid))).expect("getpgid")),
        pid,
        "child is not a group leader"
    );

    child.kill().expect("failed to kill child");
    child.wait().expect("failed to reap child");
}
