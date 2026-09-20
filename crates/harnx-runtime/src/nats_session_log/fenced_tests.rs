use super::*;
use crate::nats_test_common::spawn_nats_server;
use harnx_core::message::{MessageContent, MessageRole};

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fenced_append_reports_the_cancel_that_moved_the_tail() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "fenced-session", 1);
    let tail = log.append_event_async(&user("go")).await.unwrap();
    // Another writer interrupts before the worker's next append.
    let cancel = SessionLogEntry::cancel_request("c-1".into(), "test".into());
    let cancel_seq = log.append_event_async(&cancel).await.unwrap();
    match log.append_fenced(&user("late"), tail, "m-1").await.unwrap() {
        FencedAppend::Conflict { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, cancel_seq);
            assert!(matches!(entries[0].1, SessionLogEntry::Cancel { .. }));
        }
        FencedAppend::Appended(seq) => panic!("append must conflict, got seq {seq}"),
    }
    // A correct tail appends.
    assert!(matches!(
        log.append_fenced(&user("ok"), cancel_seq, "m-2")
            .await
            .unwrap(),
        FencedAppend::Appended(_)
    ));
}
