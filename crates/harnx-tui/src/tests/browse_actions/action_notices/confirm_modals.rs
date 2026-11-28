//! Confirming the delete/rewind modals: idle confirms dispatch the mutation
//! command; a turn that started before Enter drops the mutation.

use super::*;

#[derive(Clone)]
enum ConfirmModalKind {
    Delete {
        from: usize,
        to: usize,
    },
    Rewind {
        seq: usize,
        user_text: Option<String>,
    },
}

struct ModalConfirmCase {
    description: &'static str,
    modal: ConfirmModalKind,
    agent_busy: bool,
    expected_commands: Vec<&'static str>,
    expected_notice: Option<&'static str>,
}

impl ModalConfirmCase {
    /// Idle confirmation case: Enter dispatches `expected_commands`.
    /// Chain `busy()` for the refusal variants.
    fn new(
        description: &'static str,
        modal: ConfirmModalKind,
        expected_commands: Vec<&'static str>,
    ) -> Self {
        ModalConfirmCase {
            description,
            modal,
            agent_busy: false,
            expected_commands,
            expected_notice: None,
        }
    }

    /// A turn started after the modal opened; Enter must refuse.
    fn busy(mut self) -> Self {
        self.agent_busy = true;
        self
    }

    /// The refusal must show this notice.
    fn notice(mut self, text: &'static str) -> Self {
        self.expected_notice = Some(text);
        self
    }
}

fn confirm_cases() -> Vec<ModalConfirmCase> {
    vec![
        ModalConfirmCase::new(
            "idle delete confirmation dispatches .delete message",
            ConfirmModalKind::Delete { from: 1, to: 1 },
            vec![".delete message 1"],
        ),
        ModalConfirmCase::new(
            "idle rewind confirmation dispatches .rewind",
            ConfirmModalKind::Rewind {
                seq: 1,
                user_text: Some("done".to_string()),
            },
            vec![".rewind 1"],
        ),
        ModalConfirmCase::new(
            "busy delete confirmation closes without dispatching",
            ConfirmModalKind::Delete { from: 1, to: 1 },
            vec![],
        )
        .busy()
        .notice(BUSY_NOTICE),
        ModalConfirmCase::new(
            "busy rewind confirmation closes without dispatching",
            ConfirmModalKind::Rewind {
                seq: 1,
                user_text: Some("done".to_string()),
            },
            vec![],
        )
        .busy()
        .notice(BUSY_NOTICE),
    ]
}

#[tokio::test]
async fn confirming_transcript_modals_dispatch_or_refuse_by_busyness() {
    // A turn can start between opening the confirmation and pressing Enter
    // (e.g. another client submitting to the shared session); the confirm
    // must drop the mutation instead of racing the worker.
    for case in confirm_cases() {
        confirm_modal_case(&case).await;
    }
}

async fn confirm_modal_case(case: &ModalConfirmCase) {
    let mut harness = browsing_harness(
        vec![RowSpec::User {
            text: "done",
            seq: Some(1),
        }],
        0,
        None,
    )
    .await;
    let commands = record_commands(&mut harness);
    let target = harness.tui().current_transcript_action_target();
    harness.tui().app.modal = Some(match &case.modal {
        ConfirmModalKind::Delete { from, to } => crate::types::ModalState::ConfirmDelete {
            from: *from,
            to: *to,
            target,
        },
        ConfirmModalKind::Rewind { seq, user_text } => crate::types::ModalState::ConfirmRewind {
            seq: *seq,
            user_text: user_text.clone(),
            target,
        },
    });
    harness.tui().app.llm_busy = case.agent_busy;

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        harness.tui().app.modal,
        None,
        "the modal must close: {}",
        case.description
    );
    assert_eq!(
        *commands.lock().unwrap(),
        case.expected_commands,
        "case: {}",
        case.description
    );
    assert_eq!(
        harness.tui().app.footer_notice_text(),
        case.expected_notice,
        "case: {}",
        case.description
    );
}
