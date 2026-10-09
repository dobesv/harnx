//! Request-local memberships from explicitly trusted HTTP headers.

use super::IdentityError;
use anyhow::Context;
use http::{HeaderMap, HeaderName};

/// Header-only membership policy, separate from ordered user identity sources.
///
/// No headers are trusted by default. Configure separate policies for groups and
/// roles; neither collection supplies a user ID or session owner. The proxy must
/// strip or overwrite these headers before supplying authenticated memberships.
#[derive(Clone, Debug, Default)]
pub struct MembershipHeaders {
    names: Vec<HeaderName>,
}

impl MembershipHeaders {
    /// Validate raw HTTP header names at startup. No source prefixes or cookies.
    pub fn new(names: &[String]) -> anyhow::Result<Self> {
        let names = names
            .iter()
            .map(|name| {
                HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid membership header name '{name}'"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { names })
    }

    /// Collect all repeated fields and comma-separated entries from every name.
    ///
    /// Trim whitespace and ignore empty entries. Missing headers yield an empty
    /// collection. Duplicates are retained and have no effect on access grants.
    /// Commas always delimit entries; quoted lists and escaping aren't supported.
    /// Any non-text field rejects the whole extraction, even after valid fields.
    /// Errors never contain header values. Returned memberships are request-local,
    /// not session metadata.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Vec<String>, IdentityError> {
        let mut memberships = Vec::new();
        for name in &self.names {
            for field in headers.get_all(name) {
                let value = field.to_str().map_err(|_| IdentityError::InvalidValue)?;
                memberships.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|entry| !entry.is_empty())
                        .map(String::from),
                );
            }
        }
        Ok(memberships)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{header::COOKIE, HeaderValue};

    fn policy(names: &[&str]) -> MembershipHeaders {
        harnx_core::require_nextest();
        MembershipHeaders::new(&names.iter().map(|name| (*name).into()).collect::<Vec<_>>())
            .unwrap()
    }

    #[test]
    fn membership_names_are_validated_without_source_prefixes() {
        harnx_core::require_nextest();
        for name in [
            "",
            "x groups",
            "x:groups",
            "x\ngroups",
            "header:x-groups",
            "cookie:groups",
        ] {
            let error = MembershipHeaders::new(&[name.into()]).unwrap_err();
            assert!(error.to_string().contains("invalid membership header name"));
        }
        assert!(MembershipHeaders::new(&["x-groups".into(), "invalid name".into()]).is_err());
    }

    #[test]
    fn membership_names_are_case_insensitive() {
        let policy = policy(&["X-Groups"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        assert_eq!(policy.resolve(&headers).unwrap(), ["engineering"]);
    }

    #[test]
    fn membership_collects_repeated_comma_and_multiple_header_values() {
        let policy = policy(&["x-groups", "x-missing", "x-other-groups"]);
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
        let policy = policy(&["x-groups", "x-roles"]);
        let mut headers = HeaderMap::new();
        assert!(policy.resolve(&headers).unwrap().is_empty());
        for value in ["", " \t ", ",,", " , \t , "] {
            headers.append("x-groups", HeaderValue::from_str(value).unwrap());
        }
        assert!(policy.resolve(&headers).unwrap().is_empty());
    }

    #[test]
    fn membership_default_trusts_nothing_and_ignores_unconfigured_values() {
        let policy = policy(&["x-groups"]);
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
        let policy = policy(&["x-groups", "X-GROUPS"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("ops,ops"));
        assert_eq!(
            policy.resolve(&headers).unwrap(),
            ["ops", "ops", "ops", "ops"]
        );
    }

    #[test]
    fn membership_malformed_repeated_field_rejects_whole_extraction() {
        let policy = policy(&["x-groups"]);
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
        let policy = policy(&["x-groups", "x-other-groups"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        headers.insert("x-other-groups", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(policy.resolve(&headers), Err(IdentityError::InvalidValue));
    }

    #[test]
    fn membership_groups_and_roles_use_independent_policies() {
        let groups = policy(&["x-groups"]);
        let roles = policy(&["x-roles"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-groups", HeaderValue::from_static("engineering"));
        headers.insert("x-roles", HeaderValue::from_static("reviewer"));
        assert_eq!(groups.resolve(&headers).unwrap(), ["engineering"]);
        assert_eq!(roles.resolve(&headers).unwrap(), ["reviewer"]);
    }
}
