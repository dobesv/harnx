use super::super::{child_decision::ChildDecisionOutcome, InteractionEvent};
use super::*;
use crate::types::{MonitoredSessionKey, SubAgentView, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
use harnx_core::pending_interaction::ApprovalRef;
use harnx_runtime::interactions::InteractionDecisionOutcome;
use harnx_runtime::nats_session::HitlDecisionOutcome;

fn summary(legacy: bool) -> PendingInteractionSummary {
    PendingInteractionSummary {
        agent: "child-agent".into(),
        cluster: "child-cluster".into(),
        session_id: "child-session".into(),
        request: if legacy {
            PendingInteractionRequest::LegacyApproval {
                request: ApprovalRef {
                    invocation_id: "child-invocation".into(),
                    tool_round_seq: 10,
                    request_seq: 12,
                    tool_call_id: "reused-call".into(),
                },
            }
        } else {
            PendingInteractionRequest::Interaction {
                interaction_id: "confirm-id".into(),
                request_seq: 14,
                interaction_kind: "confirm".into(),
            }
        },
        tool_name: "bash_exec".into(),
        message: "Allow this child's command?".into(),
        pending_count: 2,
    }
}

fn row(summary: PendingInteractionSummary) -> TranscriptItem {
    TranscriptItem::SubAgentSession {
        key: MonitoredSessionKey {
            agent: summary.agent.clone(),
            session_id: summary.session_id.clone(),
            cluster: summary.cluster.clone(),
        },
        status: SubAgentStatus::Running,
        invocation_id: Some("parent-call".into()),
        progress: Some(SubAgentInvocationProgress::new(SubAgentProgress {
            invocation_id: "parent-call".into(),
            agent: summary.agent.clone(),
            session_id: summary.session_id.clone(),
            status: SubAgentProgressStatus::Running,
            pending_interaction: Some(summary),
            elapsed_ms: 1000,
            usage: Default::default(),
            tool_call_count: 1,
            title: None,
            tool_call_id: None,
        })),
    }
}

async fn tui(legacy: bool) -> Tui {
    let mut tui = Tui::init(&crate::tests::test_config()).await.unwrap();
    tui.app.modal = None;
    tui.app.transcript = vec![row(summary(legacy))];
    tui.app.transcript_focus = Some(0);
    tui.app.llm_busy = true;
    tui
}

fn modal_request(tui: &Tui) -> ChildRequest {
    let Some(ModalState::ConfirmInteraction(modal)) = &tui.app.modal else {
        panic!("missing child modal");
    };
    assert_eq!(modal.pending.kind, "confirm");
    assert!(modal
        .pending
        .message
        .contains("Allow this child's command?"));
    assert_eq!(modal.pending.pending_count, 2);
    modal.child.clone().unwrap()
}

fn pending(tui: &Tui) -> Option<&PendingInteractionSummary> {
    let TranscriptItem::SubAgentSession {
        status, progress, ..
    } = &tui.app.transcript[0]
    else {
        panic!("missing row");
    };
    pending_for_progress(status, progress.as_ref())
}

#[tokio::test]
async fn row_key_captures_child_target_and_full_legacy_or_confirm_identity() {
    for legacy in [true, false] {
        let mut tui = tui(legacy).await;
        tui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await
            .unwrap();
        let request = modal_request(&tui);
        assert_eq!(request.summary, summary(legacy));
        assert_eq!(
            request.target(),
            (
                harnx_core::session_identity::session_key(Some("child-agent"), "child-session"),
                "child-cluster".into()
            )
        );
        assert!(tui.app.llm_busy, "parent remains running");
        assert!(
            tui.app.pending_confirm_reply.is_none(),
            "never uses foreground legacy bool reply"
        );
        // Foreground snapshots must not replace or dismiss the child modal.
        tui.handle_interaction_event(InteractionEvent::Snapshot {
            generation: tui.interactions.generation,
            pending: Ok(None),
        });
        assert!(modal_request(&tui).same_request(&request));
    }
}

#[tokio::test]
async fn child_modal_queues_behind_foreground_and_drops_obsolete_generations() {
    let mut tui = tui(true).await;
    tui.app.modal = Some(ModalState::ConfirmDelete { from: 0, to: 1 });
    assert!(tui.answer_selected_child());
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { .. })
    ));
    assert_eq!(tui.interactions.children.queue.len(), 1);
    assert!(tui.answer_selected_child());
    assert_eq!(
        tui.interactions.children.queue.len(),
        1,
        "queue deduplicates exact identity"
    );
    tui.app.modal = None;
    tui.present_child_confirmation();
    let request = modal_request(&tui);
    assert_eq!(request.summary, summary(true));
    tui.app.modal = Some(ModalState::ConfirmDelete { from: 0, to: 1 });
    tui.queue_child_confirmation(request);
    tui.app.transcript[0] = row(summary(false));
    tui.app.modal = None;
    tui.present_child_confirmation();
    assert!(
        tui.app.modal.is_none(),
        "never opens queued superseded generation"
    );
}

#[tokio::test]
async fn detail_key_answers_direct_child_header_but_never_grandchild() {
    let mut tui = tui(false).await;
    assert!(tui.open_focused_root_subagent());
    tui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(modal_request(&tui).summary, summary(false));
    tui.app.modal = None;
    let view: SubAgentView = tui.app.subagent_view_stack[0].clone();
    tui.app.subagent_view_stack.push(view);
    assert!(!tui.answer_selected_child());
    assert!(tui.app.modal.is_none());
}

#[tokio::test]
async fn unsupported_interaction_and_terminal_row_have_no_answer_modal() {
    let mut tui = tui(false).await;
    let mut unsupported = summary(false);
    if let PendingInteractionRequest::Interaction {
        interaction_kind, ..
    } = &mut unsupported.request
    {
        *interaction_kind = "form".into();
    }
    tui.app.transcript[0] = row(unsupported);
    assert!(tui.answer_selected_child());
    assert!(tui.app.modal.is_none());
    assert!(
        pending(&tui).is_some(),
        "unsupported message remains visible"
    );
    if let TranscriptItem::SubAgentSession { status, .. } = &mut tui.app.transcript[0] {
        *status = SubAgentStatus::Completed;
    }
    assert!(!tui.answer_selected_child());
    assert!(pending(&tui).is_none());
}

#[tokio::test]
async fn submit_uses_captured_child_request_and_preserves_note_on_error() {
    for legacy in [true, false] {
        let mut tui = tui(legacy).await;
        tui.answer_selected_child();
        let expected = modal_request(&tui);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = captured.clone();
        tui.interactions.children.decision_override =
            Some(std::sync::Arc::new(move |request, approved, note| {
                calls.lock().unwrap().push((request, approved, note));
                Box::pin(async { Err("routing failed".into()) })
            }));
        if let Some(ModalState::ConfirmInteraction(state)) = tui.app.modal.as_mut() {
            state.note.insert_str("audit note");
        }
        tui.submit_interaction(true);
        tui.submit_interaction(false); // duplicate submit cannot send another answer
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), tui.event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let TuiEvent::Interaction(event) = event else {
            panic!("missing decision event");
        };
        tui.handle_interaction_event(event);
        assert_eq!(
            *captured.lock().unwrap(),
            vec![(expected, true, "audit note".into())]
        );
        let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
            panic!("retry modal missing");
        };
        assert_eq!(state.note.lines().join("\n"), "audit note");
        assert!(state.error.as_ref().unwrap().contains("routing failed"));
        assert!(!state.submitting);
        assert!(pending(&tui).is_some());
    }
}

#[tokio::test]
async fn outcomes_are_visible_and_clear_only_the_displayed_generation() {
    let cases = [
        (
            ChildDecisionOutcome::Legacy(HitlDecisionOutcome::Applied),
            "response applied: approved",
        ),
        (
            ChildDecisionOutcome::Legacy(HitlDecisionOutcome::AlreadyApplied {
                approved: true,
                note: None,
            }),
            "already applied: approved",
        ),
        (
            ChildDecisionOutcome::Legacy(HitlDecisionOutcome::AlreadyApplied {
                approved: false,
                note: Some("recorded note".into()),
            }),
            "answered differently elsewhere: denied (recorded note)",
        ),
        (
            ChildDecisionOutcome::Legacy(HitlDecisionOutcome::Conflicting),
            "answered differently elsewhere: denied",
        ),
        (
            ChildDecisionOutcome::Legacy(HitlDecisionOutcome::Stale),
            "no longer pending",
        ),
        (
            ChildDecisionOutcome::Middleware(InteractionDecisionOutcome::Applied),
            "response applied",
        ),
        (
            ChildDecisionOutcome::Middleware(InteractionDecisionOutcome::AlreadyApplied),
            "already applied",
        ),
        (
            ChildDecisionOutcome::Middleware(InteractionDecisionOutcome::Conflicting),
            "recorded answer unavailable",
        ),
        (
            ChildDecisionOutcome::Middleware(InteractionDecisionOutcome::Stale),
            "no longer pending",
        ),
        (
            ChildDecisionOutcome::Middleware(InteractionDecisionOutcome::WrongSequence),
            "no longer pending",
        ),
        (
            ChildDecisionOutcome::Middleware(InteractionDecisionOutcome::AfterComplete),
            "no longer pending",
        ),
    ];
    for (outcome, expected) in cases {
        let mut tui = tui(true).await;
        tui.answer_selected_child();
        let request = modal_request(&tui);
        tui.finish_child_decision(request, true, Ok(outcome));
        assert!(tui.app.modal.is_none());
        assert!(pending(&tui).is_none());
        assert!(
            tui.app.transcript.iter().any(
                |item| matches!(item, TranscriptItem::StatusLine(text) if text.contains(expected))
            ),
            "missing notice {expected}"
        );
        assert!(tui.app.llm_busy);
    }
}

#[tokio::test]
async fn late_response_cannot_clear_new_request_even_with_reused_tool_id() {
    let mut tui = tui(true).await;
    tui.answer_selected_child();
    let old = modal_request(&tui);
    let mut newer = summary(true);
    if let PendingInteractionRequest::LegacyApproval { request } = &mut newer.request {
        request.tool_round_seq = 20;
        request.request_seq = 22;
    }
    tui.app.transcript[0] = row(newer.clone());
    tui.app.modal = None;
    tui.answer_selected_child();
    tui.finish_child_decision(
        old,
        true,
        Ok(ChildDecisionOutcome::Legacy(HitlDecisionOutcome::Applied)),
    );
    assert_eq!(modal_request(&tui).summary, newer);
    assert_eq!(pending(&tui), Some(&newer));
}

#[tokio::test]
async fn lease_loss_retains_retryable_child_modal() {
    let mut tui = tui(false).await;
    tui.answer_selected_child();
    let request = modal_request(&tui);
    tui.finish_child_decision(
        request,
        false,
        Ok(ChildDecisionOutcome::Middleware(
            InteractionDecisionOutcome::LeaseLost,
        )),
    );
    assert!(pending(&tui).is_some());
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("retry modal missing");
    };
    assert!(state
        .error
        .as_ref()
        .unwrap()
        .contains("worker ownership changed"));
}

#[test]
fn awaiting_marker_message_and_action_render_only_for_nonterminal_rows() {
    for legacy in [true, false] {
        let mut item = row(summary(legacy));
        let rendered = crate::subagent_render::render_subagent_row(&item, 0, 120);
        let text = rendered
            .blocks
            .iter()
            .filter_map(|block| {
                if let crate::markdown_render::MarkdownBlockData::Paragraph { lines, .. } = block {
                    Some(
                        lines
                            .iter()
                            .flat_map(|line| line.spans.iter())
                            .map(|span| span.content.as_ref())
                            .collect::<String>(),
                    )
                } else {
                    None
                }
            })
            .collect::<String>();
        assert!(text.contains("⏸ Awaiting input:"));
        assert!(text.contains("Allow this child's command?"));
        assert!(text.contains("a/answer"));
        let detail = crate::subagent_render::render_subagent_detail(&item)
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(detail.contains("Awaiting input"));
        if let TranscriptItem::SubAgentSession { status, .. } = &mut item {
            *status = SubAgentStatus::Completed;
        }
        let detail = crate::subagent_render::render_subagent_detail(&item)
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(!detail.contains("Awaiting input"));
        if let TranscriptItem::SubAgentSession {
            status,
            progress: Some(progress),
            ..
        } = &mut item
        {
            *status = SubAgentStatus::Running;
            progress.snapshot.status = SubAgentProgressStatus::Done;
        }
        let detail = crate::subagent_render::render_subagent_detail(&item)
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(
            !detail.contains("Awaiting input"),
            "terminal progress wins over row status"
        );
    }
}

#[tokio::test]
async fn child_drilldown_does_not_render_grandchild_pending_requests() {
    let mut tui = tui(false).await;
    tui.open_focused_root_subagent();
    let child_view = tui.app.subagent_view_stack[0].clone();
    let mut grandchild = summary(false);
    grandchild.agent = "grandchild-agent".into();
    grandchild.session_id = "grandchild-session".into();
    grandchild.message = "Grandchild pending message must stay hidden".into();
    let grandchild_row = row(grandchild);
    let TranscriptItem::SubAgentSession {
        key,
        status,
        progress,
        ..
    } = grandchild_row.clone()
    else {
        unreachable!();
    };
    let grandchild_view = SubAgentView {
        key,
        status,
        progress,
    };
    let mut child_state = crate::types::MonitoredSessionState::new(SubAgentStatus::Running);
    child_state.transcript.push(grandchild_row);
    tui.app
        .monitored_sessions
        .insert(child_view.key, child_state);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
    terminal
        .draw(|frame| {
            tui.render_exclusive_transcript(frame, frame.area());
        })
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Allow this child's command?"));
    assert!(text.contains("grandchild-agent"));
    assert!(!text.contains("Grandchild pending message"));
    assert_eq!(text.matches("Awaiting input").count(), 1);
    tui.app.monitored_sessions.insert(
        grandchild_view.key.clone(),
        crate::types::MonitoredSessionState::new(SubAgentStatus::Running),
    );
    tui.app.subagent_view_stack.push(grandchild_view);
    terminal
        .draw(|frame| {
            tui.render_exclusive_transcript(frame, frame.area());
        })
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!text.contains("Grandchild pending message"));
    assert!(!text.contains("Awaiting input"));
}

#[tokio::test]
async fn child_modal_ctrl_c_interrupts_child_without_changing_parent_route_or_busy_state() {
    let mut tui = tui(true).await;
    let parent = (
        "parent-storage-key".to_string(),
        "parent-cluster".to_string(),
    );
    tui.active_remote_session = Some(parent.clone());
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = captured.clone();
    tui.exit_cancel_factory = std::sync::Arc::new(move |_, _, session, cluster| {
        calls.lock().unwrap().push((session, cluster));
        Box::pin(async { Ok(harnx_runtime::nats_session::InterruptOutcome::Idle) })
    });
    tui.answer_selected_child();
    let target = modal_request(&tui).target();
    tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(tui.app.modal.is_none());
    let outcome = tui
        .pending_exit_cancel
        .take()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    tui.monitor_interrupt(outcome);
    assert_eq!(*captured.lock().unwrap(), vec![target]);
    assert_eq!(tui.active_remote_session, Some(parent));
    assert!(tui.app.llm_busy);
    assert!(!tui.app.should_quit);
}

#[tokio::test]
async fn foreground_reply_and_read_errors_do_not_contaminate_child_modal() {
    let mut tui = tui(false).await;
    tui.answer_selected_child();
    let request = modal_request(&tui);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        unreachable!();
    };
    let key = state.pending.key.clone();
    tui.finish_interaction_decision(
        tui.interactions.generation,
        ("parent".into(), "parent-cluster".into()),
        key,
        Ok(InteractionDecisionOutcome::Applied),
    );
    assert!(modal_request(&tui).same_request(&request));
    tui.interactions.read_error = Some("foreground-only refresh error".into());
    let Some(ModalState::ConfirmInteraction(mut state)) = tui.app.modal.clone() else {
        unreachable!();
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
    terminal
        .draw(|frame| {
            tui.render_interaction_modal(frame, frame.area(), &mut state);
        })
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Child confirmation"));
    assert!(text.contains("interrupt child"));
    assert!(!text.contains("foreground-only refresh error"));
}
