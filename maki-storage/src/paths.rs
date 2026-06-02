use std::fs;
use std::path::{Path, PathBuf};
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
