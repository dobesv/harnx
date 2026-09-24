use std::collections::HashMap;

use harnx_core::safety::{TruncateOpts, DEFAULT_MAX_BYTES};
use serde::Deserialize;

pub const DEFAULT_MAX_LENGTH: usize = 5000;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct FetchParams {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub start_index: Option<usize>,
    #[serde(default)]
    pub head_lines: Option<usize>,
    #[serde(default)]
    pub tail_lines: Option<usize>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct YoutubeTranscriptParams {
    #[serde(flatten)]
    pub fetch: FetchParams,
    #[serde(default = "default_language")]
    pub lang: String,
}

fn default_language() -> String {
    "en".to_owned()
}

impl FetchParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.url.trim().is_empty() {
            return Err("'url' parameter is required and must be non-empty".to_owned());
        }
        Ok(())
    }

    pub fn truncation_opts(&self, disable_line_clipping: bool) -> TruncateOpts {
        let defaults = TruncateOpts::default();
        TruncateOpts {
            head_lines: self.head_lines.unwrap_or(defaults.head_lines),
            tail_lines: self.tail_lines.unwrap_or(defaults.tail_lines),
            line_head_bytes: if disable_line_clipping {
                0
            } else {
                defaults.line_head_bytes
            },
            line_tail_bytes: if disable_line_clipping {
                0
            } else {
                defaults.line_tail_bytes
            },
            max_output_bytes: self
                .max_output_bytes
                .unwrap_or(defaults.max_output_bytes.min(DEFAULT_MAX_BYTES)),
            marker: defaults.marker,
        }
    }

    pub fn compatibility_window(&self, value: &str) -> (String, bool) {
        let start = self.start_index.unwrap_or(0);
        let max = self.max_length.unwrap_or(DEFAULT_MAX_LENGTH);
        let chars: Vec<char> = value.chars().collect();
        if start >= chars.len() {
            return (String::new(), !chars.is_empty());
        }
        let end = if max == 0 {
            chars.len()
        } else {
            start.saturating_add(max).min(chars.len())
        };
        (
            chars[start..end].iter().collect(),
            start > 0 || end < chars.len(),
        )
    }
}

impl YoutubeTranscriptParams {
    pub fn validate(&self) -> Result<(), String> {
        self.fetch.validate()?;
        if self.lang.trim().is_empty() {
            return Err("'lang' must be non-empty".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_window_matches_upstream_semantics() {
        let mut params = FetchParams {
            url: "https://example.com".to_owned(),
            ..FetchParams::default()
        };
        params.start_index = Some(2);
        params.max_length = Some(3);
        assert_eq!(params.compatibility_window("abcdef").0, "cde");
        params.max_length = Some(0);
        assert_eq!(params.compatibility_window("abcdef").0, "cdef");
        params.start_index = Some(10);
        assert_eq!(params.compatibility_window("abcdef").0, "");
    }

    #[test]
    fn defaults_youtube_language() {
        let params: YoutubeTranscriptParams = serde_json::from_value(serde_json::json!({
            "url": "https://youtube.com/watch?v=test"
        }))
        .unwrap();
        assert_eq!(params.lang, "en");
    }
}
