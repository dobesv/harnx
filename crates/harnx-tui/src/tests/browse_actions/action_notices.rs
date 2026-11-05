//! Outcomes of the browse action keys: refusal notices and confirmation
//! modals, table-driven per action. Shared case types, builder, runner, and
//! seam helpers live here; the per-action files are `edit_actions.rs`,
//! `confirm_modals.rs`, and `copy_insert.rs`.

use super::*;

pub(super) const BUSY_NOTICE: &str =
    "Busy: wait for the current turn to finish (or answer pending approval)";

enum ExpectedOutcome {
    Notice(&'static str),
    DeleteModal { from: usize, to: usize },
    RewindModal { seq: usize },
}

struct BrowseActionCase {
    description: &'static str,
    rows: Vec<RowSpec>,
    focus: usize,
    anchor: Option<usize>,
    key: char,
    agent_busy: bool,
    expected: ExpectedOutcome,
}

impl BrowseActionCase {
    /// Start a case: idle session, no selection anchor. Finish the chain
    /// with exactly one of `notice` / `delete_modal` / `rewind_modal`.
    fn new(description: &'static str, rows: Vec<RowSpec>, focus: usize, key: char) -> Self {
        BrowseActionCase {
            description,
            rows,
            focus,
            anchor: None,
            key,
            agent_busy: false,
            expected: ExpectedOutcome::Notice(""),
        }
    }

    /// Extend the selection to a range anchored at `anchor`.
    fn range(mut self, anchor: usize) -> Self {
        self.anchor = Some(anchor);
        self
    }

    /// The session has a turn running.
    fn busy(mut self) -> Self {
        self.agent_busy = true;
        self
    }

    /// The key must show this footer notice and keep the selection.
    fn notice(mut self, text: &'static str) -> Self {
        self.expected = ExpectedOutcome::Notice(text);
        self
    }

    /// The key must open the delete confirmation modal.
    fn delete_modal(mut self, from: usize, to: usize) -> Self {
        self.expected = ExpectedOutcome::DeleteModal { from, to };
        self
    }

    /// The key must open the rewind confirmation modal.
    fn rewind_modal(mut self, seq: usize) -> Self {
        self.expected = ExpectedOutcome::RewindModal { seq };
        self
    }
}

/// Shared runner: one browsing harness per case, press the key, assert the
/// outcome.
async fn assert_browse_action(case: &BrowseActionCase) {
    let mut harness = browsing_harness(case.rows.clone(), case.focus, case.anchor).await;
    harness.tui().app.llm_busy = case.agent_busy;

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char(case.key), KeyModifiers::NONE))
        .await
        .unwrap();

    match case.expected {
        ExpectedOutcome::Notice(notice) => {
            assert_eq!(
                harness.tui().app.footer_notice_text(),
                Some(notice),
                "case: {}",
                case.description
            );
            assert_eq!(harness.tui().app.modal, None, "case: {}", case.description);
            // Refusals keep the selection so the user can narrow it or
            // pick another action.
            assert_eq!(
                harness.tui().app.transcript_focus,
                Some(case.focus),
                "case: {}",
                case.description
            );
        }
        ExpectedOutcome::DeleteModal { from, to } => {
            assert_eq!(
                harness.tui().app.modal,
                Some(crate::types::ModalState::ConfirmDelete { from, to }),
                "case: {}",
                case.description
            );
        }
        ExpectedOutcome::RewindModal { seq } => {
            assert_eq!(
                harness.tui().app.modal,
                Some(crate::types::ModalState::ConfirmRewind {
                    seq,
                    user_text: None
                }),
                "case: {}",
                case.description
            );
        }
    }
}

/// Wire the command seam to record dispatched lines; returns the shared log.
pub(super) fn record_commands(
    harness: &mut TuiTestHarness,
) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = commands.clone();
    harness.tui().command_override = Some(std::sync::Arc::new(move |line| {
        recorded.lock().unwrap().push(line.to_string());
    }));
    commands
}

mod confirm_modals;
mod copy_insert;
mod edit_actions;

fn delete_cases() -> Vec<BrowseActionCase> {
    vec![
        BrowseActionCase::new(
            "range without seqs",
            vec![
                RowSpec::User {
                    text: "First",
                    seq: None,
                },
                RowSpec::Assistant {
                    text: "Second",
                    seq: None,
                },
            ],
            1,
            'd',
        )
        .range(0)
        .notice("Can't delete this selection"),
        BrowseActionCase::new(
            "single row without a seq",
            vec![RowSpec::Assistant {
                text: "banner",
                seq: None,
            }],
            0,
            'd',
        )
        .notice("Can't delete this item"),
        BrowseActionCase::new(
            "tool call with a seq opens the modal",
            vec![RowSpec::ToolCall { seq: Some(4) }],
            0,
            'd',
        )
        .delete_modal(4, 4),
        BrowseActionCase::new(
            "while a turn is running is refused",
            vec![RowSpec::User {
                text: "done",
                seq: Some(1),
            }],
            0,
            'd',
        )
        // Mutating history mid-turn races the worker appending results.
        .busy()
        .notice(BUSY_NOTICE),
    ]
}

#[tokio::test]
async fn delete_action_outcomes() {
    for case in delete_cases() {
        assert_browse_action(&case).await;
    }
}

fn rewind_cases() -> Vec<BrowseActionCase> {
    vec![
        BrowseActionCase::new(
            "rewind on a row without a seq",
            vec![RowSpec::User {
                text: "First",
                seq: None,
            }],
            0,
            'r',
        )
        .notice("Can't rewind to this item"),
        // Issue #2557: a tool round has a valid seq, so rewind opens the
        // confirmation modal; compute_rewind_point keeps the paired
        // tool-results.
        BrowseActionCase::new(
            "rewind on a tool call with a seq opens the modal",
            vec![RowSpec::ToolCall { seq: Some(4) }],
            0,
            'r',
        )
        .rewind_modal(4),
        BrowseActionCase::new(
            "rewind while a turn is running is refused",
            vec![RowSpec::User {
                text: "done",
                seq: Some(1),
            }],
            0,
            'r',
        )
        .busy()
        .notice(BUSY_NOTICE),
    ]
}

#[tokio::test]
async fn rewind_action_outcomes() {
    for case in rewind_cases() {
        assert_browse_action(&case).await;
    }
}

#[tokio::test]
async fn browse_shift_keys_extend_selection() {
    // The guide documents Shift+Up/Down for range selection while browsing;
    // browse mode must handle them, not discard them (CodeRabbit review of
    // PR #2622).
    let mut harness = browsing_harness(
        vec![
            RowSpec::User {
                text: "Alpha",
                seq: Some(1),
            },
            RowSpec::Assistant {
                text: "Beta",
                seq: Some(2),
            },
            RowSpec::User {
                text: "Gamma",
                seq: Some(3),
            },
        ],
        2,
        None,
    )
    .await;

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT))
        .await
        .unwrap();

    assert_eq!(
        harness.tui().app.transcript_selection_anchor,
        Some(2),
        "Shift+Up anchors the selection at the starting row"
    );
    assert_eq!(
        harness.tui().app.transcript_focus,
        Some(1),
        "Shift+Up moves the focus to the previous row"
    );

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT))
        .await
        .unwrap();

    assert_eq!(
        harness.tui().app.transcript_selection_anchor,
        Some(2),
        "the anchor stays put while the focus moves"
    );
    assert_eq!(harness.tui().app.transcript_focus, Some(2));
}
