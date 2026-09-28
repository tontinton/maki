//! The executables maki used to run from the config `providers/` directory.
//! Nothing runs them anymore. This module finds the ones no Lua plugin has
//! replaced yet, so startup can say so and `maki migrate providers` can hand
//! the user a prompt that ports them.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use maki_providers::plugin;
use maki_providers::spec::ProviderRegistry;
use maki_storage::paths::{self, tilde};

const PROVIDERS_DIR: &str = "providers";
const INIT_FILE: &str = "init.lua";
pub const MIGRATE_COMMAND: &str = "maki migrate providers";
const PROMPT_TEMPLATE: &str = include_str!("prompts/provider_scripts.md");
const SCRIPTS_SLOT: &str = "{scripts}";
const PROVIDERS_DIR_SLOT: &str = "{providers_dir}";
const CONFIG_DIR_SLOT: &str = "{config_dir}";
const LOGS_DIR_SLOT: &str = "{logs_dir}";
#[cfg(unix)]
const EXECUTABLE_BITS: u32 = 0o111;

pub struct Script {
    pub slug: String,
    pub path: PathBuf,
}

/// The directory the script loader read, found the way it found it.
pub fn providers_dir() -> Option<PathBuf> {
    paths::find_config_path(PROVIDERS_DIR)
}

/// Every file the script loader would have run, sorted by slug. A built-in
/// slug or a non-executable file never loaded, so neither is listed.
pub fn scripts_in(dir: &Path) -> Vec<Script> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut scripts: Vec<Script> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_executable_file(path))
        .filter_map(|path| {
            let slug = path.file_name()?.to_str()?.to_owned();
            (plugin::is_valid_slug(&slug) && ProviderRegistry::get(&slug).is_none())
                .then_some(Script { slug, path })
        })
        .collect();
    scripts.sort_by(|a, b| a.slug.cmp(&b.slug));
    scripts
}

/// Scripts whose slug no plugin registered. Only meaningful once plugins have
/// loaded, which is also what makes a ported script drop off the list.
pub fn unported(dir: &Path) -> Vec<Script> {
    scripts_in(dir)
        .into_iter()
        .filter(|script| !plugin::is_registered(&script.slug))
        .collect()
}

pub fn startup_warning() -> Option<String> {
    let dir = providers_dir()?;
    let scripts = unported(&dir);
    (!scripts.is_empty()).then(|| warning(&dir, &scripts))
}

/// Why `slug` is unknown, when an unported script is what used to serve it.
pub fn unknown_provider_hint(slug: &str) -> Option<String> {
    let dir = providers_dir()?;
    unported(&dir)
        .iter()
        .any(|script| script.slug == slug)
        .then(|| {
            format!(
                "`{slug}` is a provider script in {}, which maki no longer runs. Run `{MIGRATE_COMMAND}` for a prompt that ports it to a Lua plugin",
                tilde(&dir)
            )
        })
}

pub fn prompt(providers_dir: &Path, scripts: &[Script]) -> String {
    let list = scripts
        .iter()
        .map(|script| format!("- `{}`: {}", script.slug, script.path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let logs_dir = paths::logs_dir().map_or_else(
        |_| "maki's log directory".to_owned(),
        |dir| dir.display().to_string(),
    );
    PROMPT_TEMPLATE
        .replace(PROVIDERS_DIR_SLOT, &providers_dir.display().to_string())
        .replace(
            CONFIG_DIR_SLOT,
            &plugin_dir(providers_dir).display().to_string(),
        )
        .replace(LOGS_DIR_SLOT, &logs_dir)
        .replace(SCRIPTS_SLOT, &list)
}

fn warning(dir: &Path, scripts: &[Script]) -> String {
    let slugs = scripts
        .iter()
        .map(|script| format!("`{}`", script.slug))
        .collect::<Vec<_>>()
        .join(", ");
    let (verb, object) = match scripts {
        [_] => ("is", "it to a Lua plugin"),
        _ => ("are", "them to Lua plugins"),
    };
    format!(
        "maki no longer runs provider scripts, so {slugs} in {} {verb} not loaded. Run `{MIGRATE_COMMAND}` for a prompt that ports {object}",
        tilde(dir)
    )
}

/// Where the global `init.lua` runs from, since a ported plugin has to be
/// `require`d by it. Without one yet, the directory holding the scripts.
fn plugin_dir(providers_dir: &Path) -> PathBuf {
    paths::config_search_dirs()
        .into_iter()
        .find(|dir| dir.join(INIT_FILE).is_file())
        .unwrap_or_else(|| {
            providers_dir
                .parent()
                .unwrap_or(providers_dir)
                .to_path_buf()
        })
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & EXECUTABLE_BITS != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[cfg(unix)]
    const SCRIPT_BODY: &str = "#!/bin/sh\n";
    const PORTABLE: &str = "acme";
    const SECOND: &str = "zeta-proxy";

    fn script(slug: &str) -> Script {
        Script {
            slug: slug.to_owned(),
            path: PathBuf::from(format!("/cfg/providers/{slug}")),
        }
    }

    #[cfg(unix)]
    fn write(dir: &Path, name: &str, mode: u32) {
        let path = dir.join(name);
        fs::write(&path, SCRIPT_BODY).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scripts_in_lists_only_what_the_loader_ran() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), SECOND, 0o755);
        write(dir.path(), PORTABLE, 0o700);
        write(dir.path(), "not-executable", 0o644);
        write(dir.path(), ".hidden", 0o755);
        write(dir.path(), "anthropic", 0o755);
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let slugs: Vec<String> = scripts_in(dir.path())
            .into_iter()
            .map(|script| script.slug)
            .collect();

        assert_eq!(slugs, [PORTABLE, SECOND]);
    }

    #[test]
    fn scripts_in_a_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scripts_in(&dir.path().join(PROVIDERS_DIR)).is_empty());
    }

    #[test_case(&[PORTABLE], "`acme` in", "is not loaded", "ports it to a Lua plugin" ; "one_script")]
    #[test_case(&[PORTABLE, SECOND], "`acme`, `zeta-proxy` in", "are not loaded", "ports them to Lua plugins" ; "two_scripts")]
    fn warning_names_every_script_in_number(
        slugs: &[&str],
        names: &str,
        loaded: &str,
        ports: &str,
    ) {
        let scripts: Vec<Script> = slugs.iter().map(|slug| script(slug)).collect();
        let text = warning(Path::new("/cfg/providers"), &scripts);
        for part in [names, loaded, ports, MIGRATE_COMMAND] {
            assert!(text.contains(part), "{part:?} missing from {text:?}");
        }
    }

    #[test]
    fn prompt_fills_every_slot() {
        let scripts = [script(PORTABLE), script(SECOND)];
        let text = prompt(Path::new("/cfg/providers"), &scripts);

        for slot in [
            SCRIPTS_SLOT,
            PROVIDERS_DIR_SLOT,
            CONFIG_DIR_SLOT,
            LOGS_DIR_SLOT,
        ] {
            assert!(!text.contains(slot), "{slot} left unfilled");
        }
        for script in &scripts {
            let line = format!("- `{}`: {}", script.slug, script.path.display());
            assert!(text.contains(&line), "{line:?} missing");
        }
    }
}
