//! Detached spawning that stays on the `posix_spawn` fast path.
//!
//! A spawned child must `setsid()` to leave maki's session, otherwise tools
//! like `sudo`, `ssh` or git credential helpers draw prompts on the TUI (or
//! stop on `SIGTTOU` until the shell timeout). `Command::pre_exec` can call
//! it, but registering a closure makes std pick `fork`+`exec`, and on macOS
//! libmalloc's atfork handler then prints a warning on every spawn (#909).
//!
//! Instead [`detached_command`] spawns the maki binary itself with
//! [`DETACHED_MARKER`]: an absolute path and no closure, so `posix_spawn`
//! applies regardless of the command's env (a `PATH` override can no longer
//! force the fork+exec fallback). The trampoline then `setsid`s and `exec`s
//! the real program — no `fork` anywhere in the chain, and the child the
//! parent spawned *is* the program, so `killpg` cleanup keeps working
//! unchanged.
//!
//! Only binaries that call [`run_detached_if_marked`] early in `main` opt in
//! as the trampoline; anywhere else (test harnesses, other hosts) falls back
//! to `pre_exec(setsid)` — same session semantics, just the slower spawn.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// `argv[1]` that re-enters the maki binary as the detached trampoline.
pub const DETACHED_MARKER: &str = "__spawn_detached";

static TRAMPOLINE_EXE: OnceLock<PathBuf> = OnceLock::new();

/// A `Command` that runs `program` as the leader of a new session, with no
/// controlling terminal. Equivalent to `pre_exec(setsid)` without leaving
/// the `posix_spawn` path when the trampoline binary registered itself via
/// [`run_detached_if_marked`].
pub fn detached_command(
    program: &OsStr,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Command {
    match TRAMPOLINE_EXE.get() {
        Some(exe) => {
            let mut cmd = Command::new(exe);
            cmd.arg(DETACHED_MARKER).arg(program).args(args);
            cmd
        }
        None => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            setsid_before_exec(&mut cmd);
            cmd
        }
    }
}

#[cfg(unix)]
fn setsid_before_exec(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn setsid_before_exec(_cmd: &mut Command) {}

/// Called first in `main`: registers this binary as the detached trampoline
/// and re-enters as it when `argv[1]` is [`DETACHED_MARKER`]. A no-op
/// everywhere else.
pub fn run_detached_if_marked() {
    #[cfg(unix)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let _ = TRAMPOLINE_EXE.set(exe);
        }
        if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(DETACHED_MARKER)) {
            run_detached();
        }
    }
}

#[cfg(unix)]
fn run_detached() -> ! {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os().skip(2);
    let Some(program) = args.next() else {
        eprintln!("{DETACHED_MARKER}: missing program");
        std::process::exit(1);
    };
    // A failed setsid would keep the child on maki's tty — the exact problem
    // this exists to avoid — so bail rather than exec degraded.
    unsafe {
        if libc::setsid() == -1 {
            eprintln!("{DETACHED_MARKER}: setsid failed");
            std::process::exit(1);
        }
    }
    let err = Command::new(&program).args(args).exec();
    eprintln!(
        "{DETACHED_MARKER}: exec {}: {err}",
        program.to_string_lossy()
    );
    std::process::exit(1);
}
