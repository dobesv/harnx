use anyhow::{bail, Result};
use semver::Version;

/// Parse a git/OCI tag string as strict semver with `v` prefix.
/// Accepts: "v1.2.3". Rejects: "1.2.3", "v1", "v1.2", "v1.2.3-beta", "latest".
/// Returns the parsed Version (without the `v`).
pub fn parse_semver_tag(tag: &str) -> Result<Version> {
    let bare = tag
        .strip_prefix('v')
        .ok_or_else(|| anyhow::anyhow!("tag '{tag}' must start with 'v' (e.g. v1.2.3)"))?;

    // Strict check: must match exactly \d+\.\d+\.\d+
    let parts: Vec<&str> = bare.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| !p.chars().all(|c| c.is_ascii_digit())) {
        bail!("tag '{tag}' is not a valid semver tag (expected v<major>.<minor>.<patch>)");
    }

    let v = Version::parse(bare)
        .map_err(|e| anyhow::anyhow!("tag '{tag}' failed semver parse: {e}"))?;

    if !v.pre.is_empty() || !v.build.is_empty() {
        bail!("tag '{tag}' must not have pre-release or build metadata (expected v<major>.<minor>.<patch>)");
    }

    Ok(v)
}

/// Returns true if `candidate` is strictly greater than `installed`.
pub fn is_newer_version(installed: &Version, candidate: &Version) -> bool {
    candidate > installed
}

/// Given a list of raw tag strings, return all that parse as valid semver tags,
/// sorted ascending. Non-conforming tags are silently skipped.
pub fn filter_semver_tags(tags: &[String]) -> Vec<Version> {
    let mut versions: Vec<Version> = tags
        .iter()
        .filter_map(|t| parse_semver_tag(t).ok())
        .collect();
    versions.sort();
    versions
}

/// Find the latest semver version in a list of raw tag strings.
/// Returns None if no valid semver tags exist.
pub fn find_latest_tag(tags: &[String]) -> Option<Version> {
    filter_semver_tags(tags).into_iter().max()
}

/// Format a Version back to a tag string: prepend "v".
pub fn version_to_tag(v: &Version) -> String {
    format!("v{v}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid() {
        let v = parse_semver_tag("v1.2.3").unwrap();
        assert_eq!(v, Version::new(1, 2, 3));
    }

    #[test]
    fn test_parse_no_v_prefix() {
        assert!(parse_semver_tag("1.2.3").is_err());
    }

    #[test]
    fn test_parse_short() {
        assert!(parse_semver_tag("v1").is_err());
        assert!(parse_semver_tag("v1.2").is_err());
    }

    #[test]
    fn test_parse_prerelease() {
        assert!(parse_semver_tag("v1.2.3-beta").is_err());
    }

    #[test]
    fn test_parse_latest() {
        assert!(parse_semver_tag("latest").is_err());
    }

    #[test]
    fn test_filter_mixed() {
        let tags: Vec<String> = vec!["v1.0.0", "v2.0.0", "1.0.0", "v1", "v1.2.3-alpha", "v0.9.0"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = filter_semver_tags(&tags);
        assert_eq!(
            result,
            vec![
                Version::new(0, 9, 0),
                Version::new(1, 0, 0),
                Version::new(2, 0, 0),
            ]
        );
    }

    #[test]
    fn test_find_latest() {
        let tags: Vec<String> = vec!["v1.0.0", "v2.0.0", "1.0.0", "v1", "v0.9.0"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(find_latest_tag(&tags), Some(Version::new(2, 0, 0)));
    }

    #[test]
    fn test_find_latest_empty() {
        assert_eq!(find_latest_tag(&[]), None);
    }

    #[test]
    fn test_is_newer() {
        let v100 = Version::new(1, 0, 0);
        let v101 = Version::new(1, 0, 1);
        let v110 = Version::new(1, 1, 0);
        let v109 = Version::new(1, 0, 9);
        assert!(is_newer_version(&v100, &v101));
        assert!(!is_newer_version(&v100, &v100));
        assert!(!is_newer_version(&v110, &v109));
    }
}
