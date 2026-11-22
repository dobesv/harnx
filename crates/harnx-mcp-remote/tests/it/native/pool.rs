//! Pool lifecycle and upstream secret-boundary regressions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use harnx_credentials::Principal;
use harnx_mcp_remote::native::client_handler::PoolClientHandler;
use harnx_mcp_remote::native::pool::{
    list_all_tools, HttpUpstreamConnector, StreamableHttpClientTransportConfig, TokenAuthFactory,
    UpstreamAuth, UpstreamAuthFactory, UpstreamConnection, UpstreamConnector, UpstreamPool,
};
use harnx_toolset::ToolInvokeError;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

use super::fixture::*;
use crate::native_common::*;

/// A token pool against the mock upstream, mirroring the connector the
/// native server builds.
fn token_pool(url: &str, strict_session: bool) -> UpstreamPool {
    let http = reqwest::Client::builder().build().expect("http client");
    let (pool, _list_changed) = UpstreamPool::new(
        Arc::new(HttpUpstreamConnector::new(url, http, strict_session)),
        Arc::new(TokenAuthFactory {
            header: reqwest::header::HeaderName::from_static("authorization"),
            scheme: None,
        }),
    );
    let _ = _list_changed;
    pool
}

/// The full anyhow chain, the shape the tool server would surface.
fn error_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

/// Poll until the session-mode upstream's live session count reaches `want`.
async fn wait_for_live_sessions(
    manager: &Arc<rmcp::transport::streamable_http_server::session::local::LocalSessionManager>,
    want: usize,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let live = manager.sessions.read().await.len();
        if live == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {want} live sessions, found {live}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Evicted pool entries must terminate their upstream session, not just be
/// dropped from the table. A session-mode upstream makes the client's
/// shutdown DELETE observable in the server's session table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evicted_pool_entries_terminate_upstream_sessions() {
    harnx_core::require_nextest();
    let (upstream, manager) = spawn_session_upstream("authorization")
        .await
        .expect("session upstream");
    let mut pool = token_pool(&upstream.url, true);
    pool.set_timeouts(
        Duration::from_millis(150),
        Duration::from_secs(10),
        Duration::from_secs(30),
    );

    let alice = Principal::User("alice".into());
    let cred_v1 = token_record("conn-1", ALICE_TOKEN);
    let _first = pool
        .client(alice.clone(), &cred_v1)
        .await
        .expect("first connect");
    wait_for_live_sessions(&manager, 1).await;

    // Reconnect with a fresh connection id: the old entry is evicted, and
    // the upstream must see the old session end.
    let cred_v2 = token_record("conn-2", ALICE_TOKEN);
    let _second = pool
        .client(alice.clone(), &cred_v2)
        .await
        .expect("reconnect");
    // Transiently two sessions (the new one is created before the old entry
    // is dropped); the eviction must bring it back to exactly the new one.
    wait_for_live_sessions(&manager, 1).await;

    // Idle expiry: the aged-out entry's session must end too when a later
    // client() call sweeps it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let bob = Principal::User("bob".into());
    let _bob = pool
        .client(bob, &token_record("conn-b", BOB_TOKEN))
        .await
        .expect("bob connects");
    wait_for_live_sessions(&manager, 1).await;

    // Drop the principal: the final removal path also terminates.
    pool.drop_principal(&alice).await;
    pool.drop_principal(&Principal::User("bob".into())).await;
    wait_for_live_sessions(&manager, 0).await;

    let _ = upstream.shutdown().await;
}

/// A stalled upstream handshake for one principal must not block every
/// other principal's cold connect. The pool's shared table lock is never
/// held across the handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_upstream_connect_does_not_block_other_principals() {
    harnx_core::require_nextest();
    let gate = HandshakeGate::new();
    let upstream = spawn_gated_upstream("authorization", gate.clone())
        .await
        .expect("gated upstream");
    let mut pool = token_pool(&upstream.url, false);
    pool.set_timeouts(
        Duration::from_secs(600),
        Duration::from_secs(30),
        Duration::from_secs(30),
    );

    gate.arm();
    let alice = Principal::User("alice".into());
    let alice_pool = Arc::new(pool);
    let stalled = {
        let pool = alice_pool.clone();
        let credential = token_record("conn-1", ALICE_TOKEN);
        tokio::spawn(async move { pool.client(alice, &credential).await })
    };
    // Give the stalled connect time to reach (and park at) the handshake.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !stalled.is_finished(),
        "alice's connect should still be parked at the gate"
    );

    // Bob connects, lists tools and is served while alice's handshake is
    // still parked.
    let start = Instant::now();
    let bob = Principal::User("bob".into());
    let bob_client = alice_pool
        .client(bob, &token_record("conn-b", BOB_TOKEN))
        .await
        .expect("bob connects while alice stalls");
    let tools = list_all_tools(&bob_client.peer)
        .await
        .expect("bob lists tools");
    assert_eq!(tools.len(), 2);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "bob was blocked by alice's stalled connect for {:?}",
        start.elapsed()
    );

    // Release the stall; alice's connect finishes normally.
    gate.release().await;
    let alice_client = stalled
        .await
        .expect("join")
        .expect("alice connects after release");
    let tools = list_all_tools(&alice_client.peer)
        .await
        .expect("alice lists tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(
        upstream.initialize_count.load(Ordering::SeqCst),
        2,
        "one handshake per connected credential"
    );

    let _ = upstream.shutdown().await;
}

/// A handshake that never completes is bounded by the pool's connect
/// timeout, and a retry after the stall succeeds (the in-flight slot was
/// released by the timeout path).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_never_completing_upstream_connect_times_out() {
    harnx_core::require_nextest();
    let gate = HandshakeGate::new();
    let upstream = spawn_gated_upstream("authorization", gate.clone())
        .await
        .expect("gated upstream");
    let mut pool = token_pool(&upstream.url, false);
    pool.set_timeouts(
        Duration::from_secs(600),
        Duration::from_millis(300),
        Duration::from_secs(30),
    );

    gate.arm();
    let alice = Principal::User("alice".into());
    let start = Instant::now();
    let error = pool
        .client(alice.clone(), &token_record("conn-1", ALICE_TOKEN))
        .await
        .err()
        .expect("connect must time out");
    let message = error_text(&error);
    assert!(
        message.contains("timed out"),
        "timeout error, got: {message}"
    );
    assert!(!message.contains(ALICE_TOKEN), "no secret in: {message}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "timeout bounded the connect"
    );

    // Let the abandoned handshake's gate finish on its own; its connect
    // future was dropped, so nothing waits on the barrier anymore.
    let released = tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(1), gate.release()).await;
    });
    let _ = released.await;

    // The retry succeeds: the in-flight slot was cleaned up.
    let client = pool
        .client(alice, &token_record("conn-1", ALICE_TOKEN))
        .await
        .expect("retry connects");
    let tools = list_all_tools(&client.peer).await.expect("list tools");
    assert_eq!(tools.len(), 2);

    let _ = upstream.shutdown().await;
}

/// A provider that echoes the submitted credential — in a JSON-RPC error,
/// in tool result content, or in the error that fails a connection — must
/// never get it back out through a tool result or the persisted flow
/// message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echoed_upstream_credentials_never_reach_results_or_flow_messages() {
    harnx_core::require_nextest();
    let upstream = spawn_leaky_upstream("authorization")
        .await
        .expect("leaky upstream");
    let (_keys, fixture) = fixture_on(&upstream).await;
    let toolset = &fixture.server.toolset;

    connect_user(toolset, &fixture.http_base, Some("alice"), ALICE_TOKEN)
        .await
        .expect("connect alice");

    // JSON-RPC error echoing the token.
    let error = invoke(toolset, "leak_error", Some("alice"), &[], Value::Null)
        .await
        .expect_err("leak_error fails");
    let message = match error {
        ToolInvokeError::Recoverable(message) => message,
        other => panic!("expected a recoverable error, got {other:?}"),
    };
    assert!(
        !message.contains(ALICE_TOKEN),
        "JSON-RPC error text must be redacted: {message}"
    );
    assert!(
        message.contains("[redacted]"),
        "redaction marker in error text: {message}"
    );

    // Tool result content echoing the token.
    let result = invoke(toolset, "leak_result", Some("alice"), &[], Value::Null)
        .await
        .expect("leak_result succeeds");
    let serialized = serde_json::to_string(&result).expect("serialize result");
    assert!(
        !serialized.contains(ALICE_TOKEN),
        "result content must be redacted: {serialized}"
    );
    assert!(
        serialized.contains("[redacted]"),
        "redaction marker in result: {serialized}"
    );

    // A credential-echoing failure while completing a connect flow must not
    // be persisted into the flow's failure message, the page, or the tool
    // result that repeats it.
    let (_challenge, challenge_id, round) = start_connect_flow(toolset, Some("bob")).await;
    upstream.set_fail_list_tools(true);
    let (status, body) =
        complete_token_form(&fixture.http_base, &challenge_id, BOB_TOKEN, None, &[])
            .await
            .expect("token form");
    assert_eq!(status, 502, "connection failed page: {body}");
    assert!(
        !body.contains(BOB_TOKEN),
        "failure page must not echo the token: {body}"
    );
    assert!(
        !body.contains("Bearer"),
        "failure page must not hint the auth scheme: {body}"
    );

    let flow = fixture
        .server
        .lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("flow read")
        .expect("flow after failure");
    assert_eq!(flow.record.status, harnx_credentials::FlowStatus::Failed);
    let stored = match flow.record.outcome {
        Some(harnx_credentials::flow::FlowOutcome::Failed { ref error_message }) => {
            error_message.expose().to_string()
        }
        other => panic!("failed flow outcome, got {other:?}"),
    };
    assert!(
        !stored.contains(BOB_TOKEN),
        "persisted failure message must be redacted: {stored}"
    );
    assert!(
        stored.contains("[redacted]"),
        "redaction marker in stored failure message: {stored}"
    );

    // The connect tool repeats the stored message to the caller; the secret
    // must be absent there too (it is what the journal would record).
    let error = invoke(
        toolset,
        "connect",
        Some("bob"),
        &[(connect_challenge_key(round), continue_response())],
        serde_json::json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect_err("failed flow cannot continue");
    let message = match error {
        ToolInvokeError::Recoverable(message) => message,
        other => panic!("expected a recoverable error, got {other:?}"),
    };
    assert!(
        !message.contains(BOB_TOKEN),
        "repeated failure message must be redacted: {message}"
    );

    let _ = upstream.shutdown().await;
}

struct SlowRefreshFactory {
    delay: Duration,
    /// Set once the modeled rotation completes — the store save a caller must
    /// never cut off.
    finished: Arc<AtomicBool>,
}

struct SlowRefreshAuth {
    delay: Duration,
    finished: Arc<AtomicBool>,
    /// The transport's header hook calls `ensure_fresh` on every request, so
    /// the modeled rotation must fire only on the first call.
    slept: AtomicBool,
    value: reqwest::header::HeaderValue,
}

impl UpstreamAuthFactory for SlowRefreshFactory {
    fn build(
        &self,
        _record: &harnx_credentials::Versioned<harnx_credentials::CredentialRecord>,
    ) -> anyhow::Result<Arc<dyn UpstreamAuth>> {
        Ok(Arc::new(SlowRefreshAuth {
            delay: self.delay,
            finished: Arc::clone(&self.finished),
            slept: AtomicBool::new(false),
            value: reqwest::header::HeaderValue::from_str(ALICE_TOKEN)
                .expect("token is a valid header value"),
        }))
    }
}

#[async_trait::async_trait]
impl UpstreamAuth for SlowRefreshAuth {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        config
            .custom_headers
            .insert(reqwest::header::AUTHORIZATION, self.value.clone());
    }

    async fn ensure_fresh(&self) -> anyhow::Result<()> {
        if self.slept.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        tokio::time::sleep(self.delay).await;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A token pool whose refresh waits `factory.delay` before succeeding, so
/// the pool's refresh handling is observable without an OAuth provider.
fn slow_refresh_pool(url: &str, delay: Duration, finished: Arc<AtomicBool>) -> UpstreamPool {
    let http = reqwest::Client::builder().build().expect("http client");
    let (pool, _list_changed) = UpstreamPool::new(
        Arc::new(HttpUpstreamConnector::new(url, http, false)),
        Arc::new(SlowRefreshFactory { delay, finished }),
    );
    let _ = _list_changed;
    pool
}

/// Concurrent cold calls for one credential must share a single upstream
/// session: the per-key in-flight slot stays claimed until the entry is
/// published, so runner-ups find the published client instead of each
/// opening their own upstream — an eviction of which would cancel live
/// sessions mid-call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_cold_calls_share_one_upstream_session() {
    harnx_core::require_nextest();
    let (upstream, manager) = spawn_session_upstream("authorization")
        .await
        .expect("session upstream");
    let pool = Arc::new(token_pool(&upstream.url, true));
    let alice = Principal::User("alice".into());
    let credential = token_record("conn-1", ALICE_TOKEN);

    let mut calls = Vec::new();
    for _ in 0..8 {
        let pool = Arc::clone(&pool);
        let principal = alice.clone();
        let credential = credential.clone();
        calls.push(tokio::spawn(async move {
            pool.client(principal, &credential).await
        }));
    }
    for call in calls {
        let client = call.await.expect("join").expect("cold call connects");
        let tools = list_all_tools(&client.peer).await.expect("list tools");
        assert_eq!(tools.len(), 2);
    }
    assert_eq!(
        upstream.initialize_count.load(Ordering::SeqCst),
        1,
        "the whole burst must open exactly one upstream session"
    );
    wait_for_live_sessions(&manager, 1).await;
    let _ = upstream.shutdown().await;
}

/// A stale connect — one built from a credential revision older than an
/// already published entry of the same principal — must never install.
/// Its upstream session must end (no keeper leak) and the newer
/// connection's entry must survive the stale call's cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_connect_cannot_replace_a_newer_connections_entry() {
    harnx_core::require_nextest();
    let gate = HandshakeGate::new();
    let (upstream, manager) = spawn_gated_session_upstream("authorization", gate.clone())
        .await
        .expect("gated session upstream");
    let pool = Arc::new(token_pool(&upstream.url, true));
    let alice = Principal::User("alice".into());

    // The old connection's connect parks at the handshake gate.
    gate.arm();
    let stale = {
        let pool = Arc::clone(&pool);
        let principal = alice.clone();
        let stale_credential = token_record_revised("conn-1", ALICE_TOKEN, 1);
        tokio::spawn(async move { pool.client(principal, &stale_credential).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !stale.is_finished(),
        "stale connect should still be parked at the gate"
    );

    // The reconnect (newer store revision) connects and publishes while
    // the old connect is still parked.
    let fresh_client = Arc::clone(&pool)
        .client(
            alice.clone(),
            &token_record_revised("conn-2", ALICE_TOKEN, 2),
        )
        .await
        .expect("fresh connect");
    let tools = list_all_tools(&fresh_client.peer)
        .await
        .expect("fresh lists tools");
    assert_eq!(tools.len(), 2);

    // Release the stale handshake: it finishes, but the fence must refuse
    // its publish instead of evicting the newer entry.
    gate.release().await;
    let stale_result = stale.await.expect("join");
    let message = match stale_result {
        Ok(_) => panic!("stale connect is fenced, but it succeeded"),
        Err(error) => error_text(&error),
    };
    assert!(
        message.contains("superseded"),
        "fence error, got: {message}"
    );

    // The fenced connection's session must end, and the newer entry's
    // session must still serve calls.
    wait_for_live_sessions(&manager, 1).await;
    let tools = list_all_tools(&fresh_client.peer)
        .await
        .expect("newer session survives the fenced stale connect");
    assert_eq!(tools.len(), 2);
    assert_eq!(
        upstream.initialize_count.load(Ordering::SeqCst),
        2,
        "one handshake per connection, stale one included"
    );
    let _ = upstream.shutdown().await;
}

/// A connector whose handshake always fails with the presented credential
/// echoed in the error text, pinning the pool's connect-error redaction
/// without depending on rmcp's own error rendering.
struct EchoingConnector;

#[async_trait::async_trait]
impl UpstreamConnector for EchoingConnector {
    async fn connect(
        &self,
        _auth: Arc<dyn UpstreamAuth>,
        _handler: PoolClientHandler,
    ) -> anyhow::Result<UpstreamConnection> {
        anyhow::bail!("handshake rejected for credential {ALICE_TOKEN}")
    }
}

/// A provider that echoes the presented credential in the error that fails
/// the connect must not get it back out through the connect error the tool
/// server reports verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_errors_redact_the_upstreams_echoed_credential() {
    harnx_core::require_nextest();
    let factory = TokenAuthFactory {
        header: reqwest::header::HeaderName::from_static("authorization"),
        scheme: None,
    };
    let (pool, _list_changed) = UpstreamPool::new(Arc::new(EchoingConnector), Arc::new(factory));
    let _ = _list_changed;
    let error = pool
        .client(
            Principal::User("alice".into()),
            &token_record("conn-1", ALICE_TOKEN),
        )
        .await
        .err()
        .expect("handshake echo fails the connect");
    let message = error_text(&error);
    assert!(
        !message.contains(ALICE_TOKEN),
        "no secret in connect error: {message}"
    );
    assert!(
        message.contains("[redacted]"),
        "redaction marker in connect error: {message}"
    );
}

/// A provider that echoes the presented credential in both handshake errors
/// must not get it back out through the connect error. rmcp's Auto
/// lifecycle summarizes the discover/initialize double failure without
/// upstream detail, so this pins the surfaced summary stays secret-free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_echoing_handshake_error_is_redacted() {
    harnx_core::require_nextest();
    let upstream = spawn_per_user_upstream("authorization", false)
        .await
        .expect("upstream");
    upstream.set_fail_handshake(true);
    let pool = token_pool(&upstream.url, false);
    let error = pool
        .client(
            Principal::User("alice".into()),
            &token_record("conn-1", ALICE_TOKEN),
        )
        .await
        .err()
        .expect("handshake echo fails the connect");
    let message = error_text(&error);
    assert!(
        !message.contains(ALICE_TOKEN),
        "no secret in connect error: {message}"
    );
    let _ = upstream.shutdown().await;
}

/// A credential refresh that outlasts the handshake deadline must still
/// complete and the connect must succeed: the refresh has its own budget,
/// so the handshake timeout cannot cut a rotation short.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_outlasting_the_handshake_deadline_still_completes() {
    harnx_core::require_nextest();
    let upstream = spawn_per_user_upstream("authorization", false)
        .await
        .expect("upstream");
    let finished = Arc::new(AtomicBool::new(false));
    let mut pool = slow_refresh_pool(
        &upstream.url,
        Duration::from_millis(1200),
        Arc::clone(&finished),
    );
    pool.set_timeouts(
        Duration::from_secs(600),
        Duration::from_secs(1),
        Duration::from_secs(30),
    );
    let client = pool
        .client(
            Principal::User("alice".into()),
            &token_record("conn-1", ALICE_TOKEN),
        )
        .await
        .expect("connects after the slow refresh");
    let tools = list_all_tools(&client.peer).await.expect("list tools");
    assert_eq!(tools.len(), 2);
    assert!(
        finished.load(Ordering::SeqCst),
        "the refresh ran to completion"
    );
    let _ = upstream.shutdown().await;
}

/// A caller that gives up on a slow refresh must not cancel the rotation:
/// it runs detached to its store save. Losing that save after the provider
/// consumed a single-use refresh token would lock the credential out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_refresh_runs_to_completion_detached() {
    harnx_core::require_nextest();
    let upstream = spawn_per_user_upstream("authorization", false)
        .await
        .expect("upstream");
    let finished = Arc::new(AtomicBool::new(false));
    let mut pool = slow_refresh_pool(
        &upstream.url,
        Duration::from_millis(500),
        Arc::clone(&finished),
    );
    pool.set_timeouts(
        Duration::from_secs(600),
        Duration::from_secs(30),
        Duration::from_millis(150),
    );
    let start = Instant::now();
    let error = pool
        .client(
            Principal::User("alice".into()),
            &token_record("conn-1", ALICE_TOKEN),
        )
        .await
        .err()
        .expect("refresh outlasts its budget");
    let message = error_text(&error);
    assert!(
        message.contains("refresh timed out"),
        "timeout error, got: {message}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the caller was bounded"
    );
    assert!(
        !finished.load(Ordering::SeqCst),
        "the caller gave up before the rotation finished"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !finished.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "detached refresh never completed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = upstream.shutdown().await;
}
