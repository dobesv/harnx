//! Deterministic, prefix-budgeted tool names.
//!
//! Raw names come from the sanitized `operationId`, or `{method}_{path}` when
//! the document omits it. Names must stay valid after the runtime prefixes the
//! server name (`<server>_<raw>`), which is capped at
//! [`MAX_MODEL_TOOL_NAME_LEN`] so unqualified model-facing names fit the
//! 64-character limit providers impose. A server name that cannot leave at
//! least [`MIN_RAW_NAME_BUDGET`] characters for the raw name is a
//! configuration error, not a silent truncation.
//!
//! Truncation and duplicate-ID collisions start with `_` plus the first 8 hex
//! characters of the SHA-256 of `"<METHOD> <path>"`. The complete candidate is
//! checked against every reserved name; a deterministic counter suffix handles
//! a literal name that already occupies it. Uncollided names stay unchanged.
//!
//! Sanitization reuses `harnx_core::package_namespace::sanitize_for_tool_name`
//! and additionally folds to strict ASCII `[A-Za-z0-9_-]`: provider function
//! names must be ASCII, and all budgets are byte budgets.

use std::collections::HashSet;

use harnx_core::package_namespace::sanitize_for_tool_name;

use crate::compact::short_hash;
use crate::error::{SpecError, SpecResult};

/// Upper bound for a model-facing `<server>_<raw>` tool name.
pub const MAX_MODEL_TOOL_NAME_LEN: usize = 64;

/// Smallest raw-name budget a usable server name must leave.
pub const MIN_RAW_NAME_BUDGET: usize = 24;

/// Stop trying suffix variants after this many final candidates.
const MAX_NAME_DISAMBIGUATION_ATTEMPTS: usize = 256;

/// Assigns unique raw tool names for one catalog build.
pub struct Namer {
    max_raw: usize,
    used: HashSet<String>,
}

impl Namer {
    /// Prepare naming for the given standalone server name.
    ///
    /// The budget is computed from the sanitized server name's byte length,
    /// so the `<server>_<raw>` model names fit [`MAX_MODEL_TOOL_NAME_LEN`]
    /// bytes for any input.
    pub fn new(server_name: &str) -> SpecResult<Self> {
        if server_name.trim().is_empty() {
            return Err(SpecError::config(
                "the tool server name is empty; a standalone server needs a name to budget tool names",
            ));
        }
        let prefix = sanitize_server_name(server_name);
        let budget = MAX_MODEL_TOOL_NAME_LEN
            .checked_sub(prefix.len() + 1)
            .ok_or_else(|| {
                SpecError::config(format!(
                    "server name {server_name:?} leaves no characters for tool names after the \
                     {MAX_MODEL_TOOL_NAME_LEN}-character model-name limit"
                ))
            })?;
        if budget < MIN_RAW_NAME_BUDGET {
            return Err(SpecError::config(format!(
                "server name {server_name:?} (sanitized {prefix:?}) leaves only {budget} \
                 characters for tool names; at least {MIN_RAW_NAME_BUDGET} are required. Choose a \
                 shorter --name."
            )));
        }
        Ok(Self {
            max_raw: budget,
            used: HashSet::new(),
        })
    }

    // qual:api — the raw-name budget is part of the catalog's public
    // interface; tests and downstream consumers read it directly.
    /// The raw-name budget this server name leaves.
    pub fn max_raw_len(&self) -> usize {
        self.max_raw
    }

    /// Sanitize a proposed base name (an `operationId` or a `{method}_{path}`
    /// fallback) and make it unique. Returns a located error if every bounded
    /// suffix candidate is already reserved.
    pub fn assign(&mut self, base: &str, method: &str, path: &str) -> SpecResult<String> {
        let base = sanitize_operation_id(base);
        if base.len() <= self.max_raw && self.used.insert(base.clone()) {
            return Ok(base);
        }

        let hash = short_hash(&format!("{method} {path}"));
        for attempt in 0..MAX_NAME_DISAMBIGUATION_ATTEMPTS {
            let suffix = if attempt == 0 {
                format!("_{hash}")
            } else {
                format!("_{hash}_{attempt}")
            };
            let trunk_len = self.max_raw.saturating_sub(suffix.len());
            let mut candidate = truncate_bytes(&base, trunk_len);
            candidate.push_str(&suffix);
            if self.used.insert(candidate.clone()) {
                return Ok(candidate);
            }
        }

        Err(SpecError::invalid(
            format!("{} {path}", method.to_uppercase()),
            format!(
                "could not allocate a unique tool name within the {}-character name budget after \
                 {MAX_NAME_DISAMBIGUATION_ATTEMPTS} deterministic suffix candidates",
                self.max_raw
            ),
        ))
    }
}

/// Sanitize an operation ID into the strict ASCII tool-name character set.
///
/// `harnx_core::package_namespace::sanitize_for_tool_name` keeps Unicode
/// letters (its own documented `[A-Za-z0-9_-]` charset is aspirational there);
/// provider function names must be ASCII, so anything outside
/// `[A-Za-z0-9_-]` folds to `_`. A leading digit gets an `op_` prefix so the
/// name can start an identifier.
pub fn sanitize_operation_id(operation_id: &str) -> String {
    let mut sanitized = sanitize_server_name(operation_id);
    if sanitized.is_empty() {
        sanitized = "op".to_string();
    }
    if sanitized.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        sanitized = format!("op_{sanitized}");
    }
    sanitized
}

/// The server name after the runtime's sanitizer, folded to strict ASCII.
///
/// `harnx_core::package_namespace::sanitize_for_tool_name` keeps Unicode
/// letters (its own documented `[A-Za-z0-9_-]` charset is aspirational there);
/// provider function names must be ASCII, so anything outside
/// `[A-Za-z0-9_-]` folds to `_`.
fn sanitize_server_name(name: &str) -> String {
    sanitize_for_tool_name(name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The `{method}_{path}` fallback base for an operation without an ID.
pub fn fallback_base(method: &str, path: &str) -> String {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    format!("{}_{}", method.to_ascii_lowercase(), segments)
}

/// Truncate to at most `max_bytes` bytes without splitting a character.
fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids_pass_through_sanitized() {
        harnx_core::require_nextest();
        assert_eq!(sanitize_operation_id("createIssue"), "createIssue");
        assert_eq!(sanitize_operation_id("weird/id op"), "weird__id_op");
        assert_eq!(sanitize_operation_id("x@y"), "x__at__y");
        assert_eq!(sanitize_operation_id(""), "op");
        assert_eq!(sanitize_operation_id("2fast"), "op_2fast");
    }

    #[test]
    fn unicode_operation_ids_fold_to_ascii() {
        harnx_core::require_nextest();
        // Provider function names must be ASCII; Unicode alphanumerics fold.
        let sanitized = sanitize_operation_id("ünïcödéOp");
        assert!(
            sanitized
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{sanitized}"
        );
        assert_eq!(sanitized, "_n_c_d_Op");
        assert_eq!(sanitize_operation_id("日本Op"), "__Op");
    }

    #[test]
    fn unicode_server_names_fold_to_ascii_within_the_byte_budget() {
        harnx_core::require_nextest();
        // Non-ASCII folds to `_`, so the byte budget counts sanitized bytes:
        // 20 é chars become 20 bytes, well inside the budget.
        let namer = Namer::new(&"é".repeat(20)).unwrap();
        assert_eq!(namer.max_raw_len(), MAX_MODEL_TOOL_NAME_LEN - 21);
        // 40 ASCII bytes leave 23 for the raw name — under the 24 minimum.
        assert!(Namer::new(&"x".repeat(40)).is_err());
        assert!(Namer::new(&"x".repeat(39)).is_ok());
        // 50 folded characters leave 13, far under the budget.
        let error = match Namer::new(&"é".repeat(50)) {
            Err(error) => error,
            Ok(_) => panic!("50-character server name must be rejected"),
        };
        assert!(error.to_string().contains("--name"), "{}", error);
        assert!(Namer::new("").is_err());
    }

    #[test]
    fn fallback_base_is_method_and_path() {
        harnx_core::require_nextest();
        assert_eq!(
            fallback_base("GET", "/rest/api/3/issue/{issueIdOrKey}"),
            "get_rest_api_3_issue_{issueIdOrKey}"
        );
    }

    #[test]
    fn long_names_truncate_with_stable_hash_suffix() {
        harnx_core::require_nextest();
        let mut namer = Namer::new("jira").unwrap();
        let long_id = "a".repeat(100);
        let name = namer.assign(&long_id, "get", "/x").unwrap();
        assert!(name.len() <= namer.max_raw_len());
        assert_eq!(name.len(), 59);
        assert!(name.ends_with(&format!("_{}", short_hash("get /x"))));
        // Deterministic: rebuilding from the same assignments gives the same name.
        let mut again = Namer::new("jira").unwrap();
        assert_eq!(name, again.assign(&long_id, "get", "/x").unwrap());
        assert_eq!(
            namer.assign("other", "get", "/y").unwrap(),
            again.assign("other", "get", "/y").unwrap()
        );
    }

    #[test]
    fn duplicate_operation_ids_get_distinct_stable_names() {
        harnx_core::require_nextest();
        let mut namer = Namer::new("jira").unwrap();
        let first = namer.assign("dupOp", "get", "/a").unwrap();
        let second = namer.assign("dupOp", "post", "/b").unwrap();
        assert_eq!(first, "dupOp");
        assert_ne!(first, second);
        assert!(second.starts_with("dupOp_"));
        // Stable across rebuilds.
        let mut rebuilt = Namer::new("jira").unwrap();
        assert_eq!(rebuilt.assign("dupOp", "get", "/a").unwrap(), first);
        assert_eq!(rebuilt.assign("dupOp", "post", "/b").unwrap(), second);
    }

    #[test]
    fn literal_operation_id_cannot_shadow_a_generated_suffix() {
        harnx_core::require_nextest();
        let mut namer = Namer::new("jira").unwrap();
        let original = namer.assign("dupOp", "get", "/first").unwrap();
        let hash = short_hash("post /generated");
        let literal = format!("dupOp_{hash}");
        assert_eq!(namer.assign(&literal, "get", "/literal").unwrap(), literal);

        let generated = namer.assign("dupOp", "post", "/generated").unwrap();
        assert_eq!(original, "dupOp");
        assert_ne!(generated, literal);
        assert!(
            generated.starts_with(&format!("dupOp_{hash}_")),
            "{generated}"
        );
        assert!(generated.len() <= namer.max_raw_len());

        let mut rebuilt = Namer::new("jira").unwrap();
        assert_eq!(rebuilt.assign("dupOp", "get", "/first").unwrap(), original);
        assert_eq!(
            rebuilt.assign(&literal, "get", "/literal").unwrap(),
            literal
        );
        assert_eq!(
            rebuilt.assign("dupOp", "post", "/generated").unwrap(),
            generated
        );
    }

    #[test]
    fn exhausted_final_candidates_return_a_located_error() {
        harnx_core::require_nextest();
        let mut namer = Namer::new("jira").unwrap();
        let base = sanitize_operation_id("dupOp");
        namer.used.insert(base.clone());
        let hash = short_hash("post /generated");
        for attempt in 0..MAX_NAME_DISAMBIGUATION_ATTEMPTS {
            let suffix = if attempt == 0 {
                format!("_{hash}")
            } else {
                format!("_{hash}_{attempt}")
            };
            let mut candidate = truncate_bytes(&base, namer.max_raw - suffix.len());
            candidate.push_str(&suffix);
            namer.used.insert(candidate);
        }

        let error = namer.assign("dupOp", "post", "/generated").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("POST /generated"), "{message}");
        assert!(
            message.contains("256 deterministic suffix candidates"),
            "{message}"
        );
    }

    #[test]
    fn unicode_long_names_stay_within_the_byte_budget() {
        harnx_core::require_nextest();
        let mut namer = Namer::new("jira").unwrap();
        let name = namer.assign(&"ü".repeat(100), "get", "/x").unwrap();
        assert!(name.len() <= namer.max_raw_len(), "{name}");
        // The folding means the sanitized candidate is ASCII underscores.
        assert!(name.starts_with("_"), "{name}");
        assert!(name.ends_with(&format!("_{}", short_hash("get /x"))));
    }
}
