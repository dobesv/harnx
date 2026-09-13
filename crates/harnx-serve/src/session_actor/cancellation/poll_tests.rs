use super::*;
use tokio::sync::{oneshot, Notify};

#[tokio::test(start_paused = true)]
async fn pending_cancellation_read_does_not_block_actor_mailbox() {
    let config = SessionActorConfig {
        base_config: Config::default(),
        call_fn: None,
        local_worker: Arc::new(Mutex::new(None)),
    };
    let (actor, handle) = make_session_actor(
        SessionKey {
            agent: "plain".into(),
            session: "mailbox".into(),
        },
        Arc::new(DashMap::new()),
        Duration::from_secs(5),
        config,
    );
    let mut events = actor.broadcast_tx.subscribe();
    let started = Arc::new(Notify::new());
    let read_started = started.clone();
    let poller = CancellationPoller::new(move || {
        let started = read_started.clone();
        async move {
            started.notify_one();
            std::future::pending().await
        }
        .boxed()
    });
    let actor_task = AbortOnDropHandle::new(tokio::spawn(actor.run(poller)));
    started.notified().await;

    handle
        .tx
        .send(SessionCommand::EmitTestEvent {
            event: Event::Custom(ag_ui_core::event::CustomEvent {
                base: base_event(),
                name: "mailbox-responsive".into(),
                value: serde_json::Value::Null,
            }),
        })
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("mailbox must respond before the two-second read timeout")
        .unwrap();
    assert!(matches!(event, Event::Custom(event) if event.name == "mailbox-responsive"));
    drop(actor_task);
}

#[tokio::test(start_paused = true)]
async fn invalidation_retains_read_but_discards_its_stale_result() {
    let (reply_tx, reply_rx) = oneshot::channel();
    let mut first_read = Some(reply_rx);
    let mut poller = CancellationPoller::new(move || {
        let reply = first_read.take();
        async move {
            match reply {
                Some(reply) => reply.await.unwrap(),
                None => Ok(None),
            }
        }
        .boxed()
    });
    assert!(poller.next().now_or_never().is_none());
    poller.invalidate();
    reply_tx
        .send(Err(anyhow::anyhow!("old read failed")))
        .expect("a command must not drop in-flight reconciliation");
    assert!(
        poller.next().await.is_none(),
        "stale failure must not demote newer cancellation state"
    );
    assert!(matches!(poller.next().await, Some(Ok(None))));
}

#[tokio::test(start_paused = true)]
async fn read_releases_shared_client_lock_while_actor_handles_command() {
    let client_lock = Arc::new(Mutex::new(()));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut poller = CancellationPoller::new({
        let client_lock = client_lock.clone();
        let started = started.clone();
        let release = release.clone();
        move || {
            let client_lock = client_lock.clone();
            let started = started.clone();
            let release = release.clone();
            async move {
                let _guard = client_lock.lock().await;
                started.notify_one();
                release.notified().await;
                Ok(None)
            }
            .boxed()
        }
    });
    assert!(poller.next().now_or_never().is_none());
    started.notified().await;
    release.notify_one();
    // A command needs the same client while the actor is not polling next().
    let guard = tokio::time::timeout(Duration::from_secs(1), client_lock.lock())
        .await
        .expect("the read must progress independently of the actor loop");
    drop(guard);
    assert!(matches!(poller.next().await, Some(Ok(None))));
}
