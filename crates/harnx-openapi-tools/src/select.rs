//! Optional, repeatable operation selection for the future
//! `--include-operation` CLI flag.
//!
//! This module is the library seam for that flag; the flag itself is not
//! wired here (T4), and the existing final `--enable-tool` raw-name toolset
//! filter stays a separate intersection applied by the toolset host after
//! this selection — neither replaces the other.
//!
//! Contract:
//!
//! * No selectors is the identity: the full catalog is kept. There is no
//!   default filtering; selection is strictly opt-in.
//! * Selectors form a positive union: an operation is included when any
//!   selector matches. Repeat selectors are fine; the result never contains
//!   an operation twice.
//! * `id:<glob>` matches the *unsanitized* source `operationId` of the
//!   effective (post-patch) document, case-sensitively. Operations without
//!   an `operationId` never match an `id:` selector — use `route:` for
//!   them. Duplicate IDs select every operation carrying that ID.
//! * `route:<METHOD> <path-glob>` matches the declared path template
//!   case-sensitively; `<METHOD>` is `*` or compared ASCII
//!   case-insensitively against the operation's lowercased method.
//! * The only wildcard is the whole-string `*`, which also spans `/`.
//!   There is no regexp, brace, comma, `?`, or negation syntax: those
//!   characters are literal bytes (and so almost always fail to match).
//! * Names are assigned against the FULL normalized inventory before
//!   selection (free [`select_operations`] after
//!   [`crate::Catalog::build`]), so collision survivors keep
//!   the names they had in the full catalog.
//! * Selection governs declarations and dispatch together: the filtered
//!   [`crate::Catalog`] excludes the operations from both `tool_specs` and
//!   the `find_*` lookups, so an excluded operation is not callable.
//! * Every selector must match at least one operation; an individually
//!   unmatched selector is an error, not a silent skip. Invalid or empty
//!   selectors are errors too. Unsupported document constructs are never
//!   hidden: normalization runs on the whole effective source first, and
//!   only a catalog that normalizes can be filtered.
//! * Matching uses the effective source document — the one the catalog was
//!   normalized from, after any ordered [`crate::spec_patch`] patches.

use std::collections::HashMap;

use serde_json::Value;

use crate::catalog::Catalog;
use crate::document::SpecDocument;
use crate::error::{SpecError, SpecResult};

/// The HTTP methods a `route:` selector may name.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

/// One parsed `--include-operation` selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationSelector {
    /// `id:<glob>` — match the source `operationId`, case-sensitively.
    Id(String),
    /// `route:<METHOD> <path-glob>` — match the operation's method and
    /// declared path template. `method` is `None` for `*`.
    Route {
        /// `None` matches any method; otherwise the lowercased method name.
        method: Option<String>,
        /// Path template glob, case-sensitive, `*` spans `/`.
        path: String,
    },
}

impl OperationSelector {
    /// Whether one operation matches this selector.
    fn matches(&self, method: &str, path: &str, operation_id: Option<&str>) -> bool {
        match self {
            OperationSelector::Id(glob) => match operation_id {
                Some(id) => glob_matches(glob, id),
                None => false,
            },
            OperationSelector::Route {
                method: selector_method,
                path: selector_path,
            } => {
                let method_ok = match selector_method {
                    None => true,
                    Some(expected) => expected == method,
                };
                method_ok && glob_matches(selector_path, path)
            }
        }
    }
}

/// Whole-string glob match where `*` matches any run of bytes, `/` included.
///
/// Every other character — `?`, `[`, `{`, `,`, `!` — is literal: there is no
/// regexp, class, brace, comma or negation syntax.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return true;
    };
    let middle: Vec<&str> = parts.collect();
    let Some((last, middle)) = middle.split_last() else {
        // No `*` at all: a plain literal comparison.
        return pattern == text;
    };
    let Some(rest) = text.strip_prefix(first) else {
        return false;
    };
    let mut rest = rest;
    for part in middle {
        if part.is_empty() {
            continue;
        }
        let Some(found) = rest.find(part) else {
            return false;
        };
        rest = &rest[found + part.len()..];
    }
    rest.ends_with(last)
}

/// A validated selector list. No selectors selects everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationSelection {
    selectors: Vec<OperationSelector>,
}

impl OperationSelection {
    /// Parse and validate a `--include-operation` value list.
    ///
    /// Errors name the offending selector: empty values, unknown prefixes,
    /// empty glob/method/path parts, non-alphabetic methods, and paths that
    /// do not start with `/` are all invalid before any catalog exists, so
    /// the server can reject configuration offline.
    pub fn parse(selectors: &[String]) -> SpecResult<Self> {
        let parsed = selectors
            .iter()
            .map(|selector| parse_selector(selector))
            .collect::<SpecResult<Vec<_>>>()?;
        Ok(Self { selectors: parsed })
    }

    /// True when the list is empty — the identity selection.
    pub fn is_empty(&self) -> bool {
        self.selectors.is_empty()
    }
}

fn parse_selector(selector: &str) -> SpecResult<OperationSelector> {
    let invalid = |detail: String| {
        Err(SpecError::config(format!(
            "invalid operation selector {selector:?}: {detail}"
        )))
    };
    if selector.trim().is_empty() {
        return invalid("the selector is empty".to_string());
    }
    if let Some(glob) = selector.strip_prefix("id:") {
        if glob.is_empty() {
            return invalid("`id:` needs an operationId glob".to_string());
        }
        return Ok(OperationSelector::Id(glob.to_string()));
    }
    if let Some(route) = selector.strip_prefix("route:") {
        let Some((method, path)) = route.split_once(' ') else {
            return invalid("`route:` needs the form `route:<METHOD> <path-glob>`".to_string());
        };
        if method.is_empty() {
            return invalid("the method part is empty".to_string());
        }
        if !path.starts_with('/') {
            return invalid("the path glob must start with `/`".to_string());
        }
        let method = if method == "*" {
            None
        } else if method.chars().all(|c| c.is_ascii_alphabetic()) {
            Some(method.to_ascii_lowercase())
        } else {
            return invalid(format!(
                "the method {method:?} must be `*` or ASCII letters"
            ));
        };
        return Ok(OperationSelector::Route {
            method,
            path: path.to_string(),
        });
    }
    invalid("the selector must start with `id:` or `route:`".to_string())
}

/// The unsanitized source `operationId` of every operation, keyed by the
/// lowercased method and the declared path template.
///
/// The caller must pass the effective source document — the one the catalog
/// was normalized from (after patches) — so `id:` selectors match what the
/// patched source actually declares.
fn source_operation_ids(document: &SpecDocument) -> HashMap<(String, String), Option<String>> {
    let mut ids = HashMap::new();
    let Some(paths) = document.root.get("paths").and_then(Value::as_object) else {
        return ids;
    };
    for (path, path_item) in paths {
        let Some(path_item) = path_item.as_object() else {
            continue;
        };
        for (key, operation) in path_item {
            let method = key.to_ascii_lowercase();
            if !METHODS.contains(&method.as_str()) || !operation.is_object() {
                continue;
            }
            let operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string);
            ids.insert((method, path.clone()), operation_id);
        }
    }
    ids
}

/// Select the operations of a catalog by a positive union of selectors.
///
/// The `document` must be the effective source this catalog was normalized
/// from (after any ordered patches); `id:` selectors match its unsanitized
/// `operationId`s. Names were already assigned against the full inventory,
/// so survivors keep their full-catalog names. The filtered catalog governs
/// declarations and dispatch together: excluded operations disappear from
/// `tool_specs` and both `find_*` lookups. An empty selector list returns
/// the full catalog unchanged; an individually unmatched selector is an
/// error. Schema diagnostics describe the source document and are kept
/// verbatim regardless of selection.
pub fn select_operations(
    catalog: &Catalog,
    document: &SpecDocument,
    selectors: &[String],
) -> SpecResult<Catalog> {
    let selection = OperationSelection::parse(selectors)?;
    if selection.is_empty() {
        return Ok(catalog.clone());
    }
    let ids = source_operation_ids(document);
    let mut per_selector_matches = vec![0usize; selection.selectors.len()];
    let mut operations = Vec::new();
    for operation in &catalog.operations {
        let operation_id = ids
            .get(&(operation.method.clone(), operation.path.clone()))
            .and_then(|id| id.clone());
        let mut keep = false;
        for (index, selector) in selection.selectors.iter().enumerate() {
            if selector.matches(&operation.method, &operation.path, operation_id.as_deref()) {
                per_selector_matches[index] += 1;
                keep = true;
            }
        }
        if keep {
            operations.push(operation.clone());
        }
    }
    for (index, count) in per_selector_matches.into_iter().enumerate() {
        if count == 0 {
            return Err(SpecError::config(format!(
                "operation selector {} of {} matched no operation: {:?}",
                index + 1,
                selectors.len(),
                selectors[index]
            )));
        }
    }
    let mut selected = catalog.clone();
    selected.operations = operations;
    Ok(selected)
}

/// Build a catalog from patched source and then select operations — the
/// single seam the future server CLI (T4) will call.
///
/// The pipeline order is fixed: the ordered `--spec-patch` list produces
/// the effective source first; that source is normalized as a whole (names
/// are assigned against the full inventory, unsupported constructs fail and
/// cannot be hidden); the selector union is applied last, so matching sees
/// the patched source. Either `spec_patches` or `operation_selectors` may be
/// empty — empty both reproduces [`Catalog::build`].
// qual:api — the pipeline entry the T4 server CLI (`--spec-patch` plus
// `--include-operation`) will call.
pub fn build_catalog_with_controls(
    document: &SpecDocument,
    server_name: &str,
    spec_patches: &[String],
    patch_limits: crate::spec_patch::SpecPatchLimits,
    operation_selectors: &[String],
) -> SpecResult<Catalog> {
    let effective = crate::spec_patch::apply_spec_patches(document, spec_patches, patch_limits)?;
    let full = Catalog::build(&effective, server_name)?;
    select_operations(&full, &effective, operation_selectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(selectors: &[&str]) -> SpecResult<OperationSelection> {
        let owned: Vec<String> = selectors
            .iter()
            .map(|selector| selector.to_string())
            .collect();
        OperationSelection::parse(&owned)
    }

    /// Positive-union matching, exercised over the private selector list.
    fn matches(
        selection: &OperationSelection,
        method: &str,
        path: &str,
        operation_id: Option<&str>,
    ) -> bool {
        selection
            .selectors
            .iter()
            .any(|selector| selector.matches(method, path, operation_id))
    }

    #[test]
    fn empty_selection_is_the_identity_marker() {
        harnx_core::require_nextest();
        // An empty list carries no selectors, so nothing matches it directly;
        // the identity behavior (full catalog) lives in `select_operations`.
        let selection = parse(&[]).unwrap();
        assert!(selection.is_empty());
        assert!(!matches(&selection, "post", "/a", None));
        assert!(!matches(&selection, "get", "/b", Some("x")));
    }

    #[test]
    fn id_selectors_are_case_sensitive_and_need_an_id() {
        harnx_core::require_nextest();
        let selection = parse(&["id:createIssue"]).unwrap();
        assert!(matches(&selection, "post", "/issue", Some("createIssue")));
        assert!(!matches(&selection, "post", "/issue", Some("createissue")));
        assert!(!matches(&selection, "post", "/issue", Some("createIssue2")));
        assert!(!matches(&selection, "post", "/issue", None));
    }

    #[test]
    fn route_selectors_match_method_case_insensitively_or_star() {
        harnx_core::require_nextest();
        for selector in ["route:GET /issue", "route:get /issue"] {
            let selection = parse(&[selector]).unwrap();
            assert!(
                matches(&selection, "get", "/issue", None),
                "{selector} must match GET /issue"
            );
            assert!(
                !matches(&selection, "post", "/issue", None),
                "{selector} must not match POST /issue"
            );
        }
        // A `*` method matches every method.
        let selection = parse(&["route:* /issue"]).unwrap();
        for method in ["get", "post", "put", "delete", "trace"] {
            assert!(
                matches(&selection, method, "/issue", None),
                "`*` must match {method} /issue"
            );
        }
        // Path stays case-sensitive.
        let selection = parse(&["route:GET /ISSUE"]).unwrap();
        assert!(!matches(&selection, "get", "/issue", None));
    }

    #[test]
    fn star_is_the_only_wildcard_and_spans_slashes() {
        harnx_core::require_nextest();
        let selection = parse(&["route:GET /rest/*/search"]).unwrap();
        assert!(matches(&selection, "get", "/rest/api/3/search", None));
        assert!(matches(&selection, "get", "/rest/other/search", None));
        assert!(!matches(&selection, "get", "/rest/search", None));
        // `*` method and `/*` path: any operation at all. A bare `*` path is
        // invalid — path globs must start with `/`.
        let selection = parse(&["route:* /*"]).unwrap();
        assert!(matches(&selection, "trace", "/any/thing", None));
        assert!(parse(&["route:* *"]).is_err());
        // `?`, braces, classes and commas are literal, never syntax.
        for bad in [
            "id:createIssu?",
            "id:[a-z]*",
            "id:{createIssue,addAttachment}",
        ] {
            let selection = parse(&[bad]).unwrap();
            assert!(
                !matches(&selection, "post", "/issue", Some("createIssue")),
                "{bad} must not act as a wildcard"
            );
        }
    }

    #[test]
    fn invalid_selectors_are_rejected_with_the_selector_named() {
        harnx_core::require_nextest();
        for bad in [
            "",
            "   ",
            "foo:bar",
            "createIssue",
            "!id:createIssue",
            "id:",
            "route:",
            "route:GET",
            "route: /x",
            "route:GET  x",
            "route:G3T /x",
            "route:get|post /x",
        ] {
            let error = parse(&[bad]).unwrap_err().to_string();
            assert!(
                error.contains("invalid operation selector"),
                "{bad:?}: {error}"
            );
            assert!(error.contains(bad), "{bad:?}: {error}");
        }
    }

    #[test]
    fn source_operation_ids_resolve_by_method_and_path() {
        harnx_core::require_nextest();
        let document = crate::document::SpecDocument::from_value(json!({
            "swagger": "2.0",
            "paths": {
                "/a": {"get": {"operationId": "getA"}},
                "/b": {"post": {}}
            }
        }))
        .unwrap();
        let ids = source_operation_ids(&document);
        assert_eq!(
            ids.get(&("get".to_string(), "/a".to_string())),
            Some(&Some("getA".to_string()))
        );
        assert_eq!(
            ids.get(&("post".to_string(), "/b".to_string())),
            Some(&None)
        );
    }

    #[test]
    fn glob_matches_whole_strings_only() {
        harnx_core::require_nextest();
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("a*c", "abc"));
        assert!(glob_matches("a*c", "a/c"));
        assert!(!glob_matches("a*c", "abcd"));
        assert!(!glob_matches("*c", "abcd"));
        assert!(glob_matches("a*", "a"));
        assert!(!glob_matches("a*a", "a"));
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "x"));
    }
}
