use std::process::Command;

use maki_config::ShellPreference;
use maki_config::providers::{ProvidersConfig, resolve_api_key_env};
use maki_config::{PROVIDER_BUILTINS, env_var_refs};

use crate::providers::anthropic::bedrock;
use crate::providers::catalog;
use crate::providers::copilot::auth as copilot_auth;
use crate::spec::ProviderRegistry;

/// Credentials other CLIs read too (gh, huggingface-cli, doctl, databricks,
/// wrangler, snowsql, wandb, vultr-cli). A models.dev entry or a header can
/// name one, and stripping it would break those tools in the bash tool with no
/// way for the user to put it back.
const SHARED_CREDENTIAL_VARS: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "HF_TOKEN",
    "DIGITALOCEAN_ACCESS_TOKEN",
    "DATABRICKS_HOST",
    "DATABRICKS_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_API_KEY",
    "SNOWFLAKE_ACCOUNT",
    "WANDB_API_KEY",
    "VULTR_API_KEY",
];

/// A bash command could print a provider key into the model's context, and an
/// MCP server is someone else's code, so neither gets the keys maki reads. Env
/// set on `cmd` after this still goes through, which is how an MCP server's
/// `environment` hands over a key on purpose.
///
/// The list is built on every call, so an edit to `providers.toml` or a
/// catalog that warmed up late counts from the next spawn.
pub fn strip_provider_keys(cmd: &mut Command) -> &mut Command {
    let config = ProvidersConfig::load_or_default();
    for var in provider_key_vars(&config, catalog::key_vars_if_available()) {
        cmd.env_remove(var);
    }
    cmd
}

/// `providers.toml` cannot change the `api_key_env` of a known slug, so that
/// var is never a key maki reads. Its `headers` are still sent though, so
/// their `${VAR}`s count.
fn provider_key_vars(config: &ProvidersConfig, catalog_vars: Vec<String>) -> Vec<String> {
    let known = ProviderRegistry::all()
        .into_iter()
        .map(|spec| spec.api_key_env)
        .chain(copilot_auth::TOKEN_ENV_VARS.iter().copied())
        .chain([bedrock::BEARER_TOKEN_ENV])
        .filter(|var| !var.is_empty())
        .map(str::to_owned);
    // The key stays the user's secret even while its plugin is off or still
    // loading.
    let bundled = PROVIDER_BUILTINS
        .iter()
        .map(|slug| resolve_api_key_env(slug, None));
    let custom = config
        .providers
        .iter()
        .filter(|(slug, _)| ProviderRegistry::get(slug).is_none())
        .map(|(slug, def)| resolve_api_key_env(slug, Some(def)));
    let header_refs = config
        .providers
        .values()
        .flat_map(|def| def.headers.values())
        .flat_map(|value| env_var_refs(value))
        .map(str::to_owned);
    known
        .chain(bundled)
        .chain(custom)
        .chain(header_refs)
        .chain(catalog_vars)
        .filter(|var| !SHARED_CREDENTIAL_VARS.contains(&var.as_str()))
        .collect()
}

pub fn shell_command(cmd: &str, pref: &ShellPreference) -> Command {
    #[cfg(unix)]
    {
        let _ = pref;
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    }
    #[cfg(windows)]
    {
        windows_shell_command(cmd, pref)
    }
}

#[cfg(any(windows, test))]
mod windows {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use maki_config::ShellPreference;

    const GIT_EXE: &str = "git.exe";
    const BASH_EXE: &str = "bash.exe";
    const CMD_EXE: &str = "cmd.exe";

    pub fn shell_command(cmd: &str, pref: &ShellPreference) -> Command {
        match resolve_shell(pref) {
            ResolvedShell::Cmd => {
                let mut c = Command::new(CMD_EXE);
                c.arg("/C").arg(cmd);
                c
            }
            ResolvedShell::BashLike(program) => {
                let mut c = Command::new(program);
                c.arg("-c").arg(cmd);
                c
            }
        }
    }

    enum ResolvedShell {
        Cmd,
        BashLike(PathBuf),
    }

    fn resolve_shell(pref: &ShellPreference) -> ResolvedShell {
        match pref {
            ShellPreference::Cmd => ResolvedShell::Cmd,
            ShellPreference::Program(path) => program_shell(path),
            ShellPreference::Auto => discover_git_bash()
                .map(ResolvedShell::BashLike)
                .unwrap_or(ResolvedShell::Cmd),
        }
    }

    fn program_shell(path: &Path) -> ResolvedShell {
        let s = path.to_string_lossy();
        let name = s.rsplit(['/', '\\']).next().unwrap_or(&s);
        if name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd") {
            ResolvedShell::Cmd
        } else {
            ResolvedShell::BashLike(path.to_path_buf())
        }
    }

    pub fn discover_git_bash() -> Option<PathBuf> {
        discover_git_bash_on_path().or_else(fallback_git_bash)
    }

    fn discover_git_bash_on_path() -> Option<PathBuf> {
        let git = find_on_path(GIT_EXE)?;
        bash_next_to_git(&git).filter(|bash| !is_wsl_bash(bash))
    }

    pub fn bash_next_to_git(git: &Path) -> Option<PathBuf> {
        if git.is_symlink()
            && let Ok(target) = std::fs::canonicalize(git)
            && let Some(bash) = bash_next_to_git_path(&target)
        {
            return Some(bash);
        }
        bash_next_to_git_path(git)
    }

    fn bash_next_to_git_path(git: &Path) -> Option<PathBuf> {
        let parent = git.parent()?;
        let file_name = parent.file_name()?;

        if file_name.eq_ignore_ascii_case("cmd")
            && let Some(root) = parent.parent()
        {
            let candidate = root.join("bin").join(BASH_EXE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        if file_name.eq_ignore_ascii_case("bin") {
            let candidate = parent.join(BASH_EXE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        if file_name.eq_ignore_ascii_case("shims")
            && let Some(scoop_root) = parent.parent()
        {
            let candidate = scoop_root
                .join("apps")
                .join("git")
                .join("current")
                .join("bin")
                .join(BASH_EXE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        let direct_candidates = [parent.join(BASH_EXE), parent.join("bin").join(BASH_EXE)];
        direct_candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
    }

    pub fn fallback_git_bash() -> Option<PathBuf> {
        fallback_git_bash_candidates()
            .into_iter()
            .find(|candidate| candidate.is_file() && !is_wsl_bash(candidate))
    }

    pub fn fallback_git_bash_candidates() -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        if let Some(scoop) = env::var_os("SCOOP") {
            candidates.push(
                PathBuf::from(scoop)
                    .join("apps")
                    .join("git")
                    .join("current")
                    .join("bin")
                    .join(BASH_EXE),
            );
        } else if let Some(user_profile) = env::var_os("USERPROFILE") {
            candidates.push(
                PathBuf::from(user_profile)
                    .join("scoop")
                    .join("apps")
                    .join("git")
                    .join("current")
                    .join("bin")
                    .join(BASH_EXE),
            );
        }

        if let Some(prog_data) = env::var_os("ProgramData") {
            candidates.push(
                PathBuf::from(prog_data)
                    .join("scoop")
                    .join("apps")
                    .join("git")
                    .join("current")
                    .join("bin")
                    .join(BASH_EXE),
            );
        }

        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local_app_data)
                    .join("Programs")
                    .join("Git")
                    .join("bin")
                    .join(BASH_EXE),
            );
        }

        if let Some(prog_files) = env::var_os("ProgramFiles") {
            candidates.push(
                PathBuf::from(prog_files)
                    .join("Git")
                    .join("bin")
                    .join(BASH_EXE),
            );
        }
        if let Some(prog_files_x86) = env::var_os("ProgramFiles(x86)") {
            candidates.push(
                PathBuf::from(prog_files_x86)
                    .join("Git")
                    .join("bin")
                    .join(BASH_EXE),
            );
        }

        candidates.push(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"));
        candidates.push(PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"));

        candidates
    }

    fn find_on_path(name: &str) -> Option<PathBuf> {
        let path_var = env::var_os("PATH")?;
        env::split_paths(&path_var)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    }

    pub fn is_wsl_bash(path: &Path) -> bool {
        if let Some(sys_root) = env::var_os("SystemRoot").or_else(|| env::var_os("windir")) {
            let wsl = PathBuf::from(sys_root).join("System32").join(BASH_EXE);
            if path.as_os_str().eq_ignore_ascii_case(wsl.as_os_str()) {
                return true;
            }
        }
        let s = path.to_string_lossy();
        s.replace('/', "\\")
            .to_ascii_lowercase()
            .ends_with(r"\system32\bash.exe")
    }
}

#[cfg(any(windows, test))]
pub use windows::{
    bash_next_to_git, discover_git_bash, fallback_git_bash_candidates, is_wsl_bash,
    shell_command as windows_shell_command,
};

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;

    use maki_config::ShellPreference;
    use maki_config::providers::ProviderDef;
    use test_case::test_case;

    use super::*;
    use crate::providers::anthropic;

    const CUSTOM_SLUG: &str = "my-proxy";
    const CUSTOM_KEY_ENV: &str = "MY_PROXY_SECRET";
    const DEFAULT_SLUG: &str = "other-proxy";
    const DEFAULT_KEY_ENV: &str = "OTHER_PROXY_API_KEY";
    const IGNORED_KEY_ENV: &str = "MAKI_TEST_IGNORED_KEY";
    const GATEWAY_HEADER: &str = "CF-Access-Client-Secret";
    const GATEWAY_SECRET_ENV: &str = "CF_ACCESS_CLIENT_SECRET";
    const SHARED_HEADER: &str = "X-GitHub-Token";
    const SHARED_TOKEN: &str = "GITHUB_TOKEN";
    const CATALOG_KEY_ENV: &str = "FIREWORKS_API_KEY";
    const SHARED_CATALOG_KEY_ENV: &str = "HF_TOKEN";
    const UNRELATED_VAR: &str = "MAKI_TEST_UNRELATED";
    /// Its plugin never loads in this crate's tests.
    const BUNDLED_KEY_ENV: &str = "MISTRAL_API_KEY";
    #[cfg(unix)]
    const SECRET: &str = "sk-secret";

    fn config() -> ProvidersConfig {
        let with_env = |env: &str| ProviderDef {
            api_key_env: Some(env.into()),
            ..Default::default()
        };
        let builtin_with_headers = ProviderDef {
            headers: BTreeMap::from([
                (GATEWAY_HEADER.into(), format!("${{{GATEWAY_SECRET_ENV}}}")),
                (SHARED_HEADER.into(), format!("${{{SHARED_TOKEN}}}")),
            ]),
            ..with_env(IGNORED_KEY_ENV)
        };
        ProvidersConfig {
            providers: HashMap::from([
                (CUSTOM_SLUG.into(), with_env(CUSTOM_KEY_ENV)),
                (DEFAULT_SLUG.into(), ProviderDef::default()),
                (anthropic::SPEC.slug.into(), builtin_with_headers),
            ]),
        }
    }

    #[test_case(anthropic::SPEC.api_key_env, true ; "builtin_key")]
    #[test_case(copilot_auth::TOKEN_ENV_VARS[1], true ; "copilot_fallback_token")]
    #[test_case(bedrock::BEARER_TOKEN_ENV, true ; "bedrock_bearer_token")]
    #[test_case(CUSTOM_KEY_ENV, true ; "custom_api_key_env")]
    #[test_case(DEFAULT_KEY_ENV, true ; "custom_default_slug_key")]
    #[test_case(IGNORED_KEY_ENV, false ; "builtin_api_key_env_override_ignored")]
    #[test_case(GATEWAY_SECRET_ENV, true ; "header_ref")]
    #[test_case(SHARED_TOKEN, false ; "shared_credential_in_header_kept")]
    #[test_case(CATALOG_KEY_ENV, true ; "catalog_key")]
    #[test_case(SHARED_CATALOG_KEY_ENV, false ; "shared_credential_in_catalog_kept")]
    #[test_case(UNRELATED_VAR, false ; "unrelated_var_kept")]
    #[test_case(BUNDLED_KEY_ENV, true ; "unloaded_bundled_plugin_key")]
    fn provider_key_vars_membership(var: &str, stripped: bool) {
        let catalog_vars = vec![CATALOG_KEY_ENV.into(), SHARED_CATALOG_KEY_ENV.into()];
        assert_eq!(
            provider_key_vars(&config(), catalog_vars)
                .iter()
                .any(|v| v == var),
            stripped
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_sees_only_keys_set_after_strip() {
        let inherited = anthropic::SPEC.api_key_env;
        let explicit = bedrock::BEARER_TOKEN_ENV;
        let mut cmd = Command::new("env");
        cmd.env(inherited, SECRET);
        let output = strip_provider_keys(&mut cmd)
            .env(explicit, SECRET)
            .output()
            .unwrap();
        let env = String::from_utf8(output.stdout).unwrap();
        assert!(!env.contains(&format!("{inherited}=")));
        assert!(env.contains(&format!("{explicit}={SECRET}")));
    }

    mod windows_shell {
        use std::path::{Path, PathBuf};

        use maki_config::ShellPreference;
        use test_case::test_case;

        const CMD: &str = "echo hello";
        const BASH_EXE_NAME: &str = "bash.exe";
        const GIT_EXE_NAME: &str = "git.exe";

        #[test]
        fn shell_command_uses_configured_bash() {
            let dir = tempfile::tempdir().unwrap();
            let bash_path = dir.path().join(BASH_EXE_NAME);
            std::fs::write(&bash_path, []).unwrap();
            let pref = ShellPreference::Program(bash_path.clone());

            let cmd = crate::child_env::windows_shell_command(CMD, &pref);
            assert_eq!(cmd.get_program(), bash_path.as_os_str());
            let args: Vec<_> = cmd.get_args().collect();
            assert_eq!(args, ["-c", CMD]);
        }

        #[test]
        fn shell_command_uses_cmd_when_configured() {
            let pref = ShellPreference::Cmd;
            let cmd = crate::child_env::windows_shell_command(CMD, &pref);
            assert_eq!(cmd.get_program(), "cmd.exe");
            let args: Vec<_> = cmd.get_args().collect();
            assert_eq!(args, ["/C", CMD]);
        }

        #[test]
        fn shell_command_uses_cmd_when_custom_program_is_cmd() {
            let pref = ShellPreference::Program(PathBuf::from(r"C:\Windows\System32\cmd.exe"));
            let cmd = crate::child_env::windows_shell_command(CMD, &pref);
            assert_eq!(cmd.get_program(), "cmd.exe");
            let args: Vec<_> = cmd.get_args().collect();
            assert_eq!(args, ["/C", CMD]);
        }

        #[test]
        fn bash_next_to_git_cmd_layout() {
            let dir = tempfile::tempdir().unwrap();
            let cmd_dir = dir.path().join("Git").join("cmd");
            let bin_dir = dir.path().join("Git").join("bin");
            std::fs::create_dir_all(&cmd_dir).unwrap();
            std::fs::create_dir_all(&bin_dir).unwrap();
            let git = cmd_dir.join(GIT_EXE_NAME);
            let bash = bin_dir.join(BASH_EXE_NAME);
            std::fs::write(&git, []).unwrap();
            std::fs::write(&bash, []).unwrap();

            assert_eq!(
                crate::child_env::bash_next_to_git(&git).as_deref(),
                Some(bash.as_path())
            );
        }

        #[test]
        fn bash_next_to_git_bin_layout() {
            let dir = tempfile::tempdir().unwrap();
            let bin_dir = dir.path().join("Git").join("bin");
            std::fs::create_dir_all(&bin_dir).unwrap();
            let git = bin_dir.join(GIT_EXE_NAME);
            let bash = bin_dir.join(BASH_EXE_NAME);
            std::fs::write(&git, []).unwrap();
            std::fs::write(&bash, []).unwrap();

            assert_eq!(
                crate::child_env::bash_next_to_git(&git).as_deref(),
                Some(bash.as_path())
            );
        }

        #[test]
        fn bash_next_to_git_scoop_shims_layout() {
            let dir = tempfile::tempdir().unwrap();
            let shims_dir = dir.path().join("scoop").join("shims");
            let git_bin_dir = dir
                .path()
                .join("scoop")
                .join("apps")
                .join("git")
                .join("current")
                .join("bin");
            std::fs::create_dir_all(&shims_dir).unwrap();
            std::fs::create_dir_all(&git_bin_dir).unwrap();
            let git_shim = shims_dir.join(GIT_EXE_NAME);
            let bash = git_bin_dir.join(BASH_EXE_NAME);
            std::fs::write(&git_shim, []).unwrap();
            std::fs::write(&bash, []).unwrap();

            assert_eq!(
                crate::child_env::bash_next_to_git(&git_shim).as_deref(),
                Some(bash.as_path())
            );
        }

        #[test_case(r"C:\Windows\System32\bash.exe", true ; "wsl_bash_uppercase")]
        #[test_case(r"c:\windows\system32\bash.exe", true ; "wsl_bash_lowercase")]
        #[test_case(r"C:\Program Files\Git\bin\bash.exe", false ; "git_bash")]
        #[test_case(r"D:\scoop\apps\git\current\bin\bash.exe", false ; "scoop_git_bash")]
        fn wsl_bash_detection(path: &str, is_wsl: bool) {
            assert_eq!(crate::child_env::is_wsl_bash(Path::new(path)), is_wsl);
        }

        #[test]
        fn fallback_candidates_include_standard_paths() {
            let candidates = crate::child_env::fallback_git_bash_candidates();
            assert!(candidates.iter().any(|c| {
                c.to_string_lossy()
                    .replace('/', "\\")
                    .ends_with(r"\Git\bin\bash.exe")
            }));
        }
    }

    #[test_case("auto", ShellPreference::Auto ; "auto")]
    #[test_case("cmd", ShellPreference::Cmd ; "cmd")]
    #[test_case(r"C:\tools\bash.exe", ShellPreference::Program(PathBuf::from(r"C:\tools\bash.exe")) ; "path")]
    fn shell_preference_parse(value: &str, expected: ShellPreference) {
        assert_eq!(ShellPreference::parse(value), expected);
    }
}
