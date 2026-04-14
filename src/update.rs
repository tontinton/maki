#[cfg(unix)]
use std::ffi::CString;
use std::io::Write;
use std::path::{Path, PathBuf};

use maki_storage::version::{self, VersionError};
use maki_storage::{DataDir, StorageError};

const INSTALL_SCRIPT_URL: &str = "https://maki.sh/install.sh";
const BACKUP_FILENAME: &str = "maki_backup";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("failed to fetch {url}: {source}")]
    Fetch {
        url: &'static str,
        #[source]
        source: isahc::Error,
    },

    #[error("failed to determine current binary path: {0}")]
    CurrentExe(std::io::Error),

    #[error("failed to backup binary to {path}: {source}")]
    Backup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write install script: {0}")]
    WriteScript(std::io::Error),

    #[error("failed to execute install script: {0}")]
    ExecScript(std::io::Error),

    #[error("install script failed with exit code {0:?}")]
    InstallFailed(Option<i32>),

    #[error("no backup found at {0}")]
    NoBackup(PathBuf),

    #[error("failed to restore backup from {path}: {source}")]
    Restore {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot access data directory: {0}")]
    Storage(#[from] StorageError),

    #[error("failed to check latest version: {0}")]
    VersionCheck(#[from] VersionError),
}

fn fetch_script() -> Result<String, UpdateError> {
    use isahc::ReadResponseExt;
    let mut response = isahc::get(INSTALL_SCRIPT_URL).map_err(|e| UpdateError::Fetch {
        url: INSTALL_SCRIPT_URL,
        source: e,
    })?;
    response.text().map_err(|e| UpdateError::Fetch {
        url: INSTALL_SCRIPT_URL,
        source: e.into(),
    })
}

fn backup_binary(exe_path: &Path, storage: &DataDir) -> Result<PathBuf, UpdateError> {
    let backup_path = storage.path().join(BACKUP_FILENAME);
    std::fs::copy(exe_path, &backup_path).map_err(|e| UpdateError::Backup {
        path: backup_path.clone(),
        source: e,
    })?;
    Ok(backup_path)
}

fn execute_script(script: &str) -> Result<(), UpdateError> {
    let mut tmp = tempfile::NamedTempFile::new().map_err(UpdateError::WriteScript)?;
    tmp.write_all(script.as_bytes())
        .map_err(UpdateError::WriteScript)?;
    tmp.flush().map_err(UpdateError::WriteScript)?;

    let status = std::process::Command::new("sh")
        .arg(tmp.path())
        .status()
        .map_err(UpdateError::ExecScript)?;

    if !status.success() {
        return Err(UpdateError::InstallFailed(status.code()));
    }
    Ok(())
}

#[cfg(unix)]
fn needs_sudo(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let Ok(cpath) = CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    unsafe { libc::access(cpath.as_ptr(), libc::W_OK) != 0 }
}

#[cfg(not(unix))]
fn needs_sudo(_path: &Path) -> bool {
    false
}

fn restore_backup(backup_path: &Path, exe_path: &Path) -> Result<(), UpdateError> {
    let err = |e| UpdateError::Restore {
        path: backup_path.to_path_buf(),
        source: e,
    };

    if needs_sudo(exe_path) {
        println!("Restoring to {} (requires sudo)...", exe_path.display());
        let status = std::process::Command::new("sudo")
            .args(["cp", "--"])
            .arg(backup_path)
            .arg(exe_path)
            .status()
            .map_err(err)?;
        if !status.success() {
            return Err(err(std::io::Error::other("sudo cp failed")));
        }
    } else {
        let tmp = exe_path.with_extension("maki_tmp");
        std::fs::copy(backup_path, &tmp).map_err(err)?;
        std::fs::rename(&tmp, exe_path).map_err(err)?;
    }
    Ok(())
}

fn prompt_yes() -> bool {
    eprint!("Run this script? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
}

pub fn update(skip_confirm: bool, no_color: bool) -> Result<(), UpdateError> {
    let latest = version::fetch_latest()?;
    if !version::is_newer(&latest, version::CURRENT) {
        println!("Already up to date (v{})", version::CURRENT);
        return Ok(());
    }

    println!("Current version: v{}", version::CURRENT);
    println!("Latest version:  v{latest}");
    println!();

    let exe_path = std::env::current_exe().map_err(UpdateError::CurrentExe)?;
    let storage = DataDir::resolve()?;

    let script = fetch_script()?;

    if no_color {
        println!("{script}");
    } else {
        println!("{}", maki_ui::highlight_ansi("bash", &script));
    }

    if !skip_confirm && !prompt_yes() {
        println!("Aborted.");
        return Ok(());
    }

    let backup_path = backup_binary(&exe_path, &storage)?;

    execute_script(&script)?;

    println!();
    println!("Updated successfully.");
    println!("Previous version saved to: {}", backup_path.display());
    println!("To restore: maki rollback");

    Ok(())
}

pub fn rollback() -> Result<(), UpdateError> {
    let exe_path = std::env::current_exe().map_err(UpdateError::CurrentExe)?;
    let storage = DataDir::resolve()?;
    let backup_path = storage.path().join(BACKUP_FILENAME);

    if !backup_path.exists() {
        return Err(UpdateError::NoBackup(backup_path));
    }

    restore_backup(&backup_path, &exe_path)?;

    println!("Restored previous version.");

    Ok(())
}
