//! Diagnostic tool for sandbox command execution.
//!
//! Spawns a sandbox child (identical to how the shell does it), then
//! probes the filesystem and tries running commands to diagnose why
//! `sh -c <command>` may fail with ENOENT inside the sandbox.
//!
//! Also does incremental namespace tests to isolate where exec breaks.
//!
//! Usage: cargo run --bin sandbox-diag

#[cfg(all(feature = "sandbox", target_os = "linux"))]
mod sandbox_impl {
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Stdio;

    use maki_sandbox::Sandbox;
    use maki_sandbox::ipc;
    use maki_sandbox::namespace::{self, NamespaceConfig};
    use nix::sched::CloneFlags;
    use nix::sys::wait::WaitStatus;
    use nix::unistd::ForkResult;

    pub fn main() {
        let ws_dir = std::env::current_dir().expect("cwd");
        let ws_name = ws_dir
            .file_name()
            .map_or_else(|| "diag".into(), |n| n.to_string_lossy().to_string());

        eprintln!("=== sandbox-diag ===");
        eprintln!(
            "host: uid={} gid={}",
            nix::unistd::getuid(),
            nix::unistd::getgid()
        );
        eprintln!();

        // 1. Basic user-ns exec test
        eprintln!("[1/8] Basic user-ns exec test...");
        test_namespace_exec(false);
        eprintln!();

        // 2. User + mount namespace exec test (no pivot_root)
        eprintln!("[2/8] User+mount namespace exec test...");
        test_namespace_exec(true);
        eprintln!();

        // 3. Sandbox spawn + filesystem probe
        eprintln!("[3/8] Spawning sandbox child (spawn_browser)...");
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            ws_dir.clone(),
            ws_name,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let sandbox = match Sandbox::new(config) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  FAILED: {e}");
                std::process::exit(1);
            }
        };
        let pid = match sandbox.pid() {
            Some(p) => p.to_string(),
            None => "unknown".into(),
        };
        let pwd = sandbox.pwd().unwrap_or_default();
        eprintln!("  OK: child_pid={pid}, pwd={pwd}");
        eprintln!();

        eprintln!("[4/8] PWD check...");
        match sandbox.pwd() {
            Ok(p) => eprintln!("  pwd = {p}"),
            Err(e) => eprintln!("  FAILED: {e}"),
        }
        eprintln!();

        eprintln!("[5/8] Probing key paths with Ls...");
        for path in &[
            "/",
            "/bin",
            "/usr/bin",
            "/usr",
            "/lib",
            "/lib64",
            "/lib/x86_64-linux-gnu",
        ] {
            probe_ls(&sandbox, path);
        }
        eprintln!();

        eprintln!("[6/8] Checking specific binaries...");
        for path in &[
            "/bin/sh",
            "/usr/bin/sh",
            "/bin/dash",
            "/bin/bash",
            "/usr/bin/find",
            "/usr/bin/cat",
            "/lib64/ld-linux-x86-64.so.2",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
        ] {
            probe_exists(&sandbox, path);
        }
        eprintln!();

        eprintln!("[7/8] Detailed exec probe...");
        // Probe stat/access via shell test commands
        for cmd in &[
            // Test file existence via shell builtins
            "test -f /bin/sh && echo /bin/sh exists || echo /bin/sh MISSING",
            "test -x /bin/sh && echo /bin/sh executable || echo /bin/sh NOT EXECUTABLE",
            "test -f /usr/bin/sh && echo /usr/bin/sh exists || echo /usr/bin/sh MISSING",
            "test -x /usr/bin/sh && echo /usr/bin/sh executable || echo /usr/bin/sh NOT EXECUTABLE",
            "test -f /lib64/ld-linux-x86-64.so.2 && echo linker exists || echo linker MISSING",
            "test -f /usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 && echo usr linker exists || echo usr linker MISSING",
            // Try ls for debug
            "ls -la /bin/sh /usr/bin/sh /lib64/ld-linux-x86-64.so.2 2>&1",
            // Try reading /bin content
            "ls /bin/ | head -20",
            // Try direct execution (not through sh -c)
            "/usr/bin/env sh -c 'echo direct_sh_works' 2>&1",
            // Try using absolute path to linker directly
            "/lib64/ld-linux-x86-64.so.2 --help 2>&1 | head -3",
        ] {
            run_cmd(&sandbox, cmd);
        }
        eprintln!();

        test_sandbox_exec_probe();

        eprintln!();
        eprintln!("Cleaning up...");
        drop(sandbox);
        eprintln!("=== done ===");
    }

    fn test_namespace_exec(with_mount_ns: bool) {
        use nix::sched::unshare;

        let ns_type = if with_mount_ns { "user+mount" } else { "user" };
        match unsafe { nix::unistd::fork() } {
            Ok(ForkResult::Child) => {
                if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                    eprintln!("  user ns FAILED: {}", std::io::Error::last_os_error());
                    std::process::exit(3);
                }
                let uid = nix::unistd::getuid().as_raw();
                let gid = nix::unistd::getgid().as_raw();
                let _ = std::fs::write("/proc/self/setgroups", b"deny\n");
                let _ = std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"));
                let _ = std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"));

                if with_mount_ns && unshare(CloneFlags::CLONE_NEWNS).is_err() {
                    eprintln!("  mount ns FAILED: {}", std::io::Error::last_os_error());
                    std::process::exit(3);
                }

                let label = if with_mount_ns {
                    "mountns_ok"
                } else {
                    "userns_ok"
                };
                let result = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("echo {label}"))
                    .output();
                match result {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        eprintln!(
                            "  status={}, stdout={stdout:?}, stderr={stderr:?}",
                            out.status
                        );
                        std::process::exit(i32::from(!out.status.success()));
                    }
                    Err(e) => {
                        eprintln!("  FAILED: {e}");
                        std::process::exit(2);
                    }
                }
            }
            Ok(ForkResult::Parent { child }) => match nix::sys::wait::waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => {
                    eprintln!("  {ns_type}: exited with code {code}");
                }
                Ok(status) => {
                    eprintln!("  {ns_type}: abnormal exit: {status:?}");
                }
                Err(e) => {
                    eprintln!("  {ns_type}: waitpid failed: {e}");
                }
            },
            Err(e) => {
                eprintln!("  fork failed: {e}");
            }
        }
    }

    fn probe_ls(sb: &Sandbox, path: &str) {
        eprint!("  ls {path} ... ");
        match sb.ls(path) {
            Ok(entries) => {
                let total = entries.len();
                let dirs = entries.iter().filter(|e| e.is_dir).count();
                let files = total - dirs;
                eprintln!("OK ({total} entries: {dirs} dirs, {files} files)");
                for entry in entries.iter().take(10) {
                    let kind = if entry.is_dir { 'd' } else { '-' };
                    eprintln!("    {kind} {}", entry.name);
                }
                if total > 10 {
                    eprintln!("    ... and {} more", total - 10);
                }
            }
            Err(e) => eprintln!("FAILED: {e}"),
        }
    }

    fn probe_exists(sb: &Sandbox, path: &str) {
        let parent = Path::new(path).parent().unwrap_or(Path::new("/"));
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        eprint!("  check {path} ... ");
        match sb.ls(&parent.to_string_lossy()) {
            Ok(entries) => {
                if entries.iter().any(|e| e.name == name) {
                    eprintln!("EXISTS");
                } else {
                    eprintln!("NOT FOUND");
                }
            }
            Err(e) => eprintln!("ls failed: {e}"),
        }
    }

    /// Test 8: Exact sandbox child setup + direct exec probe.
    /// Replicates what `child_main` + `child_inner_loop` does, but probes exec
    /// directly instead of through IPC, to isolate the failure point.
    fn test_sandbox_exec_probe() {
        eprintln!("[8/8] Sandbox inline exec probe...");

        let (mut parent_sock, mut child_sock) = match UnixStream::pair() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  socketpair FAILED: {e}");
                return;
            }
        };

        // Fork and do the exact sandbox child setup
        match unsafe { nix::unistd::fork() } {
            Ok(ForkResult::Child) => {
                drop(parent_sock);
                // ── Child ──────────────────────────────────────────
                // 1. Handshake (skip — we'll just use the sync protocol)
                // 2. filter_env
                //    Use default NamespaceConfig with minimal env
                let config = NamespaceConfig::new(
                    vec![],
                    vec![],
                    Path::new(".").to_owned(),
                    "diag".into(),
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                );
                let _ = config.filter_env();

                // 3. isolate_user_ns
                let _ = namespace::isolate_user_ns(&mut child_sock);

                // 4. isolate_mount_ns
                let has_mount_ns = namespace::isolate_mount_ns().unwrap_or(false);
                eprintln!("  has_mount_ns={has_mount_ns}");

                // 5. setup_mounts (pivot_root + bind mounts)
                let ws_dir = std::env::current_dir().expect("cwd");
                let ws_name = ws_dir
                    .file_name()
                    .map_or_else(|| "diag".into(), |n| n.to_string_lossy().to_string());
                let config2 = NamespaceConfig::new(
                    vec![],
                    vec![],
                    ws_dir,
                    ws_name,
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                );
                match config2.setup_mounts(has_mount_ns) {
                    Ok(()) => eprintln!("  setup_mounts OK"),
                    Err(e) => {
                        eprintln!("  setup_mounts FAILED: {e}");
                        std::process::exit(1);
                    }
                }

                // ── Now probe exec directly ────────────────────────
                eprintln!("\n  --- exec probes ---");

                // Probe 1: stat via std::fs
                for path in &["/", "/bin", "/bin/sh", "/usr/bin/sh", "/usr"] {
                    match std::fs::metadata(path) {
                        Ok(m) => {
                            eprintln!("  stat {path}: OK (len={}, is_dir={})", m.len(), m.is_dir());
                        }
                        Err(e) => eprintln!("  stat {path}: FAILED ({e})"),
                    }
                }

                // Probe 2: read_dir
                for path in &["/", "/bin", "/usr", "/usr/bin"] {
                    match std::fs::read_dir(path) {
                        Ok(_) => eprintln!("  read_dir {path}: OK"),
                        Err(e) => eprintln!("  read_dir {path}: FAILED ({e})"),
                    }
                }

                // Probe 3: Diagnose spawn failures - is /dev/null missing?
                eprintln!("\n  --- /dev/null probe ---");
                match std::fs::metadata("/dev") {
                    Ok(m) => eprintln!("  stat /dev: OK (is_dir={})", m.is_dir()),
                    Err(e) => eprintln!("  stat /dev: FAILED ({e})"),
                }
                match std::fs::metadata("/dev/null") {
                    Ok(_) => eprintln!("  stat /dev/null: OK"),
                    Err(e) => eprintln!("  stat /dev/null: FAILED ({e})"),
                }
                match std::fs::OpenOptions::new().read(true).open("/dev/null") {
                    Ok(_) => eprintln!("  open /dev/null: OK"),
                    Err(e) => eprintln!("  open /dev/null: FAILED ({e})"),
                }

                // Test different spawn configs to find the exact failing path
                let test_cmds = &[
                    ("sh default", "sh", vec!["-c", "echo default_ok"]),
                    ("sh piped stdout", "sh", vec!["-c", "echo piped_ok"]),
                    ("sh piped all", "sh", vec!["-c", "echo piped_all_ok"]),
                    ("sh inherit all", "sh", vec!["-c", "echo inherit_ok"]),
                ];
                for (label, cmd, args) in test_cmds {
                    // configuration 1: default spawn() - no IO config
                    eprint!("  spawn_default({label}) ... ");
                    match std::process::Command::new(cmd).args(args.iter()).spawn() {
                        Ok(mut c) => {
                            let _ = c.wait();
                            eprintln!("OK");
                        }
                        Err(e) => eprintln!("FAILED: {e}"),
                    }

                    // configuration 2: with Stdio::piped() on stdout only
                    eprint!("  spawn_piped_stdout({label}) ... ");
                    match std::process::Command::new(cmd)
                        .args(args.iter())
                        .stdout(Stdio::piped())
                        .spawn()
                    {
                        Ok(mut c) => {
                            let _ = c.wait();
                            eprintln!("OK");
                        }
                        Err(e) => eprintln!("FAILED: {e}"),
                    }

                    // configuration 3: Stdio::piped() on all three
                    eprint!("  spawn_piped_all({label}) ... ");
                    match std::process::Command::new(cmd)
                        .args(args.iter())
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                    {
                        Ok(mut c) => {
                            let _ = c.wait();
                            eprintln!("OK");
                        }
                        Err(e) => eprintln!("FAILED: {e}"),
                    }

                    // configuration 4: with Stdio::null() on all three
                    eprint!("  spawn_null_all({label}) ... ");
                    match std::process::Command::new(cmd)
                        .args(args.iter())
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(mut c) => {
                            let _ = c.wait();
                            eprintln!("OK");
                        }
                        Err(e) => eprintln!("FAILED: {e}"),
                    }

                    // configuration 5: output()
                    eprint!("  output({label}) ... ");
                    match std::process::Command::new(cmd).args(args.iter()).output() {
                        Ok(out) => eprintln!("OK status={}", out.status),
                        Err(e) => eprintln!("FAILED: {e}"),
                    }
                }

                // Probe 4: try execl style (no sh -c)
                for label in &["from_execlp", "from_execl"] {
                    eprint!("  execlp {label} ... ");
                    match unsafe { nix::unistd::fork() } {
                        Ok(ForkResult::Child) => {
                            let err = std::process::Command::new("sh")
                                .arg("-c")
                                .arg(format!("echo {label}_ok"))
                                .exec();
                            eprintln!("FAILED: {err}");
                            std::process::exit(1);
                        }
                        Ok(ForkResult::Parent { child }) => {
                            match nix::sys::wait::waitpid(child, None) {
                                Ok(WaitStatus::Exited(_, code)) => {
                                    eprintln!("exited with code {code}");
                                }
                                Ok(status) => {
                                    eprintln!("abnormal: {status:?}");
                                }
                                Err(e) => {
                                    eprintln!("waitpid failed: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("fork failed: {e}");
                        }
                    }
                }

                // Probe 5: check PATH
                eprintln!("  PATH={:?}", std::env::var("PATH").unwrap_or_default());

                std::process::exit(0);
            }
            Ok(ForkResult::Parent { child }) => {
                // ── Parent (writes uid/gid maps) ───────────────────
                drop(child_sock);
                ipc::recv_sync(&mut parent_sock, maki_sandbox::ipc::SYNC_READY).unwrap_or_else(
                    |e| {
                        eprintln!("  sync ready FAILED: {e}");
                        std::process::exit(1);
                    },
                );
                let uid = nix::unistd::getuid();
                let gid = nix::unistd::getgid();
                namespace::write_uid_map(child, uid.as_raw(), gid.as_raw()).unwrap_or_else(|e| {
                    eprintln!("  uid map FAILED: {e}");
                    std::process::exit(1);
                });
                ipc::send_sync(&mut parent_sock, maki_sandbox::ipc::SYNC_GO).unwrap_or_else(|e| {
                    eprintln!("  sync go FAILED: {e}");
                    std::process::exit(1);
                });

                match nix::sys::wait::waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => {
                        eprintln!("  exited with code {code}");
                    }
                    Ok(status) => {
                        eprintln!("  abnormal exit: {status:?}");
                    }
                    Err(e) => {
                        eprintln!("  waitpid failed: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("  fork failed: {e}");
            }
        }
    }

    fn run_cmd(sb: &Sandbox, command: &str) {
        eprint!("  $ {command} ... ");
        match sb.exec(command) {
            Ok((output, is_error)) => {
                if is_error {
                    eprintln!("ERROR (exit != 0):");
                } else {
                    eprintln!("OK:");
                }
                for line in output.lines() {
                    eprintln!("    {line}");
                }
            }
            Err(e) => {
                eprintln!("IPC FAILED: {e}");
            }
        }
    }
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn main() {
    sandbox_impl::main();
}

#[cfg(not(all(feature = "sandbox", target_os = "linux")))]
fn main() {}
