//! Integration coverage for the deferred session-claim path (T3): the
//! deterministic TOCTOU races, the registered-actor rows, and the
//! preflight outage case. These live in-crate (not under `tests/it`) so the
//! `session_claim_gate` test seam and the actor registry can be driven from
//! the test; the HTTP layer is still exercised for real through a live
//! listener. The behavioral matrix that needs no seam is in
//! `tests/it/access_sessions/claim_matrix.rs`.
//! The module name carries `nats` so nextest's broker-e2e filter (a real
//! `nats-server` per test) throttles these like the other broker tests.
// qual:allow(srp, file_length=1100) reason: "One cfg(test) module owns the whole deferred-claim matrix: every row shares the in-crate fixture helpers (claim gate seam, actor registry, live listener) and the `nats` module name that throttles the broker tests; splitting by responsibility cluster would duplicate the fixture or break that filter."

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use harnx_core::access_rules::AccessRules;
use harnx_runtime::config::{ConfigLock, NatsRouting};
use harnx_runtime::nats_session_log::{stream_name_for_session, NatsSessionLog};
use harnx_runtime::nats_session_metadata::{
    metadata_key, session_properties, SessionInitializer, SessionMetadata, SessionMetadataStore,
    SESSION_METADATA_BUCKET, SESSION_PROPERTIES_NAMESPACE,
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::session_actor::SessionKey;
use crate::test_support::TestConfigSandbox;
use crate::{Server, ServerHandle, DEFAULT_DRAIN_TIMEOUT};

const CLUSTER: &str = "claim-test";
const AGENT: &str = "plain";

/// alice and bob are plain prompt-scope users, alicia is alice's alias, and
/// root holds both prompt and admin scope so the R3 admin variant can claim
/// a foreign owner's record. admin is admin-only.
const RULES: &str = "rules:\n  - agents: ['*']\n    scopes: [prompt]\n    users: [alice, bob, alicia, root]\n  - agents: ['*']\n    scopes: [admin]\n    users: [admin, root]\n";
const ALIASES: &str = "- name: Pair\n  identities: [alicia, alice]\n";

struct ClaimFixture {
    _sandbox: TestConfigSandbox,
    nats: harnx_test_bins::NatsServerHandle,
    jetstream: async_nats::jetstream::Context,
    store: SessionMetadataStore,
    server: Arc<Server>,
    _handle: ServerHandle,
    base: String,
    client: Client,
    /// The target HTTP dispatch resolves for the test agent, so registered
    /// actors are keyed exactly like the serve process keys them
    /// (`SessionKey::local` would use the local cluster and never match).
    target: crate::session_actor::ResolvedAgentTarget,
}

async fn claim_fixture(aliases: bool) -> Result<Option<ClaimFixture>> {
    claim_fixture_with_broker(aliases, "").await
}

/// The fixture on a broker started with extra nats-server config, for
/// tests that need a degraded-but-alive broker (review D1 reroutes one KV
/// subject server-side instead of killing the process).
async fn claim_fixture_with_broker(
    aliases: bool,
    broker_config: &str,
) -> Result<Option<ClaimFixture>> {
    harnx_core::require_nextest();
    let Some(nats) = harnx_test_bins::spawn_configured_nats_server(broker_config).await? else {
        return Ok(None);
    };
    let sandbox = TestConfigSandbox::new();
    if aliases {
        sandbox.write_user_aliases(ALIASES);
    }
    sandbox.write_agent(AGENT, "You are plain.");
    sandbox.write_nats_server(CLUSTER, &format!("url: {}\n", nats.url()));
    let mut config = sandbox.config();
    config.nats_routing = NatsRouting::Cluster(CLUSTER.to_string());
    config.serve_user_id_sources = vec!["x-user".to_string()];
    let jetstream = config.nats_jetstream(CLUSTER).await?;
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let (target, _scoped) = crate::resolve_agent_target(&config, AGENT)
        .await
        .context("resolve claim test agent target")?;
    let rules = Arc::new(AccessRules::from_yaml(RULES).context("parse claim test access rules")?);
    let server = Arc::new(
        Server::new_with_access_rules(
            &Arc::new(ConfigLock::new(config)),
            PathBuf::from("web-assets"),
            Some(rules),
        )
        .context("build rules-on claim server")?,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!(
        "http://{}/v1/agents/{AGENT}/sessions",
        listener.local_addr()?
    );
    let _handle = server
        .clone()
        .run(listener, DEFAULT_DRAIN_TIMEOUT)
        .await
        .context("start claim test server")?;
    Ok(Some(ClaimFixture {
        _sandbox: sandbox,
        nats,
        jetstream,
        store,
        server,
        _handle,
        base,
        // no_proxy: the broker is local; an ambient proxy from the
        // environment would break these requests (review C11).
        client: Client::builder().no_proxy().build()?,
        target,
    }))
}

/// The agent's item route for `id`, the URL every claim request posts to.
fn session_url(f: &ClaimFixture, id: &str) -> String {
    format!("{}/{}", f.base, id)
}

/// A `session/prompt` RPC to the agent's item route.
fn rpc_prompt(f: &ClaimFixture, id: &str, user: &str, text: &str) -> RequestBuilder {
    f.client
        .post(session_url(f, id))
        .header("x-user", user)
        .header("accept", "application/json")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"text": text}
        }))
}

/// An AG-UI SSE run input that qualifies as a creating prompt.
fn sse_prompt(f: &ClaimFixture, id: &str, user: &str, text: &str) -> RequestBuilder {
    f.client
        .post(session_url(f, id))
        .header("x-user", user)
        .header("accept", "text/event-stream")
        .json(&json!({
            "messages": [{"role": "user", "content": text}],
            "tools": [], "context": [], "forwardedProps": {}
        }))
}

/// Any JSON-RPC method to the agent's item route.
fn rpc(f: &ClaimFixture, id: &str, user: &str, method: &str, params: Value) -> RequestBuilder {
    f.client
        .post(session_url(f, id))
        .header("x-user", user)
        .header("accept", "application/json")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
}

/// The registry key HTTP dispatch uses for this agent and session.
fn session_key(f: &ClaimFixture, id: &str) -> SessionKey {
    SessionKey::new(f.target.clone(), id)
}

/// Create canonical metadata owned by `user`, the racing winner's record.
async fn create_metadata(f: &ClaimFixture, session: &str, user: &str) -> Result<()> {
    let initializer = SessionInitializer::named(AGENT, Default::default()).with_user_id(user);
    f.store
        .create(&SessionMetadata::new(session, initializer))
        .await?;
    Ok(())
}

/// Upload a small attachment into `session` as `user` through the real
/// endpoint, returning its `cid:media:` reference. A ref whose object is
/// absent survives the row-g preflight but fails once an accepted prompt
/// tries to resolve it, so tests that assert acceptance need a real one.
async fn upload_media_ref(f: &ClaimFixture, session: &str, user: &str) -> Result<String> {
    const BOUNDARY: &str = "claim-attachment-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"attachment\"; filename=\"dot.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(b"claim attachment bytes");
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    let response = f
        .client
        .post(format!("{}/{session}/attachments", f.base))
        .header("x-user", user)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await?;
    let refs = body["attachment_refs"]
        .as_array()
        .context("upload refs array")?;
    assert_eq!(refs.len(), 1, "{body}");
    Ok(refs[0].as_str().context("first ref")?.to_string())
}

/// The session's owner stamp, or None when no record exists.
async fn owner(f: &ClaimFixture, session: &str) -> Result<Option<String>> {
    Ok(f.store.get_for_agent(session, AGENT).await?.map(|record| {
        session_properties(&record.metadata)
            .expect("decodable properties")
            .text("user_id")
            .map(str::to_string)
            .expect("owner stamp")
    }))
}

/// No canonical metadata record and no transcript stream may exist for a
/// refused claim: no reservation, no append, no actor-driven creation.
async fn assert_nothing_claimed(f: &ClaimFixture, session: &str) -> Result<()> {
    assert!(
        f.store.get_for_agent(session, AGENT).await?.is_none(),
        "no canonical metadata for '{session}'"
    );
    assert_no_transcript(f, session).await
}

async fn assert_no_transcript(f: &ClaimFixture, session: &str) -> Result<()> {
    let log = NatsSessionLog::for_agent(f.jetstream.clone(), AGENT, session);
    let name = stream_name_for_session(log.storage_key());
    // The typed absence answer (CodeRabbit 4677217439): STREAM_NOT_FOUND,
    // never some other lookup failure that would hide a wrongly-present
    // stream behind an `is_err`.
    match f.jetstream.get_stream(&name).await {
        Err(error) => match error.kind() {
            async_nats::jetstream::context::GetStreamErrorKind::JetStream(api)
                if api.error_code() == async_nats::jetstream::ErrorCode::STREAM_NOT_FOUND =>
            {
                Ok(())
            }
            other => bail!(
                "stream lookup for '{session}' must fail with STREAM_NOT_FOUND, got {other:?}"
            ),
        },
        Ok(_) => bail!("a transcript stream exists for '{session}'"),
    }
}

/// The session transcript serialized to JSON, for text-containment checks.
async fn transcript_json(f: &ClaimFixture, session: &str) -> Result<String> {
    let log = NatsSessionLog::for_agent(f.jetstream.clone(), AGENT, session);
    let events = log.load_events_async().await?;
    Ok(serde_json::to_string(&events)?)
}

/// Park exactly one deferred claim at the armed gate. Every race test arms
/// the gate before sending, so `during` — the state the race creates in
/// the window — runs deterministically between the guard's missing-owner
/// read and the claim's classification, preflight, reread and reservation.
/// Takes the gate, not the fixture, so `during` can mutably borrow other
/// fixture state (the outage test kills its broker inside the window).
async fn parked_claim(
    gate: &crate::parking_gate::ParkingGate,
    request: RequestBuilder,
    during: impl std::future::Future<Output = Result<()>>,
) -> Result<reqwest::Response> {
    let (arrivals, release) = gate.arm().await;
    let response = { tokio::spawn(async move { request.send().await.expect("claim request") }) };
    let deadline = Instant::now() + Duration::from_secs(30);
    while arrivals.load(Ordering::Relaxed) < 1 {
        anyhow::ensure!(
            Instant::now() < deadline,
            "claim never reached the session_claim_gate"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    during.await?;
    release.send(true)?;
    Ok(response.await.expect("join claim request"))
}

/// The exact guard 404: same status, same content-type and CORS header on
/// both responses, and byte-identical body as the guard's own refusal
/// (review C10 compares the stable headers on both sides, not just the
/// hidden one). The baseline is the same request to a whitespace-padded ID,
/// which the guard refuses before deferral, so any drift between the
/// deferred path's 404 and the guard's own rendering shows up here.
async fn assert_guard_404(hidden: reqwest::Response, baseline: reqwest::Response) -> Result<()> {
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    assert_eq!(hidden.headers()["content-type"], "application/json");
    assert_eq!(hidden.headers()["access-control-allow-origin"], "*");
    assert_eq!(baseline.status(), StatusCode::NOT_FOUND);
    assert_eq!(baseline.headers()["content-type"], "application/json");
    assert_eq!(baseline.headers()["access-control-allow-origin"], "*");
    assert_eq!(
        hidden.bytes().await?,
        baseline.bytes().await?,
        "deferred claim 404 must be byte-identical to the guard's"
    );
    Ok(())
}

async fn assert_accepted(response: reqwest::Response, context: &str) -> Result<Value> {
    assert_eq!(response.status(), StatusCode::OK, "{context}");
    let body: Value = response.json().await?;
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{context}: {body}"
    );
    Ok(body)
}

/// The whitespace-padded twin of `id`: never a claim candidate, so its
/// refusal is the guard's own 404, the byte-identical baseline that every
/// hidden case must match.
fn padded_twin(id: &str) -> String {
    format!("{id}%20")
}

/// Prompt `id` as `user` with `text`, then require the refusal to match
/// the guard's own 404 on the padded twin — the hidden-refusal shape the
/// claim rows assert over and over.
async fn assert_prompt_hidden_like_guard(
    f: &ClaimFixture,
    id: &str,
    user: &str,
    text: &str,
) -> Result<()> {
    let response = rpc_prompt(f, id, user, text).send().await?;
    let baseline = rpc_prompt(f, &padded_twin(id), user, text).send().await?;
    assert_guard_404(response, baseline).await
}

/// R3, the foreign-winner race: the guard read found no owner, the claim
/// parked at the gate, and bob's record appeared before the reservation
/// ran. The deferred path must refuse with the exact guard 404, keep bob's
/// ownership, and append, subscribe or command nothing (RPC).
#[tokio::test]
async fn claim_race_foreign_owner_after_missing_read_is_guard_404_rpc() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let response = parked_claim(
        &f.server.session_claim_gate,
        rpc_prompt(&f, "race-rpc", "alice", "from alice"),
        create_metadata(&f, "race-rpc", "bob"),
    )
    .await?;
    let baseline = rpc_prompt(&f, "race-rpc%20", "alice", "from alice")
        .send()
        .await?;
    assert_guard_404(response, baseline).await?;
    assert_eq!(owner(&f, "race-rpc").await?, Some("bob".into()));
    assert_no_transcript(&f, "race-rpc").await?;
    assert!(
        !f.server
            .session_registry
            .has_session(&session_key(&f, "race-rpc")),
        "no actor may be registered for the refused claim"
    );
    Ok(())
}

/// R3, the SSE form of the foreign-winner race: same window, same refusal,
/// and the SSE run must never subscribe or stream.
#[tokio::test]
async fn claim_race_foreign_owner_after_missing_read_is_guard_404_sse() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let response = parked_claim(
        &f.server.session_claim_gate,
        sse_prompt(&f, "race-sse", "alice", "from alice"),
        create_metadata(&f, "race-sse", "bob"),
    )
    .await?;
    let baseline = sse_prompt(&f, "race-sse%20", "alice", "from alice")
        .send()
        .await?;
    assert_guard_404(response, baseline).await?;
    assert_eq!(owner(&f, "race-sse").await?, Some("bob".into()));
    assert_no_transcript(&f, "race-sse").await?;
    assert!(
        !f.server
            .session_registry
            .has_session(&session_key(&f, "race-sse")),
        "no actor may be registered for the refused claim"
    );
    Ok(())
}

/// R3, the proceed variants: when the record that appears during the race
/// window is accessible to the caller, the claim proceeds as if the guard
/// had found it. The same-owner caller keeps ownership, and the
/// prompt+admin caller proceeds on a foreign owner's record.
#[tokio::test]
async fn claim_race_accessible_owner_after_missing_read_proceeds() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    for (label, user, winner) in [
        ("same-owner", "alice", "alice"),
        ("admin-scope", "root", "bob"),
    ] {
        let id = format!("race-{label}");
        let response = parked_claim(
            &f.server.session_claim_gate,
            rpc_prompt(&f, &id, user, "claim it"),
            create_metadata(&f, &id, winner),
        )
        .await?;
        assert_accepted(response, &format!("{label} race proceeds")).await?;
        assert_eq!(owner(&f, &id).await?, Some(winner.to_string()));
    }
    Ok(())
}

/// R3, the alias variant: alicia's claim proceeds on a record stamped for
/// her alias alice, because authorization uses `can_access_session` with
/// the caller view, never raw user_id equality.
#[tokio::test]
async fn claim_race_alias_of_owner_after_missing_read_proceeds() -> Result<()> {
    let Some(f) = claim_fixture(true).await? else {
        return Ok(());
    };
    let response = parked_claim(
        &f.server.session_claim_gate,
        rpc_prompt(&f, "race-alias", "alicia", "from alicia"),
        create_metadata(&f, "race-alias", "alice"),
    )
    .await?;
    assert_accepted(response, "alias-of-owner race proceeds").await?;
    assert_eq!(owner(&f, "race-alias").await?, Some("alice".into()));
    Ok(())
}

/// R3, the concurrent two-caller race on one unseen ID: exactly one caller
/// gets 200 and owns the session, the other gets the guard 404, and nothing
/// the loser sent reaches the transcript. Run with `--stress-count=20`.
#[tokio::test]
async fn claim_race_concurrent_two_callers_exactly_one_winner() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let id = format!("race-{}", uuid::Uuid::new_v4());
    let alice = rpc_prompt(&f, &id, "alice", "from alice");
    let bob = rpc_prompt(&f, &id, "bob", "from bob");
    let (alice, bob) = tokio::join!(async { alice.send().await }, async { bob.send().await },);
    let (alice, bob) = (alice?, bob?);
    let alice_won = alice.status() == StatusCode::OK;
    let bob_won = bob.status() == StatusCode::OK;
    assert!(
        alice_won ^ bob_won,
        "exactly one caller may win: alice {alice_won}, bob {bob_won}"
    );
    let winner = if alice_won { "alice" } else { "bob" };
    let loser = if alice_won { bob } else { alice };
    // The loser's refusal is the exact guard 404, byte-identical to the
    // guard's own rendering on a whitespace-padded ID.
    let baseline = rpc_prompt(&f, &padded_twin(&id), "alice", "guard baseline")
        .send()
        .await?;
    assert_guard_404(loser, baseline).await?;
    assert_eq!(
        owner(&f, &id).await?,
        Some(winner.to_string()),
        "the 200 caller owns the session"
    );
    let transcript = transcript_json(&f, &id).await?;
    assert!(
        transcript.contains(&format!("from {winner}")),
        "{transcript}"
    );
    let loser = if alice_won { "from bob" } else { "from alice" };
    assert!(
        !transcript.contains(loser),
        "the losing prompt must not appear in the transcript: {transcript}"
    );
    Ok(())
}

/// R10: a live actor registered for an ID with no canonical metadata is
/// not claimable. The row-h reread finds nothing and the caller gets the
/// guard 404 with nothing created.
#[tokio::test]
async fn claim_registered_actor_without_metadata_is_guard_404() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let key = session_key(&f, "actor-none");
    f.server.session_registry.get_or_spawn(key.clone());
    assert_prompt_hidden_like_guard(&f, "actor-none", "alice", "from alice").await?;
    assert_nothing_claimed(&f, "actor-none").await?;
    assert!(
        f.server.session_registry.has_session(&key),
        "the registered actor must be untouched"
    );
    Ok(())
}

/// R3 + R10, the row-h races: the winner created metadata and registered
/// its actor before the deferred caller reaches row h. The reread decides
/// — an accessible winner lets the caller proceed on the winner's session,
/// a foreign winner stays behind the guard 404, untouched.
#[tokio::test]
async fn claim_race_registered_actor_winners_row_h() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    // An accessible winner (the caller itself): the claim proceeds.
    let response = parked_claim(
        &f.server.session_claim_gate,
        rpc_prompt(&f, "actor-win", "alice", "from alice"),
        async {
            create_metadata(&f, "actor-win", "alice").await?;
            f.server
                .session_registry
                .get_or_spawn(session_key(&f, "actor-win"));
            Ok(())
        },
    )
    .await?;
    assert_accepted(response, "registered-actor race proceeds").await?;
    assert_eq!(owner(&f, "actor-win").await?, Some("alice".into()));
    // A foreign winner: the guard 404, the record and actor untouched.
    let response = parked_claim(
        &f.server.session_claim_gate,
        rpc_prompt(&f, "actor-foreign", "alice", "from alice"),
        async {
            create_metadata(&f, "actor-foreign", "bob").await?;
            f.server
                .session_registry
                .get_or_spawn(session_key(&f, "actor-foreign"));
            Ok(())
        },
    )
    .await?;
    let baseline = rpc_prompt(&f, "actor-foreign%20", "alice", "from alice")
        .send()
        .await?;
    assert_guard_404(response, baseline).await?;
    assert_eq!(owner(&f, "actor-foreign").await?, Some("bob".into()));
    // The loser claimed nothing: no transcript may exist, and the winner's
    // actor must be untouched.
    assert_no_transcript(&f, "actor-foreign").await?;
    assert!(
        f.server
            .session_registry
            .has_session(&session_key(&f, "actor-foreign")),
        "the winner's actor must be untouched"
    );
    Ok(())
}

/// R4, the registered-actor hazard: exact HITL, interaction response and
/// surface action RPCs accept actor-registry presence, so on an ID with a
/// live actor but no metadata they must still stay behind the guard's 404
/// and must not reach the actor.
#[tokio::test]
async fn claim_registered_actor_hidden_methods_stay_hidden() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let key = session_key(&f, "actor-hidden");
    f.server.session_registry.get_or_spawn(key.clone());
    let cases: Vec<(&str, Value)> = vec![
        (
            "session/hitl_decision_exact",
            json!({"request": {"invocation_id": "i", "tool_round_seq": 1,
                  "request_seq": 2, "tool_call_id": "call"}, "approved": true}),
        ),
        (
            "session/interaction_response",
            json!({"interaction_id": "i", "request_seq": 1,
                  "response": {"action": "acknowledge", "data": {}}}),
        ),
        (
            "session/surface_action",
            json!({"surface_id": "s", "publish_seq": 1,
                  "response": {"action": "acknowledge", "data": {}}}),
        ),
    ];
    for (method, params) in cases {
        let response = rpc(&f, "actor-hidden", "alice", method, params.clone())
            .send()
            .await?;
        let baseline = rpc(&f, "actor-hidden%20", "alice", method, params)
            .send()
            .await?;
        assert_guard_404(response, baseline)
            .await
            .with_context(|| method.to_string())?;
    }
    assert_nothing_claimed(&f, "actor-hidden").await?;
    assert!(f.server.session_registry.has_session(&key));
    Ok(())
}

/// R6, the outage ordering: a broker outage while the deferred claim runs
/// its attachment preflight answers 503 and leaves no metadata. The claim
/// parks at the gate (between the guard's read and the preflight), the
/// broker dies, and the released preflight classifies the outage as
/// Unavailable rather than a denial.
#[tokio::test]
async fn claim_preflight_broker_outage_is_503_without_metadata() -> Result<()> {
    let Some(mut f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let response = parked_claim(
        &f.server.session_claim_gate,
        rpc(
            &f,
            "outage",
            "alice",
            "session/prompt",
            json!({"text": "see this",
                  "attachment_refs": ["cid:media:plain/outage/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"]}),
        ),
        async {
            f.nats.kill().context("kill test nats-server")?;
            Ok(())
        },
    )
    .await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["message"], "session metadata unavailable");
    // The broker is dead, so the absence of metadata cannot be re-read from
    // the store here; the 503-before-reservation ordering is the pinned
    // behavior, and reservation never ran behind the preflight failure.
    Ok(())
}

/// C4/C8: the row-h reread uses the same fail-closed owner decoder as row
/// i, so a record that appears mid-race with undecodable properties — or
/// a present but non-string `user_id` — refuses with the guard's 404 for
/// the record's own owner and for an admin-scope caller alike, instead of
/// the 400 the guard's own owner read produces. A fresh poisoned record
/// and ID per round: the guard's read must miss, so the claim defers and
/// row h rereads the corruption.
#[tokio::test]
async fn claim_row_h_undecodable_and_non_string_owner_fail_closed() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let undecodable = json!("not session properties");
    let non_string_user = json!({ "user_id": { "value": 42 } });
    for (variant, extension) in [
        ("undecodable", undecodable),
        ("non_string", non_string_user),
    ] {
        for (caller, is_admin) in [("alice", false), ("root", true)] {
            let id = format!(
                "poison-{variant}-{}",
                if is_admin { "admin" } else { "owner" }
            );
            let response = parked_claim(
                &f.server.session_claim_gate,
                rpc_prompt(&f, &id, caller, "from me"),
                async {
                    create_metadata(&f, &id, "alice").await?;
                    poison_record_properties(&f, &id, extension.clone()).await?;
                    f.server.session_registry.get_or_spawn(session_key(&f, &id));
                    Ok(())
                },
            )
            .await?;
            let baseline = rpc_prompt(&f, &padded_twin(&id), caller, "from me")
                .send()
                .await?;
            assert_guard_404(response, baseline)
                .await
                .with_context(|| format!("{variant} as {caller}"))?;
            assert_no_transcript(&f, &id).await?;
        }
    }
    Ok(())
}

/// Overwrite the session's canonical record with `extension` under the
/// session-properties namespace — the corruption a mid-race writer, or a
/// newer format this server cannot decode, can leave behind. The raw KV
/// put skips the store's validation the way a foreign writer would.
async fn poison_record_properties(f: &ClaimFixture, session: &str, extension: Value) -> Result<()> {
    let record = f
        .store
        .get_for_agent(session, AGENT)
        .await?
        .context("record to poison")?;
    let mut metadata = record.metadata;
    metadata
        .extensions
        .insert(SESSION_PROPERTIES_NAMESPACE.to_string(), extension);
    let storage_key = harnx_core::session_identity::session_key(Some(AGENT), session);
    f.store
        .kv_store()
        .put(
            &metadata_key(&storage_key),
            serde_json::to_vec(&metadata)?.into(),
        )
        .await?;
    Ok(())
}

/// C6, row h: a live actor with no metadata yet means the claim rereads the
/// leader; a broker that dies inside the race window turns that reread into
/// the fixed 503 refusal, never a 404 that would read as "not claimable".
#[tokio::test]
async fn claim_row_h_reread_broker_outage_is_503() -> Result<()> {
    let Some(mut f) = claim_fixture(false).await? else {
        return Ok(());
    };
    f.server
        .session_registry
        .get_or_spawn(session_key(&f, "outage-h"));
    assert_claim_text_only_outage_is_503(&mut f, "outage-h").await
}

/// C6, row i: a broker that dies inside the race window turns the
/// reservation itself into the fixed 503 refusal. The prompt carries no
/// attachments, so the preflight passes without a broker round trip and
/// the outage is what the reservation hits.
#[tokio::test]
async fn claim_row_i_reservation_broker_outage_is_503() -> Result<()> {
    let Some(mut f) = claim_fixture(false).await? else {
        return Ok(());
    };
    assert_claim_text_only_outage_is_503(&mut f, "outage-i").await
}

/// The outage shape rows h and i share (review C6): the claim parks at the
/// gate, the broker dies inside the window, and the released text-only
/// request answers the fixed 503 refusal. Where it hits — the row-h reread
/// or the row-i reservation — is set up by the caller.
async fn assert_claim_text_only_outage_is_503(f: &mut ClaimFixture, id: &str) -> Result<()> {
    let response = parked_claim(
        &f.server.session_claim_gate,
        rpc_prompt(f, id, "alice", "text only"),
        async {
            f.nats.kill().context("kill test nats-server")?;
            Ok(())
        },
    )
    .await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["message"], "session metadata unavailable");
    Ok(())
}

/// D1, row i: a KV write that fails *after* connect answers the fixed 503,
/// never the 400 a malformed request gets. The broker stays alive and
/// serves every read — the guard's owner read, the reservation's record
/// read and transcript probe — but a server-side subject mapping reroutes
/// exactly this session's KV put to a subject no stream listens on, so the
/// `kv::Store::create` publish ack times out. That is the post-connect
/// `CreateError`/`UpdateError(TimedOut)` chain a broker stalled or dying
/// mid-write produces during first-prompt reservation, produced here
/// deterministically without killing anything.
#[tokio::test]
async fn claim_row_i_kv_write_failure_after_connect_is_503() -> Result<()> {
    const STALLED: &str = "stall-after-connect";
    let storage_key = harnx_core::session_identity::session_key(Some(AGENT), STALLED);
    let subject = format!("$KV.{SESSION_METADATA_BUCKET}.sessions/{storage_key}/meta");
    let broker_config = format!(
        "mappings: {{\n  \"{subject}\": \"$KV.{SESSION_METADATA_BUCKET}-stalled.{storage_key}/meta\"\n}}\n"
    );
    let Some(f) = claim_fixture_with_broker(false, &broker_config).await? else {
        return Ok(());
    };
    let response = rpc_prompt(&f, STALLED, "alice", "text only").send().await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["message"], "session metadata unavailable");
    // No metadata, no transcript: the reservation never completed, and the
    // stalled write left nothing behind (the put went to no stream at all).
    assert_nothing_claimed(&f, STALLED).await?;
    Ok(())
}

/// Review 4677217455: an ID the runtime's `SessionRef` refuses — under 6
/// characters, or longer but outside `[A-Za-z0-9_-]` — can never be
/// admitted (`NatsSession::session_ref()` panics on what it refuses), so
/// it never defers, let alone reserves: it stays behind the guard's 404
/// before any metadata write, instead of reserving an ID that stays
/// unusable. When a record with such an ID already exists (for instance,
/// left behind by rules-off implicit creation), a non-owner's prompt
/// still gets the guard's 404 from the ownership check. `thread-1` is
/// the passing control: an ID inside the runtime's alphabet creates.
#[tokio::test]
async fn claim_session_ids_the_runtime_rejects_stay_hidden() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    for id in ["demo", "bad.id1"] {
        assert_prompt_hidden_like_guard(&f, id, "alice", "from alice")
            .await
            .with_context(|| format!("runtime-refused id '{id}'"))?;
        assert_nothing_claimed(&f, id).await?;
    }
    // When a record with a runtime-refused ID already exists (owned by
    // bob), alice's prompt gets the guard's 404 from the ownership check
    // and leaves bob's record untouched.
    create_metadata(&f, "demo", "bob").await?;
    assert_prompt_hidden_like_guard(&f, "demo", "alice", "from alice").await?;
    assert_eq!(
        owner(&f, "demo").await?,
        Some("bob".into()),
        "the existing record must be untouched"
    );
    // The passing control: a 6-plus ID inside the alphabet creates.
    let created = rpc_prompt(&f, "thread-1", "alice", "from alice")
        .send()
        .await?;
    assert_accepted(created, "runtime-valid id").await?;
    assert_eq!(
        owner(&f, "thread-1").await?,
        Some("alice".into()),
        "the runtime-valid id creates alice's session"
    );
    Ok(())
}

/// C9, row g: a malformed `cid:media` reference and an over-cap ref list
/// are pre-reservation refusals on an unseen ID — the guard's exact 404,
/// nothing claimed. The over-cap list is valid and authorized
/// (CodeRabbit 4677217442): alice owns the source session, so only the
/// count can be what refuses — 32 repeats of the same ref pass preflight
/// and create, 33 stay hidden.
#[tokio::test]
async fn claim_malformed_and_over_cap_media_refs_are_hidden() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    // A media source alice owns: row-g authorization reads the source
    // session's owner stamp, independent of whether the referenced
    // object exists. The at-cap boundary below needs a ref whose object
    // exists too — an accepted prompt resolves its attachments — so this
    // goes through the real upload endpoint.
    create_metadata(&f, "media-src", "alice").await?;
    let media_ref = upload_media_ref(&f, "media-src", "alice").await?;
    // A media-prefixed ref that does not parse. A ref without the
    // `cid:media:` prefix is not an attachment ref at all and is ignored,
    // so the malformed case must carry the prefix to reach row g.
    let malformed = "cid:media:not-a-parseable-reference";
    let refs = |count: usize| {
        json!((0..count)
            .map(|_| media_ref.clone())
            .collect::<Vec<String>>())
    };
    for (label, params) in [
        (
            "malformed",
            json!({"text": "look here", "attachment_refs": [malformed]}),
        ),
        (
            "over_cap",
            json!({"text": "look here", "attachment_refs": refs(33)}),
        ),
    ] {
        let response = rpc(&f, "ref-case", "alice", "session/prompt", params)
            .send()
            .await?;
        let baseline = rpc(
            &f,
            "ref-case%20",
            "alice",
            "session/prompt",
            json!({"text": "look here", "attachment_refs": []}),
        )
        .send()
        .await?;
        assert_guard_404(response, baseline)
            .await
            .with_context(|| label.to_string())?;
        assert_nothing_claimed(&f, "ref-case").await?;
    }
    // The count boundary, with refs that are valid and authorized: 32 of
    // the same ref pass the preflight and create the session.
    let created = rpc(
        &f,
        "ref-at-cap",
        "alice",
        "session/prompt",
        json!({"text": "look here", "attachment_refs": refs(32)}),
    )
    .send()
    .await?;
    assert_accepted(created, "32 valid refs pass preflight").await?;
    assert_eq!(
        owner(&f, "ref-at-cap").await?,
        Some("alice".into()),
        "the authorized at-cap prompt creates alice's session"
    );
    Ok(())
}

/// C7: SSE gap rows — a blank user message and a user message followed by
/// an assistant message do not qualify as creating prompts, so an unseen
/// ID stays behind the guard's exact 404.
#[tokio::test]
async fn claim_sse_blank_and_user_then_assistant_stay_hidden() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let blank = json!({
        "messages": [{"role": "user", "content": "   "}],
        "tools": [], "context": [], "forwardedProps": {}
    });
    let user_then_assistant = json!({
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
        ],
        "tools": [], "context": [], "forwardedProps": {}
    });
    for (label, body) in [
        ("blank_user", blank),
        ("user_then_assistant", user_then_assistant),
    ] {
        let response = sse_body_request(&f, "sse-gap", "alice", &body)
            .send()
            .await?;
        let baseline = sse_body_request(&f, "sse-gap%20", "alice", &body)
            .send()
            .await?;
        assert_guard_404(response, baseline)
            .await
            .with_context(|| label.to_string())?;
        assert_nothing_claimed(&f, "sse-gap").await?;
    }
    Ok(())
}

/// C7: the prompt-scope requirement is visible over SSE too (row f, R5): an
/// admin-only caller's creating SSE prompt answers the collection route's
/// 403 — never a hidden 404 — and claims nothing.
#[tokio::test]
async fn claim_sse_admin_scope_is_403() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    let body = json!({
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [], "context": [], "forwardedProps": {}
    });
    let response = sse_body_request(&f, "sse-admin", "admin", &body)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let text = response.text().await?;
    assert!(
        text.contains("session creation requires prompt scope"),
        "the SSE refusal must match the collection route's 403: {text}"
    );
    assert_nothing_claimed(&f, "sse-admin").await?;
    Ok(())
}

/// An SSE run with a literal JSON body, for inputs the `sse_prompt` helper's
/// fixed shape cannot express.
fn sse_body_request(f: &ClaimFixture, id: &str, user: &str, body: &Value) -> RequestBuilder {
    f.client
        .post(session_url(f, id))
        .header("x-user", user)
        .header("accept", "text/event-stream")
        .json(body)
}

/// An exactly `CLAIM_BODY_MAX_BYTES + 1` body of valid JSON (review D5):
/// valid, so removing the cap check would let the body parse and claim —
/// the test must fail, not quietly 400 on a parse error.
fn over_cap_json_body() -> String {
    // {"x":"…"}: 8 framing bytes plus the padding.
    let padding = "a".repeat(crate::session_claim::CLAIM_BODY_MAX_BYTES + 1 - 8);
    let body = format!("{{\"x\":\"{padding}\"}}");
    assert_eq!(body.len(), crate::session_claim::CLAIM_BODY_MAX_BYTES + 1);
    body
}

/// C1/D3: an over-cap declared body hides behind the guard's 404 for a
/// missing ID and a foreign ID alike, and the precheck's refusal is final
/// before any body byte: with `Expect: 100-continue` and the oversized
/// length declared, the body stays withheld and the first bytes back are
/// the final 404 — never an interim 100 (CodeRabbit 4677217448). Sending
/// the 20 MiB body against the early refusal is what the Windows runner
/// aborted with 10053, so this test never sends it.
#[tokio::test]
async fn claim_body_over_cap_is_hidden_for_foreign_and_missing() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    create_metadata(&f, "big-foreign", "bob").await?;
    let oversized = over_cap_json_body();
    let authority = listener_authority(&f);
    let mut responses = Vec::new();
    for id in ["big-foreign", "big-missing"] {
        let mut stream = tokio::net::TcpStream::connect(authority).await?;
        let head = format!(
            "POST /v1/agents/{AGENT}/sessions/{id} HTTP/1.1\r\n\
             Host: {authority}\r\nx-user: alice\r\naccept: application/json\r\n\
             content-type: application/json\r\nContent-Length: {}\r\n\
             Expect: 100-continue\r\nConnection: close\r\n\r\n",
            oversized.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;
        // With the body withheld, the declared-length precheck alone must
        // answer: the response's first line is the final 404, not an
        // interim `100 Continue` followed by something else.
        let response = read_full_response(&mut stream, Duration::from_secs(15)).await?;
        assert!(
            response.starts_with(b"HTTP/1.1 404"),
            "the over-cap declared length must answer the final 404 first for {id}: {response:?}"
        );
        responses.push(response);
    }
    assert_raw_responses_match(&responses[0], &responses[1]);
    assert_eq!(
        owner(&f, "big-foreign").await?,
        Some("bob".into()),
        "the foreign record must be untouched"
    );
    assert_nothing_claimed(&f, "big-missing").await?;
    Ok(())
}

/// C1/D5: the same over-cap body with no declared length — chunked
/// framing, so the D3 `Content-Length` precheck cannot fire and the
/// cumulative cap in the bounded read is what must catch it — hides
/// behind the same guard 404 for a missing ID and a foreign one alike.
/// The server stops reading at the cap while the body is still being
/// sent, so the writes run in a background task and the reset a
/// refused-then-closed connection produces on them (Windows aborts such
/// connections) is expected and irrelevant once the 404 has been read.
#[tokio::test]
async fn claim_body_chunked_over_cap_is_hidden_for_foreign_and_missing() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    create_metadata(&f, "chunked-foreign", "bob").await?;
    let authority = listener_authority(&f);
    let mut responses = Vec::new();
    for id in ["chunked-foreign", "chunked-missing"] {
        let body = over_cap_json_body();
        let mut stream = tokio::net::TcpStream::connect(authority).await?;
        let head = format!(
            "POST /v1/agents/{AGENT}/sessions/{id} HTTP/1.1\r\n\
             Host: {authority}\r\nx-user: alice\r\naccept: application/json\r\n\
             content-type: application/json\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        let write = tokio::spawn(async move {
            let body = body.as_bytes();
            for chunk in body.chunks(64 * 1024) {
                writer
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await?;
                writer.write_all(chunk).await?;
                writer.write_all(b"\r\n").await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let response = read_full_response(&mut reader, Duration::from_secs(30)).await?;
        // The refusal arrived without waiting for the rest of the request;
        // whatever the write side saw by then no longer matters.
        write.abort();
        assert!(
            response.starts_with(b"HTTP/1.1 404"),
            "the chunked over-cap body must hide behind the guard's 404 for {id}: {response:?}"
        );
        responses.push(response);
    }
    assert_raw_responses_match(&responses[0], &responses[1]);
    assert_eq!(
        owner(&f, "chunked-foreign").await?,
        Some("bob".into()),
        "the foreign record must be untouched"
    );
    assert_nothing_claimed(&f, "chunked-missing").await?;
    Ok(())
}

/// C1: a candidate body that trickles and never finishes within the
/// deadline hides the request on both sides — the guard answers its 404
/// without waiting for the rest of the body, missing and foreign alike.
#[tokio::test]
async fn claim_body_deadline_hides_missing_and_foreign() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    create_metadata(&f, "slow-foreign", "bob").await?;
    crate::session_claim::TEST_CLAIM_BODY_DEADLINE_MS.store(300, Ordering::Relaxed);
    let authority = listener_authority(&f);
    let mut responses = Vec::new();
    for id in ["slow-foreign", "slow-missing"] {
        let mut stream = tokio::net::TcpStream::connect(authority).await?;
        let head = format!(
            "POST /v1/agents/{AGENT}/sessions/{id} HTTP/1.1\r\n\
             Host: {authority}\r\nx-user: alice\r\naccept: application/json\r\n\
             content-type: application/json\r\nContent-Length: 1024\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        // Ten bytes, then nothing: the body never finishes.
        stream.write_all(&[b'{'; 10]).await?;
        stream.flush().await?;
        let response = read_to_end(&mut stream, Duration::from_secs(15)).await?;
        responses.push(response);
    }
    for (id, response) in ["slow-foreign", "slow-missing"].iter().zip(&responses) {
        assert!(
            response.starts_with(b"HTTP/1.1 404"),
            "the deadline must answer the guard's 404 for {id}: {response:?}"
        );
    }
    assert_raw_responses_match(&responses[0], &responses[1]);
    assert_eq!(
        owner(&f, "slow-foreign").await?,
        Some("bob".into()),
        "the foreign record must be untouched"
    );
    assert_nothing_claimed(&f, "slow-missing").await?;
    Ok(())
}

/// C1: with `Expect: 100-continue` the interim 100 only goes out when the
/// body is first polled — a foreign ID used to reach its 404 without
/// polling while a missing ID did not. Both candidates now poll through
/// the same bounded read, so both see the same interim 100 and then the
/// same final 404.
#[tokio::test]
async fn claim_body_expect_continue_parity_between_foreign_and_missing() -> Result<()> {
    let Some(f) = claim_fixture(false).await? else {
        return Ok(());
    };
    create_metadata(&f, "expect-foreign", "bob").await?;
    let authority = listener_authority(&f);
    // A non-creating method keeps the final answer the guard's 404 on both
    // sides; the parity target is the body handling, not the result.
    let body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/get\",\"params\":{}}";
    let mut finals = Vec::new();
    for id in ["expect-foreign", "expect-missing"] {
        let mut stream = tokio::net::TcpStream::connect(authority).await?;
        let head = format!(
            "POST /v1/agents/{AGENT}/sessions/{id} HTTP/1.1\r\n\
             Host: {authority}\r\nx-user: alice\r\naccept: application/json\r\n\
             content-type: application/json\r\nExpect: 100-continue\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;
        // No body byte yet: the interim 100 can only come from the guard's
        // bounded read polling the body.
        let interim = read_until_marker(&mut stream, b"\r\n\r\n", Duration::from_secs(10)).await?;
        assert!(
            interim.starts_with(b"HTTP/1.1 100 Continue"),
            "missing interim 100 Continue for {id}: {interim:?}"
        );
        stream.write_all(body).await?;
        stream.flush().await?;
        let final_response = read_to_end(&mut stream, Duration::from_secs(15)).await?;
        finals.push(final_response);
    }
    for (id, response) in ["expect-foreign", "expect-missing"].iter().zip(&finals) {
        assert!(
            response.starts_with(b"HTTP/1.1 404"),
            "the final answer must be the guard's 404 for {id}: {response:?}"
        );
    }
    assert_raw_responses_match(&finals[0], &finals[1]);
    assert_eq!(owner(&f, "expect-foreign").await?, Some("bob".into()));
    assert_nothing_claimed(&f, "expect-missing").await?;
    Ok(())
}

/// The response's lines minus the `date:` header.
fn without_date_lines(response: &[u8]) -> Vec<&[u8]> {
    response
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.starts_with(b"date:"))
        .collect()
}

/// Byte-compare two raw HTTP responses ignoring the `date:` header, which
/// the review's own criterion excludes from parity (stable application
/// headers only).
fn assert_raw_responses_match(left: &[u8], right: &[u8]) {
    assert_eq!(
        without_date_lines(left),
        without_date_lines(right),
        "the refusal must be identical for foreign and missing"
    );
}

/// The listener's authority, from the fixture's base URL: raw-socket cases
/// reqwest cannot express connect here.
fn listener_authority(f: &ClaimFixture) -> &str {
    f.base
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .expect("fixture base url")
}

/// Read from `stream` until `marker` appears, bounded by `budget`. The
/// whole read shares one wall-clock deadline (review D6): a slow first
/// byte under load eats into the same budget instead of tripping a fixed
/// per-read timeout that ignored it.
async fn read_until_marker(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    marker: &[u8],
    budget: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + budget;
    let mut seen = Vec::new();
    while !seen.windows(marker.len()).any(|window| window == marker) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "response never arrived within budget: {seen:?}"
        );
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(remaining, stream.read(&mut buf))
            .await
            .context("read timed out")?
            .context("read failed")?;
        if n == 0 {
            bail!("connection closed before {marker:?}: {seen:?}");
        }
        seen.extend_from_slice(&buf[..n]);
    }
    Ok(seen)
}

/// Read one complete HTTP response over a raw socket: the headers through
/// the blank line, then the `content-length` body, bounded by `budget`.
/// A complete response, not a clean close, is what ends the read: the
/// tests that use this answer before the request body is consumed, so
/// the server closes once done and the reset such a close can produce
/// (Windows aborts the connection) never reaches this reader.
async fn read_full_response(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    budget: Duration,
) -> Result<Vec<u8>> {
    let mut response = read_until_marker(stream, b"\r\n\r\n", budget).await?;
    let headers_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("response headers terminator")?
        + 4;
    let length = String::from_utf8_lossy(&response[..headers_end])
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let deadline = Instant::now() + budget;
    while response.len() < headers_end + length {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "response body never arrived within budget: {response:?}"
        );
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(remaining, stream.read(&mut buf))
            .await
            .context("response body read timed out")?
            .context("response body read failed")?;
        anyhow::ensure!(
            n > 0,
            "connection closed before the response body: {response:?}"
        );
        response.extend_from_slice(&buf[..n]);
    }
    Ok(response)
}

/// Read from `stream` to EOF, bounded by `budget`.
async fn read_to_end(stream: &mut tokio::net::TcpStream, budget: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + budget;
    let mut seen = Vec::new();
    loop {
        anyhow::ensure!(
            Instant::now() < deadline,
            "response never arrived: {seen:?}"
        );
        let mut buf = [0u8; 4096];
        let n = match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(error)) => return Err(error).context("read failed"),
            Err(_) => continue,
        };
        seen.extend_from_slice(&buf[..n]);
    }
    Ok(seen)
}
