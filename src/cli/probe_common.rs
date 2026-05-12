//! Helpers shared between `cli::status` and `cli::diagnose`.
//!
//! Both verbs probe a running clewdr instance and need the same loose
//! semver check on `/api/version`'s response — keep the implementation
//! in one place so the two probe paths cannot drift.

/// Accepts `MAJOR.MINOR.PATCH` and the `-pre` / `+meta` suffixes that
/// `/api/version` may emit. The caller is expected to strip the leading
/// `v` before passing the string in.
pub(crate) fn is_semver_ish(s: &str) -> bool {
    let mut parts = s.split('.');
    let (a, b, c) = (parts.next(), parts.next(), parts.next());
    match (a, b, c) {
        (Some(x), Some(y), Some(z)) => {
            let z_num: String = z.chars().take_while(|c| c.is_ascii_digit()).collect();
            x.chars().all(|c| c.is_ascii_digit())
                && y.chars().all(|c| c.is_ascii_digit())
                && !z_num.is_empty()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_versions() {
        assert!(is_semver_ish("1.2.3"));
        assert!(is_semver_ish("0.0.1"));
        assert!(is_semver_ish("12.34.56"));
        assert!(is_semver_ish("1.2.3-pre"));
        assert!(is_semver_ish("1.2.3+meta"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(!is_semver_ish("hello"));
        assert!(!is_semver_ish("1.2"));
        assert!(!is_semver_ish("a.b.c"));
        assert!(!is_semver_ish(""));
        assert!(!is_semver_ish("v1.2.3")); // caller strips the leading "v"
    }
}
