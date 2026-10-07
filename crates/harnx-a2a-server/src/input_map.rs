//! Render A2A parts for runtime admission without fetching files or interpreting client schemas.

use a2a_lf::{Message, Part, PartContent};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use harnx_core::{agent_config::AgentConfig, input::Input};
use std::{
    fmt,
    io::{self, Write},
};

#[derive(Debug, Clone, Copy)]
pub struct InputLimits {
    /// Combined UTF-8 size of rendered data and inline text/JSON file blocks,
    /// including banners and fences, but not separators between parts.
    pub max_data_part_bytes: usize,
}

impl Default for InputLimits {
    fn default() -> Self {
        Self {
            max_data_part_bytes: 65536,
        }
    }
}

#[derive(Debug)]
pub enum InputMapError {
    DataPartTooLarge { limit: usize },
    UnsupportedMediaType { media_type: String },
    InvalidUtf8 { media_type: String },
    JsonEncoding(serde_json::Error),
}

impl fmt::Display for InputMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataPartTooLarge { limit } => write!(
                f,
                "rendered A2A data/inline file parts exceed --max-data-part-bytes ({limit})"
            ),
            Self::UnsupportedMediaType { media_type } => {
                write!(f, "unsupported raw A2A file mediaType: {media_type}")
            }
            Self::InvalidUtf8 { media_type } => {
                write!(f, "raw A2A file is not UTF-8 (mediaType: {media_type})")
            }
            Self::JsonEncoding(error) => write!(f, "cannot render A2A data as JSON: {error}"),
        }
    }
}

impl std::error::Error for InputMapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::JsonEncoding(error) => Some(error),
            _ => None,
        }
    }
}

/// Message-level adapter so callers don't silently discard message metadata.
/// Original messages and parts belong in task history, not this rendered Input.
pub fn message_to_input(message: &Message, limits: InputLimits) -> Result<Input, InputMapError> {
    if let Some(metadata) = &message.metadata {
        tracing::debug!(
            metadata_fields = metadata.len(),
            "dropping A2A message metadata"
        );
    }
    parts_to_input(&message.parts, limits)
}

/// Render text parts in order, separated by a blank line. Images use the
/// runtime's media path; their bytes aren't part of the rendered text limit.
pub fn parts_to_input(parts: &[Part], limits: InputLimits) -> Result<Input, InputMapError> {
    let mut state = PartRenderState::new(limits);
    let mut rendered = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        log_part_metadata(index, part);
        if let Some(text) = state.render_part(part)? {
            rendered.push(text);
        }
    }
    // Match ACP admission: the session supplies agent configuration later.
    let mut input = Input::new(
        rendered.join("\n\n"),
        (String::new(), vec![]),
        AgentConfig::default(),
    );
    input.medias = state.medias;
    Ok(input)
}

// Stores no output, and stops serialization as soon as the remaining budget is exceeded.
struct SizeLimitedWriter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl SizeLimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            exceeded: false,
        }
    }

    fn add_bytes(&mut self, bytes: usize) -> io::Result<()> {
        match self.bytes.checked_add(bytes) {
            Some(total) if !self.exceeded && total <= self.limit => {
                self.bytes = total;
                Ok(())
            }
            _ => {
                self.exceeded = true;
                Err(io::Error::other(
                    "rendered A2A data exceeds remaining budget",
                ))
            }
        }
    }
}

impl Write for SizeLimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.add_bytes(bytes.len())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// State and budget tracking for rendering A2A parts.
struct PartRenderState {
    limits: InputLimits,
    data_bytes: usize,
    medias: Vec<String>,
}

impl PartRenderState {
    fn new(limits: InputLimits) -> Self {
        Self {
            limits,
            data_bytes: 0,
            medias: Vec::new(),
        }
    }

    fn remaining_budget(&self) -> usize {
        self.limits
            .max_data_part_bytes
            .saturating_sub(self.data_bytes)
    }

    fn count_bytes(&mut self, bytes: usize) -> Result<(), InputMapError> {
        self.data_bytes = self
            .data_bytes
            .checked_add(bytes)
            .filter(|size| *size <= self.limits.max_data_part_bytes)
            .ok_or(InputMapError::DataPartTooLarge {
                limit: self.limits.max_data_part_bytes,
            })?;
        Ok(())
    }

    fn block_counter(
        &self,
        part: &Part,
        default_media_type: &str,
        fence: &str,
    ) -> Result<SizeLimitedWriter, InputMapError> {
        let mut counter = SizeLimitedWriter::new(self.remaining_budget());
        let media_type = part.media_type.as_deref().unwrap_or(default_media_type);
        // Count the banner without allocating a copy of client-controlled metadata.
        let result = (|| {
            write!(counter, "--- A2A data part (mediaType: {media_type}")?;
            if let Some(filename) = &part.filename {
                write!(counter, ", filename: {filename}")?;
            }
            write!(
                counter,
                ") ---\n```{fence}\n\n```\n--- End A2A data part ---"
            )
        })();
        result.map_err(|_| InputMapError::DataPartTooLarge {
            limit: self.limits.max_data_part_bytes,
        })?;
        Ok(counter)
    }

    fn render_data_part(
        &mut self,
        part: &Part,
        data: &serde_json::Value,
    ) -> Result<String, InputMapError> {
        let mut counter = self.block_counter(part, "application/json", "json")?;
        // Pretty printing can greatly expand deeply nested input. Count with
        // an early-stopping writer before allocating either rendered copy.
        serde_json::to_writer_pretty(&mut counter, data).map_err(|error| {
            if counter.exceeded {
                InputMapError::DataPartTooLarge {
                    limit: self.limits.max_data_part_bytes,
                }
            } else {
                InputMapError::JsonEncoding(error)
            }
        })?;
        self.count_bytes(counter.bytes)?;
        let json = serde_json::to_string_pretty(data).map_err(InputMapError::JsonEncoding)?;
        Ok(inline_block(part, "application/json", "json", &json))
    }

    fn render_raw_part(
        &mut self,
        part: &Part,
        bytes: &[u8],
    ) -> Result<Option<String>, InputMapError> {
        let media_type = part
            .media_type
            .as_deref()
            .unwrap_or("application/octet-stream");
        if media_type.starts_with("image/") {
            self.medias.push(format!(
                "data:{media_type};base64,{}",
                STANDARD.encode(bytes)
            ));
            return Ok(None);
        }
        let fence = resolve_raw_fence(media_type)?;
        let mut counter = self.block_counter(part, media_type, fence)?;
        // Raw bytes are already decoded by the protocol layer. UTF-8
        // validation and banner formatting don't need to scan oversized files.
        counter
            .add_bytes(bytes.len())
            .map_err(|_| InputMapError::DataPartTooLarge {
                limit: self.limits.max_data_part_bytes,
            })?;
        self.count_bytes(counter.bytes)?;
        let text = std::str::from_utf8(bytes).map_err(|_| InputMapError::InvalidUtf8 {
            media_type: media_type.to_owned(),
        })?;
        Ok(Some(inline_block(part, media_type, fence, text)))
    }

    fn render_part(&mut self, part: &Part) -> Result<Option<String>, InputMapError> {
        match &part.content {
            PartContent::Text(text) => Ok(Some(text.clone())),
            PartContent::Data(data) => self.render_data_part(part, data).map(Some),
            PartContent::Url(url) => Ok(Some(render_url_part(part, url))),
            PartContent::Raw(bytes) => self.render_raw_part(part, bytes),
        }
    }
}

fn resolve_raw_fence(media_type: &str) -> Result<&'static str, InputMapError> {
    if media_type == "application/json" {
        Ok("json")
    } else if media_type.starts_with("text/") {
        Ok("text")
    } else {
        Err(InputMapError::UnsupportedMediaType {
            media_type: media_type.to_owned(),
        })
    }
}

fn render_url_part(part: &Part, url: &str) -> String {
    let filename = part.filename.as_deref().unwrap_or(url);
    let media_type = part
        .media_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    format!("[A2A file: {filename} ({media_type}) {url}]")
}

fn inline_block(part: &Part, default_media_type: &str, fence: &str, content: &str) -> String {
    let media_type = part.media_type.as_deref().unwrap_or(default_media_type);
    let filename = part
        .filename
        .as_ref()
        .map(|name| format!(", filename: {name}"))
        .unwrap_or_default();
    format!(
        "--- A2A data part (mediaType: {media_type}{filename}) ---\n```{fence}\n{content}\n```\n--- End A2A data part ---"
    )
}

fn log_part_metadata(index: usize, part: &Part) {
    if let Some(metadata) = &part.metadata {
        tracing::debug!(
            part_index = index,
            metadata_fields = metadata.len(),
            "dropping A2A part metadata"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn jira_message(fixture: &str) -> Message {
        harnx_core::require_nextest();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/jira")
            .join(fixture);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        serde_json::from_value(value["params"]["message"].clone()).unwrap()
    }

    fn assert_jira_rendering(fixture: &str) {
        let message = jira_message(fixture);
        let before = message.clone();
        let input = message_to_input(&message, InputLimits::default()).unwrap();
        let mut remaining = input.raw_text();
        for part in &message.parts {
            match &part.content {
                PartContent::Text(text) => {
                    remaining = remaining.strip_prefix(text.as_str()).unwrap();
                }
                PartContent::Data(data) => {
                    let pretty = serde_json::to_string_pretty(data).unwrap();
                    let block = format!(
                        "--- A2A data part (mediaType: application/json) ---\n```json\n{pretty}\n```\n--- End A2A data part ---"
                    );
                    remaining = remaining.strip_prefix(block.as_str()).unwrap();
                    // Parse the rendered JSON back to catch any lost nested fields.
                    let start = input.raw_text().find("```json\n").unwrap() + "```json\n".len();
                    let end = input.raw_text()[start..].find("\n```").unwrap() + start;
                    assert_eq!(
                        serde_json::from_str::<Value>(&input.raw_text()[start..end]).unwrap(),
                        *data
                    );
                }
                other => panic!("unexpected Jira part: {other:?}"),
            }
            if !remaining.is_empty() {
                remaining = remaining.strip_prefix("\n\n").unwrap();
            }
        }
        assert!(remaining.is_empty());
        assert!(input.medias.is_empty());
        assert_eq!(message, before);
    }

    #[test]
    fn jira_assignment() {
        assert_jira_rendering("assignment.json");
    }

    #[test]
    fn jira_comment_mention() {
        assert_jira_rendering("comment_mention.json");
    }

    #[test]
    fn jira_automation() {
        assert_jira_rendering("automation.json");
    }

    #[test]
    fn jira_chat() {
        assert_jira_rendering("chat.json");
    }

    #[test]
    fn jira_assignment_exact_prompt() {
        let input =
            message_to_input(&jira_message("assignment.json"), InputLimits::default()).unwrap();
        assert_eq!(input.raw_text(), concat!(
            "Space Instructions:\nBe concise and cite Jira evidence.\n\n",
            "You have been assigned to a work item \"QA checkout flow updates\". Analyze the details of the work item and get started.\n\n",
            "Relevant Context for This Task\nThe following resources have been identified as relevant. Fetch and reference them to get additional context such as prior decisions, related work, team conventions, etc.\n",
            "- Checkout design: https://example.com/checkout-design\n\n",
            "--- A2A data part (mediaType: application/json) ---\n```json\n",
            "{\n  \"userAccountId\": \"22222\",\n  \"agentAccountId\": \"11111\",\n  \"invocationType\": \"ISSUE_ASSIGNMENT\",\n",
            "  \"issue\": {\n    \"id\": \"21930\",\n    \"fields\": {\n      \"key\": \"AW26-11\",\n      \"summary\": \"QA checkout flow updates\",\n      \"description\": \"Perform a comprehensive QA review...\"\n    }\n  }\n}",
            "\n```\n--- End A2A data part ---"
        ));
    }

    #[test]
    fn oversized_data_is_not_truncated() {
        harnx_core::require_nextest();
        let parts = [Part::data(serde_json::json!({"value": "oversized"}))];
        let error = parts_to_input(
            &parts,
            InputLimits {
                max_data_part_bytes: 1,
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            InputMapError::DataPartTooLarge { limit: 1 }
        ));
        assert_eq!(
            error.to_string(),
            "rendered A2A data/inline file parts exceed --max-data-part-bytes (1)"
        );
    }

    #[test]
    fn limit_counts_complete_blocks_and_is_cumulative() {
        harnx_core::require_nextest();
        let part = Part::data(serde_json::json!({"value": "é"}));
        let bytes = parts_to_input(std::slice::from_ref(&part), InputLimits::default())
            .unwrap()
            .raw_text()
            .len();
        let limits = InputLimits {
            max_data_part_bytes: bytes,
        };
        assert!(parts_to_input(std::slice::from_ref(&part), limits).is_ok());
        assert!(matches!(
            parts_to_input(
                std::slice::from_ref(&part),
                InputLimits {
                    max_data_part_bytes: bytes - 1
                }
            ),
            Err(InputMapError::DataPartTooLarge { .. })
        ));
        assert!(matches!(
            parts_to_input(&[part.clone(), part], limits),
            Err(InputMapError::DataPartTooLarge { .. })
        ));
    }

    #[test]
    fn limit_is_shared_by_data_and_inline_files() {
        harnx_core::require_nextest();
        let data = Part::data(serde_json::json!({"context": "value"}));
        let file = Part::raw(b"file contents".to_vec()).with_media_type("text/plain");
        let data_size = parts_to_input(std::slice::from_ref(&data), InputLimits::default())
            .unwrap()
            .raw_text()
            .len();
        let file_size = parts_to_input(std::slice::from_ref(&file), InputLimits::default())
            .unwrap()
            .raw_text()
            .len();
        let parts = [data, Part::text("text isn't charged"), file];
        assert!(parts_to_input(
            &parts,
            InputLimits {
                max_data_part_bytes: data_size + file_size
            }
        )
        .is_ok());
        assert!(matches!(
            parts_to_input(
                &parts,
                InputLimits {
                    max_data_part_bytes: data_size + file_size - 1
                }
            ),
            Err(InputMapError::DataPartTooLarge { .. })
        ));
    }

    #[test]
    fn oversized_pretty_json_is_rejected_before_rendering() {
        harnx_core::require_nextest();
        let mut data = Value::Array(vec![Value::Null; 8192]);
        for _ in 0..80 {
            data = Value::Array(vec![data]);
        }
        let limits = InputLimits::default();
        let part = Part::data(data);
        assert!(matches!(
            parts_to_input(&[part], limits),
            Err(InputMapError::DataPartTooLarge { limit }) if limit == limits.max_data_part_bytes
        ));
    }

    #[test]
    fn size_writer_stops_at_budget_without_buffering_output() {
        harnx_core::require_nextest();
        let mut counter = SizeLimitedWriter::new(4);
        counter.write_all(b"1234").unwrap();
        assert_eq!(counter.bytes, 4);
        assert!(counter.write_all(b"5").is_err());
        assert_eq!(counter.bytes, 4);
        assert!(counter.exceeded);

        let mut counter = SizeLimitedWriter::new(64);
        let data = serde_json::json!({"large": "x".repeat(65536)});
        let error = serde_json::to_writer_pretty(&mut counter, &data).unwrap_err();
        assert!(error.is_io());
        assert!(counter.exceeded);
        assert!(counter.bytes <= 64);

        let mut counter = SizeLimitedWriter::new(usize::MAX);
        counter.add_bytes(1).unwrap();
        assert!(counter.add_bytes(usize::MAX).is_err());
        assert_eq!(counter.bytes, 1);
    }

    #[test]
    fn banner_count_matches_rendered_bytes_without_copying_metadata() {
        harnx_core::require_nextest();
        for part in [
            Part::data(Value::Null),
            Part::data(Value::Null).with_filename(""),
            Part::data(Value::Null)
                .with_media_type("application/vnd.example+json")
                .with_filename("日本語.json"),
        ] {
            let state = PartRenderState::new(InputLimits::default());
            let counter = state
                .block_counter(&part, "application/json", "json")
                .unwrap();
            assert_eq!(
                counter.bytes,
                inline_block(&part, "application/json", "json", "").len()
            );
        }
        let part = Part::data(Value::Null).with_filename("x".repeat(65536));
        assert!(matches!(
            parts_to_input(&[part], InputLimits::default()),
            Err(InputMapError::DataPartTooLarge { .. })
        ));
    }

    #[test]
    fn oversized_raw_files_are_rejected_before_utf8_validation() {
        harnx_core::require_nextest();
        for media_type in ["text/plain", "application/json"] {
            let part = Part::raw(vec![0xff; 65536]).with_media_type(media_type);
            assert!(matches!(
                parts_to_input(&[part], InputLimits::default()),
                Err(InputMapError::DataPartTooLarge { limit: 65536 })
            ));
        }
    }

    #[test]
    fn raw_without_media_type_is_rejected_as_octet_stream() {
        harnx_core::require_nextest();
        let error = parts_to_input(&[Part::raw(vec![1])], InputLimits::default()).unwrap_err();
        assert!(
            matches!(error, InputMapError::UnsupportedMediaType { media_type } if media_type == "application/octet-stream")
        );
    }

    #[test]
    fn raw_unsupported_media_type_names_the_type() {
        harnx_core::require_nextest();
        let part = Part::raw(vec![0, 1]).with_media_type("application/pdf");
        let error = parts_to_input(&[part], InputLimits::default()).unwrap_err();
        assert!(
            matches!(&error, InputMapError::UnsupportedMediaType { media_type } if media_type == "application/pdf")
        );
        assert!(error.to_string().contains("application/pdf"));
    }

    #[test]
    fn raw_image_goes_to_medias_in_order() {
        harnx_core::require_nextest();
        let parts = [
            Part::text("before"),
            Part::raw(vec![0, 1, 2]).with_media_type("image/png"),
            Part::text("after"),
            Part::raw(vec![3, 4, 5]).with_media_type("image/jpeg"),
        ];
        let input = parts_to_input(
            &parts,
            InputLimits {
                max_data_part_bytes: 0,
            },
        )
        .unwrap();
        assert_eq!(input.raw_text(), "before\n\nafter");
        assert_eq!(
            input.medias,
            ["data:image/png;base64,AAEC", "data:image/jpeg;base64,AwQF"]
        );
        assert!(input.attachment_refs.is_empty());
    }

    #[test]
    fn raw_text_is_inlined_verbatim_and_limited() {
        harnx_core::require_nextest();
        let part = Part::raw(b"line one\n  line two\n".to_vec())
            .with_media_type("text/plain")
            .with_filename("notes.txt");
        let input = parts_to_input(std::slice::from_ref(&part), InputLimits::default()).unwrap();
        assert_eq!(input.raw_text(), "--- A2A data part (mediaType: text/plain, filename: notes.txt) ---\n```text\nline one\n  line two\n\n```\n--- End A2A data part ---");
        assert!(input.medias.is_empty());
        assert!(matches!(
            parts_to_input(
                &[part],
                InputLimits {
                    max_data_part_bytes: input.raw_text().len() - 1
                }
            ),
            Err(InputMapError::DataPartTooLarge { .. })
        ));
    }

    #[test]
    fn raw_json_is_decoded_and_inlined() {
        harnx_core::require_nextest();
        let part = Part::raw(br#"{"z":1,"a":2}"#.to_vec()).with_media_type("application/json");
        let input = parts_to_input(&[part], InputLimits::default()).unwrap();
        assert_eq!(input.raw_text(), "--- A2A data part (mediaType: application/json) ---\n```json\n{\"z\":1,\"a\":2}\n```\n--- End A2A data part ---");
    }

    #[test]
    fn invalid_utf8_is_rejected_without_lossy_decoding() {
        harnx_core::require_nextest();
        for media_type in ["text/plain", "application/json"] {
            let part = Part::raw(vec![0xff]).with_media_type(media_type);
            assert!(matches!(
                parts_to_input(&[part], InputLimits::default()),
                Err(InputMapError::InvalidUtf8 { .. })
            ));
        }
    }

    #[test]
    fn url_files_are_rendered_without_fetching() {
        harnx_core::require_nextest();
        let parts = [
            Part::url("https://example.invalid/file")
                .with_media_type("application/pdf")
                .with_filename("report.pdf"),
            Part::url("https://example.invalid/other"),
        ];
        let input = parts_to_input(
            &parts,
            InputLimits {
                max_data_part_bytes: 0,
            },
        )
        .unwrap();
        assert_eq!(input.raw_text(), "[A2A file: report.pdf (application/pdf) https://example.invalid/file]\n\n[A2A file: https://example.invalid/other (application/octet-stream) https://example.invalid/other]");
        assert!(input.medias.is_empty());
    }

    #[test]
    fn multiple_data_parts_preserve_part_and_key_order() {
        harnx_core::require_nextest();
        let parts = [
            Part::text("start"),
            Part::data(serde_json::from_str(r#"{"z":1,"a":{"y":2,"b":3}}"#).unwrap())
                .with_media_type("application/vnd.example+json")
                .with_filename("context.json"),
            Part::text("middle"),
            Part::data(serde_json::from_str(r#"{"second":4,"first":5}"#).unwrap()),
            Part::text("end"),
        ];
        let input = parts_to_input(&parts, InputLimits::default()).unwrap();
        assert_eq!(input.raw_text(), concat!(
            "start\n\n--- A2A data part (mediaType: application/vnd.example+json, filename: context.json) ---\n```json\n",
            "{\n  \"z\": 1,\n  \"a\": {\n    \"y\": 2,\n    \"b\": 3\n  }\n}\n```\n--- End A2A data part ---\n\nmiddle\n\n",
            "--- A2A data part (mediaType: application/json) ---\n```json\n{\n  \"second\": 4,\n  \"first\": 5\n}\n```\n--- End A2A data part ---\n\nend"
        ));
    }

    #[test]
    fn metadata_is_not_in_prompt_and_original_message_is_unchanged() {
        let mut message = jira_message("chat.json");
        message.metadata = Some([(String::from("messageSecret"), Value::from("hidden"))].into());
        message.parts[0].metadata =
            Some([(String::from("partSecret"), Value::from("hidden"))].into());
        let before = message.clone();
        let input = message_to_input(&message, InputLimits::default()).unwrap();
        assert_eq!(input.raw_text(), "Open the pod bay doors, HAL.");
        assert_eq!(message, before);
    }

    #[test]
    fn empty_parts_and_text_whitespace_are_preserved() {
        harnx_core::require_nextest();
        assert!(parts_to_input(&[], InputLimits::default())
            .unwrap()
            .is_empty());
        let input = parts_to_input(
            &[
                Part::text("  first\n"),
                Part::text(""),
                Part::text("last  "),
            ],
            InputLimits {
                max_data_part_bytes: 0,
            },
        )
        .unwrap();
        assert_eq!(input.raw_text(), "  first\n\n\n\n\nlast  ");
    }
}
