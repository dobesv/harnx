//! Bounded current-metadata reads for `session_load` (plan #2970, T3).
//!
//! Two contracts live here:
//! * a safe projection of the child's canonical metadata — the same
//!   `session_meta_view` fields, never raw execution extensions, overrides,
//!   worker fences or worker-resolved variable defaults — fitted to
//!   [`metadata_allowance`] with whole-value-or-omit priority (title and the
//!   complete recorded web URL first) and opt-in stored-variable previews;
//! * the same-window refit ladder: mandatory routing/status/pending-interaction
//!   identity is reserved first, then optional metadata and optional progress
//!   are reduced or dropped until the page both fits `max_output_bytes` and
//!   makes progress (byte or indexed). Only a mandatory-only page that cannot
//!   deliver usable content turns into a recoverable ask for a bigger budget.

#[path = "variable_preview.rs"]
mod variable_preview;

use crate::nats_session_metadata::{
    repository_contexts, session_properties, SessionMetadata, SessionProperties,
    WEB_SESSION_URL_PROPERTY,
};
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Map, Value};
use variable_preview::insert_variables;

/// Cap of the whole `metadata` field, JSON wrapper and omission indicators
/// included.
const METADATA_MAX_BYTES: usize = 4096;
/// The metadata field never claims more than this fraction of the total read
/// budget; the page keeps the rest.
const METADATA_BUDGET_FRACTION: usize = 4;
/// Serialized wrapper punctuation around the metadata value, including both
/// the key/colon and the enclosing object braces.
const METADATA_WRAPPER_BYTES: usize = "{\"metadata\":}".len();

/// The `metadata` allowance for one load: never more than 4096 bytes, and
/// never more than a quarter of the total read budget.
pub(in crate::nats_worker::subagent_toolset) fn metadata_allowance(
    max_output_bytes: usize,
) -> usize {
    METADATA_MAX_BYTES.min(max_output_bytes / METADATA_BUDGET_FRACTION)
}

/// What `session_load` discloses as `metadata`, before the refit ladder
/// decides what finally fits.
#[derive(Debug)]
pub(in crate::nats_worker::subagent_toolset) enum LoadMetadata {
    /// No canonical record: the field is JSON null in every attempt.
    Absent,
    /// Bounded projection, or its `{"truncated": true}` fallback marker.
    Bounded(Value),
    /// A record exists but even the marker cannot fit: the field is omitted.
    Omitted,
}

/// Resolve the load's `metadata` disclosure. `record` is the authoritative
/// canonical record; `None` is an absent record, distinct from a broker
/// failure, which the caller turns into an error before reaching this.
pub(in crate::nats_worker::subagent_toolset) fn load_metadata(
    record: Option<&SessionMetadata>,
    include_variables: bool,
    allowance: usize,
) -> Result<LoadMetadata, ToolInvokeError> {
    let Some(metadata) = record else {
        return Ok(LoadMetadata::Absent);
    };
    Ok(
        match bounded_view(metadata, include_variables, allowance)? {
            Some(view) => LoadMetadata::Bounded(view),
            None => LoadMetadata::Omitted,
        },
    )
}

fn bounded_view(
    metadata: &SessionMetadata,
    include_variables: bool,
    allowance: usize,
) -> Result<Option<Value>, ToolInvokeError> {
    let budget = view_budget(allowance);
    let properties = child_properties(metadata)?;
    // Identity fields get first claim on the allowance; when even they do not
    // fit, the marker or omission takes over.
    let mut view = Map::new();
    view.insert("session_id".into(), json!(metadata.session_id));
    view.insert("agent".into(), json!(metadata.agent.name()));
    view.insert("created_at".into(), json!(metadata.created_at));
    view.insert("truncated".into(), json!(false));
    if !fits(&view, budget) {
        return Ok(truncation_marker(budget));
    }
    if insert_projection_fields(&mut view, metadata, &properties, include_variables, budget) {
        view.insert("truncated".into(), json!(true));
    }
    Ok(Some(Value::Object(view)))
}

/// The view's fitting budget: the allowance minus the complete standalone
/// wrapper, so the value fits both as the first field and after a comma.
fn view_budget(allowance: usize) -> usize {
    allowance.saturating_sub(METADATA_WRAPPER_BYTES)
}

/// Same failure surface as the child's own metadata view: a properties
/// extension that no longer decodes is a data error, not a missing record.
fn child_properties(metadata: &SessionMetadata) -> Result<SessionProperties, ToolInvokeError> {
    session_properties(metadata).map_err(|error| {
        ToolInvokeError::Recoverable(format!("child session properties unavailable: {error:#}"))
    })
}

/// The smallest legal projection: a truncation marker, or `None` when even
/// the marker cannot fit (the caller omits the field entirely).
fn truncation_marker(budget: usize) -> Option<Value> {
    let mut marker = Map::new();
    marker.insert("truncated".into(), json!(true));
    fits(&marker, budget).then(|| Value::Object(marker))
}

/// Insert every non-identity projection field that fits, in priority order.
/// Returns whether any requested content had to be clipped or omitted.
fn insert_projection_fields(
    view: &mut Map<String, Value>,
    metadata: &SessionMetadata,
    properties: &SessionProperties,
    include_variables: bool,
    budget: usize,
) -> bool {
    // Every pass must run even after one truncates, so the later passes can
    // still fill whatever room is left.
    [
        insert_title(view, metadata, budget),
        insert_properties(view, properties, budget),
        insert_repositories(view, metadata, budget),
        insert_variables(view, metadata, include_variables, budget),
    ]
    .contains(&true)
}

fn fits(view: &Map<String, Value>, budget: usize) -> bool {
    serde_json::to_vec(view)
        .map(|bytes| bytes.len() <= budget)
        .unwrap_or(false)
}

fn insert_title(view: &mut Map<String, Value>, metadata: &SessionMetadata, budget: usize) -> bool {
    let Some(title) = metadata.title.value.clone() else {
        return false;
    };
    // Title has priority over every collection: only identity outranks it,
    // and identity already fit when this runs.
    view.insert("title".into(), json!(title));
    if fits(view, budget) {
        return false;
    }
    view.remove("title");
    true
}

/// Whole-value-or-omit property entries: the recorded web URL first and
/// always complete — a clipped URL is a misleading link — then the rest in
/// sorted order.
fn insert_properties(
    view: &mut Map<String, Value>,
    properties: &SessionProperties,
    budget: usize,
) -> bool {
    let mut entries: Vec<(String, Value)> = properties
        .iter()
        .map(|(name, property)| (name.clone(), json!(property)))
        .collect();
    if let Some(position) = entries
        .iter()
        .position(|(name, _)| name == WEB_SESSION_URL_PROPERTY)
    {
        let url = entries.remove(position);
        entries.insert(0, url);
    }
    let mut omitted = false;
    let mut kept = Map::new();
    view.insert("properties".into(), Value::Object(Map::new()));
    for (name, property) in entries {
        kept.insert(name.clone(), property);
        view.insert("properties".into(), Value::Object(kept.clone()));
        if fits(view, budget) {
            continue;
        }
        kept.remove(&name);
        view.insert("properties".into(), Value::Object(kept.clone()));
        omitted = true;
        break;
    }
    if kept.is_empty() {
        view.remove("properties");
    }
    omitted
}

/// Observed repositories in sorted order, whole entries or none.
// qual:allow(iosp) — whole-entry-or-omit budget fitting: every candidate must
// be re-serialized inside the view to measure the exact compact size, so the
// loop's insert/fit calls are the fitting algorithm itself, not dispatch.
fn insert_repositories(
    view: &mut Map<String, Value>,
    metadata: &SessionMetadata,
    budget: usize,
) -> bool {
    let mut contexts = repository_contexts(metadata);
    contexts.sort_by(|a, b| (&a.repository, &a.branch).cmp(&(&b.repository, &b.branch)));
    let mut omitted = false;
    let mut kept: Vec<Value> = Vec::new();
    view.insert("observed_repositories".into(), json!([]));
    for context in contexts {
        kept.push(json!(context));
        view.insert("observed_repositories".into(), json!(kept));
        if fits(view, budget) {
            continue;
        }
        kept.pop();
        view.insert("observed_repositories".into(), json!(kept));
        omitted = true;
        break;
    }
    if kept.is_empty() {
        view.remove("observed_repositories");
    }
    omitted
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod read_tests;
