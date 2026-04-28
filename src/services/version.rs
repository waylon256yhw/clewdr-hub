//! Shared version parsing.
//!
//! Both the in-process self-updater (`services::update`) and the
//! `import-config` verb (`cli::import`) compare clewdr_version strings.
//! Until commit #9 each rolled its own — `update.rs` hand-parsed
//! `(u32, u32, u32)` tuples, `import.rs` reached for `semver::Version`.
//! This module unifies on `semver::Version`.
//!
//! The function tolerates a leading `v` so it accepts both
//! `env!("CARGO_PKG_VERSION")` (`1.2.4`) and the GitHub release tag form
//! (`v1.2.4`) that arrives on the update path.

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
        // semver permits both. They compare per the spec — prereleases
        // sort *below* the matching release, build metadata is ignored
        // for ordering. Both behaviours are what we want for update
        // detection.
        assert!(parse_clewdr_version("1.2.4-alpha").is_ok());
        assert!(parse_clewdr_version("1.2.4+build.5").is_ok());
        assert!(parse_clewdr_version("v1.2.4-rc.1+sha.abc123").is_ok());
    }

    #[test]
    fn semver_ordering_matches_update_intuition() {
        // Cross-check: the hand-rolled (u32,u32,u32) parser couldn't
        // reason about prereleases. semver does — and a prerelease must
        // *not* outrank the matching release on the update path
        // (otherwise users on 1.2.4 would get nagged by 1.2.4-rc).
        let stable = parse_clewdr_version("1.2.4").unwrap();
        let prerelease = parse_clewdr_version("1.2.4-rc.1").unwrap();
        assert!(prerelease < stable);

        let newer = parse_clewdr_version("1.3.0").unwrap();
        assert!(stable < newer);
    }
}
