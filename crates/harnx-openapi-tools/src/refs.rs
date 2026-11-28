//! JSON pointer resolution against a parsed specification document.
//!
//! Only local pointers (`#/components/schemas/Foo`, `#/definitions/Foo`) are
//! resolvable here; anything else is an external reference, which the catalog
//! rejects with context rather than silently dropping the operation.

use serde_json::Value;

use crate::error::{SpecError, SpecResult};

/// Resolve a local JSON pointer (`#/a/b`) inside `root`.
///
/// Returns `None` when the pointer is not local, is malformed, or names a
/// location that does not exist. Pointer segments are unescaped (`~1` → `/`,
/// `~0` → `~`) per RFC 6901; array indexing is not needed for the object trees
/// a Swagger/OpenAPI document uses.
pub fn resolve_pointer<'a>(root: &'a Value, pointer: &str, context: &str) -> SpecResult<&'a Value> {
    let fragment = match pointer.strip_prefix('#') {
        Some("") | None => {
            return Err(SpecError::unsupported(
                context,
                format!("self or non-local reference {pointer:?} is not supported; inline the schema or use a #/components/schemas (Swagger 2.0: #/definitions) reference"),
            ));
        }
        Some(fragment) => fragment.strip_prefix('/').ok_or_else(|| {
            SpecError::unsupported(context, format!("malformed local reference {pointer:?}"))
        })?,
    };
    let mut current = root;
    for raw_segment in fragment.split('/') {
        let segment = raw_segment.replace("~1", "/").replace("~0", "~");
        let next = current
            .get(&segment)
            .ok_or_else(|| SpecError::UnresolvedRef {
                context: context.to_string(),
                reference: pointer.to_string(),
                message: format!("segment {segment:?} not found"),
            })?;
        current = next;
    }
    Ok(current)
}
