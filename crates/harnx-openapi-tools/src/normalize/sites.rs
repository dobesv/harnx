use serde_json::{Map, Value};

use crate::document::SpecDocument;
use crate::error::{SpecError, SpecResult};

use super::{METHODS, PATH_ITEM_FIELDS};

/// One operation under normalization, with its borrowed document context.
pub(super) struct OperationSite<'a> {
    pub(super) document: &'a SpecDocument,
    pub(super) path: &'a str,
    pub(super) method: &'a str,
    pub(super) path_item: &'a Map<String, Value>,
    pub(super) operation: &'a Map<String, Value>,
    pub(super) context: String,
}

/// Every operation site of a document, in document order, rejecting malformed
/// path items and unsupported path-item `$ref`s up front.
pub(super) fn operation_sites<'a>(
    document: &'a SpecDocument,
) -> SpecResult<Vec<OperationSite<'a>>> {
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
