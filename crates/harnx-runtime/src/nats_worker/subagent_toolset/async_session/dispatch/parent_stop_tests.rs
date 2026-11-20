//! A durable send outlives parent stops, including stops inside its inline wait.
use super::*;
use crate::nats_session::NatsSessionConfig;
use crate::nats_session_metadata::{
    AdmissionAuthority, InvocationAdmission, SessionInitializer, SessionOverrides,
};
use crate::nats_worker::subagent_toolset::async_session::observation;
use harnx_core::{
    message::{MessageContent, MessageRole},
    tool::ToolCall,
};
use harnx_toolset::{ToolInvocation, Toolset};
use tokio::sync::Notify;

const CHILD: &str = "detached-child";
const PROMPT: &str = "durable-dispatch";

struct Fixture {
    js: async_nats::jetstream::Context,
    toolset: SubagentToolset,
    parent: NatsSession,
    context: ToolInvocationContext,
    args: Value,
}

impl Fixture {
    async fn new(url: &str) -> Result<Self> {
        let js = async_nats::jetstream::new(async_nats::connect(url).await?);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        let (parent, limits) = seed_parent(&js).await?;
        let args = json!({"message": "detached work", "session_id": CHILD, "__harnx_tool_call_id": PROMPT});
        let context = journaled_context(
            &js,
            &limits,
            chrono::Utc::now(),
            &args,
            parent.storage_key(),
        )
        .await?;
        let toolset = SubagentToolset::new(
            "metis",
            SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
            SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
        );
        Ok(Self {
            js,
            toolset,
            parent,
            context,
            args,
        })
    }

    fn child_log(&self) -> NatsSessionLog {
        NatsSessionLog::new(
            self.js.clone(),
            harnx_core::session_identity::session_key(Some("metis"), CHILD),
        )
    }

    async fn cancel_observer_tool(&self, tool: &str) -> Result<()> {
        self.toolset
            .cancel(ToolInvocation {
                tool: tool.into(),
                args: self.args.clone(),
                cancel: CancellationToken::new(),
                context: ToolInvocationContext {
                    checkpoint: Some(json!({"session_id": CHILD})),
                    ..self.context.clone()
                },
            })
            .await?;
        Ok(())
    }

    async fn stop_parent(&self, stop: ParentStop) -> Result<()> {
        match stop {
            ParentStop::Cancel => {
                assert!(self
                    .parent
                    .interrupt("parent cancelled")
                    .await?
                    .cancel_seq()
                    .is_some());
            }
            ParentStop::PromptStop => {
                // A2A restart recovery fences its stop to the original prompt.
                assert!(self
                    .parent
                    .interrupt_prompt(1, "restart recovery")
                    .await?
                    .cancel_seq()
                    .is_some());
            }
            ParentStop::Error => {
                NatsSessionLog::new(self.js.clone(), self.parent.storage_key())
                    .append_event_async(&SessionLogEntry::Error {
                        message: "parent worker failed".into(),
                        fence_token: 1,
                        timestamp: None,
                    })
                    .await?;
            }
        }
        Ok(())
    }
}

fn message(id: &str, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.into()),
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn tool_round() -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            "missing_tool".into(),
            json!({}),
            Some("orphan".into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    }
}

async fn seed_parent(
    js: &async_nats::jetstream::Context,
) -> Result<(NatsSession, RunLimitsRecord)> {
    let parent = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            session_id: Some("detached-parent".into()),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            activation_route: crate::SessionActivationRoute::ClusterShared,
        },
        js.client().clone(),
        js.clone(),
        harnx_core::abort::create_abort_signal(),
    )
    .await?;
    let admission = InvocationAdmission::new(
        &AdmissionAuthority::External {
            admitted_at: chrono::Utc::now(),
        },
        "parent-invocation".into(),
        None,
        None,
    );
    let metadata = parent.metadata_store();
    let key = parent.storage_key();
    metadata.reserve_admission(key, &admission, &[]).await?;
    metadata
        .bind_prompt_admission(key, "parent-invocation", "parent-invocation")
        .await?;
    let limits = RunLimitsRecord::admit_root(
        admission.run_id,
        admission.invocation_id,
        admission.admitted_at,
        Default::default(),
        None,
        CallTimeoutOverride::Omitted,
    )?;
    metadata.put_run_limits(key, &limits).await?;
    metadata.put_invocation_limits(key, &limits).await?;
    let log = NatsSessionLog::new(js.clone(), key);
    assert_eq!(
        log.append_event_async(&message("parent-invocation", "delegate work"))
            .await?,
        1
    );
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            "metis_session_send".into(),
            json!({}),
            Some(PROMPT.into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    })
    .await?;
    Ok((parent, limits))
}

#[derive(Clone, Copy)]
enum ParentStop {
    Cancel,
    PromptStop,
    Error,
}

async fn dispatch_and_stop(f: &Fixture, inline_wait: bool, stop: ParentStop) -> Result<()> {
    let cancel = CancellationToken::new();
    if inline_wait {
        let entered = Arc::new(Notify::new());
        let mut args = f.args.clone();
        args["wait"] = json!({"forever": true});
        let send = observation::OBSERVATION_ENTERED.scope(
            entered.clone(),
            observation::send(&f.toolset, args, cancel.clone(), f.context.clone()),
        );
        tokio::pin!(send);
        tokio::time::timeout(Duration::from_secs(60), async {
            tokio::select! {
                result = &mut send => panic!("inline wait returned before parent stop: {result:?}"),
                _ = entered.notified() => {}
            }
        })
        .await
        .context("send never entered inline observation")?;
        f.stop_parent(stop).await?;
        f.cancel_observer_tool("session_send").await?;
        cancel.cancel();
        let result = send.await?;
        assert_eq!(
            result["terminal"], false,
            "observer cancellation isn't child settlement"
        );
    } else {
        let (receipt, _) = dispatch_send(
            &f.toolset,
            f.args.clone(),
            cancel.clone(),
            f.context.clone(),
        )
        .await?;
        assert_eq!(receipt.handle.prompt_id, PROMPT);
        f.stop_parent(stop).await?;
        f.cancel_observer_tool("session_send").await?;
        cancel.cancel();
    }
    f.cancel_observer_tool("session_wait").await?;
    let admission = f
        .toolset
        .session_metadata
        .admission(f.child_log().storage_key(), PROMPT)
        .await?
        .unwrap();
    assert!(
        admission.detached,
        "session_send must durably detach before append"
    );
    Ok(())
}

async fn exercise_parent_stop(resume: bool, inline_wait: bool, stop: ParentStop) -> Result<()> {
    let _env_guard = env_lock().await;
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let seeded = seed_remote_config(server.url());
    let _env = subagent_test_env(server.url(), &seeded);
    let f = Fixture::new(server.url()).await?;
    dispatch_and_stop(&f, inline_wait, stop).await?;
    let log = f.child_log();
    if resume {
        // Worker died after a durable tool round but before a terminal boundary.
        log.append_event_async(&tool_round()).await?;
        assert!(matches!(
            harnx_core::session_reconstruct::reconstruct_state_from_nats(
                &log.load_events_latest_async().await?
            )
            .turn_status,
            harnx_core::session_reconstruct::TurnStatus::InFlightResumable { .. }
        ));
    }
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let daemon = spawn_metis_worker_with_call_fn(
        server.url(),
        blocking_model(entered.clone(), release.clone()),
    );
    tokio::time::timeout(Duration::from_secs(60), entered.notified())
        .await
        .context("detached child refused initial input or worker resume")?;
    if !resume {
        // Queue input under the same admission while its first model call runs.
        // The next iteration must cross next_turn_input's ancestor check again.
        f.toolset
            .session_metadata
            .bind_prompt_admission(log.storage_key(), "steering", PROMPT)
            .await?;
        log.append_event_async(&message("steering", "follow-up work"))
            .await?;
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(60), entered.notified())
            .await
            .context("detached child's next turn refused after parent stop")?;
    }
    release.notify_one();
    let entries = terminal_entries(&log, 1).await?;
    daemon.abort();
    let _ = daemon.await;
    assert!(
        !entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
        "{entries:?}"
    );
    assert!(entries.iter().any(
        |(_, entry)| matches!(entry, SessionLogEntry::Message { role, .. } if role.is_assistant())
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn next_turn_survives_parent_cancellation_after_send() -> Result<()> {
    exercise_parent_stop(false, false, ParentStop::Cancel).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_resume_survives_parent_cancellation_after_send() -> Result<()> {
    exercise_parent_stop(true, false, ParentStop::Cancel).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn next_turn_survives_parent_prompt_stop_during_inline_wait() -> Result<()> {
    exercise_parent_stop(false, true, ParentStop::PromptStop).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_resume_survives_parent_prompt_stop_during_inline_wait() -> Result<()> {
    exercise_parent_stop(true, true, ParentStop::PromptStop).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_resume_survives_parent_error() -> Result<()> {
    exercise_parent_stop(true, false, ParentStop::Error).await
}

async fn freeze_expired_child(f: &Fixture) -> Result<()> {
    let log = f.child_log();
    let metadata = &f.toolset.session_metadata;
    let admission = metadata
        .admission(log.storage_key(), PROMPT)
        .await?
        .unwrap();
    assert!(admission.detached);
    // Model worker pickup after the inherited deadline, without wall-clock
    // sleeps that make admission race a one-second execution allowance.
    let mut parent = admission.parent.unwrap();
    parent.deadline = Some(chrono::Utc::now() - chrono::Duration::hours(1));
    let limits = RunLimitsRecord::admit_child(
        &parent,
        admission.invocation_id,
        InvocationEdgeKind::Delegation,
        admission.admitted_at,
        Default::default(),
        None,
        CallTimeoutOverride::Omitted,
    )?;
    assert_eq!(limits.deadline, parent.deadline);
    metadata
        .put_invocation_limits(log.storage_key(), &limits)
        .await?;
    Ok(())
}

async fn exercise_child_stop(expired: bool) -> Result<()> {
    let _env_guard = env_lock().await;
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let seeded = seed_remote_config(server.url());
    let _env = subagent_test_env(server.url(), &seeded);
    let f = Fixture::new(server.url()).await?;
    let (receipt, _) = dispatch_send(
        &f.toolset,
        f.args.clone(),
        CancellationToken::new(),
        f.context.clone(),
    )
    .await?;
    assert_eq!(receipt.handle.prompt_id, PROMPT);
    if expired {
        freeze_expired_child(&f).await?;
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let model = blocking_model(entered.clone(), Arc::new(Notify::new()));
    let call_fn: crate::agent_loop::AgentCallFn = Arc::new({
        let calls = calls.clone();
        move |config, input, abort| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            model(config, input, abort)
        }
    });
    let daemon = spawn_metis_worker_with_call_fn(server.url(), call_fn);
    if !expired {
        tokio::time::timeout(Duration::from_secs(60), entered.notified())
            .await
            .context("child never started before explicit cancellation")?;
        let result = f
            .toolset
            .session_cancel(
                json!({"session_id": CHILD}),
                ToolInvocationContext {
                    call_id: "explicit-stop".into(),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(result["outcome"], "accepted");
    }
    let entries = terminal_entries(&f.child_log(), 1).await?;
    daemon.abort();
    let _ = daemon.await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        usize::from(!expired)
    );
    assert!(
        entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
        "{entries:?}"
    );
    if expired {
        assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { cancellation_id: Some(id), .. } if id == &format!("deadline:{PROMPT}"))), "{entries:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_child_still_stops_on_explicit_session_cancel() -> Result<()> {
    exercise_child_stop(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_child_still_stops_at_inherited_deadline() -> Result<()> {
    exercise_child_stop(true).await
}
