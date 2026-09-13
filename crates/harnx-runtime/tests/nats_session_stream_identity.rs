mod common;

use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session_log::NatsSessionLog;

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
        js.get_stream("SESSION_aqCV1g")
            .await?
            .cached_info()
            .state
            .messages,
        2
    );
    Ok(())
}
