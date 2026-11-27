//! Opt-in stored-variable previews for the load projection (plan #2970, T3):
//! key-sorted, whole names only, head/tail clips measured in serde-escaped
//! JSON content bytes, exact UTF-8 byte counts and the summary of omissions.

use super::fits;
use crate::nats_session_metadata::SessionMetadata;
use crate::nats_worker::subagent_toolset::session_read;
use serde_json::{json, Map, Value};

/// Variable entries a projection returns at most.
const MAX_VARIABLE_ENTRIES: usize = 8;
/// Combined head/tail escaped JSON content bytes per variable preview.
const VARIABLE_PREVIEW_MAX_BYTES: usize = 256;
/// Half of the preview cap, spent on each end of a clipped value.
const VARIABLE_PREVIEW_HALF_BYTES: usize = VARIABLE_PREVIEW_MAX_BYTES / 2;

/// Opt-in stored-variable previews: at most [`MAX_VARIABLE_ENTRIES`] sorted
/// entries, whole names preserved, values clipped to a head/tail preview
/// whose combined escaped JSON content is at most
/// [`VARIABLE_PREVIEW_MAX_BYTES`] bytes, plus the required summary.
pub(super) fn insert_variables(
    view: &mut Map<String, Value>,
    metadata: &SessionMetadata,
    include_variables: bool,
    budget: usize,
) -> bool {
    // Opt-in only: without the flag no variable name, value or preview is
    // ever inserted, and the caller sees no `variables` key at all.
    if !include_variables {
        return false;
    }
    let mut pairs: Vec<(&String, &String)> = metadata.variables.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let total = pairs.len();
    let mut kept = Map::new();
    let mut returned = 0;
    let mut truncated = false;
    view.insert("variables".into(), Value::Object(Map::new()));
    set_summary(view, total, returned);
    if !fits(view, budget) {
        // Without room for a truthful summary, disclose no entry counts. The
        // caller still marks the projection truncated when values exist.
        view.remove("variables");
        view.remove("variables_summary");
        return total > 0;
    }

    for (name, value) in pairs.iter().take(MAX_VARIABLE_ENTRIES) {
        let preview = variable_preview(value);
        kept.insert((*name).clone(), preview.value);
        view.insert("variables".into(), Value::Object(kept.clone()));
        set_summary(view, total, returned + 1);
        if fits(view, budget) {
            truncated |= preview.clipped;
            returned += 1;
            continue;
        }
        kept.remove(*name);
        view.insert("variables".into(), Value::Object(kept.clone()));
        set_summary(view, total, returned);
        truncated = true;
        break;
    }

    truncated |= total > returned;
    // Every retained state was measured with its matching summary, so this is
    // a guard against future changes that add fields between fit checks.
    if !fits(view, budget) {
        view.remove("variables");
        view.remove("variables_summary");
        return total > 0 || truncated;
    }
    truncated
}

fn set_summary(view: &mut Map<String, Value>, total: usize, returned: usize) {
    view.insert(
        "variables_summary".into(),
        json!({
            "total": total,
            "returned": returned,
            "omitted": total - returned,
        }),
    );
}

struct VariablePreview {
    value: Value,
    clipped: bool,
}

fn variable_preview(value: &str) -> VariablePreview {
    let total = value.len();
    let escaped: usize = value.chars().map(session_read::escaped_char_len).sum();
    if escaped <= VARIABLE_PREVIEW_MAX_BYTES {
        return VariablePreview {
            value: json!({
                "head": value,
                "tail": "",
                "total_bytes": total,
                "omitted_bytes": 0,
            }),
            clipped: false,
        };
    }
    // Fit the tail first: when the two windows would overlap (cheap tail
    // characters, expensive head characters), the head shrinks back to the
    // tail's start so the combined escaped content stays within the cap.
    let tail_start = fit_text_suffix(value, VARIABLE_PREVIEW_HALF_BYTES);
    let (head_end, _) = session_read::fit_text_prefix(value, VARIABLE_PREVIEW_HALF_BYTES);
    let head_end = head_end.min(tail_start);
    VariablePreview {
        value: json!({
            "head": &value[..head_end],
            "tail": &value[tail_start..],
            "total_bytes": total,
            "omitted_bytes": tail_start - head_end,
        }),
        clipped: true,
    }
}

/// Longest suffix of `text` whose serde_json string-content length fits in
/// `max_json_len`, as the byte offset where it starts.
fn fit_text_suffix(text: &str, max_json_len: usize) -> usize {
    let mut used = 0usize;
    for (pos, c) in text.char_indices().rev() {
        let cost = session_read::escaped_char_len(c);
        if used + cost > max_json_len {
            return pos + c.len_utf8();
        }
        used += cost;
    }
    0
}
