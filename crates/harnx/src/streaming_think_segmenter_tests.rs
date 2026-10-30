use super::*;
use harnx_core::text::{split_think_blocks, ThinkSegment};

fn coalesce(segments: Vec<Segment>) -> Vec<(bool, String)> {
    let mut result: Vec<(bool, String)> = Vec::new();
    for segment in segments {
        let (thought, text) = match segment {
            Segment::Text(text) => (false, text),
            Segment::Thought(text) => (true, text),
            Segment::ThoughtEnd => continue,
        };
        if let Some((_, last_text)) = result.last_mut().filter(|(kind, _)| *kind == thought) {
            last_text.push_str(&text);
            continue;
        }
        result.push((thought, text));
    }
    result
}

fn assert_matches_splitter(input: &str) {
    let expected = coalesce(
        split_think_blocks(input)
            .into_iter()
            .map(|segment| match segment {
                ThinkSegment::Text(text) => Segment::Text(text.into()),
                ThinkSegment::Thought(text) => Segment::Thought(text.into()),
            })
            .collect(),
    );
    // Every two-chunk partition, including inside either tag and at UTF-8 boundaries.
    for split in input
        .char_indices()
        .map(|(pos, _)| pos)
        .chain([input.len()])
    {
        let mut parser = StreamingThinkSegmenter::default();
        let mut segments = parser.segment(&input[..split]);
        segments.extend(parser.segment(&input[split..]));
        segments.extend(parser.flush());
        assert_eq!(coalesce(segments), expected, "split at {split}: {input:?}");
    }
    let mut parser = StreamingThinkSegmenter::default();
    let mut segments = Vec::new();
    for ch in input.chars() {
        segments.extend(parser.segment(&ch.to_string()));
        assert!(parser.pending.len() <= "\n</thinking".len());
    }
    segments.extend(parser.flush());
    assert_eq!(
        coalesce(segments),
        expected,
        "one char at a time: {input:?}"
    );
}

#[test]
fn split_tags_across_chunks_both_variants() {
    assert_matches_splitter("<think>\nreason\n</think>\n\nanswer");
    assert_matches_splitter("<thinking>\nreason\n</thinking>answer");
}

#[test]
fn holdback_never_emits_partial_tags() {
    let mut parser = StreamingThinkSegmenter::default();
    assert_eq!(
        parser.segment("before<thi"),
        [Segment::Text("before".into())]
    );
    assert!(parser.segment("n").is_empty());
    assert_eq!(
        parser.segment("k>reason</thi"),
        [Segment::Thought("reason".into())]
    );
    assert!(parser.segment("nk").is_empty());
    assert_eq!(
        parser.segment(">after"),
        [Segment::ThoughtEnd, Segment::Text("after".into())]
    );
    assert!(parser.flush().is_empty());
}

#[test]
fn mid_text_block_preserves_order_and_unicode() {
    assert_matches_splitter("before 🌍 <think>why λ</think> after 🦀");
}

#[test]
fn multiple_blocks_and_adjacent_blocks_have_explicit_ends() {
    assert_matches_splitter("a<think>one</think>b<thinking>two</thinking>c");
    let mut parser = StreamingThinkSegmenter::default();
    assert_eq!(
        parser.segment("<think>one</think><thinking>two</thinking>"),
        [
            Segment::Thought("one".into()),
            Segment::ThoughtEnd,
            Segment::Thought("two".into()),
            Segment::ThoughtEnd,
        ]
    );
}

#[test]
fn unclosed_trailing_block_flushes_in_thought_style() {
    assert_matches_splitter("answer<think>\nunfinished\n");
    assert_matches_splitter("<thinking>unfinished\n</think");
    let mut parser = StreamingThinkSegmenter::default();
    assert_eq!(
        parser.segment("<think>body\n</thi"),
        [Segment::Thought("body".into())]
    );
    assert_eq!(
        parser.flush(),
        [Segment::Thought("\n</thi".into()), Segment::ThoughtEnd]
    );
    assert_eq!(
        parser.segment("next turn"),
        [Segment::Text("next turn".into())]
    );
    assert!(parser.flush().is_empty());
}

#[test]
fn mismatched_closer_remains_literal_content() {
    assert_matches_splitter("<think>one</thinking>two</think>answer");
    assert_matches_splitter("<thinking>one</think>two</thinking>answer");
    assert_matches_splitter("orphan </think> stays text");
}

#[test]
fn boundary_newlines_trim_exactly_once() {
    assert_matches_splitter("<think>\n\nbody\n\n</think>\n\nanswer");
    assert_matches_splitter("<think>\r\nbody\r\n</think>");
    assert_matches_splitter("<think>\nbody\n");
}

#[test]
fn invalid_prefixes_flush_as_text() {
    assert_matches_splitter("a < 3 <thimble> <Think> nope <thi");
    let mut parser = StreamingThinkSegmenter::default();
    assert!(parser.segment("<thi").is_empty());
    assert_eq!(parser.flush(), [Segment::Text("<thi".into())]);
    assert!(parser.flush().is_empty());
}

#[test]
fn empty_blocks_and_empty_input() {
    assert_matches_splitter("");
    assert_matches_splitter("<think></think><thinking>\n</thinking>answer");
    assert_matches_splitter("<think>");
    assert_matches_splitter("<thinking>\n");
}

#[test]
fn nesting_and_fences_match_shared_splitter() {
    assert_matches_splitter("```\n<think>outer <think>nested</think>text\n```");
}
