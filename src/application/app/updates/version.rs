use anyhow::{Context, Result};
use std::cmp::Ordering;

/// Linux's release counter belongs to the package, not the base version's
/// prerelease channel: v1.0.3-linux.2 is still a stable release.
pub(super) struct ReleaseVersion {
    pub base: semver::Version,
    linux_build: u64,
}

impl ReleaseVersion {
    pub fn parse(tag: &str, linux: bool) -> Result<Self> {
        let tag = tag.strip_prefix('v').unwrap_or(tag);
        let (base, linux_build) = if linux {
            let (base, build) = tag
                .rsplit_once("-linux.")
                .context("缺少 Linux 发布构建号")?;
            (base, build.parse().context("Linux 发布构建号无效")?)
        } else {
            (tag, 0)
        };
        Ok(Self {
            base: semver::Version::parse(base)?,
            linux_build,
        })
    }

    pub fn current(tag: &str, linux: bool) -> Result<Self> {
        if linux && !tag.contains("-linux.") {
            // Untagged developer builds have no published Linux build number.
            Ok(Self {
                base: semver::Version::parse(tag)?,
                linux_build: 0,
            })
        } else {
            Self::parse(tag, linux)
        }
    }

    pub fn cmp_precedence(&self, other: &Self) -> Ordering {
        self.base
            .cmp_precedence(&other.base)
            .then_with(|| self.linux_build.cmp(&other.linux_build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_builds_sort_numerically_and_same_build_is_current() {
        let nine = ReleaseVersion::parse("v1.0.2-beta.5-linux.9", true).unwrap();
        let ten = ReleaseVersion::parse("v1.0.2-beta.5-linux.10", true).unwrap();
        let installed = ReleaseVersion::current("1.0.2-beta.5-linux.10", true).unwrap();
        assert_eq!(ten.cmp_precedence(&nine), Ordering::Greater);
        assert_eq!(ten.cmp_precedence(&installed), Ordering::Equal);
        assert!(!ten.base.pre.is_empty());
    }

    #[test]
    fn stable_linux_release_is_not_a_prerelease() {
        let stable = ReleaseVersion::parse("v1.0.3-linux.1", true).unwrap();
        assert!(stable.base.pre.is_empty());
    }

    #[test]
    fn base_version_takes_precedence_over_linux_build() {
        let older = ReleaseVersion::parse("v1.0.2-beta.5-linux.99", true).unwrap();
        let newer = ReleaseVersion::parse("v1.0.2-beta.6-linux.1", true).unwrap();
        assert_eq!(newer.cmp_precedence(&older), Ordering::Greater);
    }

    #[test]
    fn linux_candidates_require_a_valid_linux_build() {
        for tag in ["v1.0.3", "v1.0.3-linux.bad", "v1.0.3-linux.", "bad-linux.1"] {
            assert!(ReleaseVersion::parse(tag, true).is_err(), "{tag}");
        }
    }

    #[test]
    fn untagged_developer_builds_have_no_release_counter() {
        let current = ReleaseVersion::current("1.0.2-beta.5", true).unwrap();
        let published = ReleaseVersion::parse("v1.0.2-beta.5-linux.1", true).unwrap();
        assert_eq!(published.cmp_precedence(&current), Ordering::Greater);
    }

    #[test]
    fn windows_versions_keep_standard_semver_ordering() {
        let beta = ReleaseVersion::parse("v1.0.3-beta.1", false).unwrap();
        let stable = ReleaseVersion::parse("v1.0.3", false).unwrap();
        assert_eq!(stable.cmp_precedence(&beta), Ordering::Greater);
        assert!(stable.base.pre.is_empty());
    }
}
