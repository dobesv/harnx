//! Small text-processing utilities shared across crates: reasoning-tag
//! stripping and an unicode-aware token-length heuristic.

use fancy_regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;
use unicode_segmentation::UnicodeSegmentation;

pub static THINK_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\s*<think>.*?</think>(\s*|$)").unwrap());

pub static CODE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)```\w*(.*)```").unwrap());

/// Remove a leading `<think>...</think>` block (if any) from a string.
pub fn strip_think_tag(text: &str) -> Cow<'_, str> {
    THINK_TAG_RE.replace_all(text, "")
}

/// Return the contents of the first fenced code block in `text`, or
/// the whole input if no block is present.
pub fn extract_code_block(text: &str) -> &str {
    CODE_BLOCK_RE
        .captures(text)
        .ok()
        .and_then(|v| v?.get(1).map(|v| v.as_str().trim()))
        .unwrap_or(text)
}

/// Rough estimate of how many LLM tokens a string represents.
/// Uses unicode-word segmentation and per-word heuristics:
/// ASCII words ~1.3 tokens, single non-ASCII chars ~1 token, multi-char
/// non-ASCII words ~0.5 tokens per character.
pub fn estimate_token_length(text: &str) -> usize {
    let words: Vec<&str> = text.unicode_words().collect();
    let mut output: f32 = 0.0;
    for word in words {
        if word.is_ascii() {
            output += 1.3;
        } else {
            let count = word.chars().count();
            if count == 1 {
                output += 1.0
            } else {
                output += (count as f32) * 0.5;
            }
        }
    }
    output.ceil() as usize
}
