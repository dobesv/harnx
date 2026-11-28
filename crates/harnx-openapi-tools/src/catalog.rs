//! The normalized catalog: one operation table plus its compact `ToolSpec`s.
//!
//! This is the stable T2 interface consumed by T3 (dispatch reads the
//! [`NormalizedOperation`] table; discovery publishes only the compact
//! [`ToolSpec`] list) and T4 (the server registers the serialized specs).
//! Dispatch data — path templates, parameter styles, media branches — stays
//! here, never in discovery payloads.

use harnx_toolset::ToolSpec;
use serde_json::Value;

use crate::document::{SpecDocument, SpecKind};
use crate::error::SpecResult;
use crate::naming::Namer;
use crate::normalize::{normalize_document, NormalizedOperation};
use crate::security::SecuritySchemes;

/// A whole normalized specification, ready for discovery and dispatch.
#[derive(Clone, Debug)]
pub struct Catalog {
    /// The standalone server name the raw names were budgeted for.
    pub server_name: String,
    pub spec_kind: SpecKind,
    pub spec_version: String,
    /// `info.title`, informational.
    pub title: String,
    /// `info.version`, informational.
    pub info_version: String,
    /// Normalized server URLs (Swagger 2.0 `host`/`basePath`/`schemes`,
    /// OpenAPI 3.0 `servers[].url`) — documentation only. The configured
    /// `--base-url` controls execution and is never taken from here.
    pub server_metadata: Vec<String>,
    /// Startup diagnostics for credential placements this release cannot
    /// supply. No values, only scheme/parameter names.
    pub credential_diagnostics: Vec<String>,
    /// Startup diagnostics for upstream schemas that contradict themselves
    /// (for example a `required` name with no property and
    /// `additionalProperties: false`). The schemas are preserved verbatim —
    /// no compatibility weakening — so an operation with one stays
    /// constructible only up to its source; see the plan's blocked-acceptance
    /// record.
    pub schema_diagnostics: Vec<String>,
    /// The document's declared scheme names, informational.
    pub security_scheme_names: Vec<String>,
    pub operations: Vec<NormalizedOperation>,
}

impl Catalog {
    /// Normalize a document into a catalog for the given server name.
    ///
    /// Fails on the first unsupported or invalid construct, with method/path
    /// context — nothing registers partially.
    pub fn build(document: &SpecDocument, server_name: &str) -> SpecResult<Self> {
        let mut namer = Namer::new(server_name)?;
        let schemes = SecuritySchemes::collect(document)?;
        let mut schema_diagnostics = Vec::new();
        let operations =
            normalize_document(document, &mut namer, &schemes, &mut schema_diagnostics)?;
        Ok(Self {
            server_name: server_name.to_string(),
            spec_kind: document.kind,
            spec_version: document.version.clone(),
            title: info_field(&document.root, "title"),
            info_version: info_field(&document.root, "version"),
            server_metadata: server_metadata(document),
            credential_diagnostics: dedup(&schemes.unsupported_scheme_types),
            schema_diagnostics,
            security_scheme_names: schemes.scheme_names,
            // Document order (the workspace preserves JSON key order), which
            // keeps names and inventory deterministic.
            operations,
        })
    }

    // qual:api — the future tool server CLI (T4) applies the operator's
    // repeatable `--spec-patch <JAQ>` values through this seam; the patched
    // (effective) source then drives discovery and dispatch together.
    /// Normalize a document after applying the ordered operator patch list.
    ///
    /// An empty patch list behaves exactly like [`Catalog::build`]. Any
    /// patch failure — compile, runtime, cardinality, non-object output, or
    /// limit — fails atomically: nothing registers from a half-patched
    /// source.
    pub fn build_patched(
        document: &SpecDocument,
        server_name: &str,
        patches: &[String],
        limits: crate::spec_patch::SpecPatchLimits,
    ) -> SpecResult<Self> {
        let patched = crate::spec_patch::apply_spec_patches(document, patches, limits)?;
        Self::build(&patched, server_name)
    }

    /// The compact discovery list. One `ToolSpec` per operation.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.operations
            .iter()
            .map(|operation| ToolSpec {
                name: operation.raw_tool_name.clone(),
                description: operation.description.clone(),
                input_schema: operation.input_schema.clone(),
                cancellation_guarantee: Default::default(),
                idempotent_hint: operation.is_idempotent(),
                read_only_hint: operation.is_read_only(),
                timeout_secs: None,
                meta: None,
            })
            .collect()
    }

    // qual:api — the serialized size is the payload a NATS KV registration
    // (T4) must carry; tests pin it against the broker limit.
    /// Serialized size of the discovery list — the payload a stock NATS KV
    /// registration must carry for all operations. Byte budget is a transport
    /// concern of the server (T4), but the measurement belongs here so tests
    /// can pin it.
    pub fn serialized_tool_catalog_bytes(&self) -> usize {
        serde_json::to_vec(&self.tool_specs())
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    // qual:api — dispatch (T3) resolves operations by raw tool name.
    /// Look up an operation by its raw tool name.
    pub fn find_operation(&self, raw_name: &str) -> Option<&NormalizedOperation> {
        self.operations
            .iter()
            .find(|operation| operation.raw_tool_name == raw_name)
    }

    // qual:api — dispatch (T3) resolves operations by method and path.
    /// Look up an operation by method and path template.
    pub fn find_by_method_path(&self, method: &str, path: &str) -> Option<&NormalizedOperation> {
        self.operations
            .iter()
            .find(|operation| operation.method == method && operation.path == path)
    }

    /// The `(method, path)` inventory, in document order.
    pub fn inventory(&self) -> Vec<(String, String)> {
        self.operations
            .iter()
            .map(|operation| (operation.method.clone(), operation.path.clone()))
            .collect()
    }
}

impl NormalizedOperation {
    /// Conservative idempotency hint: read-only methods and PUT/DELETE are
    /// wire-idempotent for API replay purposes.
    fn is_idempotent(&self) -> bool {
        matches!(
            self.method.as_str(),
            "get" | "head" | "options" | "put" | "delete"
        )
    }
}

/// `info.<field>` as a string, empty when absent.
fn info_field(root: &Value, field: &str) -> String {
    root.get("info")
        .and_then(|info| info.get(field))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The input messages in order, without duplicates.
fn dedup(messages: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for message in messages {
        if !seen.contains(message) {
            seen.push(message.clone());
        }
    }
    seen
}

/// Server URLs declared by the document, for documentation only.
fn server_metadata(document: &SpecDocument) -> Vec<String> {
    let mut urls = Vec::new();
    match document.kind {
        SpecKind::Swagger2 => {
            let host = document
                .root
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or("localhost");
            let base_path = document
                .root
                .get("basePath")
                .and_then(Value::as_str)
                .unwrap_or("/");
            let schemes: Vec<String> = document
                .root
                .get("schemes")
                .and_then(Value::as_array)
                .map(|schemes| {
                    schemes
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_else(|| vec!["https".to_string()]);
            for scheme in schemes {
                urls.push(format!("{scheme}://{host}{base_path}"));
            }
        }
        SpecKind::OpenApi3 => {
            if let Some(servers) = document.root.get("servers").and_then(Value::as_array) {
                for server in servers {
                    if let Some(url) = server.get("url").and_then(Value::as_str) {
                        urls.push(url.to_string());
                    }
                }
            }
        }
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(value: Value) -> SpecDocument {
        SpecDocument::from_value(value).unwrap()
    }

    #[test]
    fn swagger2_host_and_basepath_become_documentation_metadata() {
        harnx_core::require_nextest();
        let document = document(json!({
            "swagger": "2.0",
            "info": {"title": "Pets", "version": "1.0"},
            "host": "petstore.example.com",
            "basePath": "/v2",
            "schemes": ["https"],
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "summary": "List pets",
                        "parameters": [
                            {"name": "limit", "in": "query", "type": "integer"}
                        ]
                    }
                }
            }
        }));
        let catalog = Catalog::build(&document, "petstore").unwrap();
        assert_eq!(
            catalog.server_metadata,
            vec!["https://petstore.example.com/v2".to_string()]
        );
        assert_eq!(catalog.title, "Pets");
        assert_eq!(catalog.operations.len(), 1);
        let operation = &catalog.operations[0];
        assert_eq!(operation.raw_tool_name, "listPets");
        assert_eq!(operation.method, "get");
        assert_eq!(operation.query_params[0].name, "limit");
        assert_eq!(catalog.find_operation("listPets").unwrap().path, "/pets");
    }

    #[test]
    fn catalog_exposes_inventory_and_specs() {
        harnx_core::require_nextest();
        let document = document(json!({
            "swagger": "2.0",
            "paths": {
                "/a": {"get": {"operationId": "getA"}},
                "/b": {"post": {"operationId": "createB"}}
            }
        }));
        let catalog = Catalog::build(&document, "srv").unwrap();
        assert_eq!(
            catalog.inventory(),
            vec![
                ("get".to_string(), "/a".to_string()),
                ("post".to_string(), "/b".to_string())
            ]
        );
        let specs = catalog.tool_specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "getA");
        assert!(specs[0].idempotent_hint);
        assert!(specs[0].read_only_hint);
        assert!(!specs[1].idempotent_hint);
        assert!(catalog.serialized_tool_catalog_bytes() > 0);
    }

    #[test]
    fn media_branches_survive_into_dispatch_data_only() {
        harnx_core::require_nextest();
        let document = document(json!({
            "openapi": "3.0.1",
            "paths": {
                "/upload": {
                    "post": {
                        "operationId": "upload",
                        "requestBody": {
                            "required": true,
                            "content": {
                                "multipart/form-data": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {"file": {"type": "string", "format": "binary"}}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }));
        let catalog = Catalog::build(&document, "srv").unwrap();
        let operation = catalog.find_operation("upload").unwrap();
        let branches: Vec<String> = operation
            .body
            .as_ref()
            .unwrap()
            .branches
            .iter()
            .map(|branch| branch.media_type.to_string())
            .collect();
        assert_eq!(branches, vec!["multipart/form-data"]);
        // The published input is the envelope; branches stay out of it.
        assert!(operation.input_schema["properties"]["body"]["properties"]["parts"].is_object());
    }
}
