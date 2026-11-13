use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::unistd::dup2;
use tracing::{debug, warn};

use crate::error::SandboxError;
use crate::namespace::{MissingMask, NamespaceConfig};

const BWRAP_BIN: &str = "bwrap";
const SANDBOX_EXE: &str = "/run/maki";
const SANDBOX_PROBE_BIN: &str = "/usr/bin/true";
const SANDBOX_HOME: &str = "/home/maki";
const SANDBOX_WORKSPACE: &str = "/home/maki/workspace";
const SANDBOX_FD: RawFd = 3;
const ENV_SANDBOX_FD: &str = "MAKI_SANDBOX_FD";
pub(crate) const INNER_FLAG: &str = "--sandbox-bwrap-inner";

const SYSTEM_MOUNTS: &[&str] = &["/usr", "/lib", "/lib64"];
const SYSTEM_SYMLINKS: &[(&str, &str)] = &[("usr/bin", "/bin"), ("usr/sbin", "/sbin")];

#[must_use]
pub fn installed() -> bool {
    binary_path().is_some()
}

/// The `bwrap` binary on `PATH`, if any.
#[must_use]
pub fn binary_path() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(BWRAP_BIN))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Check that bwrap can build a real sandbox on this host.
///
/// `--version` alone proves nothing: the binary runs, but user namespaces may
/// still be refused. This runs a throwaway command inside the same layout the
/// sandbox uses.
///
/// # Errors
///
/// Returns [`SandboxError::IsolationUnavailable`] when bwrap is missing or
/// cannot create a namespace on this host.
pub fn probe() -> Result<(), SandboxError> {
    let bin = binary_path().ok_or_else(missing)?;
    let mut args = base_args();
    for dir in SYSTEM_MOUNTS {
        if Path::new(dir).exists() {
            args.extend(["--ro-bind".into(), (*dir).into(), (*dir).into()]);
        }
    }
    for (target, link) in SYSTEM_SYMLINKS {
        args.extend(["--symlink".into(), (*target).into(), (*link).into()]);
    }
    args.push(SANDBOX_PROBE_BIN.into());

    let output = Command::new(&bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SandboxError::IsolationUnavailable(format!("run bwrap probe: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(SandboxError::IsolationUnavailable(format!(
        "bwrap could not create a sandbox ({}): {}",
        output.status,
        stderr.trim()
    )))
}

/// Spawn bwrap with the sandbox filesystem and `sock` on [`SANDBOX_FD`].
///
/// The returned child is the bwrap monitor; it exits with the inner maki
/// process, so its pid is what the caller waits on.
///
/// # Errors
///
/// Returns [`SandboxError`] if the current executable cannot be resolved, the
/// environment cannot be passed through, or bwrap cannot be started.
pub(crate) fn spawn(config: &NamespaceConfig, sock: UnixStream) -> Result<Child, SandboxError> {
    let exe = std::env::current_exe()
        .map_err(|e| SandboxError::Fork(format!("resolve current executable: {e}")))?;
    let bin = binary_path().ok_or_else(missing)?;
    let args = build_args(config, &exe)?;
    let fd = sock.as_raw_fd();

    let mut command = Command::new(&bin);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if fd != SANDBOX_FD {
                dup2(fd, SANDBOX_FD).map_err(std::io::Error::from)?;
            }
            fcntl(SANDBOX_FD, FcntlArg::F_SETFD(FdFlag::empty())).map_err(std::io::Error::from)?;
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|e| SandboxError::Fork(format!("spawn bwrap: {e}")))?;
    drop(sock);

    if let Some(stderr) = child.stderr.take()
        && let Err(e) = thread::Builder::new()
            .name("bwrap-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    warn!(target: "sandbox::bwrap", "{line}");
                }
            })
    {
        warn!(error = %e, "sandbox: failed to start bwrap stderr reader");
    }

    Ok(child)
}

fn missing() -> SandboxError {
    SandboxError::IsolationUnavailable(format!(
        "{BWRAP_BIN} is not installed. Install bubblewrap (e.g. `sudo apt install bubblewrap`) or select the native sandbox backend"
    ))
}

fn base_args() -> Vec<OsString> {
    [
        "--unshare-user",
        "--uid",
        "0",
        "--gid",
        "0",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
    ]
    .map(OsString::from)
    .to_vec()
}

fn build_args(config: &NamespaceConfig, exe: &Path) -> Result<Vec<OsString>, SandboxError> {
    let mut args = base_args();
    args.push("--clearenv".into());
    for (key, value) in config.filtered_env()? {
        args.extend(["--setenv".into(), key.into(), value.into()]);
    }
    args.extend([
        "--setenv".into(),
        ENV_SANDBOX_FD.into(),
        SANDBOX_FD.to_string().into(),
    ]);

    args.extend(["--tmpfs".into(), "/".into()]);
    args.extend(["--proc".into(), "/proc".into()]);
    args.extend(["--dev".into(), "/dev".into()]);
    args.extend(["--tmpfs".into(), "/tmp".into()]);
    args.extend(["--tmpfs".into(), "/etc".into()]);
    args.extend([
        "--ro-bind".into(),
        exe.as_os_str().to_owned(),
        SANDBOX_EXE.into(),
    ]);

    for dir in SYSTEM_MOUNTS {
        if Path::new(dir).exists() {
            args.extend(["--ro-bind".into(), (*dir).into(), (*dir).into()]);
        }
    }
    if Path::new("/etc/ssl").is_dir() {
        args.extend(["--ro-bind".into(), "/etc/ssl".into(), "/etc/ssl".into()]);
    }
    for (target, link) in SYSTEM_SYMLINKS {
        args.extend(["--symlink".into(), (*target).into(), (*link).into()]);
    }

    args.extend(["--dir".into(), SANDBOX_HOME.into()]);
    args.extend(["--dir".into(), SANDBOX_WORKSPACE.into()]);
    args.extend([
        "--bind".into(),
        config.workspace_dir.as_os_str().to_owned(),
        workspace_target(&config.workspace_name),
    ]);
    for (host, name) in &config.extra_workspace_dirs {
        args.extend([
            "--bind".into(),
            host.as_os_str().to_owned(),
            format!("{SANDBOX_WORKSPACE}/{name}").into(),
        ]);
    }
    for (host, name) in &config.home_mounts {
        args.extend([
            "--bind".into(),
            host.as_os_str().to_owned(),
            home_target(name),
        ]);
    }
    for (host, name) in &config.readonly_mounts {
        args.extend([
            "--ro-bind".into(),
            host.as_os_str().to_owned(),
            home_target(name),
        ]);
    }

    for (host, sandbox_path) in &config.symlinks {
        match std::fs::read_link(host) {
            Ok(target) => args.extend([
                "--symlink".into(),
                target.as_os_str().to_owned(),
                sandbox_path.into(),
            ]),
            Err(e) => warn!(
                host = %host.display(),
                error = %e,
                "sandbox: read symlink target failed"
            ),
        }
    }

    for masked in &config.masked_paths {
        let target = config.host_to_sandbox(&masked.path);
        match std::fs::metadata(&masked.path) {
            Ok(meta) if meta.is_dir() => {
                args.extend(["--tmpfs".into(), target.into_os_string()]);
            }
            Ok(meta) if meta.is_file() => {
                args.extend([
                    "--ro-bind".into(),
                    "/dev/null".into(),
                    target.into_os_string(),
                ]);
            }
            Ok(_) => debug!(
                target = %target.display(),
                "sandbox: mask target is not a file or directory, skipping"
            ),
            Err(_) if masked.missing == MissingMask::CreateDirectory => {
                args.extend(["--tmpfs".into(), target.into_os_string()]);
            }
            Err(_) => debug!(
                target = %target.display(),
                "sandbox: mask target absent, nothing to hide"
            ),
        }
    }

    args.extend([
        "--chdir".into(),
        workspace_target(&config.workspace_name),
        SANDBOX_EXE.into(),
        INNER_FLAG.into(),
    ]);
    Ok(args)
}

fn home_target(name: &str) -> OsString {
    if name.is_empty() {
        SANDBOX_HOME.into()
    } else {
        format!("{SANDBOX_HOME}/{name}").into()
    }
}

fn workspace_target(name: &str) -> OsString {
    format!("{SANDBOX_WORKSPACE}/{name}").into()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::namespace::{MaskedPath, NamespaceConfig};

    fn config() -> NamespaceConfig {
        let mut config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/workspace"),
            "project".into(),
            vec![(PathBuf::from("/home/user/.cargo"), ".cargo".into())],
            vec![(PathBuf::from("/home/user/.rustup"), ".rustup".into())],
            vec![],
            vec![(PathBuf::from("/extra"), "extra".into())],
            vec![],
        );
        config
            .masked_paths
            .push(MaskedPath::directory(PathBuf::from("/workspace/.maki")));
        config
    }

    fn text(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn args_isolate_and_enter_the_workspace() {
        let args = text(&build_args(&config(), Path::new("/usr/bin/maki")).unwrap());
        for flag in [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
        ] {
            assert!(args.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert_eq!(
            args.get(args.len() - 4..),
            Some(
                [
                    "--chdir".to_string(),
                    "/home/maki/workspace/project".to_string(),
                    SANDBOX_EXE.to_string(),
                    INNER_FLAG.to_string()
                ]
                .as_slice()
            )
        );
        assert!(args.iter().any(|arg| arg == "/usr/bin/maki"));
        assert!(args.iter().any(|arg| arg == INNER_FLAG));
    }

    #[test]
    fn args_mount_the_workspace_home_and_extra_dirs() {
        let args = text(&build_args(&config(), Path::new("/usr/bin/maki")).unwrap());
        let joined = args.join("\n");
        for expected in [
            "--bind\n/workspace\n/home/maki/workspace/project",
            "--bind\n/extra\n/home/maki/workspace/extra",
            "--bind\n/home/user/.cargo\n/home/maki/.cargo",
            "--ro-bind\n/home/user/.rustup\n/home/maki/.rustup",
        ] {
            assert!(
                joined.contains(expected),
                "missing {expected} in:\n{joined}"
            );
        }
    }

    #[test]
    fn args_pass_the_socket_fd_and_filtered_env() {
        let args = text(&build_args(&config(), Path::new("/usr/bin/maki")).unwrap());
        let joined = args.join("\n");
        assert!(joined.contains("--clearenv"));
        assert!(joined.contains(&format!("--setenv\n{ENV_SANDBOX_FD}\n{SANDBOX_FD}")));
        assert!(joined.contains("--setenv\nPATH\n"));
        assert!(joined.contains("--setenv\nHOME\n/home/maki"));
    }

    #[test]
    fn args_mask_missing_workspace_config_with_an_empty_dir() {
        let args = text(&build_args(&config(), Path::new("/usr/bin/maki")).unwrap());
        let joined = args.join("\n");
        assert!(
            joined.contains("--tmpfs\n/home/maki/workspace/project/.maki"),
            "a missing .maki is created empty so sandboxed code cannot plant it:\n{joined}"
        );
    }
}
