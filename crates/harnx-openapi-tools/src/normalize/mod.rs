//! Operation normalization: a parsed document to a table of
//! [`NormalizedOperation`]s ready for cataloging.
//!
//! Parameter merge, classification and styles live in [`params`]; request
//! bodies in [`body`]; input-schema assembly in [`input`].

mod body;
mod input;
mod params;

use serde_json::{Map, Value};

use crate::compact::SchemaCompactor;
use crate::document::SpecDocument;
use crate::error::{SpecError, SpecResult};
use crate::naming::{fallback_base, Namer};
use crate::security::{operation_security, SecuritySchemes};

pub use body::{BodySpec, FormField};
pub use input::path_template_names;
pub use params::{MergedParam, ParamSpec, ParamStyle};

/// The HTTP methods an operation can use: every method OpenAPI 3.0
/// advertises, so no advertised method is skipped.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

/// Path-item fields that are not operations. Any other path-item key that is
/// not an HTTP method or a vendor extension is rejected with a diagnostic,
/// so a method this release cannot normalize is never silently dropped.
const PATH_ITEM_FIELDS: [&str; 4] = ["summary", "description", "servers", "parameters"];

/// One normalized API operation: the T2 deliverable consumed by dispatch.
#[derive(Clone, Debug)]
pub struct NormalizedOperation {
    /// Lowercased HTTP method (`get`, `post`, ...).
    pub method: String,
    /// The path template exactly as declared (`/rest/api/3/issue/{issueIdOrKey}`).
    pub path: String,
    /// The tool's raw name; the runtime prefixes the server name.
    pub raw_tool_name: String,
    /// Operation summary (plus a deprecated marker); the tool description.
    pub description: String,
    pub deprecated: bool,
    pub tags: Vec<String>,
    /// Security requirement scheme names, informational only.
    pub security: Vec<String>,
    pub path_params: Vec<ParamSpec>,
    pub query_params: Vec<ParamSpec>,
    pub header_params: Vec<ParamSpec>,
    /// Parameters the spec identifies as credentials: excluded from model
    /// arguments, supplied only by operator configuration.
    pub protected_params: Vec<crate::security::ProtectedParam>,
    pub body: Option<BodySpec>,
    /// The full tool input schema (groups + `$defs`).
    pub input_schema: Value,
}

impl NormalizedOperation {
    /// `OPERATION /path` context for diagnostics.
    pub fn context(&self) -> String {
        format!("{} {}", self.method.to_uppercase(), self.path)
    }

    pub fn is_read_only(&self) -> bool {
        matches!(self.method.as_str(), "get" | "head" | "options")
    }
}

/// One operation under normalization, with its borrowed document context.
struct OperationSite<'a> {
    document: &'a SpecDocument,
    path: &'a str,
    method: &'a str,
    path_item: &'a Map<String, Value>,
    operation: &'a Map<String, Value>,
    context: String,
}

/// The mutable state threaded through every operation: name assignment and
/// the contradictory-schema diagnostics the server surfaces at startup.
struct NormalizeState<'a> {
    namer: &'a mut Namer,
    schemes: &'a SecuritySchemes,
    schema_diagnostics: &'a mut Vec<String>,
}

/// Normalize every operation of a document into one table.
///
/// Names are assigned by `namer` from the operation ID (or method/path
/// fallback); `schema_diagnostics` collects the contradictory-schema
/// messages the server should surface at startup. Credential-placement
/// diagnostics are derived from the schemes themselves (see
/// [`Catalog::build`](crate::catalog::Catalog)), not per operation.
pub fn normalize_document(
    document: &SpecDocument,
    namer: &mut Namer,
    schemes: &SecuritySchemes,
    schema_diagnostics: &mut Vec<String>,
) -> SpecResult<Vec<NormalizedOperation>> {
    operation_sites(document)?
        .into_iter()
        .map(|site| {
            let state = NormalizeState {
                namer: &mut *namer,
                schemes,
                schema_diagnostics: &mut *schema_diagnostics,
            };
            normalize_operation(&site, state)
        })
        .collect()
}

/// Every operation site of a document, in document order, rejecting malformed
/// path items and unsupported path-item `$ref`s up front.
fn operation_sites<'a>(document: &'a SpecDocument) -> SpecResult<Vec<OperationSite<'a>>> {
    let Some(paths) = document.root.get("paths").and_then(Value::as_object) else {
        return Err(SpecError::invalid(
            "document",
            "the `paths` field is missing or not an object",
        ));
    };
    let mut sites = Vec::new();
    for (path, path_item) in paths {
        sites.extend(path_item_sites(document, path, path_item)?);
    }
    Ok(sites)
}

/// Diagnostic location for a path-level problem.
fn path_location(path: &str) -> String {
    format!("path {path}")
}

/// The operation sites of one path item.
fn path_item_sites<'a>(
    document: &'a SpecDocument,
    path: &'a str,
    path_item: &'a Value,
) -> SpecResult<Vec<OperationSite<'a>>> {
    let Some(path_item) = path_item.as_object() else {
        return Err(SpecError::invalid(
            path_location(path),
            "the path item is not an object",
        ));
    };
    if path_item.contains_key("$ref") {
        return Err(SpecError::unsupported(
            path_location(path),
            "path-item `$ref` is not supported; inline the referenced path item",
        ));
    }
    let mut sites = Vec::new();
    for method in METHODS {
        if let Some(operation) = path_item.get(method) {
            sites.push(operation_site(
                document, path, method, path_item, operation,
            )?);
        }
    }
    for key in path_item.keys() {
        if METHODS.contains(&key.as_str())
            || PATH_ITEM_FIELDS.contains(&key.as_str())
            || key.starts_with("x-")
        {
            continue;
        }
        return Err(SpecError::unsupported(
            path_location(path),
            format!(
                "unsupported HTTP method `{key}`; supported methods: {}",
                METHODS.join(", ")
            ),
        ));
    }
    Ok(sites)
}

/// The site of one operation, validating that the entry is an object.
fn operation_site<'a>(
    document: &'a SpecDocument,
    path: &'a str,
    method: &'a str,
    path_item: &'a Map<String, Value>,
    operation: &'a Value,
) -> SpecResult<OperationSite<'a>> {
    let Some(operation) = operation.as_object() else {
        return Err(SpecError::invalid(
            format!("{} {path}", method.to_uppercase()),
            "the operation is not an object",
        ));
    };
    Ok(OperationSite {
        document,
        path,
        method,
        path_item,
        operation,
        context: format!("{} {path}", method.to_uppercase()),
    })
}

fn normalize_operation(
    site: &OperationSite<'_>,
    state: NormalizeState<'_>,
) -> SpecResult<NormalizedOperation> {
    let mut compactor = SchemaCompactor::new(&site.document.root, site.context.clone());
    let parts = collect_parts(site, state.schemes, &mut compactor)?;
    record_diagnostics(state.schema_diagnostics, &mut compactor);
    let defs = compactor.finish();
    let mut body = parts.body;
    resolve_body_input(&mut body, &defs)?;
    let mut normalized = NormalizedOperation {
        method: site.method.to_string(),
        path: site.path.to_string(),
        raw_tool_name: assign_tool_name(site, state.namer),
        description: operation_description(site),
        deprecated: is_deprecated(site.operation),
        tags: operation_tags(site),
        security: parts.security,
        path_params: parts.groups.path,
        query_params: parts.groups.query,
        header_params: parts.groups.headers,
        protected_params: parts.groups.protected,
        body,
        input_schema: Value::Null,
    };
    normalized.input_schema = input::assemble_input_schema(&normalized, &defs)?;
    input::validate_path_template(&normalized, &site.context)?;
    Ok(normalized)
}

/// Resolve an optional body's published input once all definitions exist.
fn resolve_body_input(body: &mut Option<BodySpec>, defs: &Map<String, Value>) -> SpecResult<()> {
    if let Some(body) = body.as_mut() {
        body.resolve_input(defs)?;
    }
    Ok(())
}

/// Whether the operation is marked deprecated.
fn is_deprecated(operation: &Map<String, Value>) -> bool {
    operation.get("deprecated") == Some(&Value::Bool(true))
}

/// The merged parameters, classified groups, request body and security
/// requirements of one operation.
struct OperationParts {
    groups: params::ParamGroups,
    body: Option<BodySpec>,
    security: Vec<String>,
}

fn collect_parts(
    site: &OperationSite<'_>,
    schemes: &SecuritySchemes,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<OperationParts> {
    let merged =
        params::merge_parameters(site.document, site.path_item, site.operation, &site.context)?;
    let groups = params::classify_parameters(
        site.document,
        merged.clone(),
        schemes,
        &site.context,
        compactor,
    )?;
    let body = body::build_body(
        site.document,
        &merged,
        site.operation,
        &site.context,
        compactor,
    )?;
    let security = operation_security(&site.document.root, &Value::Object(site.operation.clone()));
    schemes.validate_requirements(&security, &site.context)?;
    Ok(OperationParts {
        groups,
        body,
        security,
    })
}

/// Copy the compactor's contradictory-schema diagnostics into the document's
/// collection, deduplicated.
fn record_diagnostics(diagnostics: &mut Vec<String>, compactor: &mut SchemaCompactor<'_>) {
    for message in compactor.take_diagnostics() {
        if !diagnostics.contains(&message) {
            diagnostics.push(message);
        }
    }
}

fn assign_tool_name(site: &OperationSite<'_>, namer: &mut Namer) -> String {
    let base = site
        .operation
        .get("operationId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fallback_base(site.method, site.path));
    namer.assign(&base, site.method, site.path)
}

fn operation_description(site: &OperationSite<'_>) -> String {
    let summary = site
        .operation
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} {}", site.method.to_uppercase(), site.path));
    if site.operation.get("deprecated") == Some(&Value::Bool(true)) {
        format!("{summary} (deprecated)")
    } else {
        summary
    }
}

fn operation_tags(site: &OperationSite<'_>) -> Vec<String> {
    site.operation
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_groups_reflect_requiredness() {
        harnx_core::require_nextest();
        let operation = NormalizedOperation {
            method: "post".to_string(),
            path: "/pets/{petId}".to_string(),
            raw_tool_name: "updatePet".to_string(),
            description: String::new(),
            deprecated: false,
            tags: Vec::new(),
            security: Vec::new(),
            path_params: vec![ParamSpec {
                name: "petId".to_string(),
                required: true,
                style: ParamStyle::Simple,
                schema: json!({"type": "string"}),
            }],
            query_params: vec![ParamSpec {
                name: "verbose".to_string(),
                required: false,
                style: ParamStyle::Form { explode: true },
                schema: json!({"type": "boolean"}),
            }],
            header_params: Vec::new(),
            protected_params: Vec::new(),
            body: Some(BodySpec {
                required: true,
                branches: Vec::new(),
                input: crate::envelope::BodyInput::Direct(json!({"type": "object"})),
                form_fields: Vec::new(),
            }),
            input_schema: Value::Null,
        };
        let schema = input::assemble_input_schema(&operation, &Map::new()).unwrap();
        assert_eq!(schema["required"], json!(["path", "body"]));
        assert_eq!(schema["properties"]["path"]["required"], json!(["petId"]));
        assert!(schema["properties"]["query"].get("required").is_none());
        assert_eq!(schema["properties"]["body"], json!({"type": "object"}));
    }
}
