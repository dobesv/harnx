//! Security-scheme analysis: informational schemes and protected inputs.
//!
//! Specification `security`/`securityDefinitions`/`securitySchemes` are
//! informational. They neither choose authentication nor gate registration:
//! all HTTP authorization comes from operator configuration (supplied header
//! credentials, client_credentials OAuth), never from model arguments or spec
//! choices.
//!
//! Two things do derive from the schemes:
//!
//! - A parameter that the spec itself identifies as a credential — a header
//!   parameter named by an `apiKey`-in-header scheme, or a query parameter
//!   named by an `apiKey`-in-query scheme at the matching location — is a
//!   protected input. It is excluded from callable model arguments
//!   (credentials are never model arguments) and recorded in the catalog for
//!   the operator.
//! - API keys in query or cookie are placements this release cannot supply.
//!   The catalog emits a diagnostic naming the scheme, its placement and its
//!   parameter name (never a value) as soon as the scheme is declared —
//!   whether or not an operation redundantly declares the parameter too — and
//!   keeps every operation available for other operator-selected credentials.
//!
//! Ordinary parameters that merely look like credentials by name are never
//! protected — only parameters at the scheme's own location that the scheme
//! itself names are.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{SpecError, SpecResult};

/// Which placement an `apiKey` security scheme declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiKeyPlacement {
    Header,
    Query,
    Cookie,
}

impl ApiKeyPlacement {
    /// The wire location name, for diagnostics.
    pub fn wire_location(self) -> &'static str {
        WIRE_LOCATIONS[self as usize]
    }
}

/// Diagnostic names of [`ApiKeyPlacement`], indexed by discriminant.
const WIRE_LOCATIONS: [&str; 3] = ["header", "query", "cookie"];

/// A security scheme identified as carrying a credential in a parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedParam {
    /// The wire parameter name (for header schemes, lowercased for matching).
    pub name: String,
    /// The security scheme that named it.
    pub scheme: String,
    pub placement: ApiKeyPlacement,
}

/// The document's security schemes, reduced to what normalization needs.
#[derive(Clone, Debug, Default)]
pub struct SecuritySchemes {
    /// Parameter name (lowercased) → scheme, for header API keys.
    pub header_api_keys: BTreeMap<String, String>,
    /// Parameter name → scheme, for query API keys.
    pub query_api_keys: BTreeMap<String, String>,
    /// Parameter name → scheme, for cookie API keys.
    pub cookie_api_keys: BTreeMap<String, String>,
    /// All declared scheme names, informational.
    pub scheme_names: Vec<String>,
    /// Unsupported placements/types seen, for startup diagnostics.
    pub unsupported_scheme_types: Vec<String>,
}

impl SecuritySchemes {
    /// Collect the security schemes of a Swagger 2.0 or OpenAPI 3.0 document.
    pub fn collect(document: &crate::document::SpecDocument) -> Self {
        let mut schemes = Self::default();
        let declarations = match document.kind {
            crate::document::SpecKind::Swagger2 => document.root.get("securityDefinitions"),
            crate::document::SpecKind::OpenApi3 => document
                .root
                .get("components")
                .and_then(|components| components.get("securitySchemes")),
        };
        let Some(declarations) = declarations.and_then(Value::as_object) else {
            return schemes;
        };
        for (name, declaration) in declarations {
            schemes.scheme_names.push(name.clone());
            schemes.collect_scheme(name, declaration);
        }
        schemes
    }

    /// Register one declared scheme.
    fn collect_scheme(&mut self, name: &str, declaration: &Value) {
        let kind = declaration
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "mutualTLS" {
            self.unsupported_scheme_types.push(format!(
                "scheme {name:?} uses unsupported type \"mutualTLS\""
            ));
        }
        if kind != "apiKey" {
            return;
        }
        let Some(parameter_name) = declaration.get("name").and_then(Value::as_str) else {
            return;
        };
        match declaration.get("in").and_then(Value::as_str) {
            Some("header") => {
                self.header_api_keys
                    .insert(parameter_name.to_ascii_lowercase(), name.to_string());
            }
            Some("query") => {
                self.query_api_keys
                    .insert(parameter_name.to_string(), name.to_string());
                self.push_unsupported_placement(name, ApiKeyPlacement::Query, parameter_name);
            }
            Some("cookie") => {
                self.cookie_api_keys
                    .insert(parameter_name.to_string(), name.to_string());
                self.push_unsupported_placement(name, ApiKeyPlacement::Cookie, parameter_name);
            }
            other => {
                self.unsupported_scheme_types.push(format!(
                    "API key scheme {name:?} sends its credential in {}, which this release \
                     does not support; supply credentials through operator header configuration \
                     or OAuth instead",
                    other.unwrap_or("an unspecified location")
                ));
            }
        }
    }

    /// The unsupported-placement diagnostic for a scheme, with its parameter
    /// name but never a value. Emitted at collection time so it appears even
    /// when no operation declares the parameter redundantly.
    fn push_unsupported_placement(
        &mut self,
        scheme: &str,
        placement: ApiKeyPlacement,
        parameter_name: &str,
    ) {
        let diagnostic = format!(
            "security scheme {scheme:?} places its API key {parameter_name:?} in the {}; this \
             release supports operator-supplied header credentials and client-credentials OAuth \
             only, so the credential is not supplied through tool arguments",
            placement.wire_location()
        );
        if !self.unsupported_scheme_types.contains(&diagnostic) {
            self.unsupported_scheme_types.push(diagnostic);
        }
    }

    /// The scheme a header parameter (by wire name, lowercased) belongs to.
    pub fn header_scheme(&self, name: &str) -> Option<&String> {
        self.header_api_keys.get(&name.to_ascii_lowercase())
    }

    /// The scheme a query parameter belongs to.
    pub fn query_scheme(&self, name: &str) -> Option<&String> {
        self.query_api_keys.get(name)
    }

    /// The scheme a cookie parameter belongs to.
    pub fn cookie_scheme(&self, name: &str) -> Option<&String> {
        self.cookie_api_keys.get(name)
    }

    /// Validate an operation's security requirements: unknown scheme names are
    /// a spec error, not a silent ignore.
    pub fn validate_requirements(&self, required: &[String], context: &str) -> SpecResult<()> {
        for name in required {
            if !self.scheme_names.contains(name) {
                return Err(SpecError::invalid(
                    context,
                    format!("security requirement names undeclared scheme {name:?}"),
                ));
            }
        }
        Ok(())
    }
}

/// The security requirement scheme names for one operation: the operation's
/// own `security` when present, otherwise the document's global `security`.
/// An empty operation-level `security: []` means no requirements.
pub fn operation_security(root: &Value, operation: &Value) -> Vec<String> {
    let requirements = operation
        .get("security")
        .and_then(Value::as_array)
        .or_else(|| root.get("security").and_then(Value::as_array));
    let mut names = Vec::new();
    for requirement in requirements
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        for name in requirement.keys() {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::SpecDocument;
    use serde_json::json;

    fn oas3(security_schemes: Value, security: Value) -> SpecDocument {
        let root = json!({
            "openapi": "3.0.1",
            "paths": {},
            "security": security,
            "components": {"securitySchemes": security_schemes}
        });
        SpecDocument::from_value(root).unwrap()
    }

    #[test]
    fn header_api_keys_map_to_protected_header_params() {
        harnx_core::require_nextest();
        let document = oas3(
            json!({"tokenHeader": {"type": "apiKey", "in": "header", "name": "X-Api-Token"}}),
            json!([{"tokenHeader": []}]),
        );
        let schemes = SecuritySchemes::collect(&document);
        assert_eq!(schemes.header_scheme("x-api-token").unwrap(), "tokenHeader");
        assert!(schemes.header_scheme("x-other").is_none());
        assert!(schemes.unsupported_scheme_types.is_empty());
    }

    #[test]
    fn query_and_cookie_api_keys_are_diagnosed_with_parameter_names() {
        harnx_core::require_nextest();
        let document = oas3(
            json!({
                "queryKey": {"type": "apiKey", "in": "query", "name": "apiKey"},
                "cookieKey": {"type": "apiKey", "in": "cookie", "name": "session"},
                "basic": {"type": "http", "scheme": "basic"}
            }),
            json!([]),
        );
        let schemes = SecuritySchemes::collect(&document);
        assert_eq!(schemes.query_scheme("apiKey").unwrap(), "queryKey");
        assert_eq!(schemes.cookie_scheme("session").unwrap(), "cookieKey");
        let messages = schemes.unsupported_scheme_types.join("\n");
        assert!(messages.contains("queryKey"), "{messages}");
        assert!(messages.contains("query"), "{messages}");
        // Diagnostics name the parameter, never a value.
        assert!(messages.contains("apiKey"), "{messages}");
        assert!(messages.contains("cookieKey"), "{messages}");
        assert!(messages.contains("session"), "{messages}");
    }

    #[test]
    fn ordinary_names_are_never_credentials() {
        harnx_core::require_nextest();
        let document = oas3(
            json!({"tokenHeader": {"type": "apiKey", "in": "header", "name": "X-Api-Token"}}),
            json!([]),
        );
        let schemes = SecuritySchemes::collect(&document);
        // "token" and "Authorization" are not scheme-named parameters.
        assert!(schemes.header_scheme("token").is_none());
        assert!(schemes.header_scheme("authorization").is_none());
    }

    #[test]
    fn operation_security_overrides_global_and_empty_means_none() {
        harnx_core::require_nextest();
        let root = json!({"security": [{"basic": []}]});
        assert_eq!(
            operation_security(&root, &json!({})),
            vec!["basic".to_string()]
        );
        assert_eq!(
            operation_security(&root, &json!({"security": []})),
            Vec::<String>::new()
        );
        assert_eq!(
            operation_security(&root, &json!({"security": [{"oauth": ["read"]}]})),
            vec!["oauth".to_string()]
        );
    }

    #[test]
    fn undeclared_scheme_requirements_are_invalid() {
        harnx_core::require_nextest();
        let document = oas3(
            json!({"basic": {"type": "http", "scheme": "basic"}}),
            json!([]),
        );
        let schemes = SecuritySchemes::collect(&document);
        assert!(schemes
            .validate_requirements(&["missing".to_string()], "GET /x")
            .is_err());
        assert!(schemes
            .validate_requirements(&["basic".to_string()], "GET /x")
            .is_ok());
    }
}
