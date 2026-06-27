use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use etcetera::base_strategy::BaseStrategy;

const FALLBACK_DIR: &str = ".maki";
const APP_NAME: &str = "maki";

static STRATEGY: OnceLock<Option<Paths>> = OnceLock::new();

struct Paths {
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    fallback: bool,
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

fn resolve() -> Option<&'static Paths> {
    STRATEGY
        .get_or_init(|| {
            let s = etcetera::choose_base_strategy().ok()?;
            let fallback_dir = etcetera::home_dir()
                .ok()
                .map(|h| h.join(FALLBACK_DIR))
                .filter(|d| d.is_dir());
            let fallback = fallback_dir.is_some();
            let (data, cache) = match fallback_dir {
                Some(dir) => (dir.clone(), dir),
                None => (s.data_dir().join(APP_NAME), s.cache_dir().join(APP_NAME)),
            };
            Some(Paths {
                config: s.config_dir().join(APP_NAME),
                data,
                cache,
                fallback,
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

fn xdg_sibling(data: &Path, name: &str) -> PathBuf {
    data.parent()
        .and_then(|p| p.parent())
        .map(|base| base.join(name).join(APP_NAME))
        .unwrap_or_else(|| data.join(name))
}

pub fn config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    if p.fallback {
        return ensure(&p.data);
    }
    ensure(&p.config)
}

pub fn xdg_config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.config)
}

pub fn data_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.data)
}

pub fn state_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    if p.fallback {
        return ensure(&p.data);
    }
    ensure(&xdg_sibling(&p.data, "state"))
}

pub fn logs_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    if p.fallback {
        return ensure(&p.data);
    }
    ensure(&xdg_sibling(&p.data, "logs"))
}

pub fn cache_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.cache)
}

pub struct XdgPaths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub logs: PathBuf,
}

pub fn xdg_paths() -> Result<XdgPaths, std::io::Error> {
    let s = etcetera::choose_base_strategy().map_err(|_| err())?;
    let data = s.data_dir().join(APP_NAME);
    Ok(XdgPaths {
        config: s.config_dir().join(APP_NAME),
        state: xdg_sibling(&data, "state"),
        logs: xdg_sibling(&data, "logs"),
    })
}

pub fn home() -> Option<PathBuf> {
    etcetera::home_dir().ok()
}

pub fn legacy_home_dir() -> Option<PathBuf> {
    etcetera::home_dir()
        .ok()
        .map(|h| h.join(FALLBACK_DIR))
        .filter(|d| d.is_dir())
}

pub fn user_config_dirs(home: Option<&Path>, subdir: &str) -> Vec<PathBuf> {
    let legacy = home
        .map(|h| h.join(FALLBACK_DIR).join(subdir))
        .or_else(|| legacy_home_dir().map(|d| d.join(subdir)));
    let xdg = config_dir().ok().map(|d| d.join(subdir));
    [legacy, xdg].into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
