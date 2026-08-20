//! What a user declares when they add a package.

use crate::version::Version;

/// One package, as declared by `maki.pack.add`.
///
/// Field names and defaults follow Neovim's `vim.pack.Spec`, so a user moving
/// from a Neovim plugin list does not have to translate anything.
#[derive(Debug, Clone)]
pub struct Spec {
    /// Any URI `git clone` accepts.
    pub src: String,
    /// Directory and owner name. Derived from `src` when not given.
    pub name: String,
    pub version: Version,
}

impl Spec {
    pub fn new(src: impl Into<String>) -> Self {
        let src = src.into();
        let name = derive_name(&src);
        Self {
            src,
            name,
            version: Version::default(),
        }
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    #[must_use]
    pub fn with_version(mut self, version: Version) -> Self {
        self.version = version;
        self
    }
}

/// Whether a name is safe to use as a directory under the package root.
///
/// This is a security boundary, not a tidiness rule. A name becomes a path
/// component, and `Path::join` *replaces* the base when given an absolute path,
/// so a package called `/etc` would make the package root `/etc` and a removal
/// would delete it. A name containing `..` escapes upward the same way.
pub fn name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        // A Windows drive prefix would also escape, and names are shared
        // across platforms, so reject it everywhere rather than only there.
        && !name.contains(':')
        // A name has to be writable on a command line. `/packdel -demo` reads
        // the word as an option, and `/packdel my pack` as two names, so a
        // name that cannot survive either is refused at the point it is
        // chosen rather than becoming a package nobody can remove.
        && !name.starts_with('-')
        && !name.starts_with('+')
        && !name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// The repository name in a git URI.
///
/// Handles the forms git itself accepts: https URLs, scp-style `host:path`, and
/// plain paths, with or without a `.git` suffix or a trailing slash. An
/// unusable `src` yields an empty name, which the caller rejects rather than
/// guessing.
pub fn derive_name(src: &str) -> String {
    let trimmed = src.trim().trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or("");
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("https://github.com/user/repo", "repo" ; "https")]
    #[test_case("https://github.com/user/repo.git", "repo" ; "https_with_git_suffix")]
    #[test_case("https://github.com/user/repo/", "repo" ; "trailing_slash")]
    #[test_case("https://github.com/user/repo.git/", "repo" ; "git_suffix_and_slash")]
    #[test_case("git@github.com:user/repo.git", "repo" ; "scp_style")]
    #[test_case("ssh://git@host/user/repo.git", "repo" ; "ssh_url")]
    #[test_case("/srv/git/repo.git", "repo" ; "local_path")]
    #[test_case("repo", "repo" ; "bare_name")]
    #[test_case("https://example.com/a.b/my-plugin.nvim", "my-plugin.nvim" ; "dots_kept_when_not_git_suffix")]
    fn derives_name_from_src(src: &str, expected: &str) {
        assert_eq!(derive_name(src), expected);
    }

    #[test_case("repo", true ; "plain")]
    #[test_case("my-plugin.nvim", true ; "dots_inside")]
    #[test_case("under_score", true ; "underscore")]
    #[test_case("", false ; "empty")]
    #[test_case("..", false ; "parent")]
    #[test_case("../escape", false ; "traversal")]
    #[test_case("/etc", false ; "absolute")]
    #[test_case("a/b", false ; "separator")]
    #[test_case("a\\b", false ; "backslash")]
    #[test_case("C:evil", false ; "drive_prefix")]
    #[test_case(".hidden", false ; "leading_dot")]
    #[test_case("-option", false ; "command_option")]
    #[test_case("++flag", false ; "command_flag")]
    #[test_case("two words", false ; "command_separator")]
    #[test_case("escape\u{1b}sequence", false ; "control_character")]
    fn name_safety(name: &str, expected: bool) {
        assert_eq!(name_is_safe(name), expected, "{name:?}");
    }

    /// Names derived from ordinary sources must survive the safety check, or
    /// the guard would reject real packages.
    #[test]
    fn derived_names_from_real_sources_are_safe() {
        for src in [
            "https://github.com/user/repo.git",
            "git@github.com:user/repo.git",
            "ssh://git@host/user/my-plugin.nvim",
        ] {
            assert!(name_is_safe(&derive_name(src)), "{src}");
        }
    }

    #[test]
    fn unusable_src_yields_an_empty_name() {
        assert_eq!(derive_name(""), "");
        assert_eq!(derive_name("///"), "");
    }

    #[test]
    fn explicit_name_overrides_the_derived_one() {
        let spec = Spec::new("https://github.com/user/repo").with_name("other");
        assert_eq!(spec.name, "other");
        assert_eq!(spec.src, "https://github.com/user/repo");
    }
}
