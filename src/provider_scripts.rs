//! The executables maki used to run from the config `providers/` directory.
//! Nothing runs them anymore. This module finds the ones no Lua plugin has
//! replaced yet, so startup can say so and `maki migrate providers` can hand
//! the user a prompt that ports them.

use std::fmt::Write;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use maki_providers::plugin;
use maki_providers::spec::ProviderRegistry;
use maki_storage::paths::{self, tilde};
use maki_storage::{StateDir, model, version};

use crate::update;

const PROVIDERS_DIR: &str = "providers";
const INIT_FILE: &str = "init.lua";
pub const MIGRATE_COMMAND: &str = "maki migrate providers";
const PROMPT_TEMPLATE: &str = include_str!("prompts/provider_scripts.md");
const SCRIPTS_SLOT: &str = "{scripts}";
const PROVIDERS_DIR_SLOT: &str = "{providers_dir}";
const CONFIG_DIR_SLOT: &str = "{config_dir}";
const LOGS_DIR_SLOT: &str = "{logs_dir}";
const OLD_MAKI_SLOT: &str = "{old_maki}";
const EXPECTED_MODELS_SLOT: &str = "{expected_models}";
const PATH_SLOT: &str = "{path}";
const OLD_MAKI_STEP: &str = "- My previous maki, which still runs these scripts, is at `{path}`.\n  Before you write any file, run `{path} models` and keep the lines for these slugs: the plugins must list the same models.\n";
const OLD_MAKI_MODELS: &str = "the same models `{path} models` listed for it";
const SCRIPT_MODELS: &str =
    "the models the script's `models` printed, or its base's catalog when it has no `models`";
const USED_MODELS: &str =
    ", including every model I used with it (listed next to the script above)";
const VERSION_FLAG: &str = "--version";
const VERSION_PREFIX: &str = "maki ";
/// Every release before this one still ships the script loader.
const FIRST_RELEASE_WITHOUT_SCRIPTS: &str = "0.5.8";
#[cfg(unix)]
const EXECUTABLE_BITS: u32 = 0o111;
#[cfg(not(unix))]
const SCRIPT_EXTENSIONS: [&str; 4] = ["exe", "bat", "cmd", "ps1"];

pub struct Script {
    pub slug: String,
    pub path: PathBuf,
}

/// What the agent checks a port against, since the running maki can no longer
/// show what a script used to serve.
pub struct Reference {
    pub old_maki: Option<PathBuf>,
    pub saved_models: Vec<String>,
}

impl Reference {
    pub fn find() -> Self {
        Self {
            old_maki: old_maki(),
            saved_models: saved_models(),
        }
    }

    fn used_with<'a>(&'a self, script: &'a Script) -> impl Iterator<Item = &'a String> {
        self.saved_models.iter().filter(|spec| {
            spec.strip_prefix(script.slug.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

/// The binary `maki update` replaced, when it is old enough to still run the
/// scripts, and so can run the porting agent and show what the scripts served.
fn old_maki() -> Option<PathBuf> {
    let path = update::backup_path().ok()?;
    let output = Command::new(&path)
        .arg(VERSION_FLAG)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && runs_scripts(&String::from_utf8_lossy(&output.stdout)))
        .then_some(path)
}

fn runs_scripts(version_output: &str) -> bool {
    version_output
        .trim()
        .strip_prefix(VERSION_PREFIX)
        .is_some_and(|version| version::is_newer(FIRST_RELEASE_WITHOUT_SCRIPTS, version))
}

fn saved_models() -> Vec<String> {
    let Ok(state) = StateDir::resolve() else {
        return Vec::new();
    };
    let mut specs = model::read_recents(&state);
    if let Some(current) = model::read_model(&state)
        && !specs.contains(&current)
    {
        specs.insert(0, current);
    }
    specs
}

/// The directory the script loader read, found the way it found it.
pub fn providers_dir() -> Option<PathBuf> {
    paths::find_config_path(PROVIDERS_DIR)
}

/// Every file the script loader would have run, sorted by slug. A slug maki
/// ships or a non-executable file never loaded, so neither is listed.
pub fn scripts_in(dir: &Path) -> Vec<Script> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut scripts: Vec<Script> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|path| {
            let slug = script_slug(&path)?.to_owned();
            (plugin::is_valid_slug(&slug) && !ProviderRegistry::is_shipped(&slug))
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
    (!scripts.is_empty()).then(|| maki_lua::sanitize_message(&warning(&dir, &scripts)))
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

pub fn prompt(providers_dir: &Path, scripts: &[Script], reference: &Reference) -> String {
    let list = scripts
        .iter()
        .map(|script| script_line(script, reference))
        .collect::<Vec<_>>()
        .join("\n");
    let old_maki = reference
        .old_maki
        .as_ref()
        .map(|path| path.display().to_string());
    let mut expected_models = old_maki.as_ref().map_or_else(
        || SCRIPT_MODELS.to_owned(),
        |path| OLD_MAKI_MODELS.replace(PATH_SLOT, path),
    );
    if scripts
        .iter()
        .any(|script| reference.used_with(script).next().is_some())
    {
        expected_models.push_str(USED_MODELS);
    }
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
        .replace(
            OLD_MAKI_SLOT,
            &old_maki.map_or_else(String::new, |path| OLD_MAKI_STEP.replace(PATH_SLOT, &path)),
        )
        .replace(EXPECTED_MODELS_SLOT, &expected_models)
        .replace(SCRIPTS_SLOT, &list)
}

fn script_line(script: &Script, reference: &Reference) -> String {
    let mut line = format!("- `{}`: {}", script.slug, script.path.display());
    let used: Vec<String> = reference
        .used_with(script)
        .map(|spec| format!("`{spec}`"))
        .collect();
    if !used.is_empty() {
        let _ = write!(line, " (models I used: {})", used.join(", "));
    }
    line
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
    paths::config_dir()
        .ok()
        .filter(|dir| dir.join(INIT_FILE).is_file())
        .unwrap_or_else(|| {
            providers_dir
                .parent()
                .unwrap_or(providers_dir)
                .to_path_buf()
        })
}

/// The slug the script loader gave a file it ran, `None` for one it skipped.
#[cfg(unix)]
fn script_slug(path: &Path) -> Option<&str> {
    let meta = path.metadata().ok()?;
    if !meta.is_file() || meta.permissions().mode() & EXECUTABLE_BITS == 0 {
        return None;
    }
    path.file_name()?.to_str()
}

/// Windows has no executable bit, so the loader ran files by extension and
/// named the provider after the stem: `acme.bat` was `acme`.
#[cfg(not(unix))]
fn script_slug(path: &Path) -> Option<&str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !path.is_file() || !SCRIPT_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    path.file_stem()?.to_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[cfg(unix)]
    const SCRIPT_BODY: &str = "#!/bin/sh\n";
    const PORTABLE: &str = "acme";
    const SECOND: &str = "zeta-proxy";
    const OLD_MAKI: &str = "/state/maki/maki_backup";

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
        write(dir.path(), "deepseek", 0o755);
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let slugs: Vec<String> = scripts_in(dir.path())
            .into_iter()
            .map(|script| script.slug)
            .collect();

        assert_eq!(slugs, [PORTABLE, SECOND]);
    }

    #[cfg(not(unix))]
    #[test]
    fn scripts_in_lists_only_what_the_loader_ran() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "zeta-proxy.CMD",
            "acme.bat",
            "notes.txt",
            "no-extension",
            "anthropic.exe",
        ] {
            fs::write(dir.path().join(name), "").unwrap();
        }

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

    #[test_case(None ; "without_old_maki")]
    #[test_case(Some(OLD_MAKI) ; "with_old_maki")]
    fn prompt_fills_every_slot(old_maki: Option<&str>) {
        let scripts = [script(PORTABLE), script(SECOND)];
        let reference = Reference {
            old_maki: old_maki.map(PathBuf::from),
            saved_models: Vec::new(),
        };
        let text = prompt(Path::new("/cfg/providers"), &scripts, &reference);

        for slot in [
            SCRIPTS_SLOT,
            PROVIDERS_DIR_SLOT,
            CONFIG_DIR_SLOT,
            LOGS_DIR_SLOT,
            OLD_MAKI_SLOT,
            EXPECTED_MODELS_SLOT,
            PATH_SLOT,
        ] {
            assert!(!text.contains(slot), "{slot} left unfilled");
        }
        for script in &scripts {
            let line = format!("- `{}`: {}\n", script.slug, script.path.display());
            assert!(text.contains(&line), "{line:?} missing");
        }
        let old_models = format!("`{OLD_MAKI} models`");
        assert_eq!(text.contains(&old_models), old_maki.is_some());
        assert_eq!(text.contains(SCRIPT_MODELS), old_maki.is_none());
        assert!(!text.contains(USED_MODELS));
    }

    #[test]
    fn prompt_lists_saved_models_under_their_own_script() {
        let scripts = [script(PORTABLE), script(SECOND)];
        let reference = Reference {
            old_maki: None,
            saved_models: ["acme/big", "acme-two/other", "anthropic/opus", "acme/small"]
                .map(String::from)
                .to_vec(),
        };
        let text = prompt(Path::new("/cfg/providers"), &scripts, &reference);

        assert!(
            text.contains(
                "- `acme`: /cfg/providers/acme (models I used: `acme/big`, `acme/small`)\n"
            )
        );
        assert!(text.contains("- `zeta-proxy`: /cfg/providers/zeta-proxy\n"));
        assert!(text.contains(USED_MODELS));
    }

    #[test_case("maki 0.5.7\n", true ; "last_release_with_scripts")]
    #[test_case("maki 0.2.0", true ; "old_release")]
    #[test_case("maki 0.5.8", false ; "first_release_without_scripts")]
    #[test_case("maki 1.0.0", false ; "newer_release")]
    #[test_case("maki garbage", false ; "unparsable_version")]
    #[test_case("", false ; "no_output")]
    fn runs_scripts_only_before_the_removal(output: &str, expected: bool) {
        assert_eq!(runs_scripts(output), expected);
    }
}
