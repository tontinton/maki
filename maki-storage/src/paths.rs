use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use etcetera::base_strategy::BaseStrategy;

use crate::StateDir;

const FALLBACK_DIR: &str = ".maki";
const APP_NAME: &str = "maki";
const PROJECTS_DIR: &str = "projects";
const SESSIONS_DIR: &str = "sessions";
const AUTH_DIR: &str = "auth";
pub(crate) const GIT_MARKER: &str = ".git";
const UNNAMED_PROJECT: &str = "root";
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

static STRATEGY: OnceLock<Option<Paths>> = OnceLock::new();

struct Paths {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    cache: PathBuf,
}

/// Lexical path normalization that never hits the filesystem.
///
/// Returns an absolute path with `..` and `.` components resolved, but without
/// calling `canonicalize`. This means no `\\?\` prefix on Windows and no symlink
/// resolution. Use this for display, logging, and scope matching.
pub fn normalize_path(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    normalize_abs_path(&abs)
}

fn normalize_abs_path(abs: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::ParentDir => {
                // Only pop if the trailing component is a normal directory,
                // never a root or prefix.
                if let Some(Component::Normal(_)) = result.components().next_back() {
                    result.pop();
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Canonicalize a path (resolving symlinks) but strip the `\\?\` prefix
/// that Windows adds. Falls back to `normalize_path` if the path does not
/// exist yet.
///
/// Contract: the input is a "normal" path (no `\\?\` prefix). The output is
/// always display-friendly: no `\\?\`, no `..` components. On Windows UNC
/// paths (`\\?\UNC\server\share`), the result is `\\server\share`.
///
/// The result is for display, logging, and scope matching only. Do not pass
/// it to Win32 filesystem APIs if the path exceeds 260 characters (the
/// `\\?\` prefix is what bypasses that limit).
pub fn canonicalize_clean(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(canon) => strip_windows_extended_prefix(&canon),
        Err(_) => normalize_path(path),
    }
}

/// Canonicalize a path by resolving each component left-to-right through
/// the filesystem.
///
/// At each step, the accumulated path is canonicalized so that symlinks
/// are resolved *before* a subsequent `..` component can traverse through
/// them. For non-existent tail components, falls back to lexical append.
///
/// This is the correct canonicalization for security-sensitive path checks
/// (boundary verification, scope matching) where symlink escapes matter.
/// Unlike `canonicalize_clean`, this never resolves `..` lexically when
/// a symlink is in play.
///
/// Returns `None` if the root/prefix portion of the path cannot be resolved.
pub fn incremental_canonicalize(path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let next = current.join("..");
                if let Ok(canon) = next.canonicalize() {
                    current = strip_windows_extended_prefix(&canon);
                } else if let Some(Component::Normal(_)) = current.components().next_back() {
                    current.pop();
                }
            }
            Component::Normal(name) => {
                let next = current.join(name);
                match next.canonicalize() {
                    Ok(canon) => current = strip_windows_extended_prefix(&canon),
                    Err(_) => {
                        // `current` is already canonical from a prior iteration,
                        // so we can append the non-existent tail directly without
                        // re-resolving the parent.
                        current = next;
                    }
                }
            }
        }
    }

    if current.as_os_str().is_empty() {
        None
    } else {
        Some(current)
    }
}

/// Resolve a leading `~`. The one answer to what a tilde means, because a
/// spelling one layer expands and another does not is two names for one file.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), home()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// The identity of a file, independent of how a path was spelled: relative or
/// absolute, with `..` or not, through a symlink or not, under `~` or spelled
/// out, existing or not yet.
///
/// Over-resolving is safe here; under-resolving is the bug, because two keys
/// for one file mean two locks for one file, or a staleness check that looks
/// up an entry nobody wrote.
pub fn canonical_key(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let abs = std::path::absolute(&expanded).unwrap_or(expanded);
    incremental_canonicalize(&abs).unwrap_or_else(|| normalize_path(&abs))
}

/// Per-project state, laid out exactly like the memory plugin's
/// `projects/<id>/` so both land in one folder: the project is the nearest
/// ancestor of `cwd` holding a `.git`, else `cwd` itself, taken as spelled.
pub fn project_dir(state: &StateDir, cwd: &Path) -> PathBuf {
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(GIT_MARKER).exists())
        .unwrap_or(cwd);
    state.path().join(PROJECTS_DIR).join(project_id(root))
}

fn project_id(root: &Path) -> String {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(UNNAMED_PROJECT);
    let hash = fnv1a_64(root.as_os_str().as_encoded_bytes());
    format!("{name}-{hash:016x}")
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Strip the `\\?\` prefix that Windows `canonicalize` adds, using the
/// Rust `Prefix` enum for correct WTF-8 handling (no `.to_str()` lossy
/// conversion).
///
/// `\\?\C:\foo` becomes `C:\foo`.
/// `\\?\UNC\server\share\dir` becomes `\\server\share\dir`.
///
/// **Contract**: the result is for display, logging, and scope matching only.
/// Do not pass it to Win32 filesystem APIs if the path exceeds 260 characters
/// (the `\\?\` prefix is what bypasses that limit).
#[cfg(windows)]
fn strip_windows_extended_prefix(canon: &Path) -> PathBuf {
    use std::path::Prefix;

    let mut components = canon.components();
    let Some(Component::Prefix(pfx)) = components.next() else {
        return canon.to_path_buf();
    };
    let rest = components.as_path();
    match pfx.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)).join(rest),
        Prefix::VerbatimUNC(server, share) => {
            let mut base = PathBuf::from(r"\\");
            base.push(server);
            base.push(share);
            base.join(rest)
        }
        _ => canon.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn strip_windows_extended_prefix(canon: &Path) -> PathBuf {
    canon.to_path_buf()
}

fn state_logs(s: &impl BaseStrategy, fallback: &Path) -> (PathBuf, PathBuf) {
    let state_base = s.state_dir();
    let state = state_base
        .as_ref()
        .map(|d| d.join(APP_NAME))
        .unwrap_or_else(|| fallback.to_path_buf());
    let logs = state_base
        .as_ref()
        .and_then(|d| d.parent().map(|p| p.join("logs").join(APP_NAME)))
        .unwrap_or_else(|| fallback.to_path_buf());
    (state, logs)
}

fn resolve() -> Option<&'static Paths> {
    STRATEGY
        .get_or_init(|| {
            let s = etcetera::choose_base_strategy().ok()?;
            let data = s.data_dir().join(APP_NAME);
            let (state, logs) = state_logs(&s, &data);
            Some(Paths {
                config: s.config_dir().join(APP_NAME),
                data,
                state,
                logs,
                cache: s.cache_dir().join(APP_NAME),
            })
        })
        .as_ref()
}

fn err() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot determine base directories",
    )
}

fn ensure(path: &Path) -> Result<PathBuf, std::io::Error> {
    fs::create_dir_all(path)?;
    Ok(path.to_path_buf())
}

pub fn config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.config)
}

pub fn data_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.data)
}

pub fn state_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.state)
}

pub fn logs_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.logs)
}

pub fn cache_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.cache)
}

pub fn home() -> Option<PathBuf> {
    etcetera::home_dir().ok()
}

/// `path` as a user would type it, with the home directory as `~`.
pub fn tilde(path: &Path) -> String {
    match home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The `~/.maki` directory of the pre-XDG layout, if it exists. Only migration
/// reads this; nothing in the running app resolves paths under it.
pub fn legacy_home_dir() -> Option<PathBuf> {
    etcetera::home_dir()
        .ok()
        .map(|h| h.join(FALLBACK_DIR))
        .filter(|d| d.is_dir())
}

/// `~/.maki` only when it holds maki's own data, so a stray directory of that
/// name never reads as a legacy install worth warning about.
pub fn legacy_data_dir() -> Option<PathBuf> {
    let legacy = legacy_home_dir()?;
    [SESSIONS_DIR, AUTH_DIR, PROJECTS_DIR]
        .iter()
        .any(|name| legacy.join(name).exists())
        .then_some(legacy)
}

/// The global config file `name`, if it exists. Writes still go through
/// `config_dir()`, so a miss here means the file was never created.
pub fn find_config_path(name: &str) -> Option<PathBuf> {
    let path = config_dir().ok()?.join(name);
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const KEYED_FILE: &str = "f.rs";
    const SUBDIR: &str = "sub";
    const STATE_SUBDIR: &str = "state";

    #[test_case(b"", 0xcbf29ce484222325 ; "empty")]
    #[test_case(b"a", 0xaf63dc4c8601ec8c ; "single_byte")]
    fn fnv1a_64_matches_reference_vectors(input: &[u8], expected: u64) {
        assert_eq!(fnv1a_64(input), expected);
    }

    #[test_case("/home/user/maki", "maki-87a295246017c748" ; "named_dir")]
    #[test_case("/", "root-af63a24c860189fe" ; "filesystem_root")]
    fn project_id_matches_memory_plugin(root: &str, expected: &str) {
        assert_eq!(project_id(Path::new(root)), expected);
    }

    #[test]
    fn a_dir_outside_any_repo_is_its_own_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = StateDir::from_path(tmp.path().join(STATE_SUBDIR));
        let dir = tmp.path().join(SUBDIR);
        fs::create_dir(&dir).unwrap();

        assert_eq!(
            project_dir(&state, &dir),
            state.path().join(PROJECTS_DIR).join(project_id(&dir))
        );
    }

    #[test_case(|_rel, abs| abs.join(KEYED_FILE); "absolute")]
    #[test_case(|rel, _abs| rel.join(KEYED_FILE); "relative")]
    #[test_case(|rel, _abs| rel.join(SUBDIR).join("..").join(KEYED_FILE); "parent_component")]
    fn every_spelling_of_one_file_is_one_key(spell: fn(&Path, &Path) -> PathBuf) {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::TempDir::new_in(&cwd).unwrap();
        let abs = dir.path();
        let rel = PathBuf::from(abs.file_name().unwrap());
        fs::create_dir(abs.join(SUBDIR)).unwrap();

        let expected = canonical_key(&abs.join(KEYED_FILE));
        assert_eq!(
            canonical_key(&spell(&rel, abs)),
            expected,
            "before the file exists"
        );

        fs::write(abs.join(KEYED_FILE), "content").unwrap();
        assert_eq!(
            canonical_key(&spell(&rel, abs)),
            expected,
            "once the file exists"
        );
    }

    #[test]
    fn tilde_spelling_is_one_key() {
        let home = home().expect("no home dir");
        assert_eq!(
            canonical_key(Path::new("~").join(KEYED_FILE).as_path()),
            canonical_key(&home.join(KEYED_FILE))
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_spelling_is_one_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join(SUBDIR);
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        fs::write(real.join(KEYED_FILE), "content").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            canonical_key(&link.join(KEYED_FILE)),
            canonical_key(&real.join(KEYED_FILE))
        );
    }

    #[test]
    fn normalize_path_resolves_parent() {
        let cwd = std::env::current_dir().unwrap();
        let input = cwd.join("a").join("b").join("..").join("c");
        let expected = cwd.join("a").join("c");
        assert_eq!(normalize_path(&input), expected);
    }

    #[test]
    fn normalize_path_resolves_dot() {
        let cwd = std::env::current_dir().unwrap();
        let input = cwd.join("a").join(".").join("b");
        let expected = cwd.join("a").join("b");
        assert_eq!(normalize_path(&input), expected);
    }

    #[test]
    fn normalize_path_does_not_pop_past_root() {
        // /../etc should produce /etc, not the relative "etc"
        let result = normalize_path(Path::new("/../etc"));
        assert!(result.is_absolute(), "must stay absolute: {result:?}");
        #[cfg(unix)]
        assert_eq!(result, PathBuf::from("/etc"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_local_drive() {
        let input = Path::new(r"\\?\C:\Users\test\file.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"C:\Users\test\file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_unc_share() {
        let input = Path::new(r"\\?\UNC\server\share\dir\file.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"\\server\share\dir\file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_no_prefix() {
        let input = Path::new(r"C:\already\normal\path.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"C:\already\normal\path.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn canonicalize_clean_strips_extended_prefix() {
        let tmp = std::env::temp_dir();
        let result = canonicalize_clean(&tmp);
        let s = result.to_str().unwrap();
        assert!(
            !s.starts_with(r"\\?\"),
            "should not have \\\\?\\ prefix: {s}"
        );
    }
}
