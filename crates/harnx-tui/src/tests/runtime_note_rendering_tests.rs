use super::{line_to_plain, render_entry_lines, rendered_span_matches};
use crate::types::{TranscriptItem, Tui};
use harnx_core::message::{Message, MessageContent, MessageRole};
use ratatui::style::{Color, Modifier};

/// Raw timing notice as it appears in a durable session log.
const TIMING_NOTE: &str = "[Runtime note] ⏱ v1 {\"version\":\"v1\",\
\"origin\":\"2026-11-12T20:40:32Z\",\"invocation_id\":\"ca615ba5\",\
\"coverage\":1,\"highest_elapsed_bucket\":35} | \
start: 2026-11-12T20:40:32Z, current: 2026-11-13T05:27:12Z, elapsed: 526m 40s, \
deadline: 2026-11-13T20:40:32Z, remaining: 913m 19s, budget: 1440m";

fn runtime_note(text: &str) -> TranscriptItem {
    let compact = harnx_core::runtime_note::compact_display(text).expect("runtime note");
    TranscriptItem::RuntimeNote {
        text: text.to_string(),
        compact,
        seq: None,
        timestamp: None,
    }
}

fn plain_lines(item: &TranscriptItem) -> Vec<String> {
    render_entry_lines(item, false, false, false)
        .iter()
        .map(line_to_plain)
        .collect()
}

#[test]
fn timing_note_renders_as_dim_compact_one_liner() {
    let item = runtime_note(TIMING_NOTE);
    let lines = render_entry_lines(&item, false, false, false);
    let expected = "⏱ 526m 40s elapsed · 913m 19s left of 1440m";
    assert_eq!(plain_lines(&item), vec![expected.to_string()]);
    // Dim dark gray, like system text — not the cyan bold "> " user style.
    assert!(rendered_span_matches(&lines, expected, |style| {
        style.fg == Some(Color::DarkGray) && style.add_modifier == Modifier::DIM
    }));
    insta::assert_snapshot!("runtime_note_timing_compact", expected);
}

#[test]
fn other_note_kinds_render_compact() {
    let sub_agent =
        runtime_note("[Runtime note] Started sub-agent 'pantheon/plato' in session child-direct.");
    assert_eq!(
        plain_lines(&sub_agent),
        vec!["Sub-agent 'pantheon/plato' started (session child-direct)".to_string()]
    );

    let cancel = runtime_note(
        "[Runtime note] The user interrupted this turn. Incomplete tool calls above were cancelled.",
    );
    assert_eq!(
        plain_lines(&cancel),
        vec!["Turn interrupted; incomplete tool calls cancelled".to_string()]
    );

    let summary =
        runtime_note("[Runtime note] Earlier conversation summary:\n\nPRIOR WORK SUMMARY body");
    assert_eq!(
        plain_lines(&summary),
        vec!["Earlier conversation summary".to_string()]
    );

    let unknown = runtime_note("[Runtime note] Some future note kind.");
    assert_eq!(
        plain_lines(&unknown),
        vec!["Some future note kind.".to_string()]
    );
}

#[test]
fn ordinary_user_text_keeps_cyan_prompt_style() {
    let item = TranscriptItem::UserText {
        text: "hello world".to_string(),
        seq: None,
        timestamp: None,
    };
    let lines = render_entry_lines(&item, false, false, false);
    // User entries pad a trailing blank line after the text.
    assert_eq!(
        plain_lines(&item),
        vec!["> hello world".to_string(), String::new()]
    );
    assert!(rendered_span_matches(&lines, "hello world", |style| {
        style.fg == Some(Color::Cyan) && style.add_modifier.contains(Modifier::BOLD)
    }));
}

#[test]
fn runtime_note_text_is_copyable_and_insertable() {
    // `c`/`i` (browse and detail view) all read through this helper; the
    // transcript shows only the compact line, so the raw note must come from
    // here.
    let item = runtime_note(TIMING_NOTE);
    let text = Tui::get_transcript_item_text(&item).expect("runtime note text");
    assert_eq!(text, TIMING_NOTE);
}

#[test]
fn detail_view_shows_full_raw_note_text() {
    let item = TranscriptItem::RuntimeNote {
        text: TIMING_NOTE.to_string(),
        compact: "⏱ 526m 40s elapsed · 913m 19s left of 1440m".to_string(),
        seq: Some(12),
        timestamp: None,
    };
    let plain = Tui::render_entry_detail(&item)
        .iter()
        .map(line_to_plain)
        .collect::<Vec<_>>();
    assert!(plain.contains(&"── runtime note ──".to_string()));
    assert!(plain.contains(&"seq: 12".to_string()));
    // The full raw note appears, not just the compact one-liner.
    assert!(plain
        .iter()
        .any(|line| line.contains("\"highest_elapsed_bucket\":35}")));
}

#[test]
fn history_replay_turns_runtime_notes_into_runtime_note_items() {
    let messages = vec![
        Message::new(MessageRole::User, MessageContent::Text(TIMING_NOTE.into()))
            .with_id(harnx_core::runtime_note::new_runtime_note_message_id()),
        Message::new(
            MessageRole::User,
            MessageContent::Text("ordinary prompt".into()),
        )
        .with_id("admitted-prompt-id"),
    ];
    let items = crate::lifecycle::messages_to_transcript_items(
        &messages,
        &std::collections::HashMap::new(),
    );
    assert!(matches!(
        items.first(),
        Some(TranscriptItem::RuntimeNote { text, compact, .. })
            if text == TIMING_NOTE && compact == "⏱ 526m 40s elapsed · 913m 19s left of 1440m"
    ));
    assert!(matches!(
        items.get(1),
        Some(TranscriptItem::UserText { text, .. }) if text == "ordinary prompt"
    ));
}

#[test]
fn lookalike_prompt_with_admitted_id_stays_user_text() {
    // A prompt whose text merely starts with the note prefix was admitted
    // under an ordinary id: history replay must keep normal user rendering
    // (full text, markdown links, edit/rewind affordances), not compact it.
    let lookalike =
        "[Runtime note] What is this syntax?\n\nPlease explain [docs](https://docs.example.com).";
    let messages = vec![
        Message::new(MessageRole::User, MessageContent::Text(lookalike.into()))
            .with_id("admitted-prompt-id"),
        // Same text under a reserved runtime-note id does compact.
        Message::new(MessageRole::User, MessageContent::Text(lookalike.into()))
            .with_id(harnx_core::runtime_note::new_runtime_note_message_id()),
        // Legacy id-less row falls back to content classification.
        Message::new(MessageRole::User, MessageContent::Text(lookalike.into())),
    ];
    let items = crate::lifecycle::messages_to_transcript_items(
        &messages,
        &std::collections::HashMap::new(),
    );
    assert!(matches!(
        items.first(),
        Some(TranscriptItem::UserText { text, .. }) if text == lookalike
    ));
    // The admitted lookalike keeps its markdown-link rows.
    assert!(matches!(
        items.get(1),
        Some(TranscriptItem::MarkdownLink { url, .. }) if url == "https://docs.example.com"
    ));
    // Reserved-id and id-less rows with the same text both compact.
    let runtime_notes: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::RuntimeNote { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(runtime_notes, vec![lookalike, lookalike]);
}

#[test]
fn live_user_event_projects_runtime_note() {
    let items = crate::agent_events::replayed_user_items(TIMING_NOTE.to_string());
    assert!(matches!(
        items.as_slice(),
        [TranscriptItem::RuntimeNote { text, compact, .. }]
            if text == TIMING_NOTE && compact == "⏱ 526m 40s elapsed · 913m 19s left of 1440m"
    ));

    let items = crate::agent_events::replayed_user_items("live prompt".to_string());
    assert!(matches!(
        items.as_slice(),
        [TranscriptItem::UserText { text, .. }] if text == "live prompt"
    ));
}
