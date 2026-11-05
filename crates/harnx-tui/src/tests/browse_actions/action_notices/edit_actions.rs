//! `e` (edit) action cases and the dispatch of `.edit message` commands.

use super::*;

fn edit_row_cases() -> Vec<BrowseActionCase> {
    vec![
        BrowseActionCase::new(
            "edit on an assistant message",
            vec![RowSpec::Assistant {
                text: "assistant prose",
                seq: Some(2),
            }],
            0,
            'e',
        )
        .notice("Only user messages can be edited"),
        BrowseActionCase::new(
            "edit on a single row without a seq",
            vec![RowSpec::Assistant {
                text: "no seq",
                seq: None,
            }],
            0,
            'e',
        )
        .notice("Can't edit this item"),
        BrowseActionCase::new(
            "edit while a turn is running is refused",
            vec![RowSpec::User {
                text: "done",
                seq: Some(1),
            }],
            0,
            'e',
        )
        // Editing mid-turn would rewrite history the worker still uses.
        .busy()
        .notice(BUSY_NOTICE),
    ]
}

fn edit_range_cases() -> Vec<BrowseActionCase> {
    vec![
        BrowseActionCase::new(
            "edit on a range without user messages",
            vec![
                RowSpec::Assistant {
                    text: "prose",
                    seq: Some(2),
                },
                RowSpec::ToolCall { seq: Some(3) },
            ],
            1,
            'e',
        )
        .range(0)
        .notice("Only user messages can be edited"),
        BrowseActionCase::new(
            "edit on a range with multiple user messages",
            vec![
                RowSpec::User {
                    text: "One",
                    seq: Some(1),
                },
                RowSpec::Assistant {
                    text: "Two",
                    seq: Some(2),
                },
                RowSpec::User {
                    text: "Three",
                    seq: Some(3),
                },
            ],
            2,
            'e',
        )
        .range(0)
        .notice("Edit supports a single user message at a time"),
    ]
}

fn edit_cases() -> Vec<BrowseActionCase> {
    let mut cases = edit_row_cases();
    cases.extend(edit_range_cases());
    cases
}

#[tokio::test]
async fn edit_action_outcomes() {
    for case in edit_cases() {
        assert_browse_action(&case).await;
    }
}

#[tokio::test]
async fn edit_range_with_single_user_message_and_tool_round_dispatches_edit() {
    // A range holding exactly one user message proceeds to the standard edit
    // flow: `.edit message <from>-<to>` rewrites only the user entry and
    // leaves the tool round untouched. The command seam makes the dispatch
    // observable (CodeRabbit review of PR #2622).
    let mut harness = browsing_harness(
        vec![
            RowSpec::User {
                text: "run it",
                seq: Some(2),
            },
            RowSpec::ToolCall { seq: Some(3) },
        ],
        1,
        Some(0),
    )
    .await;
    let commands = record_commands(&mut harness);

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        *commands.lock().unwrap(),
        vec![".edit message 2-3".to_string()],
        "single-user-message ranges dispatch the edit command"
    );
    // The edit flow exits browsing to hand the prompt to the editor.
    assert_eq!(harness.tui().app.transcript_focus, None);
}

#[tokio::test]
async fn edit_on_single_user_row_dispatches_edit_command() {
    let mut harness = browsing_harness(
        vec![RowSpec::User {
            text: "run it",
            seq: Some(2),
        }],
        0,
        None,
    )
    .await;
    let commands = record_commands(&mut harness);

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        *commands.lock().unwrap(),
        vec![".edit message 2".to_string()]
    );
    assert_eq!(harness.tui().app.transcript_focus, None);
}
