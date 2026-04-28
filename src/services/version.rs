//! Shared version parsing + SemVer-precedence comparison.
//!
//! Both the in-process self-updater (`services::update`) and the
//! `import-config` verb (`cli::import`) compare clewdr_version strings.
//! Until commit #9 each rolled its own — `update.rs` hand-parsed
//! `(u32, u32, u32)` tuples, `import.rs` reached for `semver::Version`.
//! This module unifies on `semver::Version`.
//!
//! [`parse_clewdr_version`] tolerates a leading `v` so it accepts both
//! `env!("CARGO_PKG_VERSION")` (`1.2.4`) and the GitHub release tag form
//! (`v1.2.4`) that arrives on the update path.
//!
//! [`is_newer_release`] is the comparison the updater wants. It goes
//! through [`Version::cmp_precedence`] rather than the default `Ord`
//! because SemVer 2.0 explicitly says build metadata MUST NOT
//! participate in version precedence — and `Ord` on `semver::Version`
//! is a *total* order over the struct that does include it.

use std::cmp::Ordering;

use semver::Version;

use crate::error::ClewdrError;

/// Parse a clewdr version string into a [`Version`]. Strips an optional
/// leading `v`; otherwise rejects anything that isn't valid semver,
/// preserving the offending input in the error so the operator can see
/// what failed at a glance.
pub fn parse_clewdr_version(s: &str) -> Result<Version, ClewdrError> {
    Version::parse(s.trim_start_matches('v')).map_err(|_| ClewdrError::InvalidVersion {
        version: s.to_string(),
    })
}

/// Returns `true` when `latest` is a strictly newer release than `current`
/// per **SemVer 2.0 precedence**, which deliberately ignores build
/// metadata (`+build.5`, `+sha.abc123` …). Two tags that differ only in
/// build metadata are the same release and must not retrigger the update
/// flow.
///
/// Why not just `latest > current`? `semver::Version`'s default [`Ord`]
/// is a total order over the struct — it falls back to comparing build
/// metadata strings when major/minor/patch/pre are equal. So a release
/// tag like `v1.2.4+build.5` against a running `1.2.4` shows as
/// "greater" under default `Ord`, and the updater would download +
/// self-replace with the same binary on every check. The semver crate
/// explicitly provides [`Version::cmp_precedence`] for the SemVer-spec
/// path; we go through it.
pub fn is_newer_release(current: &Version, latest: &Version) -> bool {
    latest.cmp_precedence(current) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_v_from_release_tags() {
        assert_eq!(
            parse_clewdr_version("v1.2.4").unwrap(),
            Version::new(1, 2, 4)
        );
        assert_eq!(
            parse_clewdr_version("1.2.4").unwrap(),
            Version::new(1, 2, 4)
        );
    }

    #[test]
    fn rejects_garbage_with_offending_string_in_error() {
        match parse_clewdr_version("not a version") {
            Err(ClewdrError::InvalidVersion { version }) => assert_eq!(version, "not a version"),
            other => panic!("expected InvalidVersion, got {other:?}"),
        }
        assert!(parse_clewdr_version("1.2").is_err());
        assert!(parse_clewdr_version("").is_err());
    }

    #[test]
    fn accepts_prerelease_and_build_metadata() {
        // semver permits both. Their *ordering* under SemVer precedence
        // is asserted in the dedicated tests below.
        assert!(parse_clewdr_version("1.2.4-alpha").is_ok());
        assert!(parse_clewdr_version("1.2.4+build.5").is_ok());
        assert!(parse_clewdr_version("v1.2.4-rc.1+sha.abc123").is_ok());
    }

    #[test]
    fn higher_minor_or_patch_is_newer() {
        let cur = parse_clewdr_version("1.2.4").unwrap();
        assert!(is_newer_release(
            &cur,
            &parse_clewdr_version("1.3.0").unwrap()
        ));
        assert!(is_newer_release(
            &cur,
            &parse_clewdr_version("1.2.5").unwrap()
        ));
        assert!(is_newer_release(
            &cur,
            &parse_clewdr_version("2.0.0").unwrap()
        ));
        // Equal versions are not newer in either direction.
        let same = parse_clewdr_version("1.2.4").unwrap();
        assert!(!is_newer_release(&cur, &same));
        assert!(!is_newer_release(&same, &cur));
    }

    #[test]
    fn prerelease_does_not_outrank_matching_stable() {
        // 1.2.4-rc < 1.2.4 — pin so a prerelease tag never silently tells
        // a running stable that it has an "update" available.
        let stable = parse_clewdr_version("1.2.4").unwrap();
        let rc = parse_clewdr_version("1.2.4-rc.1").unwrap();
        assert!(!is_newer_release(&stable, &rc));
        assert!(is_newer_release(&rc, &stable));
    }

    #[test]
    fn build_metadata_does_not_count_as_newer() {
        // SemVer 2.0 precedence ignores build metadata. Default `Ord` on
        // Version does *not* — under it, 1.2.4+build.5 sorts above 1.2.4
        // and would make the updater nag forever. is_newer_release goes
        // through cmp_precedence specifically to dodge that.
        let plain = parse_clewdr_version("1.2.4").unwrap();
        let with_build = parse_clewdr_version("1.2.4+build.5").unwrap();
        assert!(!is_newer_release(&plain, &with_build));
        assert!(!is_newer_release(&with_build, &plain));

        // Sanity: confirm default Ord *disagrees* on this pair, so the
        // assertions above genuinely pin the cmp_precedence path and
        // aren't trivially true by accident.
        assert!(with_build > plain);
    }

    #[test]
    fn prerelease_with_differing_build_metadata_is_same_release() {
        let a = parse_clewdr_version("1.2.4-rc.1+sha.aaa").unwrap();
        let b = parse_clewdr_version("1.2.4-rc.1+sha.bbb").unwrap();
        assert!(!is_newer_release(&a, &b));
        assert!(!is_newer_release(&b, &a));
    }
}
