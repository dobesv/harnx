//! Request-local memberships from explicitly trusted HTTP headers.

use super::{HeaderSpec, IdentityError, IdentitySource};
use anyhow::{bail, Context};
use http::{HeaderMap, HeaderName};

/// Configured alias memberships retain their existing comma-list normalization.
fn normalize_membership_iter<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    values
        .into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .collect()
}

/// Merge header-supplied and configured memberships.
///
/// Header values are kept exactly as given (order + duplicates), then normalized
/// configured values are appended if not already present. Configured values are also
/// deduplicated among themselves.
pub fn merge_memberships(header: Vec<String>, configured: &[String]) -> Vec<String> {
    // Start with header values exactly as given (duplicates and order preserved)
    let mut result = header;

    // Normalize configured values using shared helper (split, trim, filter empties, dedupe among themselves)
    let normalized_config = normalize_membership_iter(configured.iter().map(String::as_str));

    let mut seen_config = std::collections::HashSet::new();
    for value in normalized_config {
        // Check if already present in header (case-sensitive exact match)
        let already_in_header = result.iter().any(|h| h == &value);
        if !already_in_header {
            // Also dedupe against already-added configured values
            if seen_config.insert(value.clone()) {
                result.push(value);
            }
        }
    }

    result
}

/// Header-only membership policy, separate from ordered user identity sources.
///
/// No headers are trusted by default. Configure separate policies for groups and
/// roles; neither collection supplies a user ID or session owner. The proxy must
/// strip or overwrite these headers before supplying authenticated memberships.
#[derive(Clone, Debug, Default)]
pub struct MembershipHeaders {
    sources: Vec<HeaderSpec>,
}

impl MembershipHeaders {
    /// Validate `header:NAME`, `header-csv:NAME`, or bare header names at startup.
    /// Cookies and bearer identity sources cannot supply memberships.
    pub fn new(sources: &[String]) -> anyhow::Result<Self> {
        let sources = sources
            .iter()
            .map(|raw| {
                match raw.parse::<IdentitySource>()
                    .with_context(|| format!("invalid membership header source '{raw}'"))? {
                    IdentitySource::Header(source) => Ok(source),
                    _ => bail!("unsupported membership source '{raw}': expected header:NAME or header-csv:NAME"),
                }
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { sources })
    }

    /// Raw header names for adapters that receive headers through SDK parameters.
    pub fn names(&self) -> impl Iterator<Item = &HeaderName> {
        self.sources.iter().map(HeaderSpec::name)
    }

    /// Collect memberships from every configured source and repeated field.
    ///
    /// `header:` and bare names keep one opaque, trimmed value per field.
    /// `header-csv:` joins repeated fields with commas before parsing; commas
    /// outside quotes delimit items, except escaped commas (`\,`). Quoted items
    /// decode `\"` and `\\`; other escapes stay literal. Invalid lists or non-text
    /// fields reject the whole extraction, even after valid fields.
    /// Empty items are ignored; missing headers yield an empty collection.
    /// Order and duplicates are retained and have no effect on access grants.
    /// Errors never contain header values. Returned memberships are request-local,
    /// not session metadata.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Vec<String>, IdentityError> {
        let mut values = Vec::new();
        for source in &self.sources {
            values.extend(
                source
                    .values(headers, super::header::HeaderFields::All)?
                    .into_iter()
                    .filter(|value| !value.is_empty()),
            );
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{header::COOKIE, HeaderValue};

    fn csv_policy(names: &[&str]) -> MembershipHeaders {
        harnx_core::require_nextest();
        MembershipHeaders::new(
            &names
                .iter()
                .map(|name| format!("header-csv:{name}"))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn membership_specs_reject_invalid_names_and_unsupported_sources() {
        harnx_core::require_nextest();
        for name in [
            "",
            "x groups",
            "x:groups",
            "x\ngroups",
            "header:",
            "header-csv:",
            "header-csv:bad name",
            "cookie:groups",
            "forge-fit",
            "bearer-claim:groups",
        ] {
            let error = MembershipHeaders::new(&[name.into()]).unwrap_err();
            assert!(error.to_string().contains("membership"));
        }
        assert!(MembershipHeaders::new(&["x-groups".into(), "invalid name".into()]).is_err());
    }

    #[test]
    fn membership_header_specs_make_comma_handling_explicit() {
        harnx_core::require_nextest();
        let dn = "CN=admins,OU=eng,O=Acme";
        let mut headers = HeaderMap::new();
        headers.append("x-groups", format!(" {dn} ").parse().unwrap());
        headers.append("x-groups", " CN=ops,O=Acme ".parse().unwrap());
        for raw in ["X-Groups", "header:X-Groups", "HeAdEr:X-Groups"] {
            let policy = MembershipHeaders::new(&[raw.into()]).unwrap();
            assert_eq!(policy.resolve(&headers).unwrap(), [dn, "CN=ops,O=Acme"]);
        }
        let quoted = r#""CN=admins,OU=eng,O=Acme", "CN=ops,O=Acme""#;
        headers.insert("x-groups", quoted.parse().unwrap());
        let opaque = MembershipHeaders::new(&["header:x-groups".into()]).unwrap();
        assert_eq!(opaque.resolve(&headers).unwrap(), [quoted]);
        let csv = MembershipHeaders::new(&["HeAdEr-CsV:X-Groups".into()]).unwrap();
        assert_eq!(csv.resolve(&headers).unwrap(), [dn, "CN=ops,O=Acme"]);
        headers.insert("x-groups", "\"unterminated".parse().unwrap());
        assert_eq!(opaque.resolve(&headers).unwrap(), ["\"unterminated"]);
        assert_eq!(csv.resolve(&headers), Err(IdentityError::InvalidValue));
    }

    #[test]
    fn membership_names_are_case_insensitive() {
        let policy = csv_policy(&["X-Groups"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        assert_eq!(policy.resolve(&headers).unwrap(), ["engineering"]);
    }

    #[test]
    fn membership_collects_repeated_comma_and_multiple_header_values() {
        let policy = csv_policy(&["x-groups", "x-missing", "x-other-groups"]);
        let mut headers = HeaderMap::new();
        headers.append(
            "x-groups",
            HeaderValue::from_static(" \t engineering , , ops \t,"),
        );
        headers.append("x-groups", HeaderValue::from_static("support,engineering"));
        headers.append("x-other-groups", HeaderValue::from_static(" , finance, "));
        headers.append("x-other-groups", HeaderValue::from_static("reviewers"));
        assert_eq!(
            policy.resolve(&headers).unwrap(),
            [
                "engineering",
                "ops",
                "support",
                "engineering",
                "finance",
                "reviewers"
            ]
        );
    }

    #[test]
    fn membership_missing_and_empty_values_yield_no_memberships() {
        let policy = csv_policy(&["x-groups", "x-roles"]);
        let mut headers = HeaderMap::new();
        assert!(policy.resolve(&headers).unwrap().is_empty());
        for value in ["", " \t ", ",,", " , \t , "] {
            headers.append("x-groups", HeaderValue::from_str(value).unwrap());
        }
        assert!(policy.resolve(&headers).unwrap().is_empty());
    }

    #[test]
    fn membership_default_trusts_nothing_and_ignores_unconfigured_values() {
        let policy = csv_policy(&["x-groups"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-roles", HeaderValue::from_bytes(b"\xff").unwrap());
        headers.insert(COOKIE, HeaderValue::from_static("groups=admin"));
        assert!(policy.resolve(&headers).unwrap().is_empty());
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        assert!(MembershipHeaders::default()
            .resolve(&headers)
            .unwrap()
            .is_empty());
        assert_eq!(policy.resolve(&headers).unwrap(), ["engineering"]);
    }

    #[test]
    fn membership_duplicate_header_names_and_values_are_retained() {
        let policy = csv_policy(&["x-groups", "X-GROUPS"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("ops,ops"));
        assert_eq!(
            policy.resolve(&headers).unwrap(),
            ["ops", "ops", "ops", "ops"]
        );
    }

    #[test]
    fn membership_quoted_lists_are_identical_across_repeated_fields() {
        let policy = csv_policy(&["x-groups"]);
        let raw = r#""CN=admins,OU=eng,O=Acme", "CN=Smith\, Alice,O=Acme", plain-group"#;
        let expected = [
            "CN=admins,OU=eng,O=Acme",
            r"CN=Smith\, Alice,O=Acme",
            "plain-group",
        ];
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", raw.parse().unwrap());
        assert_eq!(policy.resolve(&headers).unwrap(), expected);
        for (index, _) in raw.match_indices(',') {
            headers.remove("x-groups");
            headers.append("x-groups", raw[..index].parse().unwrap());
            headers.append("x-groups", raw[index + 1..].parse().unwrap());
            assert_eq!(
                policy.resolve(&headers).unwrap(),
                expected,
                "split at {index}"
            );
        }
    }

    #[test]
    fn membership_invalid_quotes_reject_all_names_without_leaking_values() {
        let policy = csv_policy(&["x-groups", "x-other-groups"]);
        for raw in [r#""secret"#, r#""secret"junk, admin"#] {
            let mut headers = HeaderMap::new();
            headers.insert("x-groups", "valid".parse().unwrap());
            headers.append("x-other-groups", raw.parse().unwrap());
            headers.append("x-other-groups", "later".parse().unwrap());
            let error = policy.resolve(&headers).unwrap_err();
            assert_eq!(error, IdentityError::InvalidValue);
            assert!(!error.to_string().contains("secret"));
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", "\"unfinished".parse().unwrap());
        headers.insert("x-other-groups", "other\"".parse().unwrap());
        assert_eq!(policy.resolve(&headers), Err(IdentityError::InvalidValue));
    }

    #[test]
    fn membership_malformed_repeated_field_rejects_whole_extraction() {
        let policy = csv_policy(&["x-groups"]);
        let mut headers = HeaderMap::new();
        headers.append("x-groups", HeaderValue::from_static("sensitive-membership"));
        headers.append("x-groups", HeaderValue::from_bytes(b"secret-\xff").unwrap());
        headers.append("x-groups", HeaderValue::from_static("valid-later"));
        let error = policy.resolve(&headers).unwrap_err();
        assert_eq!(error, IdentityError::InvalidValue);
        assert_eq!(error.to_string(), "invalid user identity source");
        assert_eq!(format!("{error:?}"), "InvalidValue");
    }

    #[test]
    fn membership_malformed_later_header_name_rejects_whole_extraction() {
        let policy = csv_policy(&["x-groups", "x-other-groups"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        headers.insert("x-other-groups", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(policy.resolve(&headers), Err(IdentityError::InvalidValue));
    }

    #[test]
    fn membership_groups_and_roles_use_independent_policies() {
        let groups = csv_policy(&["x-groups"]);
        let roles = csv_policy(&["x-roles"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        headers.insert("x-roles", HeaderValue::from_static("reviewer"));
        assert_eq!(groups.resolve(&headers).unwrap(), ["engineering"]);
        assert_eq!(roles.resolve(&headers).unwrap(), ["reviewer"]);
    }

    #[test]
    fn configured_membership_values_basic() {
        harnx_core::require_nextest();
        let result = normalize_membership_iter(["engineering", "ops , support ", ",,admin,"]);
        assert_eq!(result, ["engineering", "ops", "support", "admin"]);
    }

    #[test]
    fn configured_membership_values_preserve_duplicates() {
        harnx_core::require_nextest();
        let result = normalize_membership_iter(["ops,ops", "ops"]);
        assert_eq!(result, ["ops", "ops", "ops"]);
    }

    #[test]
    fn configured_membership_values_filter_empty() {
        harnx_core::require_nextest();
        let result = normalize_membership_iter(["", "   ", ",,"]);
        assert!(result.is_empty());
    }

    #[test]
    fn merge_memberships_header_only() {
        harnx_core::require_nextest();
        let header = vec!["engineering".into(), "ops".into()];
        let configured: Vec<String> = vec![];
        let result = merge_memberships(header.clone(), &configured);
        assert_eq!(result, header); // Order and duplicates preserved
    }

    #[test]
    fn merge_memberships_header_only_with_duplicates() {
        harnx_core::require_nextest();
        let header = vec!["ops".into(), "ops".into(), "engineering".into()];
        let configured: Vec<String> = vec![];
        let result = merge_memberships(header.clone(), &configured);
        assert_eq!(result, header); // All duplicates kept
    }

    #[test]
    fn merge_memberships_config_only() {
        harnx_core::require_nextest();
        let header: Vec<String> = vec![];
        let configured = vec!["engineering".into(), "ops".into()];
        let result = merge_memberships(header, &configured);
        assert_eq!(result, ["engineering", "ops"]);
    }

    #[test]
    fn merge_memberships_config_only_with_commas_and_whitespace() {
        harnx_core::require_nextest();
        let header: Vec<String> = vec![];
        let configured = vec!["engineering , ops ".into(), " support ".into()];
        let result = merge_memberships(header, &configured);
        // Normalized and deduplicated among configured
        assert_eq!(result, ["engineering", "ops", "support"]);
    }

    #[test]
    fn merge_memberships_overlap_no_new_duplicates() {
        harnx_core::require_nextest();
        let header = vec!["engineering".into(), "ops".into()];
        let configured = vec!["ops".into(), "support".into()];
        let result = merge_memberships(header, &configured);
        // Header values kept, configured values added only if not in header
        assert_eq!(result, ["engineering", "ops", "support"]);
    }

    #[test]
    fn merge_memberships_config_deduplicates_itself() {
        harnx_core::require_nextest();
        let header: Vec<String> = vec![];
        let configured = vec!["ops".into(), "ops".into(), "engineering".into()];
        let result = merge_memberships(header, &configured);
        // Deduplicated among configured
        assert_eq!(result, ["ops", "engineering"]);
    }

    #[test]
    fn merge_memberships_config_with_duplicates_in_header() {
        harnx_core::require_nextest();
        let header = vec!["ops".into(), "ops".into(), "engineering".into()];
        let configured = vec!["ops".into(), "support".into()];
        let result = merge_memberships(header, &configured);
        // Header kept as-is (including its duplicates), configured added only if not present
        // "ops" already in header, so only "support" added
        assert_eq!(result, ["ops", "ops", "engineering", "support"]);
    }
}
