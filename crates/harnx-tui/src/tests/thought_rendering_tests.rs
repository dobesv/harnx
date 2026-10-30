use super::{line_to_plain, render_entry_lines, rendered_span_matches};
use crate::types::TranscriptItem;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;

fn assistant_text(text: &str) -> TranscriptItem {
    TranscriptItem::AssistantText {
        text: text.into(),
        seq: None,
        timestamp: None,
        rendered_cache: None,
    }
}

fn assert_thought_style(lines: &[Line<'static>], text: &str) {
    assert!(rendered_span_matches(lines, "Thinking", |style| {
        style.add_modifier == Modifier::DIM && style.fg.is_none()
    }));
    assert!(rendered_span_matches(lines, text, |style| {
        style.fg == Some(Color::DarkGray) && style.add_modifier == Modifier::DIM | Modifier::ITALIC
    }));
}

#[test]
fn assistant_think_block_snapshot() {
    let item = assistant_text("<think>\nreasoning\n</think>\n\nanswer **bold**");
    let lines = render_entry_lines(&item, false, false, false);
    let plain = lines
        .iter()
        .map(line_to_plain)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(plain, "Thinking\nreasoning\nanswer bold");
    assert_thought_style(&lines, "reasoning");
    assert!(rendered_span_matches(&lines, "bold", |style| {
        style.add_modifier.contains(Modifier::BOLD)
            && !style
                .add_modifier
                .intersects(Modifier::DIM | Modifier::ITALIC)
    }));
    insta::assert_snapshot!("assistant_think_block", plain);
}

#[test]
fn explicit_and_tagged_thoughts_share_plain_text_style() {
    let text = "**plain**, `code`\nsecond line";
    let thought = TranscriptItem::ThoughtText(text.into());
    let tagged = assistant_text(&format!("<think>\n{text}\n</think>"));
    let thought_lines = render_entry_lines(&thought, false, false, false);
    let tagged_lines = render_entry_lines(&tagged, false, false, false);

    assert_eq!(thought_lines, tagged_lines);
    assert_eq!(
        thought_lines.iter().map(line_to_plain).collect::<Vec<_>>(),
        ["Thinking", "**plain**, `code`", "second line"]
    );
    assert_thought_style(&thought_lines, "**plain**, `code`");
    assert_thought_style(&thought_lines, "second line");
}

#[test]
fn assistant_thinking_blocks_render_in_order_including_unclosed_tail() {
    let item = assistant_text("before **bold**<thinking>\nfirst\n</thinking>between<think>\nlast");
    let lines = render_entry_lines(&item, false, false, false);
    assert_eq!(
        lines.iter().map(line_to_plain).collect::<Vec<_>>(),
        [
            "before bold",
            "Thinking",
            "first",
            "between",
            "Thinking",
            "last"
        ]
    );
    assert_thought_style(&lines, "first");
    assert_thought_style(&lines, "last");
}

#[test]
fn replayed_assistant_thought_uses_same_renderer() {
    use harnx_core::message::{Message, MessageContent, MessageRole};

    let text = "<thinking>\nreplayed reasoning\n</thinking>\n\nanswer **bold**";
    let messages = [Message::new(
        MessageRole::Assistant,
        MessageContent::Text(text.into()),
    )];
    let items = crate::lifecycle::messages_to_transcript_items(&messages, &Default::default());
    assert!(matches!(
        items.as_slice(),
        [TranscriptItem::AssistantText { .. }]
    ));
    let lines = render_entry_lines(&items[0], false, false, false);
    assert_eq!(
        lines,
        render_entry_lines(&assistant_text(text), false, false, false)
    );
    assert_thought_style(&lines, "replayed reasoning");
    assert!(rendered_span_matches(&lines, "bold", |style| {
        style.add_modifier.contains(Modifier::BOLD)
    }));
}

#[test]
fn segmented_assistant_preserves_markdown_tables_links_and_wrapped_height() {
    use crate::assistant_render::{render_assistant_text, render_thought_text};
    use crate::markdown_render::{render_markdown, MarkdownBlockData};
    use ratatui::style::Style;

    let thought = "a thought that wraps across several narrow terminal lines";
    let answer = "[answer](https://example.com)\n\n| a | b |\n|---|---|\n| 1 | 2 |";
    let width = 20;
    let rendered = render_assistant_text(&format!("<think>{thought}</think>{answer}"), width, None);
    let expected_thought = render_thought_text(thought, width);
    let expected_answer = render_markdown(answer, Style::default(), width, None);

    assert_eq!(
        rendered.total_height,
        expected_thought.total_height + expected_answer.total_height
    );
    assert!(expected_thought.total_height > 2);
    assert_eq!(
        rendered.blocks.len(),
        expected_thought.blocks.len() + expected_answer.blocks.len()
    );
    assert!(matches!(
        rendered.blocks.last(),
        Some(MarkdownBlockData::Table { .. })
    ));
    assert_eq!(rendered.links, expected_answer.links);
    assert_eq!(rendered.links[0].url, "https://example.com");
}
