#[cfg(all(feature = "sandbox", target_os = "linux"))]
mod sandbox_impl {
    use std::io::{self, Write};
    use std::path::PathBuf;

    use tracing_subscriber::EnvFilter;

    use maki_sandbox::Sandbox;
    use maki_sandbox::profiles;

    fn print_ls(sandbox: &Sandbox, label: &str, path: &str) {
        eprintln!("--- ls {label} ---");
        match sandbox.ls(path) {
            Ok(entries) => {
                for e in &entries {
                    let kind = if e.is_dir { "d" } else { "-" };
                    eprintln!("  {kind} {}", e.name);
                }
            }
            Err(e) => eprintln!("  error: {e}"),
        }
        eprintln!();
    }

    fn print_usage(profiles: &[profiles::SandboxProfile]) {
        eprintln!("Usage: sandbox-shell [OPTIONS] [WORKSPACE_DIR] [COMMAND...]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  -p, --profile <NAME>  Enable a profile (can be repeated)");
        eprintln!("  -x, --exec-only       Run command and exit (no interactive shell)");
        eprintln!("  -l, --list-profiles   List available profiles");
        eprintln!("  -h, --help            Show this help");
        eprintln!();
        eprintln!("Profiles:");
        for p in profiles {
            let dirs: Vec<&str> = p.mounts.iter().map(|m| m.path.as_str()).collect();
            eprintln!("  {:10} {}", p.name, dirs.join(", "));
        }
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  sandbox-shell .                        # browse workspace, no profiles");
        eprintln!("  sandbox-shell -p rust .                # enable Rust profile");
        eprintln!("  sandbox-shell -x -p rust . cargo build # build in sandbox, then exit");
    }

    pub fn main() {
        // Handle --sandbox-inner (called by child_main after pivot_root)
        if std::env::args().nth(1).as_deref() == Some("--sandbox-inner") {
            maki_sandbox::child::child_inner_main();
        }

        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_target(false)
            .init();

        let args: Vec<String> = std::env::args().skip(1).collect();
        let all_profiles = profiles::builtin_profiles();
        let mut enabled_profiles: Vec<String> = Vec::new();
        let mut positional: Vec<String> = Vec::new();
        let mut exec_only = false;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-h" | "--help" => {
                    print_usage(&all_profiles);
                    return;
                }
                "-l" | "--list-profiles" => {
                    for p in &all_profiles {
                        let dirs: Vec<&str> = p.mounts.iter().map(|m| m.path.as_str()).collect();
                        eprintln!("  {:10} {}", p.name, dirs.join(", "));
                    }
                    return;
                }
                "-x" | "--exec-only" => {
                    exec_only = true;
                }
                "-p" | "--profile" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("error: --profile requires a name");
                        std::process::exit(1);
                    }
                    let name = &args[i];
                    let known: Vec<&str> = all_profiles.iter().map(|p| p.name.as_str()).collect();
                    if !known.contains(&name.as_str()) {
                        eprintln!("error: unknown profile '{name}'");
                        eprintln!("available profiles: {}", known.join(", "));
                        std::process::exit(1);
                    }
                    enabled_profiles.push(name.clone());
                }
                _ => {
                    positional.push(args[i].clone());
                }
            }
            i += 1;
        }

        let ws_dir = if let Some(arg) = positional.first() {
            PathBuf::from(arg)
        } else {
            match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: cannot determine current directory: {e}");
                    std::process::exit(1);
                }
            }
        };

        let ws_name = ws_dir
            .file_name()
            .map_or_else(|| "sandbox".into(), |n| n.to_string_lossy().to_string());

        let enabled_profiles_list: Vec<profiles::SandboxProfile> = all_profiles
            .into_iter()
            .filter(|p| enabled_profiles.contains(&p.name))
            .collect();

        if !enabled_profiles.is_empty() {
            eprintln!("profiles: {}", enabled_profiles.join(", "));
        }

        let config = profiles::build_namespace_config(
            &enabled_profiles_list,
            ws_dir.clone(),
            ws_name,
            vec![],
            vec![],
        );

        maki_sandbox::namespace::probe().unwrap_or_else(|e| {
            eprintln!("error: sandbox isolation not available:\n{e}");
            std::process::exit(1);
        });

        let sandbox = match Sandbox::new(config) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sandbox spawn failed: {e}");
                std::process::exit(1);
            }
        };

        let pwd = match sandbox.pwd() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("pwd failed: {e}");
                std::process::exit(1);
            }
        };

        if !exec_only {
            match sandbox.pid() {
                Some(pid) => eprintln!("child_pid={}", pid.as_raw()),
                None => eprintln!("child_pid=unknown"),
            }
            eprintln!("pwd={pwd}");
            eprintln!();

            print_ls(&sandbox, "/", "/");
            print_ls(&sandbox, &pwd, &pwd);
            print_ls(&sandbox, "/home/maki", "/home/maki");
        }

        if positional.len() > 1 {
            let cmd = positional[1..].join(" ");
            if exec_only {
                eprintln!("--- exec: {cmd} ---");
            }
            match sandbox.exec(&cmd) {
                Ok((output, is_error)) => {
                    let prefix = if is_error { "ERR" } else { "OUT" };
                    for line in output.lines() {
                        eprintln!("  {prefix}: {line}");
                    }
                }
                Err(e) => eprintln!("  ipc error: {e}"),
            }
            eprintln!();

            if exec_only {
                return;
            }
        }

        eprintln!("--- interactive (type 'exit' or 'quit' to quit) ---");
        let stdin = std::io::stdin();
        loop {
            eprint!("$ ");
            let _ = io::stderr().flush();

            let mut line = String::new();
            if stdin.read_line(&mut line).is_err() || line.is_empty() {
                break;
            }
            let cmd = line.trim();
            if cmd == "exit" || cmd == "quit" {
                break;
            }
            if cmd.is_empty() {
                continue;
            }

            if cmd == "pwd" {
                match sandbox.pwd() {
                    Ok(p) => eprintln!("  {p}"),
                    Err(e) => eprintln!("  error: {e}"),
                }
            } else if cmd.starts_with("ls ") {
                let path = cmd.strip_prefix("ls ").expect("starts_with guard");
                match sandbox.ls(path) {
                    Ok(entries) => {
                        for e in &entries {
                            let kind = if e.is_dir { "d" } else { "-" };
                            eprintln!("  {kind} {}", e.name);
                        }
                    }
                    Err(e) => eprintln!("  error: {e}"),
                }
            } else if cmd.starts_with("cd ") {
                let path = cmd.strip_prefix("cd ").expect("starts_with guard");
                match sandbox.cd(path) {
                    Ok(()) => match sandbox.pwd() {
                        Ok(p) => eprintln!("  {p}"),
                        Err(e) => eprintln!("  cd ok, but pwd failed: {e}"),
                    },
                    Err(e) => eprintln!("  error: {e}"),
                }
            } else {
                match sandbox.exec(cmd) {
                    Ok((output, is_error)) => {
                        let prefix = if is_error { "ERR" } else { "OUT" };
                        for line in output.lines() {
                            eprintln!("  {prefix}: {line}");
                        }
                    }
                    Err(e) => eprintln!("  ipc error: {e}"),
                }
            }
        }
        // sandbox dropped here — Drop sends Exit and waits for child
    }
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn main() {
    sandbox_impl::main();
}

#[cfg(not(all(feature = "sandbox", target_os = "linux")))]
fn main() {}
