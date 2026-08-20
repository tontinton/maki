#[cfg(windows)]
use std::env;
#[cfg(windows)]
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::sync::OnceLock;

/// Escape hatch for a Bash we fail to find, or find in the wrong place.
#[cfg(windows)]
const BASH_OVERRIDE_VAR: &str = "MAKI_BASH";

/// Error message shown when `MAKI_BASH` points at something we cannot run.
#[cfg(windows)]
const BASH_OVERRIDE_ERROR: &str = "MAKI_BASH does not point at a file";

/// Error message shown when no Bash is found on Windows.
#[cfg(windows)]
const BASH_NOT_FOUND_ERROR: &str = "bash not found on Windows. Install Git for Windows:\n  \
     winget install --id Git.Git -e --source winget\n  \
     or download from https://git-scm.com/download/win\n\n  \
     Have a bash somewhere else? Point MAKI_BASH at it.\n\n  \
     Alternatively, run maki inside WSL: \
     https://learn.microsoft.com/en-us/windows/wsl/install";

/// Bash locations relative to a Git for Windows root.
#[cfg(any(windows, test))]
const GIT_ROOT_BASH: [&str; 2] = [r"bin\bash.exe", r"usr\bin\bash.exe"];

/// Git for Windows dir under a `%ProgramFiles%`-style root.
#[cfg(any(windows, test))]
const GIT_DIR: &str = "Git";

/// The Git for Windows dir that ends up on PATH. Bash is not in it, it is
/// in the sibling `bin`, which is why a PATH scan alone misses Git Bash.
#[cfg(any(windows, test))]
const GIT_PATH_DIR: &str = r"\cmd";

/// Bash locations relative to the system drive.
#[cfg(any(windows, test))]
const SYSTEM_DRIVE_BASH: [&str; 4] = [
    r"cygwin64\bin\bash.exe",
    r"cygwin\bin\bash.exe",
    r"msys64\usr\bin\bash.exe",
    r"msys32\usr\bin\bash.exe",
];

/// Where Windows keeps its app execution aliases. It is on everyone's PATH
/// and its `bash.exe` is the WSL launcher, not a Bash we can use.
#[cfg(any(windows, test))]
const WINDOWS_APPS_DIR: &str = r"\Microsoft\WindowsApps";

#[cfg(windows)]
const DEFAULT_SYSTEM_DRIVE: &str = "C:";

#[cfg(windows)]
const DEFAULT_SYSTEM_ROOT: &str = r"C:\Windows";

#[cfg(windows)]
const PROGRAM_FILES_VARS: [&str; 3] = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"];

/// Strip quotes and spaces, use one separator, drop the trailing one.
///
/// Everything downstream compares and joins raw strings, so it only has to
/// deal with `\` no matter how the user spelled their PATH.
#[cfg(any(windows, test))]
fn normalize_windows_dir(dir: &str) -> String {
    dir.trim()
        .trim_matches('"')
        .replace('/', r"\")
        .trim_end_matches('\\')
        .to_string()
}

/// Is this normalized dir a Windows absolute path (`C:\...` or `\\server`)?
///
/// Written by hand instead of `Path::is_absolute` so the lookup is plain
/// string work that can be tested on any platform.
#[cfg(any(windows, test))]
fn is_windows_absolute(dir: &str) -> bool {
    let bytes = dir.as_bytes();
    bytes.starts_with(br"\\")
        || matches!(bytes, [drive, b':', sep, ..] if drive.is_ascii_alphabetic() && *sep == b'\\')
}

#[cfg(any(windows, test))]
fn is_under(dir: &str, ancestor: &str) -> bool {
    let dir = dir.to_lowercase();
    let ancestor = ancestor.to_lowercase();
    dir == ancestor || dir.starts_with(&format!(r"{ancestor}\"))
}

/// What comes before `suffix`, when the dir ends with it.
#[cfg(any(windows, test))]
fn strip_dir_suffix<'a>(dir: &'a str, suffix: &str) -> Option<&'a str> {
    let head = dir.len().checked_sub(suffix.len())?;
    dir.get(head..)?
        .eq_ignore_ascii_case(suffix)
        .then(|| &dir[..head])
}

/// Bash paths to try, best first.
///
/// `C:\Windows\System32\bash.exe` (the legacy WSL launcher, present on any
/// machine with WSL enabled) sits early in PATH and is not a real Bash, so
/// PATH entries under `%SystemRoot%` and the `WindowsApps` dir are dropped.
/// Entries that are not absolute (a trailing `;` yields an empty one) are
/// dropped too, so a `bash.exe` sitting in the user's repo is never executed.
///
/// What is left is ordered by how sure we are: Git for Windows puts its `cmd`
/// dir on PATH, so an install found that way is both recognized and the one
/// the user picked, and it wins. Then the install roots and drives we guess
/// at. A bare `bash.exe` in some PATH dir comes last: an install we recognize
/// always wins over an executable we only know the name of.
#[cfg(any(windows, test))]
fn bash_candidates(
    program_roots: &[String],
    system_drive: &str,
    path: &str,
    system_root: &str,
) -> Vec<String> {
    let from_roots = program_roots.iter().flat_map(|root| {
        let root = normalize_windows_dir(root);
        GIT_ROOT_BASH.map(move |rel| format!(r"{root}\{GIT_DIR}\{rel}"))
    });

    let drive = normalize_windows_dir(system_drive);
    let from_drive = SYSTEM_DRIVE_BASH.map(|rel| format!(r"{drive}\{rel}"));

    let system_root = normalize_windows_dir(system_root);
    let dirs: Vec<String> = path
        .split(';')
        .map(normalize_windows_dir)
        .filter(|dir| {
            is_windows_absolute(dir)
                && !is_under(dir, &system_root)
                && strip_dir_suffix(dir, WINDOWS_APPS_DIR).is_none()
        })
        .collect();

    let from_git_on_path = dirs
        .iter()
        .filter_map(|dir| strip_dir_suffix(dir, GIT_PATH_DIR))
        .flat_map(|root| GIT_ROOT_BASH.map(|rel| format!(r"{root}\{rel}")));
    let from_path = dirs.iter().map(|dir| format!(r"{dir}\bash.exe"));

    from_git_on_path
        .chain(from_roots)
        .chain(from_drive)
        .chain(from_path)
        .collect()
}

/// Find a real Bash: whatever `MAKI_BASH` points at, else Git for Windows,
/// Cygwin or MSYS2.
#[cfg(windows)]
fn find_bash() -> Result<PathBuf, String> {
    let env_or = |var: &str, default: &str| {
        env::var(var)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    if let Some(bash) = env::var_os(BASH_OVERRIDE_VAR).filter(|value| !value.is_empty()) {
        let bash = PathBuf::from(bash);
        if bash.is_file() {
            return Ok(bash);
        }
        return Err(format!("{BASH_OVERRIDE_ERROR}: {}", bash.display()));
    }

    let program_roots: Vec<String> = PROGRAM_FILES_VARS
        .iter()
        .filter_map(|var| env::var(var).ok())
        .collect();

    bash_candidates(
        &program_roots,
        &env_or("SystemDrive", DEFAULT_SYSTEM_DRIVE),
        &env::var("PATH").unwrap_or_default(),
        &env_or("SystemRoot", DEFAULT_SYSTEM_ROOT),
    )
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
    .ok_or_else(|| BASH_NOT_FOUND_ERROR.to_string())
}

/// Resolve Bash once per process: every tool call and every `jobstart` goes
/// through here, and the lookup hits the filesystem many times.
///
/// Only a hit is cached. Our own error tells the user to install Git for
/// Windows, and the install lands in a place we already look at, so a miss
/// has to stay retryable or bash would keep failing until maki restarts.
#[cfg(windows)]
fn cached_bash() -> Result<&'static Path, String> {
    static BASH: OnceLock<PathBuf> = OnceLock::new();
    if let Some(bash) = BASH.get() {
        return Ok(bash);
    }
    let found = find_bash()?;
    Ok(BASH.get_or_init(|| found))
}

/// Build a `bash -c` command for the given shell string.
///
/// Mirrors Neovim's list-form `jobstart(['bash', '-c', ...])`: the command
/// string is passed as a single argv element, so quoting is preserved by the
/// C runtime / libuv argument parser instead of being reinterpreted by
/// cmd.exe. On Windows, `MAKI_BASH`, known install locations and PATH are
/// searched for Git Bash, Cygwin or MSYS2; there is no WSL fallback, since a
/// Linux shell cannot resolve the Windows paths the rest of the tools hand it.
pub fn bash_command(cmd: &str) -> Result<Command, String> {
    #[cfg(unix)]
    let mut command = Command::new("bash");
    #[cfg(windows)]
    let mut command = Command::new(cached_bash()?);
    command.arg("-c").arg(cmd);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::bash_candidates;

    const PROGRAM_FILES: &str = r"C:\Program Files";
    const GIT_BASH: &str = r"C:\Program Files\Git\bin\bash.exe";
    const SYSTEM_DRIVE: &str = "C:";
    const SYSTEM_ROOT: &str = r"C:\Windows";
    const TOOLS_BIN: &str = r"C:\tools\bin";
    const PATH_BASH: &str = r"C:\tools\bin\bash.exe";
    const PORTABLE_GIT_CMD: &str = r"D:\Portable\Git\cmd";
    const PORTABLE_GIT_BASH: &str = r"D:\Portable\Git\bin\bash.exe";
    const WINDOWS_APPS: &str = r"C:\Users\me\AppData\Local\Microsoft\WindowsApps";
    const MISSING_CANDIDATE: &str = "missing candidate";

    fn candidates(path: &str) -> Vec<String> {
        bash_candidates(
            &[PROGRAM_FILES.to_string()],
            SYSTEM_DRIVE,
            path,
            SYSTEM_ROOT,
        )
    }

    #[test_case(r"C:\Windows\System32" ; "system32")]
    #[test_case(r"C:\WINDOWS\system32" ; "system32_other_case")]
    #[test_case(r"C:/Windows/System32" ; "system32_forward_slashes")]
    #[test_case(r"C:\Windows" ; "system_root_itself")]
    #[test_case(WINDOWS_APPS ; "windows_apps_wsl_alias")]
    #[test_case(r"D:\Users\me\AppData\Local\Microsoft\windowsapps" ; "windows_apps_other_case")]
    #[test_case("" ; "empty_entry_from_trailing_semicolon")]
    #[test_case("tools" ; "relative_entry")]
    #[test_case(r"\Git\bin" ; "drive_relative_entry")]
    fn bash_candidates_ignores_path_entry(entry: &str) {
        assert_eq!(candidates(entry), candidates(""));
    }

    #[test_case(TOOLS_BIN ; "plain")]
    #[test_case(r"  C:\tools\bin  " ; "surrounded_by_spaces")]
    #[test_case(r#""C:\tools\bin""# ; "quoted")]
    #[test_case(r"C:\tools\bin\" ; "trailing_separator")]
    #[test_case(r"C:/tools/bin" ; "forward_slashes")]
    fn bash_candidates_keeps_path_entry(entry: &str) {
        assert!(candidates(entry).contains(&PATH_BASH.to_string()));
    }

    #[test_case(r"C:\Windows-tools\bin" ; "prefixed_by_system_root_name")]
    #[test_case(r"\\server\share\bin" ; "unc")]
    fn bash_candidates_keeps_unusual_path_entry(entry: &str) {
        let kept = format!(r"{entry}\bash.exe");
        assert!(candidates(entry).contains(&kept));
    }

    #[test_case(PORTABLE_GIT_CMD ; "plain")]
    #[test_case(r"d:\portable\git\CMD" ; "other_case")]
    fn bash_candidates_follows_git_on_path_to_its_bash(entry: &str) {
        let found = bash_candidates(&[], SYSTEM_DRIVE, entry, SYSTEM_ROOT);
        let bash = found
            .iter()
            .find(|candidate| candidate.to_lowercase() == PORTABLE_GIT_BASH.to_lowercase());
        assert!(bash.is_some(), "no git bash in {found:?}");
    }

    #[test]
    fn bash_candidates_prefers_git_on_path_over_a_bare_bash() {
        let path = format!("{TOOLS_BIN};{PORTABLE_GIT_CMD}");
        let found = bash_candidates(&[], SYSTEM_DRIVE, &path, SYSTEM_ROOT);
        let position = |wanted: &str| {
            found
                .iter()
                .position(|candidate| candidate == wanted)
                .unwrap_or_else(|| panic!("{MISSING_CANDIDATE}: {wanted} not in {found:?}"))
        };
        assert!(position(PORTABLE_GIT_BASH) < position(PATH_BASH));
    }

    #[test]
    fn bash_candidates_prefers_install_roots_over_path() {
        let found = candidates(r"C:\Windows\System32;C:\tools\bin");
        assert_eq!(found.first().map(String::as_str), Some(GIT_BASH));
        assert!(found.contains(&PATH_BASH.to_string()));
    }

    #[test]
    fn bash_candidates_uses_given_roots_and_drive() {
        let found = bash_candidates(&[r"D:\Apps".to_string()], "D:", "", SYSTEM_ROOT);
        assert!(found.contains(&r"D:\Apps\Git\bin\bash.exe".to_string()));
        assert!(found.contains(&r"D:\msys64\usr\bin\bash.exe".to_string()));
    }
}
