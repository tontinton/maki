//! Environment sanitation and program resolution for spawned child processes.

use std::collections::HashMap;

/// macOS env vars that keep libmalloc stack logging enabled in the exec'd
/// child (the child's runtime prints "recording ... lite mode" at init and
/// may warn when it turns it off). Inherited values are stripped unless the
/// spawn site explicitly sets them. See tontinton/maki#909.
#[cfg(target_os = "macos")]
const MALLOC_STACK_LOGGING_VARS: &[&str] =
    &["MallocStackLogging", "MallocStackLoggingNoCompact", "MallocStackLoggingDirectory"];

/// Remove inherited MallocStackLogging* vars from a command's environment,
/// preserving any the caller set explicitly.
pub trait StripInheritedMallocStackLogging {
    fn env_remove_var(&mut self, var: &str);
}

impl StripInheritedMallocStackLogging for std::process::Command {
    fn env_remove_var(&mut self, var: &str) {
        self.env_remove(var);
    }
}

impl StripInheritedMallocStackLogging for async_process::Command {
    fn env_remove_var(&mut self, var: &str) {
        self.env_remove(var);
    }
}

pub fn strip_inherited_malloc_stack_logging(cmd: &mut impl StripInheritedMallocStackLogging) {
    #[cfg(target_os = "macos")]
    for var in MALLOC_STACK_LOGGING_VARS {
        cmd.env_remove_var(var);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cmd;
}

/// Resolve a bare program name against the child's effective PATH so
/// std::process keeps the posix_spawn fast path (see #909). Returns the input
/// unchanged when it already contains a separator or nothing is found.
pub fn resolve_program(program: &str, env: &HashMap<String, String>) -> std::ffi::OsString {
    if program.contains('/') || program.contains('\\') || program.is_empty() {
        return std::ffi::OsString::from(program);
    }
    let path = env
        .get("PATH")
        .map(String::as_str)
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("PATH").map(std::path::PathBuf::from));
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
    }
    std::ffi::OsString::from(program)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn strips_inherited_vars_but_keeps_explicit() {
        unsafe {
            std::env::set_var("MallocStackLogging", "1");
            std::env::set_var("MallocStackLoggingNoCompact", "1");
        }
        let mut cmd = std::process::Command::new("true");
        cmd.env("MallocStackLoggingLevel", "verbose");
        strip_inherited_malloc_stack_logging(&mut cmd);
        let get = |name: &str| -> Option<String> {
            cmd.get_envs()
                .find(|(k, _)| k.to_string_lossy() == name)
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("MallocStackLogging"), None);
        assert_eq!(get("MallocStackLoggingNoCompact"), None);
        assert_eq!(get("MallocStackLoggingDirectory"), None);
        assert_eq!(get("MallocStackLoggingLevel"), Some("verbose".into()));
        unsafe {
            std::env::remove_var("MallocStackLogging");
            std::env::remove_var("MallocStackLoggingNoCompact");
        }
    }
}

#[cfg(test)]
mod resolve_program_tests {
    use super::*;
    use std::collections::HashMap;

    fn env_with(path: Option<&str>) -> HashMap<String, String> {
        let mut m = HashMap::new();
        if let Some(p) = path {
            m.insert("PATH".to_string(), p.to_string());
        }
        m
    }

    #[test]
    fn bare_name_resolves_against_config_path() {
        let dir = std::env::temp_dir();
        let bin = dir.join("maki-resolve-test-bin");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = env_with(Some(&format!("{}:/usr/bin", dir.display())));
        let got = resolve_program("maki-resolve-test-bin", &env);
        assert_eq!(got, bin.as_os_str());
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn absolute_and_relative_paths_pass_through() {
        let env = env_with(Some("/usr/bin"));
        assert_eq!(resolve_program("/bin/sleep", &env), std::ffi::OsString::from("/bin/sleep"));
        assert_eq!(resolve_program("./tool", &env), std::ffi::OsString::from("./tool"));
        assert_eq!(resolve_program("", &env), std::ffi::OsString::from(""));
    }

    #[test]
    fn missing_binary_passes_through_for_start_failed_message() {
        let env = env_with(Some("/nonexistent-dir"));
        assert_eq!(resolve_program("definitely-not-a-bin", &env), std::ffi::OsString::from("definitely-not-a-bin"));
    }
}
