//! Every git invocation maki makes.
//!
//! Commands are built by the functions here and run by one runner, so no call
//! site can forget the hardening flags. Neovim shells out to `git` too, which
//! keeps credential helpers, SSH agents, and proxy settings working without
//! maki owning any of that.

use std::path::{Path, PathBuf};

/// Peels whatever a ref names down to a commit.
const COMMIT_PEEL: &str = "^{commit}";
const PLUGIN_MANIFEST: &str = "plugin.toml";

/// `ext::` hands its argument to a shell, and it is what a hostile
/// `url.<base>.insteadOf` reaches for. Git already refuses it by default, but
/// that default sits in config a repository can override, while a `-c` on the
/// command line outranks every config file. Only this one protocol is pinned,
/// since `protocol.allow` would also move `file`, and that is how a local path
/// source gets cloned.
const EXT_TRANSPORT_DENIED: &str = "protocol.ext.allow=never";
const NO_TERMINAL_PROMPT: (&str, &str) = ("GIT_TERMINAL_PROMPT", "0");
const NO_ASKPASS: (&str, &str) = ("GIT_ASKPASS", "");
/// These pairs are meant to configure one invocation, so anything still set in
/// maki's environment was put there by someone else. Zero pairs reads the same
/// as none, and unlike `GIT_CONFIG_NOSYSTEM` the user's own config is untouched.
const NO_INJECTED_CONFIG: (&str, &str) = ("GIT_CONFIG_COUNT", "0");

/// Flags every invocation carries, whatever it is doing.
///
/// `core.hooksPath` points at a directory maki keeps empty, so a repository
/// cannot run code during a clone, a fetch, or a checkout. Cloning does not
/// import hooks from a remote, so this is defence in depth for the case that
/// matters more: a checkout directory on this machine whose `.git/hooks`
/// something else has written.
pub fn hardening_args(empty_hooks_dir: &Path) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!("core.hooksPath={}", empty_hooks_dir.display()),
        "-c".to_owned(),
        EXT_TRANSPORT_DENIED.to_owned(),
    ]
}

/// Clone arguments. `partial` asks the server for a blobless clone, which is
/// much smaller; a server that refuses it makes the command fail, and the
/// caller retries without it.
/// `--` closes the option list, so a source beginning with `-` is treated as a
/// repository rather than as a git option. There is no shell here, so this is
/// argument injection, not command injection, but `--upload-pack=` alone is
/// enough to run a program.
pub fn clone_args(empty_hooks_dir: &Path, src: &str, dest: &Path, partial: bool) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.push("clone".to_owned());
    if partial {
        args.push("--filter=blob:none".to_owned());
    }
    args.push("--".to_owned());
    args.push(src.to_owned());
    args.push(dest.display().to_string());
    args
}

/// Whether a revision can be passed to git as a positional argument.
///
/// `git rev-parse --not-a-flag` would be read as an option, and revisions come
/// from a package specification, so they are checked rather than trusted.
pub fn revision_is_safe(rev: &str) -> bool {
    !rev.is_empty() && !rev.starts_with('-')
}

/// Whether an HTTP source embeds user information that Git would persist.
///
/// Tokens in clone URLs are commonly written as a username, a password, or
/// both. Git records the URL in `.git/config`, and maki also records the source
/// in its lockfile. Reject that form and let Git's credential helper provide
/// credentials without putting them on disk as part of the repository URL.
pub fn http_source_has_userinfo(src: &str) -> bool {
    let Some((scheme, rest)) = src.trim().split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.contains('@')
}

/// `--tags` is explicit because a version names a tag as often as a branch, and
/// a plain fetch only brings in the tags reachable from the branches it
/// fetched. `--prune` drops the remote-tracking refs of branches that are gone,
/// so a deleted one cannot keep resolving here long after it went away.
pub fn fetch_args(empty_hooks_dir: &Path) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.extend([
        "fetch".to_owned(),
        "--tags".to_owned(),
        "--prune".to_owned(),
        "origin".to_owned(),
    ]);
    args
}

pub fn has_commit_args(empty_hooks_dir: &Path, rev: &str) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.extend([
        "cat-file".to_owned(),
        "-e".to_owned(),
        format!("{rev}{COMMIT_PEEL}"),
    ]);
    args
}

pub fn checkout_args(empty_hooks_dir: &Path, rev: &str) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.extend(["checkout".to_owned(), "--detach".to_owned(), rev.to_owned()]);
    args
}

/// Resolves a ref to a commit id. An annotated tag is an object of its own, so
/// a bare `rev-parse` would record the tag's id in the lockfile rather than the
/// commit that lockfile is supposed to reproduce.
pub fn rev_parse_args(empty_hooks_dir: &Path, rev: &str) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.extend(["rev-parse".to_owned(), format!("{rev}{COMMIT_PEEL}")]);
    args
}

pub(crate) fn manifest_exists_args(empty_hooks_dir: &Path, rev: &str) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.extend([
        "ls-tree".to_owned(),
        "--name-only".to_owned(),
        rev.to_owned(),
        "--".to_owned(),
        PLUGIN_MANIFEST.to_owned(),
    ]);
    args
}

pub(crate) fn read_manifest_args(empty_hooks_dir: &Path, rev: &str) -> Vec<String> {
    let mut args = hardening_args(empty_hooks_dir);
    args.extend(["show".to_owned(), format!("{rev}:{PLUGIN_MANIFEST}")]);
    args
}

/// Environment every invocation runs with. Disabling the terminal prompt makes
/// a credential request fail immediately instead of hanging a startup on a
/// prompt nobody is watching.
pub fn hardening_env() -> [(&'static str, &'static str); 3] {
    [NO_TERMINAL_PROMPT, NO_ASKPASS, NO_INJECTED_CONFIG]
}

/// What one git invocation produced.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    Missing,
    #[error("git {args} failed with status {status}: {stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
}

/// Hides credentials embedded in a URL, so an error message can be shown or
/// logged without leaking a token that was only ever meant for git.
pub fn redact(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let word = part.trim_end_matches(char::is_whitespace);
            let whitespace = &part[word.len()..];
            let redacted = match word.split_once("://") {
                Some((scheme, rest)) => match rest.rsplit_once('@') {
                    Some((_creds, host)) => format!("{scheme}://***@{host}"),
                    None => word.to_owned(),
                },
                None => word.to_owned(),
            };
            redacted + whitespace
        })
        .collect()
}

/// Runs git off the calling thread.
///
/// `maki.pack.add` runs while `init.lua` is being sourced, and `init.lua`
/// executes on the single Lua thread inside an async call. A blocking
/// `Command::output` there would stall the whole VM, the watchdog included, so
/// every invocation goes through `smol::unblock` and the caller awaits it. On a
/// first start a clone can take seconds, which is exactly when this matters.
///
/// The directory is required, never optional. Git reads the config of whatever
/// repository it stands in, a clone included, and the inherited process
/// directory is wherever the user launched maki, usually a repository the agent
/// is editing. An `insteadOf` in its `.git/config` is enough to point a clone
/// at a transport that runs a command, so every caller names a directory maki
/// owns and the question goes away.
pub async fn run(args: Vec<String>, cwd: PathBuf) -> Result<GitOutput, GitError> {
    let display = redact(&args.join(" "));
    let output = smol::unblock(move || {
        let mut cmd = std::process::Command::new("git");
        cmd.args(&args);
        cmd.current_dir(cwd);
        for (key, value) in hardening_env() {
            cmd.env(key, value);
        }
        cmd.output()
    })
    .await;

    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GitError::Missing),
        Err(e) => {
            return Err(GitError::Failed {
                args: display,
                status: "not started".to_owned(),
                stderr: e.to_string(),
            });
        }
    };

    if output.status.success() {
        Ok(GitOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    } else {
        Err(GitError::Failed {
            args: display,
            status: output.status.to_string(),
            stderr: redact(String::from_utf8_lossy(&output.stderr).trim()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use test_case::test_case;

    fn hooks() -> PathBuf {
        PathBuf::from("/data/site/.nohooks")
    }

    fn hooks_flag_present(args: &[String]) -> bool {
        args.windows(2)
            .any(|w| w[0] == "-c" && w[1].starts_with("core.hooksPath="))
    }

    #[test]
    fn git_runs_without_prompts_or_environment_supplied_config() {
        let env = hardening_env();
        assert!(env.contains(&NO_TERMINAL_PROMPT), "{env:?}");
        assert!(env.contains(&NO_ASKPASS), "{env:?}");
        assert!(env.contains(&NO_INJECTED_CONFIG), "{env:?}");
    }

    /// Refused on the command line, where no repository's config can reach it.
    #[test]
    fn every_command_denies_the_ext_transport() {
        for args in [
            clone_args(&hooks(), "src", Path::new("/dest"), true),
            fetch_args(&hooks()),
            checkout_args(&hooks(), "main"),
            rev_parse_args(&hooks(), "HEAD"),
            has_commit_args(&hooks(), "abc123"),
        ] {
            assert!(
                args.windows(2)
                    .any(|w| w[0] == "-c" && w[1] == EXT_TRANSPORT_DENIED),
                "{args:?}"
            );
        }
    }

    /// The hardening is the point of routing every command through here, so
    /// each builder is checked rather than only the shared helper.
    #[test]
    fn every_command_disables_hooks() {
        assert!(hooks_flag_present(&clone_args(
            &hooks(),
            "src",
            Path::new("/dest"),
            true
        )));
        assert!(hooks_flag_present(&fetch_args(&hooks())));
        assert!(hooks_flag_present(&checkout_args(&hooks(), "main")));
        assert!(hooks_flag_present(&rev_parse_args(&hooks(), "HEAD")));
        assert!(hooks_flag_present(&manifest_exists_args(
            &hooks(),
            "abc123"
        )));
        assert!(hooks_flag_present(&read_manifest_args(&hooks(), "abc123")));
    }

    /// The flags have to come before the subcommand, or git treats them as
    /// arguments to it and the hardening silently does nothing.
    #[test]
    fn hardening_precedes_the_subcommand() {
        let args = clone_args(&hooks(), "src", Path::new("/dest"), false);
        let subcommand = args.iter().position(|a| a == "clone").unwrap();
        let config = args.iter().position(|a| a == "-c").unwrap();
        assert!(config < subcommand);
    }

    #[test]
    fn partial_clone_is_opt_in_so_it_can_be_retried_without_it() {
        let partial = clone_args(&hooks(), "src", Path::new("/dest"), true);
        let full = clone_args(&hooks(), "src", Path::new("/dest"), false);
        assert!(partial.iter().any(|a| a == "--filter=blob:none"));
        assert!(!full.iter().any(|a| a == "--filter=blob:none"));
    }

    #[test]
    fn checkout_detaches_so_no_branch_tracks_the_revision() {
        let args = checkout_args(&hooks(), "abc123");
        assert!(args.iter().any(|a| a == "--detach"));
        assert_eq!(args.last().unwrap(), "abc123");
    }

    #[test]
    fn manifest_lookup_uses_a_fixed_path_after_the_option_terminator() {
        let args = manifest_exists_args(&hooks(), "abc123");
        assert_eq!(&args[args.len() - 3..], ["abc123", "--", PLUGIN_MANIFEST]);
        assert_eq!(
            read_manifest_args(&hooks(), "abc123").last().unwrap(),
            "abc123:plugin.toml"
        );
    }

    /// A source is a positional argument, so it must sit after `--`. Without
    /// it, a src of `--upload-pack=...` is an option git will act on.
    #[test]
    fn clone_puts_the_source_after_an_option_terminator() {
        let args = clone_args(&hooks(), "--upload-pack=evil", Path::new("/dest"), true);
        let terminator = args.iter().position(|a| a == "--").expect("`--` required");
        let src = args
            .iter()
            .position(|a| a == "--upload-pack=evil")
            .expect("src present");
        assert!(
            terminator < src,
            "a source that looks like an option must follow `--`: {args:?}"
        );
    }

    /// A clone URL can carry a token. Errors are shown and logged, so the
    /// credential must not travel with them.
    #[test]
    fn credentials_in_a_url_are_redacted() {
        let out = redact("clone -- https://user:tok3n@github.com/u/r /dest");
        assert!(!out.contains("tok3n"), "token leaked: {out}");
        assert!(!out.contains("user:"), "user leaked: {out}");
        assert!(out.contains("github.com/u/r"), "host should survive: {out}");
        assert!(out.contains("/dest"));
    }

    #[test_case("https://token@example.com/repo", true ; "username_token")]
    #[test_case("https://user:token@example.com/repo", true ; "password_token")]
    #[test_case("HTTP://user@example.com/repo", true ; "scheme_case")]
    #[test_case("https://example.com/user@repo", false ; "at_in_path")]
    #[test_case("ssh://git@example.com/repo", false ; "ssh_username")]
    #[test_case("git@example.com:user/repo", false ; "scp_style")]
    fn detects_http_user_information(src: &str, expected: bool) {
        assert_eq!(http_source_has_userinfo(src), expected);
    }

    #[test]
    fn redaction_leaves_ordinary_urls_alone() {
        let plain = "clone -- https://github.com/u/r /dest";
        assert_eq!(redact(plain), plain);
    }

    #[test]
    fn redaction_preserves_whitespace() {
        let source = "/a path/with spaces\nand a second line";
        assert_eq!(redact(source), source);
    }

    #[test]
    fn revisions_that_look_like_options_are_rejected() {
        assert!(revision_is_safe("main"));
        assert!(revision_is_safe("v1.2.3"));
        assert!(revision_is_safe("abc123"));
        assert!(!revision_is_safe("--upload-pack=evil"));
        assert!(!revision_is_safe("-x"));
        assert!(!revision_is_safe(""));
    }

    /// A failure has to name the command, the status, and git's own message,
    /// or a user cannot tell a network problem from a bad revision.
    #[test]
    fn failure_reports_command_status_and_stderr() {
        let err = GitError::Failed {
            args: "clone x y".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: "repository not found".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("clone x y"));
        assert!(msg.contains("128"));
        assert!(msg.contains("repository not found"));
    }

    #[test]
    fn a_failing_command_reports_rather_than_panicking() {
        let dir = tempfile::TempDir::new().unwrap();
        let hooks = dir.path().join("nohooks");
        std::fs::create_dir_all(&hooks).unwrap();

        let result = smol::block_on(run(
            rev_parse_args(&hooks, "definitely-not-a-ref"),
            dir.path().to_path_buf(),
        ));
        assert!(result.is_err(), "an unknown revision must be an error");
    }
}
