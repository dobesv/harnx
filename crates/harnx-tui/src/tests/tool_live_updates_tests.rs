use super::*;
use crate::tool_render::{format_location, format_locations, tool_kind_icon, tool_status_icon};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{ToolKind, ToolLocation, ToolStatus};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[tokio::test]
async fn test_inplace_tool_call_update_sequence() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript.clear();

    // 1. Tool starts
    let started = AgentEvent::Tool(ToolEvent::Started {
        id: "call-1".to_string(),
        name: "read".to_string(),
        kind: ToolKind::Read,
        markdown: None,
        input: serde_json::json!({"path": "src/main.rs"}),
        locations: vec![ToolLocation {
            path: PathBuf::from("src/main.rs"),
            line: None,
        }],
    });
    tui.render_agent_event(started).await;

    assert_eq!(tui.app.transcript.len(), 1);
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            id,
            tool_name,
            kind,
            locations,
            title,
            final_elapsed_ms,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("call-1"));
            assert_eq!(tool_name, "read");
            assert!(matches!(*kind, Some(ToolKind::Read)));
            assert_eq!(locations.len(), 1);
            assert_eq!(title, &None);
            assert!(final_elapsed_ms.is_none());
        }
        other => panic!("expected ToolCall, got: {:?}", other),
    }

    // 2. First live progress update: title + in_progress status
    let update1 = AgentEvent::Tool(ToolEvent::Update {
        id: "call-1".to_string(),
        markdown: None,
        status: Some(ToolStatus::InProgress),
        content: None,
        title: Some("Reading src/main.rs".to_string()),
        kind: Some(ToolKind::Read),
        locations: Some(vec![ToolLocation {
            path: PathBuf::from("src/main.rs"),
            line: Some(10),
        }]),
        usage: Some(CompletionTokenUsage::new(Some(100), Some(20), None)),
    });
    tui.render_agent_event(update1).await;

    // Must update in place: NO new lines / status lines added to transcript!
    assert_eq!(
        tui.app.transcript.len(),
        1,
        "ToolEvent::Update must not append detached StatusLine items"
    );
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            id,
            title,
            status,
            locations,
            usage,
            final_elapsed_ms,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("call-1"));
            assert_eq!(title.as_deref(), Some("Reading src/main.rs"));
            assert!(matches!(*status, Some(ToolStatus::InProgress)));
            assert_eq!(locations.len(), 1);
            assert_eq!(locations[0].line, Some(10));
            assert_eq!(usage.as_ref().map(|u| u.input_tokens), Some(100));
            assert!(final_elapsed_ms.is_none());
        }
        other => panic!("expected ToolCall, got: {:?}", other),
    }

    // 3. Second live progress update: refined title + refined markdown
    let update2 = AgentEvent::Tool(ToolEvent::Update {
        id: "call-1".to_string(),
        markdown: Some("read **src/main.rs** (first 50 lines)".to_string()),
        status: Some(ToolStatus::InProgress),
        content: None,
        title: Some("Reading src/main.rs (50 lines)".to_string()),
        kind: None,
        locations: None,
        usage: None,
    });
    tui.render_agent_event(update2).await;

    assert_eq!(tui.app.transcript.len(), 1);
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            body,
            title,
            status,
            locations,
            ..
        } => {
            assert_eq!(title.as_deref(), Some("Reading src/main.rs (50 lines)"));
            assert!(matches!(*status, Some(ToolStatus::InProgress)));
            assert_eq!(locations.len(), 1); // Unchanged when patch.locations is None
            match body {
                Some(ToolCallBody::Markdown(m)) => {
                    assert_eq!(m, "read **src/main.rs** (first 50 lines)")
                }
                other => panic!("expected Markdown body, got {:?}", other),
            }
        }
        other => panic!("expected ToolCall, got: {:?}", other),
    }

    // 4. Tool completes
    let completed = AgentEvent::Tool(ToolEvent::Completed {
        id: "call-1".to_string(),
        output: serde_json::json!("file contents here"),
        markdown: None,
    });
    tui.render_agent_event(completed).await;

    // Transcript has the ToolCall (updated in place to completed) + result
    assert!(tui.app.transcript.len() >= 2);
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            status,
            final_elapsed_ms,
            title,
            ..
        } => {
            assert!(matches!(*status, Some(ToolStatus::Completed)));
            assert!(final_elapsed_ms.is_some(), "timer should be stopped");
            assert_eq!(title.as_deref(), Some("Reading src/main.rs (50 lines)"));
        }
        other => panic!("expected ToolCall, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_tool_update_fallback_missing_started_creates_minimal_row() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript.clear();

    // No Started event was received (e.g. dropped or out-of-order).
    // Send ToolEvent::Update directly.
    let update = AgentEvent::Tool(ToolEvent::Update {
        id: "orphan-call".to_string(),
        markdown: Some("executing command".to_string()),
        status: Some(ToolStatus::InProgress),
        content: None,
        title: Some("Running background build".to_string()),
        kind: Some(ToolKind::Execute),
        locations: Some(vec![ToolLocation {
            path: PathBuf::from("Cargo.toml"),
            line: None,
        }]),
        usage: None,
    });

    // Must not panic or crash!
    tui.render_agent_event(update).await;

    assert_eq!(
        tui.app.transcript.len(),
        1,
        "Fallback should create a minimal row"
    );
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            id,
            tool_name,
            title,
            status,
            kind,
            locations,
            final_elapsed_ms,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("orphan-call"));
            assert_eq!(tool_name, "tool");
            assert_eq!(title.as_deref(), Some("Running background build"));
            assert!(matches!(*status, Some(ToolStatus::InProgress)));
            assert!(matches!(*kind, Some(ToolKind::Execute)));
            assert_eq!(locations.len(), 1);
            assert!(final_elapsed_ms.is_none());
        }
        other => panic!("expected ToolCall, got {:?}", other),
    }

    // Subsequent update with the same ID updates this minimal row
    let update2 = AgentEvent::Tool(ToolEvent::Update {
        id: "orphan-call".to_string(),
        markdown: None,
        status: Some(ToolStatus::InProgress),
        content: None,
        title: Some("Running background build (step 2)".to_string()),
        kind: None,
        locations: None,
        usage: None,
    });
    tui.render_agent_event(update2).await;
    assert_eq!(tui.app.transcript.len(), 1);
    assert_eq!(
        match &tui.app.transcript[0] {
            TranscriptItem::ToolCall { title, .. } => title.as_deref(),
            _ => None,
        },
        Some("Running background build (step 2)")
    );

    // Completed correlates with this row
    let completed = AgentEvent::Tool(ToolEvent::Completed {
        id: "orphan-call".to_string(),
        output: serde_json::json!("done"),
        markdown: None,
    });
    tui.render_agent_event(completed).await;
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            final_elapsed_ms,
            status,
            ..
        } => {
            assert!(final_elapsed_ms.is_some());
            assert!(matches!(*status, Some(ToolStatus::Completed)));
        }
        other => panic!("expected ToolCall, got {:?}", other),
    }
}

#[tokio::test]
async fn test_tool_update_fallback_late_update_after_completed_ignored() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript.clear();

    let started = AgentEvent::Tool(ToolEvent::Started {
        id: "fast-call".to_string(),
        name: "quick_op".to_string(),
        kind: ToolKind::Other,
        markdown: None,
        input: serde_json::json!({}),
        locations: vec![],
    });
    tui.render_agent_event(started).await;

    let completed = AgentEvent::Tool(ToolEvent::Completed {
        id: "fast-call".to_string(),
        output: serde_json::json!("ok"),
        markdown: None,
    });
    tui.render_agent_event(completed).await;

    let transcript_len_before = tui.app.transcript.len();

    // Late update arriving after tool completion (D3: reject after completion)
    let late_update = AgentEvent::Tool(ToolEvent::Update {
        id: "fast-call".to_string(),
        markdown: None,
        status: Some(ToolStatus::InProgress),
        content: None,
        title: Some("Late progress update".to_string()),
        kind: None,
        locations: None,
        usage: None,
    });
    tui.render_agent_event(late_update).await;

    // Must be ignored: no new rows, and existing completed state not mutated
    assert_eq!(tui.app.transcript.len(), transcript_len_before);
    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall {
            final_elapsed_ms,
            title,
            ..
        } => {
            assert!(final_elapsed_ms.is_some(), "tool should remain completed");
            assert_eq!(title, &None, "late update must not overwrite completed row");
        }
        other => panic!("expected ToolCall, got {:?}", other),
    }
}

#[test]
fn test_locations_clearing_on_empty_vec() {
    let mut item = TranscriptItem::ToolCall {
        tool_name: "find".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("call-find".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: Some("Finding *.rs".to_string()),
        status: Some(ToolStatus::InProgress),
        kind: Some(ToolKind::Search),
        locations: vec![
            ToolLocation {
                path: PathBuf::from("a.rs"),
                line: None,
            },
            ToolLocation {
                path: PathBuf::from("b.rs"),
                line: None,
            },
        ],
        usage: None,
    };

    // Apply patch with Some(vec![]) - must clear locations per D3/D4
    let applied = item.apply_tool_update(None, None, None, None, Some(vec![]), None);
    assert!(applied);

    match item {
        TranscriptItem::ToolCall { locations, .. } => {
            assert!(locations.is_empty(), "Some(vec![]) must clear locations");
        }
        _ => panic!("expected ToolCall"),
    }
}

#[test]
fn test_patch_cannot_set_terminal_status() {
    let mut item = TranscriptItem::ToolCall {
        tool_name: "test".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("call-term".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: None,
        status: Some(ToolStatus::InProgress),
        kind: None,
        locations: vec![],
        usage: None,
    };

    // Attempt to set Completed via patch
    item.apply_tool_update(None, Some(ToolStatus::Completed), None, None, None, None);
    match &item {
        TranscriptItem::ToolCall { status, .. } => {
            assert!(
                matches!(*status, Some(ToolStatus::InProgress)),
                "patch cannot set Completed"
            );
        }
        _ => panic!("expected ToolCall"),
    }

    // Attempt to set Failed via patch
    item.apply_tool_update(None, Some(ToolStatus::Failed), None, None, None, None);
    match &item {
        TranscriptItem::ToolCall { status, .. } => {
            assert!(
                matches!(*status, Some(ToolStatus::InProgress)),
                "patch cannot set Failed"
            );
        }
        _ => panic!("expected ToolCall"),
    }
}

#[test]
fn test_tool_render_helpers() {
    assert_eq!(tool_kind_icon(ToolKind::Read), "⎘");
    assert_eq!(tool_kind_icon(ToolKind::Edit), "✎");
    assert_eq!(tool_kind_icon(ToolKind::Search), "⌕");
    assert_eq!(tool_kind_icon(ToolKind::Execute), "⚡");

    let (run_icon, run_color) = tool_status_icon(Some(ToolStatus::InProgress), true, 0);
    assert_eq!(run_icon, "⠋");
    assert_eq!(run_color, ratatui::style::Color::Yellow);

    let (comp_icon, comp_color) = tool_status_icon(Some(ToolStatus::Completed), false, 0);
    assert_eq!(comp_icon, "✓");
    assert_eq!(comp_color, ratatui::style::Color::Green);

    let (fail_icon, fail_color) = tool_status_icon(Some(ToolStatus::Failed), false, 0);
    assert_eq!(fail_icon, "✗");
    assert_eq!(fail_color, ratatui::style::Color::Red);

    let loc = ToolLocation {
        path: PathBuf::from("src/lib.rs"),
        line: Some(42),
    };
    assert_eq!(format_location(&loc), "src/lib.rs:42");

    let locs = vec![
        ToolLocation {
            path: PathBuf::from("a.rs"),
            line: None,
        },
        ToolLocation {
            path: PathBuf::from("b.rs"),
            line: None,
        },
        ToolLocation {
            path: PathBuf::from("c.rs"),
            line: None,
        },
    ];
    assert_eq!(format_locations(&locs), "a.rs, b.rs (+1 more)");
}

#[test]
fn test_render_tool_call_with_live_progress_row() {
    let item = TranscriptItem::ToolCall {
        tool_name: "read".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("call-1".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: Some("Reading src/lib.rs".to_string()),
        status: Some(ToolStatus::InProgress),
        kind: Some(ToolKind::Read),
        locations: vec![ToolLocation {
            path: PathBuf::from("src/lib.rs"),
            line: Some(10),
        }],
        usage: None,
    };

    let lines = render_entry_lines(&item, false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert_eq!(plain.len(), 1);
    let row = &plain[0];
    assert!(row.contains("⎘"), "must contain Read kind icon: {row}");
    assert!(row.contains("read"), "must contain tool name: {row}");
    assert!(
        row.contains("[in_progress]"),
        "must contain status badge: {row}"
    );
    assert!(
        row.contains("Reading src/lib.rs"),
        "must contain live title: {row}"
    );
}

#[test]
fn test_render_tool_call_completed_shows_check_icon() {
    let item = TranscriptItem::ToolCall {
        tool_name: "read".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("call-1".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: Some(250),
        rendered_cache: None,
        title: Some("Reading src/lib.rs".to_string()),
        status: Some(ToolStatus::Completed),
        kind: Some(ToolKind::Read),
        locations: vec![ToolLocation {
            path: PathBuf::from("src/lib.rs"),
            line: None,
        }],
        usage: None,
    };

    let lines = render_entry_lines(&item, false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert_eq!(plain.len(), 1);
    let row = &plain[0];
    assert!(row.contains("✓"), "must contain check icon: {row}");
    assert!(row.contains("⎘"), "must contain Read kind icon: {row}");
    assert!(
        row.contains("Reading src/lib.rs"),
        "must contain title: {row}"
    );
}

#[test]
fn test_render_tool_call_running_timer_preserved() {
    let item = TranscriptItem::ToolCall {
        tool_name: "read".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("call-timer".to_string()),
        start_anchor: Instant::now()
            .checked_sub(Duration::from_millis(6_000))
            .unwrap(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: Some("Reading huge file".to_string()),
        status: Some(ToolStatus::InProgress),
        kind: Some(ToolKind::Read),
        locations: vec![],
        usage: None,
    };

    let lines = render_entry_lines(&item, false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert_eq!(plain.len(), 1);
    let row = &plain[0];
    assert!(
        row.contains("(6s)"),
        "running timer should be preserved: {row}"
    );
}

#[test]
fn test_render_tool_call_final_timer_preserved() {
    let item = TranscriptItem::ToolCall {
        tool_name: "read".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("call-timer".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: Some(7_500),
        rendered_cache: None,
        title: Some("Reading huge file".to_string()),
        status: Some(ToolStatus::Completed),
        kind: Some(ToolKind::Read),
        locations: vec![],
        usage: None,
    };

    let lines = render_entry_lines(&item, false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert_eq!(plain.len(), 1);
    let row = &plain[0];
    assert!(
        row.contains("(7s)"),
        "final elapsed timer should be preserved: {row}"
    );
}

#[tokio::test]
async fn test_tool_call_inplace_updates_snapshot() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().app.transcript.clear();

    // 1. Started
    harness
        .tui()
        .render_agent_event(AgentEvent::Tool(ToolEvent::Started {
            id: "call-live-1".to_string(),
            name: "find".to_string(),
            kind: ToolKind::Search,
            markdown: None,
            input: serde_json::json!({"pattern": "**/*.rs"}),
            locations: vec![ToolLocation {
                path: PathBuf::from("crates/harnx-tui"),
                line: None,
            }],
        }))
        .await;

    // 2. Live update with title and discovered locations
    harness
        .tui()
        .render_agent_event(AgentEvent::Tool(ToolEvent::Update {
            id: "call-live-1".to_string(),
            markdown: None,
            status: Some(ToolStatus::InProgress),
            content: None,
            title: Some("Finding \"**/*.rs\" in crates/harnx-tui".to_string()),
            kind: Some(ToolKind::Search),
            locations: Some(vec![
                ToolLocation {
                    path: PathBuf::from("crates/harnx-tui/src/lib.rs"),
                    line: None,
                },
                ToolLocation {
                    path: PathBuf::from("crates/harnx-tui/src/main.rs"),
                    line: None,
                },
            ]),
            usage: None,
        }))
        .await;

    harness.render();
    let running_screen = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("tool_call_inplace_update_running", running_screen);

    // 3. Completed
    harness
        .tui()
        .render_agent_event(AgentEvent::Tool(ToolEvent::Completed {
            id: "call-live-1".to_string(),
            output: serde_json::json!("crates/harnx-tui/src/lib.rs\ncrates/harnx-tui/src/main.rs"),
            markdown: None,
        }))
        .await;

    harness.render();
    let completed_screen = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("tool_call_inplace_update_completed", completed_screen);
}

#[tokio::test]
async fn test_tool_call_completed_transitions_status_with_metadata_only() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript.clear();

    // 1. Tool starts without status or metadata
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Started {
        id: "meta-call-1".to_string(),
        name: "search".to_string(),
        kind: ToolKind::Search,
        markdown: None,
        input: serde_json::json!({}),
        locations: vec![],
    }))
    .await;

    // 2. Update with title only (status is None!)
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Update {
        id: "meta-call-1".to_string(),
        markdown: None,
        status: None,
        content: None,
        title: Some("Searching repository".to_string()),
        kind: None,
        locations: None,
        usage: None,
    }))
    .await;

    // 3. Completed: must transition to Completed (✓) per P-STATUS
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Completed {
        id: "meta-call-1".to_string(),
        output: serde_json::json!([]),
        markdown: None,
    }))
    .await;

    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall { status, .. } => {
            assert!(matches!(*status, Some(ToolStatus::Completed)));
        }
        other => panic!("expected ToolCall, got: {:?}", other),
    }

    let lines = render_entry_lines(&tui.app.transcript[0], false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert!(
        plain[0].contains("✓"),
        "must show check icon on completion: {}",
        plain[0]
    );
}

#[tokio::test]
async fn test_tool_call_failed_transitions_status_with_metadata_only() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript.clear();

    // 1. Tool starts without status or metadata
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Started {
        id: "meta-call-2".to_string(),
        name: "fetch".to_string(),
        kind: ToolKind::Fetch,
        markdown: None,
        input: serde_json::json!({}),
        locations: vec![],
    }))
    .await;

    // 2. Update with title only (status is None!)
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Update {
        id: "meta-call-2".to_string(),
        markdown: None,
        status: None,
        content: None,
        title: Some("Fetching remote url".to_string()),
        kind: None,
        locations: None,
        usage: None,
    }))
    .await;

    // 3. Failed: must transition to Failed (✗) per P-STATUS
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Failed {
        id: "meta-call-2".to_string(),
        error: "timeout".to_string(),
    }))
    .await;

    match &tui.app.transcript[0] {
        TranscriptItem::ToolCall { status, .. } => {
            assert!(matches!(*status, Some(ToolStatus::Failed)));
        }
        other => panic!("expected ToolCall, got: {:?}", other),
    }

    let lines = render_entry_lines(&tui.app.transcript[0], false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert!(
        plain[0].contains("✗"),
        "must show cross icon on failure: {}",
        plain[0]
    );
}

#[test]
fn test_tool_call_render_no_duplicate_title_when_name_equals_title() {
    let item = TranscriptItem::ToolCall {
        tool_name: "tool".to_string(),
        body: None,
        seq: None,
        timestamp: None,
        id: Some("dupe-1".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: Some("tool".to_string()),
        status: Some(ToolStatus::InProgress),
        kind: Some(ToolKind::Other),
        locations: vec![],
        usage: None,
    };

    let lines = render_entry_lines(&item, false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert_eq!(plain.len(), 1);
    // "tool" should appear only once (the tool name), not duplicated as title suffix
    let count = plain[0].matches("tool").count();
    assert_eq!(
        count, 1,
        "tool name should not be duplicated in header: {}",
        plain[0]
    );
}

#[test]
fn test_tool_call_timer_rendered_on_header_line_not_body() {
    let item = TranscriptItem::ToolCall {
        tool_name: "bash_exec".to_string(),
        body: Some(ToolCallBody::Yaml(
            "command: cargo test\nworking_dir: /app".to_string(),
        )),
        seq: None,
        timestamp: None,
        id: Some("timer-header-1".to_string()),
        start_anchor: Instant::now(),
        final_elapsed_ms: Some(12_000),
        rendered_cache: None,
        title: Some("Running tests".to_string()),
        status: Some(ToolStatus::Completed),
        kind: Some(ToolKind::Execute),
        locations: vec![],
        usage: None,
    };

    let lines = render_entry_lines(&item, false, false, false);
    let plain: Vec<String> = lines.iter().map(line_to_plain).collect();
    assert!(plain.len() >= 3);
    // Header line (line 0) must contain the timer
    assert!(
        plain[0].contains("(12s)"),
        "header line must contain the timer: {}",
        plain[0]
    );
    // Body lines must NOT contain the timer
    for body_line in &plain[1..] {
        assert!(
            !body_line.contains("(12s)"),
            "body line must not contain the timer: {}",
            body_line
        );
    }
}
