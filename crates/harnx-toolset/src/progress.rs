//! Tool progress contract and versioned wire payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Request capability enabling live tool progress messages and final snapshots.
pub const CAPABILITY_TOOL_PROGRESS: &str = "harnx:tool_progress";

/// Maximum bytes retained for a concise string field.
pub const TOOL_PROGRESS_MAX_STRING_BYTES: usize = 4 * 1024;
/// Maximum bytes retained for rendered markdown.
pub const TOOL_PROGRESS_MAX_MARKDOWN_BYTES: usize = 64 * 1024;
/// Maximum locations retained in one update.
pub const TOOL_PROGRESS_MAX_LOCATIONS: usize = 64;
/// Maximum structured content blocks retained in one update.
pub const TOOL_PROGRESS_MAX_CONTENT_BLOCKS: usize = 64;
/// Maximum serialized bytes retained across structured content blocks.
pub const TOOL_PROGRESS_MAX_CONTENT_BYTES: usize = 64 * 1024;
/// Maximum raw bytes retained in one image content block.
pub const TOOL_PROGRESS_MAX_IMAGE_BYTES: usize = 16 * 1024;

/// Versioned payload carried in [`crate::ProgressMessage::chunk`] and final replies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", content = "patch")]
pub enum ProgressChunk {
    #[serde(rename = "1")]
    V1(ToolProgressPatch),
}

/// Incremental display-state patch emitted by a toolset.
///
/// Omitted fields leave prior state unchanged. Collections replace prior values,
/// so an explicit empty collection clears that field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProgressPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolProgressStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolProgressKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolProgressLocation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ToolProgressUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolProgressContent>>,
}

impl ToolProgressPatch {
    /// Apply producer-side display bounds before buffering or publication.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.title = self
            .title
            .map(|value| truncate_utf8(value, TOOL_PROGRESS_MAX_STRING_BYTES));
        self.markdown = self
            .markdown
            .map(|value| truncate_utf8(value, TOOL_PROGRESS_MAX_MARKDOWN_BYTES));
        self.status = self.status.filter(ToolProgressStatus::is_non_terminal);
        self.locations = self.locations.map(bound_locations);
        self.content = self.content.map(bound_content);
        self
    }

    /// Merge another patch into this snapshot, replacing every supplied field.
    pub fn merge(&mut self, patch: Self) {
        replace_if_some(&mut self.title, patch.title);
        replace_if_some(&mut self.status, patch.status);
        replace_if_some(&mut self.kind, patch.kind);
        replace_if_some(&mut self.locations, patch.locations);
        replace_if_some(&mut self.usage, patch.usage);
        replace_if_some(&mut self.markdown, patch.markdown);
        replace_if_some(&mut self.content, patch.content);
    }

    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.kind.is_none()
            && self.locations.is_none()
            && self.usage.is_none()
            && self.markdown.is_none()
            && self.content.is_none()
    }
}

fn replace_if_some<T>(target: &mut Option<T>, value: Option<T>) {
    if value.is_some() {
        *target = value;
    }
}

fn bound_locations(mut locations: Vec<ToolProgressLocation>) -> Vec<ToolProgressLocation> {
    locations.truncate(TOOL_PROGRESS_MAX_LOCATIONS);
    for location in &mut locations {
        let path = location.path.to_string_lossy().into_owned();
        location.path = truncate_utf8(path, TOOL_PROGRESS_MAX_STRING_BYTES).into();
    }
    locations
}

fn bound_content(mut content: Vec<ToolProgressContent>) -> Vec<ToolProgressContent> {
    content.truncate(TOOL_PROGRESS_MAX_CONTENT_BLOCKS);
    let mut bounded = Vec::with_capacity(content.len());
    let mut serialized_bytes = 0usize;
    for mut block in content {
        block.apply_bounds();
        let block_bytes = serde_json::to_vec(&block).map_or(0, |value| value.len());
        if serialized_bytes.saturating_add(block_bytes) > TOOL_PROGRESS_MAX_CONTENT_BYTES {
            break;
        }
        serialized_bytes += block_bytes;
        bounded.push(block);
    }
    bounded
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgressStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolProgressStatus {
    fn is_non_terminal(&self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgressKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProgressLocation {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProgressUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolProgressContent {
    Text {
        text: String,
    },
    Image {
        data: Vec<u8>,
        mime: String,
    },
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Opaque {
        kind: String,
        value: Value,
    },
}

impl ToolProgressContent {
    fn apply_bounds(&mut self) {
        match self {
            Self::Text { text } => {
                *text = truncate_utf8(std::mem::take(text), TOOL_PROGRESS_MAX_STRING_BYTES);
            }
            Self::Image { data, mime } => {
                data.truncate(TOOL_PROGRESS_MAX_IMAGE_BYTES);
                *mime = truncate_utf8(std::mem::take(mime), TOOL_PROGRESS_MAX_STRING_BYTES);
            }
            Self::ResourceLink { uri, name } => {
                *uri = truncate_utf8(std::mem::take(uri), TOOL_PROGRESS_MAX_STRING_BYTES);
                *name = name
                    .take()
                    .map(|value| truncate_utf8(value, TOOL_PROGRESS_MAX_STRING_BYTES));
            }
            Self::Opaque { kind, value } => {
                *kind = truncate_utf8(std::mem::take(kind), TOOL_PROGRESS_MAX_STRING_BYTES);
                bound_json_strings(value);
            }
        }
    }
}

fn bound_json_strings(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = truncate_utf8(std::mem::take(text), TOOL_PROGRESS_MAX_STRING_BYTES);
        }
        Value::Array(values) => {
            values.truncate(TOOL_PROGRESS_MAX_CONTENT_BLOCKS);
            values.iter_mut().for_each(bound_json_strings);
        }
        Value::Object(values) => {
            let entries = std::mem::take(values);
            for (key, mut value) in entries.into_iter().take(TOOL_PROGRESS_MAX_CONTENT_BLOCKS) {
                bound_json_strings(&mut value);
                values.insert(truncate_utf8(key, TOOL_PROGRESS_MAX_STRING_BYTES), value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Object-safe sink supplied to `Toolset::invoke_with_context` implementations.
pub trait ToolProgress: Send + Sync {
    fn update(&self, patch: ToolProgressPatch);
}

/// Cloneable call-bound progress handle. Its default sink discards updates.
#[derive(Clone)]
pub struct ToolProgressHandle(Arc<dyn ToolProgress>);

impl ToolProgressHandle {
    pub fn new(sink: Arc<dyn ToolProgress>) -> Self {
        Self(sink)
    }

    pub fn update(&self, patch: ToolProgressPatch) {
        self.0.update(patch);
    }
}

impl Default for ToolProgressHandle {
    fn default() -> Self {
        Self(Arc::new(NoopToolProgress))
    }
}

impl fmt::Debug for ToolProgressHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ToolProgressHandle(<sink>)")
    }
}

#[derive(Debug)]
struct NoopToolProgress;

impl ToolProgress for NoopToolProgress {
    fn update(&self, _patch: ToolProgressPatch) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_patch() -> ToolProgressPatch {
        ToolProgressPatch {
            title: Some("Indexing files".into()),
            status: Some(ToolProgressStatus::InProgress),
            kind: Some(ToolProgressKind::Search),
            locations: Some(vec![ToolProgressLocation {
                path: "src/lib.rs".into(),
                line: Some(42),
            }]),
            usage: Some(ToolProgressUsage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 2,
                cache_write_tokens: 1,
            }),
            markdown: Some("**Scanning**".into()),
            content: Some(vec![ToolProgressContent::Text {
                text: "src/lib.rs".into(),
            }]),
        }
    }

    #[test]
    fn versioned_chunk_round_trips() {
        let chunk = ProgressChunk::V1(full_patch());
        let wire = serde_json::to_value(&chunk).unwrap();
        assert_eq!(wire["version"], "1");
        assert_eq!(wire["patch"]["kind"], "search");
        assert_eq!(
            serde_json::from_value::<ProgressChunk>(wire).unwrap(),
            chunk
        );
    }

    #[test]
    fn bounds_strings_on_utf8_boundaries() {
        let patch = ToolProgressPatch {
            title: Some("é".repeat(TOOL_PROGRESS_MAX_STRING_BYTES)),
            markdown: Some("文".repeat(TOOL_PROGRESS_MAX_MARKDOWN_BYTES)),
            content: Some(vec![ToolProgressContent::Opaque {
                kind: "k".repeat(TOOL_PROGRESS_MAX_STRING_BYTES + 1),
                value: json!({"message": "é".repeat(TOOL_PROGRESS_MAX_STRING_BYTES)}),
            }]),
            ..Default::default()
        }
        .bounded();

        assert!(patch.title.unwrap().len() <= TOOL_PROGRESS_MAX_STRING_BYTES);
        assert!(patch.markdown.unwrap().len() <= TOOL_PROGRESS_MAX_MARKDOWN_BYTES);
        let encoded = serde_json::to_string(&patch.content.unwrap()).unwrap();
        assert!(encoded.contains('é'));
    }

    #[test]
    fn bounds_location_and_content_counts() {
        let patch = ToolProgressPatch {
            locations: Some(
                (0..TOOL_PROGRESS_MAX_LOCATIONS + 10)
                    .map(|index| ToolProgressLocation {
                        path: format!("{index}.rs").into(),
                        line: None,
                    })
                    .collect(),
            ),
            content: Some(
                (0..TOOL_PROGRESS_MAX_CONTENT_BLOCKS + 10)
                    .map(|index| ToolProgressContent::Text {
                        text: index.to_string(),
                    })
                    .collect(),
            ),
            ..Default::default()
        }
        .bounded();

        assert_eq!(patch.locations.unwrap().len(), TOOL_PROGRESS_MAX_LOCATIONS);
        assert_eq!(
            patch.content.unwrap().len(),
            TOOL_PROGRESS_MAX_CONTENT_BLOCKS
        );
    }

    #[test]
    fn bounds_total_content_bytes() {
        let patch = ToolProgressPatch {
            content: Some(
                (0..10)
                    .map(|_| ToolProgressContent::Text {
                        text: "x".repeat(TOOL_PROGRESS_MAX_CONTENT_BYTES),
                    })
                    .collect(),
            ),
            ..Default::default()
        }
        .bounded();
        let encoded = serde_json::to_vec(&patch.content.unwrap()).unwrap();
        assert!(encoded.len() <= TOOL_PROGRESS_MAX_CONTENT_BYTES + 2);
    }

    #[test]
    fn terminal_status_is_removed_before_publication() {
        let patch = ToolProgressPatch {
            status: Some(ToolProgressStatus::Completed),
            ..Default::default()
        }
        .bounded();
        assert!(patch.is_empty());
    }

    #[test]
    fn patch_merge_replaces_supplied_fields_and_preserves_omissions() {
        let mut snapshot = full_patch();
        snapshot.merge(ToolProgressPatch {
            title: Some("Done scanning".into()),
            locations: Some(Vec::new()),
            ..Default::default()
        });

        assert_eq!(snapshot.title.as_deref(), Some("Done scanning"));
        assert_eq!(snapshot.locations, Some(Vec::new()));
        assert_eq!(snapshot.kind, Some(ToolProgressKind::Search));
    }

    #[test]
    fn default_handle_is_noop() {
        ToolProgressHandle::default().update(full_patch());
    }
}
