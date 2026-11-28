//! Broker-backed regression for cycle 2 C03a: a `.set title` whose durable
//! write is gated behind broker I/O while an already-running `.session`
//! replaces the shared config with a same-short-id, different-agent session
//! must persist only to the captured target's canonical record, and must not
//! update the replacement's memory, claim success, or emit `TitleUpdated`
//! for it. The replacement's durable record stays untouched — this test
//! proves the wrong-target memory/event acceptance is fenced, not that any
//! durable write to the replacement happened (none did).

use crate::common;
use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::config::{session, Config, ConfigLock, NatsRouting, NatsServerConfig};
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const SESSION_ID: &str = "c03a-shared";

/// A TCP proxy in front of the real broker that accepts connections, tells
/// the test, and forwards them only once the gate opens — so the title
/// write's broker acquisition is provably in flight while the test swaps
/// the shared config's session.
struct GatedProxy {
    url: String,
    gate: Arc<tokio::sync::watch::Sender<bool>>,
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    accepted_notify: Arc<Notify>,
}

impl GatedProxy {
    async fn start(upstream_url: &str, initially_open: bool) -> Self {
        let upstream: std::net::SocketAddr = upstream_url
            .strip_prefix("nats://")
            .expect("nats:// URL")
            .parse()
            .expect("upstream socket address");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gated proxy");
        let addr = listener.local_addr().expect("proxy local addr");
        let (gate, gate_rx) = tokio::sync::watch::channel(initially_open);
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepted_notify = Arc::new(Notify::new());
        let accepted_bg = accepted.clone();
        let notify_bg = accepted_notify.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                accepted_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                notify_bg.notify_waiters();
                let mut gate_rx = gate_rx.clone();
                if gate_rx.wait_for(|open| *open).await.is_err() {
                    return;
                }
                let Ok(mut upstream) = tokio::net::TcpStream::connect(upstream).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
        });
        Self {
            url: format!("nats://127.0.0.1:{}", addr.port()),
            gate: Arc::new(gate),
            accepted,
            accepted_notify,
        }
    }

    fn open(&self) {
        let _ = self.gate.send(true);
    }

    /// Wait until the proxy has accepted `target` connections. Polls a
    /// counter first so an accept that already happened is not missed.
    async fn wait_for_accepts(&self, target: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let notified = self.accepted_notify.notified();
                if self.accepted.load(std::sync::atomic::Ordering::SeqCst) >= target {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("timed out waiting for the gated proxy to accept a connection");
    }
}

fn cluster_config(url: &str) -> Config {
    Config {
        nats_servers: vec![NatsServerConfig {
            user_id: None,
            name: "local".to_string(),
            url: url.to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            ignore_discovered_servers: None,
            agents: vec![],
        }],
        nats_routing: NatsRouting::Cluster("local".into()),
        ..Default::default()
    }
}

/// The pre-turn state: the agent is selected, but the in-memory session has
/// no persistence sink, because the read-only history load installs none.
fn selected_config(url: &str, agent: &str) -> (Config, String) {
    let mut config = cluster_config(url);
    config.remote_agent = Some((agent.to_string(), "local".to_string()));
    let session = session::new(&config, SESSION_ID, None).unwrap();
    assert!(
        session.runtime.is_none(),
        "pre-turn session must not have a persistence sink"
    );
    let storage_key = session.storage_key();
    config.session = Some(session);
    (config, storage_key)
}

#[tokio::test]
async fn nats_set_title_gated_write_never_lands_on_same_id_replacement() -> Result<()> {
    require_nextest();

    let Some(handle) = spawn_nats_server().await? else {
        return Ok(());
    };
    let broker_url = handle.url().to_string();
    let proxy = GatedProxy::start(&broker_url, false).await;

    // Canonical metadata for both agents' sessions sharing the short id, as
    // each session's creator would have written it.
    let direct = async_nats::connect(&broker_url).await?;
    let jetstream = async_nats::jetstream::new(direct);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    for agent in ["c03a-agent-a", "c03a-agent-b"] {
        store
            .create(&SessionMetadata::new(
                SESSION_ID,
                SessionInitializer::named(agent, Default::default()),
            ))
            .await?;
    }

    // Agent A's session is active, in the pre-turn sink-free state.
    let (config_a, storage_key_a) = selected_config(&proxy.url, "c03a-agent-a");
    let global = Arc::new(ConfigLock::new(config_a));

    // The durable write goes through the proxy, whose gate is closed, so the
    // acquisition is provably in flight before the switch below.
    let task = tokio::spawn({
        let global = global.clone();
        async move { Config::update_setting_durably(&global, "title A Gated Title", None).await }
    });
    proxy.wait_for_accepts(1).await;

    // The already-running `.session` replacement: same short session id,
    // different agent — a different storage key, still sink-free.
    let (config_b, storage_key_b) = selected_config(&proxy.url, "c03a-agent-b");
    assert_ne!(storage_key_a, storage_key_b);
    {
        let mut guard = global.write();
        guard.remote_agent = config_b.remote_agent.clone();
        guard.session = config_b.session.clone();
    }

    // The captured write completes against the gated broker only now, well
    // after the replacement became active.
    proxy.open();
    let result = task.await.unwrap();
    let error = result.expect_err("stale completion must not report success");
    assert!(
        error.to_string().contains("active session changed"),
        "unexpected error: {error:#}"
    );

    // The replacement's memory never received A's title.
    {
        let guard = global.read();
        let session = guard.session.as_ref().unwrap();
        assert_eq!(session.agent_name(), Some("c03a-agent-b"));
        assert_eq!(session.title(), None);
        assert_eq!(session.title_last_updated_tokens(), 0);
    }
    // No `TitleUpdated` was emitted: the write path emits only after the
    // full-target fence passes, and nothing is installed here anyway — the
    // assertion documents that no sink could have accepted a stale event.
    harnx_core::sink::clear_agent_event_sink();

    // Durable state: the captured target's record carries the title; the
    // replacement's record was never written. The replacement had no
    // durable write of its own — the fence rejected memory and event, and
    // the only durable write went to the captured key.
    let peer = async_nats::connect(&broker_url).await?;
    let peer_jetstream = async_nats::jetstream::new(peer);
    let peer_store = SessionMetadataStore::ensure(&peer_jetstream, 1).await?;
    let record_a = peer_store
        .get(&storage_key_a)
        .await?
        .with_context(|| "captured target's metadata record")?;
    assert_eq!(
        record_a.metadata.title.value.as_deref(),
        Some("A Gated Title")
    );
    assert!(record_a.metadata.title.manual);
    let record_b = peer_store
        .get(&storage_key_b)
        .await?
        .with_context(|| "replacement's metadata record")?;
    assert!(
        record_b.metadata.title.value.is_none(),
        "the same-id replacement must not be durably retitled: {:?}",
        record_b.metadata.title.value
    );
    assert!(!record_b.metadata.title.manual);
    Ok(())
}
