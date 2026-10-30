//! Small text-processing utilities shared across crates: reasoning-tag
//! splitting and stripping, and a Unicode-aware token-length heuristic.

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

/// Case-sensitive opening/closing tag pairs shared by thought renderers.
pub const THINK_TAG_PAIRS: [(&str, &str); 2] =
    [("<think>", "</think>"), ("<thinking>", "</thinking>")];

/// A borrowed span of normal text or thought content, without its wrapper tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkSegment<'a> {
    Text(&'a str),
    Thought(&'a str),
}

/// Split text into ordered normal-text and thought segments.
///
/// Recognizes [`THINK_TAG_PAIRS`] at any position, including inside fenced code
/// blocks; Markdown fences are not treated specially. Each opener ends at the
/// first matching closer. Other tags inside a thought (including mismatched
/// closers and nested openers) remain literal text, with no nesting support.
/// An opener without a matching closer makes the rest of the input a thought.
///
/// Removes at most one `\n` immediately after an opener and immediately before
/// a matching closer. All other whitespace is preserved, including trailing
/// newlines in an unclosed thought. Empty segments are omitted.
pub fn split_think_blocks(mut text: &str) -> Vec<ThinkSegment<'_>> {
    let mut segments = Vec::new();
    while let Some((start, (open, close))) = THINK_TAG_PAIRS
        .iter()
        .filter_map(|&(open, close)| text.find(open).map(|start| (start, (open, close))))
        .min_by_key(|&(start, _)| start)
    {
        if start > 0 {
            segments.push(ThinkSegment::Text(&text[..start]));
        }
        let content = &text[start + open.len()..];
        let end = content.find(close);
        let thought = &content[..end.unwrap_or(content.len())];
        let thought = thought.strip_prefix('\n').unwrap_or(thought);
        let thought = if end.is_some() {
            thought.strip_suffix('\n').unwrap_or(thought)
        } else {
            thought
        };
        if !thought.is_empty() {
            segments.push(ThinkSegment::Thought(thought));
        }
        text = end.map_or("", |end| &content[end + close.len()..]);
    }
    if !text.is_empty() {
        segments.push(ThinkSegment::Text(text));
    }
    segments
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

#[cfg(test)]
mod tests {
    use super::ThinkSegment::{Text, Thought};
    use super::{split_think_blocks, strip_think_tag, THINK_TAG_PAIRS};

    #[test]
    fn split_think_blocks_without_tags_preserves_text() {
        assert_eq!(split_think_blocks("Hello\n世界"), vec![Text("Hello\n世界")]);
        assert!(split_think_blocks("").is_empty());
    }

    #[test]
    fn split_think_blocks_leading_harnx_format() {
        assert_eq!(
            split_think_blocks("<think>\nreasoning\n</think>\n\nanswer **bold**"),
            vec![Thought("reasoning"), Text("\n\nanswer **bold**")]
        );
    }

    #[test]
    fn split_think_blocks_in_middle() {
        assert_eq!(
            split_think_blocks("before <think>reasoning</think> after"),
            vec![Text("before "), Thought("reasoning"), Text(" after")]
        );
    }

    #[test]
    fn split_think_blocks_multiple_mixed_and_adjacent_blocks() {
        assert_eq!(
            split_think_blocks(
                "<thinking>first</thinking>between<think>second</think><think>third</think>answer"
            ),
            vec![
                Thought("first"),
                Text("between"),
                Thought("second"),
                Thought("third"),
                Text("answer")
            ]
        );
    }

    #[test]
    fn split_think_blocks_thinking_variant() {
        assert_eq!(
            split_think_blocks("<thinking>\nreasoning\n</thinking>answer"),
            vec![Thought("reasoning"), Text("answer")]
        );
    }

    #[test]
    fn split_think_blocks_unclosed_trailing_opener() {
        for (open, _) in THINK_TAG_PAIRS {
            let input = format!("answer{open}\nunfinished\n");
            assert_eq!(
                split_think_blocks(&input),
                vec![Text("answer"), Thought("unfinished\n")]
            );
        }
    }

    #[test]
    fn split_think_blocks_mismatched_closer_stays_literal() {
        assert_eq!(
            split_think_blocks("<think>one</thinking>two</think>answer"),
            vec![Thought("one</thinking>two"), Text("answer")]
        );
        assert_eq!(
            split_think_blocks("<thinking>one</think>two</thinking>answer"),
            vec![Thought("one</think>two"), Text("answer")]
        );
        assert_eq!(
            split_think_blocks("<think>one</thinking>answer"),
            vec![Thought("one</thinking>answer")]
        );
        assert_eq!(
            split_think_blocks("before</think>after</thinking>"),
            vec![Text("before</think>after</thinking>")]
        );
    }

    #[test]
    fn split_think_blocks_drops_empty_segments() {
        for input in [
            "<think></think>",
            "<think>\n</think>",
            "<thinking>\n\n</thinking>",
            "<think>",
            "<thinking>\n",
        ] {
            assert!(split_think_blocks(input).is_empty(), "{input:?}");
        }
        assert_eq!(
            split_think_blocks("before<think></think>after"),
            vec![Text("before"), Text("after")]
        );
    }

    #[test]
    fn split_think_blocks_trims_only_one_newline_at_each_boundary() {
        assert_eq!(
            split_think_blocks("<think>\n\n reasoning \n\n</think>\n"),
            vec![Thought("\n reasoning \n"), Text("\n")]
        );
        assert_eq!(split_think_blocks("<think> </think>"), vec![Thought(" ")]);
    }

    #[test]
    fn split_think_blocks_is_case_sensitive() {
        assert_eq!(
            split_think_blocks("<Think>reasoning</Think>"),
            vec![Text("<Think>reasoning</Think>")]
        );
    }

    #[test]
    fn split_think_blocks_inside_code_fences_are_not_special() {
        assert_eq!(
            split_think_blocks("```text\n<think>reasoning</think>\n```"),
            vec![Text("```text\n"), Thought("reasoning"), Text("\n```")]
        );
    }

    #[test]
    fn split_think_blocks_does_not_nest() {
        assert_eq!(
            split_think_blocks("<think>one<thinking>two</thinking></think>answer"),
            vec![Thought("one<thinking>two</thinking>"), Text("answer")]
        );
    }

    #[test]
    fn strip_think_tag_retains_existing_leading_only_behavior() {
        assert_eq!(
            strip_think_tag(" <think>\nreasoning\n</think>\n\nanswer"),
            "answer"
        );
        for input in [
            "before<think>reasoning</think>after",
            "<thinking>reasoning</thinking>answer",
            "<think>unfinished",
        ] {
            assert_eq!(strip_think_tag(input), input);
        }
    }
}
