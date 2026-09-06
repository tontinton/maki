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
    state: PathBuf,
    logs: PathBuf,
    cache: PathBuf,
    xdg_config: PathBuf,
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
            let fallback_dir = etcetera::home_dir()
                .ok()
                .map(|h| h.join(FALLBACK_DIR))
                .filter(|d| d.is_dir());
            let xdg_config = s.config_dir().join(APP_NAME);
            let (data, cache, config) = match &fallback_dir {
                Some(dir) => (dir.clone(), dir.clone(), dir.clone()),
                None => (
                    s.data_dir().join(APP_NAME),
                    s.cache_dir().join(APP_NAME),
                    xdg_config.clone(),
                ),
            };
            let (state, logs) = if fallback_dir.is_some() {
                (data.clone(), data.clone())
            } else {
                state_logs(&s, &data)
            };
            Some(Paths {
                config,
                data,
                state,
                logs,
                cache,
                xdg_config,
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

pub fn xdg_config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.xdg_config)
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

pub struct XdgPaths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub logs: PathBuf,
}

pub fn xdg_paths() -> Result<XdgPaths, std::io::Error> {
    let s = etcetera::choose_base_strategy().map_err(|_| err())?;
    let data = s.data_dir().join(APP_NAME);
    let (state, logs) = state_logs(&s, &data);
    Ok(XdgPaths {
        config: s.config_dir().join(APP_NAME),
        state,
        logs,
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

/// Where to look for user config, best match first. Writes still go to
/// `config_dir()`.
///
/// The two are not the same: `config_dir()` collapses to `~/.maki` the moment
/// that directory exists, so anything that reads it alone goes blind to
/// `~/.config/maki`, which is where the docs tell people to put their files.
pub fn config_search_dirs() -> Vec<PathBuf> {
    config_search_dirs_from(home().as_deref(), xdg_config_dir().ok().as_deref())
}

pub fn find_config_path(name: &str) -> Option<PathBuf> {
    config_search_dirs()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
}

/// Pure core of `config_search_dirs`: no env reads, no process-home fallback,
/// so tests can hand it tempdirs.
pub fn config_search_dirs_from(home: Option<&Path>, xdg_config: Option<&Path>) -> Vec<PathBuf> {
    let legacy = home.map(|h| h.join(FALLBACK_DIR)).filter(|d| d.is_dir());
    let xdg = xdg_config
        .map(Path::to_path_buf)
        .filter(|d| Some(d) != legacy.as_ref());
    [legacy, xdg].into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const KEYED_FILE: &str = "f.rs";
    const SUBDIR: &str = "sub";

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

    #[test]
    fn search_dirs_returns_legacy_and_xdg() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(FALLBACK_DIR);
        let xdg = home.path().join(".config").join(APP_NAME);
        fs::create_dir(&legacy).unwrap();

        let dirs = config_search_dirs_from(Some(home.path()), Some(&xdg));
        assert_eq!(dirs, vec![legacy, xdg]);
    }

    #[test]
    fn search_dirs_omits_legacy_when_it_does_not_exist() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join(".config").join(APP_NAME);

        let dirs = config_search_dirs_from(Some(home.path()), Some(&xdg));
        assert_eq!(dirs, vec![xdg]);
    }

    #[test]
    fn search_dirs_omits_legacy_when_home_none() {
        let xdg = tempfile::tempdir().unwrap();

        let dirs = config_search_dirs_from(None, Some(xdg.path()));
        assert_eq!(dirs, vec![xdg.path().to_path_buf()]);
    }

    #[test]
    fn search_dirs_omits_xdg_when_xdg_none() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(FALLBACK_DIR);
        fs::create_dir(&legacy).unwrap();

        let dirs = config_search_dirs_from(Some(home.path()), None);
        assert_eq!(dirs, vec![legacy]);
    }

    #[test]
    fn search_dirs_does_not_repeat_the_same_dir() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(FALLBACK_DIR);
        fs::create_dir(&legacy).unwrap();

        let dirs = config_search_dirs_from(Some(home.path()), Some(&legacy));
        assert_eq!(dirs, vec![legacy]);
    }

    #[test]
    fn search_dirs_neither_depends_on_process_env() {
        let home_a = tempfile::tempdir().unwrap();
        let xdg_a = home_a.path().join(".config").join(APP_NAME);

        let hostile = tempfile::tempdir().unwrap();

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: setting a variable is only sound while no other thread reads
        // the environment, and the runner is what holds that up: `just test`
        // runs `cargo nextest`, which gives every test its own process.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", hostile.path()) };

        let dirs = config_search_dirs_from(Some(home_a.path()), Some(&xdg_a));

        // SAFETY: same one process per test rule as above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            !dirs.iter().any(|p| p.starts_with(hostile.path())),
            "combiner read XDG_CONFIG_HOME: {dirs:?}"
        );
    }
}
