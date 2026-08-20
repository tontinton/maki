//! Which revision of a package to use.
//!
//! A plain branch, tag, or commit is handed to git untouched. A range is
//! resolved here, because no git command understands a semver constraint.

use semver::{Version as SemVer, VersionReq};

/// What a package specification asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Version {
    /// No version given: track the remote's default branch.
    #[default]
    DefaultBranch,
    /// A branch, a tag, or a commit, passed to git as written.
    Rev(String),
    /// A semver constraint, resolved against the repository's tags.
    Range(VersionReq),
}

impl Version {
    /// Parses the constraint syntax `maki.version.range()` accepts.
    pub fn range(spec: &str) -> Result<Self, semver::Error> {
        VersionReq::parse(spec).map(Self::Range)
    }
}

/// A tag as semver, tolerating the common `v` prefix.
///
/// Tags that are not versions at all are simply not candidates; a repository is
/// free to carry `latest` or `nightly` alongside its releases.
fn as_semver(tag: &str) -> Option<SemVer> {
    let trimmed = tag.strip_prefix('v').unwrap_or(tag);
    SemVer::parse(trimmed).ok()
}

/// The greatest tag the constraint admits.
///
/// Neovim installs "the greatest/last semver tag inside the version
/// constraint", so this picks by version order rather than by tag order or by
/// commit date.
pub(crate) fn best_match<'a, I>(req: &VersionReq, tags: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    tags.into_iter()
        .filter_map(|tag| as_semver(tag).map(|v| (v, tag)))
        .filter(|(v, _)| req.matches(v))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, tag)| tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(s: &str) -> VersionReq {
        VersionReq::parse(s).unwrap()
    }

    #[test]
    fn picks_the_greatest_match_not_the_last_listed() {
        let tags = ["v1.0.0", "v1.4.2", "v1.2.0"];
        assert_eq!(best_match(&req("^1"), tags), Some("v1.4.2"));
    }

    #[test]
    fn respects_the_constraint_upper_bound() {
        let tags = ["v1.9.0", "v2.0.0", "v2.5.1"];
        assert_eq!(best_match(&req("^1"), tags), Some("v1.9.0"));
    }

    #[test]
    fn tolerates_tags_with_and_without_the_v_prefix() {
        assert_eq!(best_match(&req(">=1"), ["1.2.3"]), Some("1.2.3"));
        assert_eq!(best_match(&req(">=1"), ["v1.2.3"]), Some("v1.2.3"));
    }

    /// A repository may tag things that are not releases. Those are not
    /// candidates, but they must not stop the real tags from resolving.
    #[test]
    fn ignores_tags_that_are_not_versions() {
        let tags = ["nightly", "latest", "v1.1.0", "release-candidate"];
        assert_eq!(best_match(&req("^1"), tags), Some("v1.1.0"));
    }

    #[test]
    fn no_matching_tag_resolves_to_nothing() {
        assert_eq!(best_match(&req("^3"), ["v1.0.0", "v2.0.0"]), None);
        assert_eq!(best_match(&req("^1"), []), None);
    }

    /// Ordering is by version, so 10 must beat 9 even though it sorts earlier
    /// as text.
    #[test]
    fn orders_numerically_not_lexically() {
        let tags = ["v1.9.0", "v1.10.0"];
        assert_eq!(best_match(&req("^1"), tags), Some("v1.10.0"));
    }

    /// A prerelease must not be picked for a plain caret range, matching cargo
    /// and npm, so an alpha tag never becomes an accidental upgrade.
    #[test]
    fn prerelease_is_not_matched_by_a_plain_range() {
        let tags = ["v1.0.0", "v1.1.0-alpha.1"];
        assert_eq!(best_match(&req("^1"), tags), Some("v1.0.0"));
    }
}
