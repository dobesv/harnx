//! User alias configuration loaded at startup.
//!
//! Aliases expand a caller's identity into a matching entry's identity set.
//! Lookup scans entries in order and returns the identities from the first
//! entry containing the caller. Unmatched callers retain their singleton.
//!
//! Configuration is discovered through `config_paths::local_path("users.yaml")`.
//! Missing file is a valid no-op; present malformed files (including empty,
//! comment-only, whitespace-only, or null documents) fail startup with
//! path context.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_yaml::Value;
use std::path::Path;

/// A single alias entry with required name and identities.
///
/// Name is display metadata, never a principal. Identities is the set
/// into which a matching caller is expanded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAliasEntry {
    /// Display name for this alias entry. Not used as a principal.
    pub name: String,
    /// Identity set for expansion. May be empty (valid no-op).
    pub identities: Vec<String>,
}

/// Validated user aliases configuration.
///
/// Top-level YAML must be a sequence of alias entries with required name
/// and identities fields. Unknown fields are rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserAliases {
    entries: Vec<UserAliasEntry>,
}

impl UserAliases {
    /// Parse and validate user aliases from YAML.
    ///
    /// Returns Ok for valid configurations (including empty sequence `[]`).
    /// Returns Err for malformed YAML, schema violations, or documents that
    /// don't contain an actual YAML sequence (null, scalar, empty document).
    /// Errors include context for diagnostics but do not include path
    /// (caller adds path context via `load`).
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        // First, parse as generic Value to check the actual root type.
        // We must reject null documents, scalars, and other non-sequence roots.
        let value: Value =
            serde_yaml::from_str(yaml).context("failed to parse user aliases YAML")?;

        // Require sequence at root - reject null, scalar, mapping, etc.
        match value {
            Value::Sequence(seq) => {
                // We have a sequence - now validate each entry against the strict schema.
                // Deserialize each entry individually to enforce required fields and deny_unknown_fields.
                let entries: Vec<UserAliasEntry> = seq
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry_value)| {
                        serde_yaml::from_value(entry_value).with_context(|| {
                            format!("failed to parse alias entry at index {}", index)
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self { entries })
            }
            Value::Null => {
                bail!("null document is not a valid alias sequence");
            }
            Value::Mapping(_) => {
                bail!("mapping root is not a valid alias sequence (expected sequence)");
            }
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                bail!("scalar root is not a valid alias sequence (expected sequence)");
            }
            _ => {
                bail!("invalid YAML root type (expected sequence)");
            }
        }
    }

    /// Load aliases from a file, including path in any error.
    pub fn load(path: &Path) -> Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read user aliases from {}", path.display()))?;
        Self::from_yaml(&yaml)
            .with_context(|| format!("failed to load user aliases from {}", path.display()))
    }

    /// Expand a caller identity into its matching entry's identities.
    ///
    /// Scans entries in order and returns identities from the first entry
    /// containing the exact caller identity. Unmatched callers return a
    /// singleton slice containing only the original identity, including when
    /// it is empty. Every supplied string is treated as an identity; anonymous
    /// callers are represented separately by integrations as `None`.
    pub fn expand_caller(&self, caller: &str) -> ExpandedIdentities<'_> {
        // Find first entry containing the exact caller (avoid String allocation)
        for entry in &self.entries {
            if entry.identities.iter().any(|id| id == caller) {
                return ExpandedIdentities::matched(&entry.identities);
            }
        }

        // Unmatched caller retains singleton identity
        ExpandedIdentities::unmatched(caller)
    }

    /// Iterate over alias entries for diagnostics/testing.
    pub fn entries(&self) -> impl Iterator<Item = &UserAliasEntry> {
        self.entries.iter()
    }

    /// Check if there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Result of caller identity expansion.
///
/// Holds either a borrowed slice from a matching entry or an owned singleton
/// identity for an unmatched caller. Anonymous callers are not passed through
/// this API; integrations represent them separately as `None`.
#[derive(Debug, Clone)]
pub struct ExpandedIdentities<'a> {
    identities: std::borrow::Cow<'a, [String]>,
}

impl<'a> ExpandedIdentities<'a> {
    /// Create from a matching entry (borrowed identities).
    fn matched(identities: &'a [String]) -> Self {
        Self {
            identities: std::borrow::Cow::Borrowed(identities),
        }
    }

    /// Create for an unmatched caller (owned single identity).
    fn unmatched(caller: &str) -> Self {
        Self {
            identities: std::borrow::Cow::Owned(vec![caller.to_string()]),
        }
    }

    /// Access the expanded identity slice.
    pub fn as_slice(&'a self) -> &'a [String] {
        &self.identities
    }

    /// Iterate over identities.
    pub fn iter(&'a self) -> impl Iterator<Item = &'a str> {
        self.identities.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create aliases from inline YAML.
    fn aliases(yaml: &str) -> Result<UserAliases> {
        UserAliases::from_yaml(yaml)
    }

    /// Helper to expand and collect identities.
    fn expand(aliases: &UserAliases, caller: &str) -> Vec<String> {
        aliases
            .expand_caller(caller)
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    struct ExpansionCase {
        name: &'static str,
        yaml: &'static str,
        expectations: &'static [(&'static str, &'static [&'static str])],
    }

    // Each row keeps its original fixture and caller checks, including overlapping
    // entries. A/B followed by B/C must never produce the transitive union A/B/C.
    const EXPANSION_CASES: &[ExpansionCase] = &[
        ExpansionCase {
            name: "matching_caller_expands_to_entry_identities",
            yaml: r#"
- name: alice-and-bob
  identities: [alice, bob]
"#,
            expectations: &[("alice", &["alice", "bob"]), ("bob", &["alice", "bob"])],
        },
        ExpansionCase {
            name: "unmatched_caller_returns_singleton",
            yaml: r#"
- name: team-a
  identities: [alice, bob]
"#,
            expectations: &[("charlie", &["charlie"])],
        },
        ExpansionCase {
            name: "empty_aliases_returns_singleton",
            yaml: "[]",
            expectations: &[("alice", &["alice"])],
        },
        ExpansionCase {
            name: "empty_identity_matches_exact_empty_string_entry",
            yaml: r#"
- name: team-a
  identities: ["", alice]
"#,
            expectations: &[("", &["", "alice"])],
        },
        ExpansionCase {
            name: "unmatched_empty_identity_returns_empty_string_singleton",
            yaml: r#"
- name: team-a
  identities: [alice]
"#,
            expectations: &[("", &[""])],
        },
        ExpansionCase {
            name: "first_entry_containing_caller_wins",
            yaml: r#"
- name: team-a
  identities: [alice, bob]
- name: team-b
  identities: [bob, charlie]
"#,
            expectations: &[
                ("alice", &["alice", "bob"]),
                ("bob", &["alice", "bob"]),
                ("charlie", &["bob", "charlie"]),
            ],
        },
        ExpansionCase {
            name: "duplicate_identities_across_entries_do_not_merge",
            yaml: r#"
- name: team-a
  identities: [alice, bob]
- name: team-b
  identities: [alice, charlie]
"#,
            expectations: &[("bob", &["alice", "bob"]), ("alice", &["alice", "bob"])],
        },
        ExpansionCase {
            name: "overlapping_entries_do_not_transitively_expand",
            yaml: r#"
- name: alias-a
  identities: [alice, bob]
- name: alias-b
  identities: [bob, charlie]
"#,
            expectations: &[
                ("alice", &["alice", "bob"]),
                ("bob", &["alice", "bob"]),
                ("charlie", &["bob", "charlie"]),
            ],
        },
        ExpansionCase {
            name: "matching_is_case_sensitive",
            yaml: r#"
- name: team-a
  identities: [Alice]
"#,
            expectations: &[("Alice", &["Alice"]), ("alice", &["alice"])],
        },
        ExpansionCase {
            name: "exact_identity_strings_not_globbed",
            yaml: r#"
- name: team-a
  identities: ["alice@example.com"]
"#,
            expectations: &[
                ("alice@example.com", &["alice@example.com"]),
                ("alice", &["alice"]),
            ],
        },
        ExpansionCase {
            name: "name_is_not_matched_as_identity",
            yaml: r#"
- name: team-a
  identities: [alice, bob]
"#,
            expectations: &[("team-a", &["team-a"])],
        },
        ExpansionCase {
            name: "empty_identities_array_is_valid",
            yaml: r#"
- name: no-identities
  identities: []
"#,
            expectations: &[("alice", &["alice"])],
        },
        ExpansionCase {
            name: "entry_with_empty_identities_does_not_match",
            yaml: r#"
- name: empty-team
  identities: []
- name: real-team
  identities: [bob]
"#,
            expectations: &[("alice", &["alice"]), ("bob", &["bob"])],
        },
        ExpansionCase {
            name: "duplicate_entries_treated_independently",
            yaml: r#"
- name: team-a
  identities: [alice]
- name: team-a
  identities: [bob]
"#,
            expectations: &[("alice", &["alice"]), ("bob", &["bob"])],
        },
    ];

    #[test]
    fn caller_expansion_cases() {
        for case in EXPANSION_CASES {
            let aliases = aliases(case.yaml).unwrap_or_else(|error| {
                panic!("{}: could not parse {:?}: {error:#}", case.name, case.yaml)
            });
            for (caller, expected) in case.expectations {
                assert_eq!(
                    expand(&aliases, caller),
                    *expected,
                    "{}: caller {caller:?}, YAML {:?}",
                    case.name,
                    case.yaml
                );
            }
        }
    }

    struct RejectionCase {
        name: &'static str,
        yaml: &'static str,
        error_terms: &'static [&'static str],
    }

    // Preserve each original error predicate: any listed term is accepted.
    // Empty terms means the original case required only an error (scalar identities).
    const REJECTION_CASES: &[RejectionCase] = &[
        RejectionCase {
            name: "name_field_is_required",
            yaml: r#"
- identities: [alice]
"#,
            error_terms: &["missing field", "parse"],
        },
        RejectionCase {
            name: "identities_field_is_required",
            yaml: r#"
- name: test-entry
"#,
            error_terms: &["missing field", "parse"],
        },
        RejectionCase {
            name: "unknown_fields_are_rejected",
            yaml: r#"
- name: team-a
  identities: [alice]
  unknown_field: value
"#,
            error_terms: &["unknown field", "expected", "parse"],
        },
        RejectionCase {
            name: "top_level_mapping_is_rejected",
            yaml: r#"
name: team-a
identities: [alice]
"#,
            error_terms: &["mapping", "sequence", "parse"],
        },
        RejectionCase {
            name: "empty_document_is_rejected",
            yaml: "",
            error_terms: &["null", "parse", "empty"],
        },
        RejectionCase {
            name: "null_root_is_rejected",
            yaml: "null",
            error_terms: &["null", "sequence"],
        },
        RejectionCase {
            name: "comment_only_is_rejected",
            yaml: "# just a comment\n# nothing else",
            error_terms: &["null", "parse"],
        },
        RejectionCase {
            name: "whitespace_only_is_rejected",
            yaml: "   \n  \n",
            error_terms: &["null", "parse", "empty"],
        },
        RejectionCase {
            name: "document_marker_alone_is_rejected",
            yaml: "---",
            error_terms: &["null", "sequence"],
        },
        RejectionCase {
            name: "document_marker_with_newline_is_rejected",
            yaml: "---\n",
            error_terms: &["null", "sequence"],
        },
        RejectionCase {
            name: "document_marker_with_end_marker_is_rejected",
            yaml: "---\n...",
            error_terms: &["sequence", "scalar", "parse"],
        },
        RejectionCase {
            name: "document_marker_with_inline_comment_is_rejected",
            yaml: "--- # just a comment",
            error_terms: &["null", "sequence"],
        },
        RejectionCase {
            name: "document_marker_with_comment_line_is_rejected",
            yaml: "---\n# just a comment\n",
            error_terms: &["null", "parse", "sequence"],
        },
        RejectionCase {
            name: "scalar_string_root_is_rejected",
            yaml: "scalar_value",
            error_terms: &["scalar", "sequence"],
        },
        RejectionCase {
            name: "scalar_number_root_is_rejected",
            yaml: "42",
            error_terms: &["scalar", "sequence", "number"],
        },
        RejectionCase {
            name: "scalar_bool_root_is_rejected",
            yaml: "true",
            error_terms: &["scalar", "sequence", "bool"],
        },
        RejectionCase {
            name: "identities_string_scalar_is_rejected",
            yaml: r#"
- name: test
  identities: scalar
"#,
            error_terms: &[],
        },
        RejectionCase {
            name: "name_must_be_string",
            yaml: r#"
- name: 123
  identities: [alice]
"#,
            error_terms: &["parse", "string"],
        },
    ];

    #[test]
    fn invalid_documents_and_schema_are_rejected() {
        for case in REJECTION_CASES {
            let error = aliases(case.yaml)
                .expect_err(&format!("{}: YAML {:?}", case.name, case.yaml))
                .to_string();
            assert!(
                case.error_terms.is_empty()
                    || case.error_terms.iter().any(|term| error.contains(term)),
                "{}: YAML {:?}, expected one of {:?}, got: {error}",
                case.name,
                case.yaml,
                case.error_terms
            );
        }
    }

    #[test]
    fn empty_sequence_is_valid() {
        // Explicit empty sequence is valid no-op configuration
        let aliases = aliases("[]").unwrap();
        assert!(aliases.is_empty());
        assert_eq!(expand(&aliases, "alice"), vec!["alice"]);
    }

    #[test]
    fn duplicate_identity_in_same_entry_is_silent() {
        let aliases = aliases(
            r#"
- name: team-a
  identities: [alice, bob, alice]
"#,
        )
        .unwrap();
        let identities: Vec<_> = aliases
            .expand_caller("alice")
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(identities.contains(&"alice".to_string()));
        assert!(identities.contains(&"bob".to_string()));
    }

    #[test]
    fn identities_must_be_sequence_of_strings() {
        assert!(
            aliases(
                r#"
- name: test
  identities: [alice, 123, bob]
"#,
            )
            .is_err(),
            "unquoted numeric identity must be rejected"
        );

        let aliases = aliases(
            r#"
- name: test
  identities: [alice, "123", bob]
"#,
        )
        .expect("quoted numeric identity is a string");
        assert_eq!(expand(&aliases, "123"), vec!["alice", "123", "bob"]);
    }

    #[test]
    fn load_missing_file_returns_error_with_path() {
        let path = Path::new("/nonexistent/path/users.yaml");
        let error = UserAliases::load(path).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/path/users.yaml"));
    }

    #[test]
    fn load_parse_error_includes_path() {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let path = dir.path().join("malformed.yaml");
        std::fs::write(&path, "not: a: sequence: structure").unwrap();

        let error = UserAliases::load(&path).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
    }
}
