//! Detached spawning that stays on the `posix_spawn` fast path.
//!
//! A spawned child must `setsid()` to leave maki's session, otherwise tools
//! like `sudo`, `ssh` or git credential helpers draw prompts on the TUI (or
//! stop on `SIGTTOU` until the shell timeout). `Command::pre_exec` can call
//! it, but registering a closure makes std pick `fork`+`exec`, and on macOS
//! libmalloc's atfork handler then prints a warning on every spawn (#909).
//!
//! On macOS [`detached_command`] therefore spawns the maki binary itself with
//! [`DETACHED_MARKER`]: an absolute path and no closure, so `posix_spawn`
//! applies regardless of the command's env (a `PATH` override can no longer
//! force the fork+exec fallback). The trampoline then `setsid`s and `exec`s
//! the real program. Because `setsid` only lands after the spawn returns,
//! there is a window where the child is still in maki's group; kill paths
//! signal the pid itself in addition to the group to cover it.
//!
//! Everywhere else (including Linux, where the warning does not exist) the
//! command gets plain `pre_exec(setsid)` — same session semantics, just the
//! slower spawn.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// `argv[1]` that re-enters the maki binary as the detached trampoline.
#[cfg(target_os = "macos")]
pub const DETACHED_MARKER: &str = "__spawn_detached";

#[cfg(target_os = "macos")]
static TRAMPOLINE_EXE: OnceLock<PathBuf> = OnceLock::new();

/// A `Command` that runs `program` as the leader of a new session, with no
/// controlling terminal. On macOS this goes through the re-exec trampoline
/// (once [`run_detached_if_marked`] registered this binary) to stay on the
/// `posix_spawn` path; elsewhere it is `pre_exec(setsid)`.
pub fn detached_command(program: &OsStr, args: &[&OsStr]) -> Command {
    #[cfg(target_os = "macos")]
    if let Some(exe) = TRAMPOLINE_EXE.get()
        && !needs_trampoline_fallback(program)
    {
        let mut cmd = Command::new(exe);
        cmd.arg(DETACHED_MARKER).arg(program).args(args);
        return cmd;
    }

    let mut cmd = Command::new(program);
    cmd.args(args);
    setsid_before_exec(&mut cmd);
    cmd
}

/// Programs the trampoline branch must reject: they need `execvp`'s search or
/// a `cwd`-relative lookup the trampoline cannot perform, and skipping this
/// check would turn a missing binary into a stderr line and exit 1 instead of
/// a `spawn()` error.
#[cfg(target_os = "macos")]
fn needs_trampoline_fallback(program: &OsStr) -> bool {
    let path = Path::new(program);
    if !path.is_absolute() {
        return true;
    }
    !is_executable_file(path)
}

#[cfg(target_os = "macos")]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.is_file()
        && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(unix)]
fn setsid_before_exec(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
    unsafe {
        cmd.pre_exec(|| {
            let _ = rustix::process::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn setsid_before_exec(_cmd: &mut Command) {}

/// Called first in `main`: registers this binary as the detached trampoline
/// and re-enters as it when `argv[1]` is [`DETACHED_MARKER`]. A no-op
/// everywhere else.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn run_detached_if_marked() {
    #[cfg(target_os = "macos")]
    {
        if let Ok(exe) = std::env::current_exe() {
            let _ = TRAMPOLINE_EXE.set(exe);
        }
        if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(DETACHED_MARKER)) {
            run_detached();
        }
    }
}

#[cfg(target_os = "macos")]
fn run_detached() -> ! {
    use std::os::unix::process::CommandExt;

    let mut args = std::env::args_os().skip(2);
    let Some(program) = args.next() else {
        eprintln!("{DETACHED_MARKER}: missing program");
        std::process::exit(1);
    };
    // A failed setsid would keep the child on maki's tty — the exact problem
    // this exists to avoid — so bail rather than exec degraded.
    if rustix::process::setsid().is_err() {
        eprintln!("{DETACHED_MARKER}: setsid failed");
        std::process::exit(1);
    }
    let err = Command::new(&program).args(args).exec();
    eprintln!(
        "{DETACHED_MARKER}: exec {}: {err}",
        program.to_string_lossy()
    );
    std::process::exit(1);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::{is_executable_file, needs_trampoline_fallback};

    #[test]
    fn relative_programs_need_the_fallback() {
        assert!(needs_trampoline_fallback(OsStr::new("bash")));
        assert!(needs_trampoline_fallback(OsStr::new("./tool")));
        assert!(needs_trampoline_fallback(OsStr::new("../tool")));
    }

    #[test]
    fn absolute_missing_or_non_exec_programs_need_the_fallback() {
        assert!(needs_trampoline_fallback(OsStr::new(
            "/nonexistent/maki-does-not-ship-this"
        )));

        let dir = std::env::temp_dir().join("maki-spawn-non-exec");
        std::fs::write(&dir, b"").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(needs_trampoline_fallback(dir.as_os_str()));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn absolute_exec_files_skip_the_fallback() {
        assert!(!needs_trampoline_fallback(OsStr::new("/bin/sh")));
        assert!(is_executable_file(Path::new("/bin/sh")));
    }
}
