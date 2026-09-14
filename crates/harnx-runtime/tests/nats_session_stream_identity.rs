mod common;

use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session_log::{stream_name_for_session, NatsSessionLog};

#[test]
fn session_stream_names_stay_distinct_on_case_insensitive_filesystems() {
    let first = stream_name_for_session("aqCV1g");
    let second = stream_name_for_session("aqcV1g");
    assert_ne!(first.to_ascii_lowercase(), second.to_ascii_lowercase());
}

#[test]
fn session_stream_names_are_bounded_and_do_not_sanitize_distinct_ids_together() {
    assert_ne!(
        stream_name_for_session("with.dot"),
        stream_name_for_session("with_dot")
    );
    let name = stream_name_for_session(&"a".repeat(200));
    let encoded = name.strip_prefix("SESSION_").unwrap();
    assert_eq!(encoded.len(), 64);
    assert!(encoded
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
}

#[tokio::test]
async fn case_distinct_sessions_have_independent_transcripts() -> Result<()> {
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let old = NatsSessionLog::new(js.clone(), "aqCV1g");
    old.append_event_async(&SessionLogEntry::Cancel { fence_token: 11 })
        .await?;
    let new = NatsSessionLog::new(js.clone(), "aqcV1g");
    assert!(new.load_events_async().await?.is_empty());
    new.append_event_async(&SessionLogEntry::Cancel { fence_token: 22 })
        .await?;
    old.append_event_async(&SessionLogEntry::Cancel { fence_token: 33 })
        .await?;
    let old_entries = old.load_events_async().await?;
    assert_eq!(old_entries.len(), 2);
    assert!(matches!(
        old_entries[1].1,
        SessionLogEntry::Cancel { fence_token: 33 }
    ));
    let new_entries = new.load_events_async().await?;
    assert_eq!(new_entries.len(), 1);
    assert!(matches!(
        new_entries[0].1,
        SessionLogEntry::Cancel { fence_token: 22 }
    ));
    assert_eq!(
        js.get_stream(stream_name_for_session("aqCV1g"))
            .await?
            .cached_info()
            .state
            .messages,
        2
    );
    Ok(())
}
