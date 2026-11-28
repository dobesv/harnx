//! Operation normalization: a parsed document to a table of
//! [`NormalizedOperation`]s ready for cataloging.
//!
//! Parameter merge, classification and styles live in [`params`]; request
//! bodies in [`body`]; input-schema assembly in [`input`].

mod body;
mod input;
mod params;
mod sites;

use serde_json::{Map, Value};

use crate::compact::{
    materialization::{MaterializationBudget, MaterializationCost},
    SchemaCompactor,
};
use crate::document::SpecDocument;
use crate::error::SpecResult;
use crate::naming::{fallback_base, Namer};
use crate::security::{operation_security, SecuritySchemes};

use sites::{operation_sites, OperationSite};

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

/// The mutable state threaded through every operation: name assignment and
/// the contradictory-schema diagnostics the server surfaces at startup.
struct NormalizeState<'a> {
    namer: &'a mut Namer,
    schemes: &'a SecuritySchemes,
    schema_diagnostics: &'a mut Vec<String>,
    materialization_budget: &'a mut MaterializationBudget,
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
    normalize_document_with_budget(
        document,
        namer,
        schemes,
        schema_diagnostics,
        MaterializationBudget::new(
            crate::compact::materialization::DEFAULT_MATERIALIZATION_VALUE_BUDGET,
            crate::compact::materialization::DEFAULT_MATERIALIZATION_BYTE_BUDGET,
        ),
    )
    .map(|(operations, _)| operations)
}

fn normalize_document_with_budget(
    document: &SpecDocument,
    namer: &mut Namer,
    schemes: &SecuritySchemes,
    schema_diagnostics: &mut Vec<String>,
    mut materialization_budget: MaterializationBudget,
) -> SpecResult<(Vec<NormalizedOperation>, MaterializationCost)> {
    let mut operations = Vec::new();
    for site in operation_sites(document)? {
        let state = NormalizeState {
            namer: &mut *namer,
            schemes,
            schema_diagnostics: &mut *schema_diagnostics,
            materialization_budget: &mut materialization_budget,
        };
        operations.push(normalize_operation(&site, state)?);
    }
    Ok((operations, materialization_budget.used()))
}

fn normalize_operation(
    site: &OperationSite<'_>,
    state: NormalizeState<'_>,
) -> SpecResult<NormalizedOperation> {
    let (value_budget, byte_budget) = state.materialization_budget.remaining();
    let mut compactor = SchemaCompactor::with_budget(
        &site.document.root,
        site.context.clone(),
        value_budget,
        byte_budget,
    );
    let parts = collect_parts(site, state.schemes, &mut compactor)?;
    record_diagnostics(state.schema_diagnostics, &mut compactor);
    let materialized = compactor.materialization_used();
    let defs = compactor.finish()?;
    state.materialization_budget.charge(
        materialized,
        &site.context,
        "document materialization budget",
    )?;
    let mut body = parts.body;
    resolve_body_input(&mut body, &defs, &site.context)?;
    let mut normalized = NormalizedOperation {
        method: site.method.to_string(),
        path: site.path.to_string(),
        raw_tool_name: assign_tool_name(site, state.namer)?,
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
fn resolve_body_input(
    body: &mut Option<BodySpec>,
    defs: &Map<String, Value>,
    context: &str,
) -> SpecResult<()> {
    if let Some(body) = body.as_mut() {
        body.resolve_input(defs, context)?;
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

fn assign_tool_name(site: &OperationSite<'_>, namer: &mut Namer) -> SpecResult<String> {
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
                style: ParamStyle::Simple { explode: false },
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

#[cfg(test)]
#[path = "materialization_tests.rs"]
mod materialization_tests;
