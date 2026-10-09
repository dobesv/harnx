# Multi-replica fault-injection foundation

Admission, supervision and streaming regressions exercise shared authority at
controlled boundaries. Run with `--features fault-injection`; normal server builds
contain no controls or counters. Separate-process route tests also run in default builds.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection -E 'test(multi_replica)' --stress-count=20
```

## Current memberships and original ownership

`multi_replica/auth.rs` and `auth/` use independent ACL-enabled backends with
real NATS/worker counters. Group-only and role-only requests can retry one
principal's retained identity without changing its task. Shared memberships and
admin scopes don't merge distinct principals' first messages. Revocation and
user/group/role namespace collisions deny all remote routes and direct task
APIs before fingerprint checks or cancellation effects.

Recovery tests revoke every ACL on the recovering backend, pause before metadata
creation or executable prompt append, then remove the former owner's scoped lease.
The background sweep settles the same identity without HTTP or prompt replay.
An admin follow-up reconstructs a missing registry under the original binding
owner. Anonymous rules-off recovery preserves the intentional shared principal.
Assertions inspect immutable owner, registry/context metadata, prompt counts and
actual worker requests. No memberships or raw identity sources may be persisted.
Unit tests in `store/registry/identity/tests.rs` reject owner/export/cluster or
allocation mismatch and verify original-owner checks on registry conflicts.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection -E 'test(multi_replica::auth) | test(store::registry::identity)' --stress-count=20
```

## Reproductions and boundaries

`multi_replica.rs` retains `support::Harness` (broker, worker and scripted LLM) for
both backends' whole lifetime. Each backend has its own `Backend`, `Runner`,
`A2aStore`, router, admission gates and dedupe LRU. Requests use the actual Axum
JSON-RPC stack through `tower::ServiceExt::oneshot`, not separate OS processes.
The worker daemon is real; its streaming OpenAI-compatible endpoint is scripted.

## Separate-process route coverage

`e2e/replicas/` starts two real `harnx-a2a-server` binaries with distinct PIDs,
ports and startup logs. They share one isolated R1 NATS broker and a real worker.
The route driver assigns every request A, B, A, B and checks both destinations
for SendMessage, SendStreamingMessage, SubscribeToTask, GetTask, ListTasks and
CancelTask. Concurrent admissions have their destinations assigned before either
HTTP request starts. Internal claim races still use the deterministic fault harness.

The counted-tool script calls the real native `harnx-bash-tools` server to append
one line per side effect, then holds the next model response after `Hello `.
Tests check one durable prompt, one side effect per identity, independent exact
snapshot/delta accumulation, Busy and fingerprint mismatch errors, retained
terminal retries, and exact cancellation without stopping another context/turn.
Disconnect/reconnect is a separate case. Principal/export tests check all protected
read/cancel/subscribe routes and first-message scope, plus an old terminal archive
without `stream_seq` and without context authority.

The process-death test SIGKILLs A, observes worker lease renewal, and waits for B's
production background sweep after real default lease expiry. No HTTP polling or
lease deletion triggers recovery. B's already-open subscription receives real idle
keep-alive comments and then Failed. The paused-clock keep-alive test isolates the
upstream SSE encoder; it doesn't advance clocks on live broker requests.
A prepared old-owner CAS is rejected after takeover. Restarted A and B return the
same failed identity without another tool call; a new turn in that context completes
and a delayed cancel for the failed task doesn't stop it. The archive stays unchanged.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo build --workspace
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server -E 'test(e2e::replicas)' --stress-count=20
```

These shell/SIGKILL fixtures run on Unix, including Linux's light sandbox. They
aren't an ALB test, an R3/quorum test, a selective partition test or a throughput
benchmark. Drain old turns and stop old writers before upgrading; no mixed legacy
writers and no automatic replay. Runtime readers must understand `admission_closed`
before coordinated writers emit it. See the operations guide for rollout details.

## Deterministic shared-authority reproductions

- `identical_first_messages_share_context_task_prompt_and_execution`: both real
  handlers pass a dedupe miss concurrently. Shared reservation picks one context,
  task and prompt, and the real worker makes exactly one LLM request. Both retries
  follow the same winner despite independent local caches.
- `sibling_reconciliation_does_not_cancel_an_active_owner_turn`: hold A before
  artifact publication while B reads/reconciles the context. B observes the scoped
  owner and emits no Cancel; A remains active.
- `pause_hooks_bound_real_claim_prompt_append_and_publication`: Claim now bounds
  committed context reservation. Before append there is no executable prompt;
  after append exactly one raw prompt exists while admission projection is zero.
- `multi_replica/admission.rs`: candidate lease before metadata, initialization
  wait, poisoned LRU, fingerprint/Busy/terminal-target ordering, follow-up races,
  real successful acknowledgement suppression, each first-message crash boundary,
  active-owner takeover and terminal transport backpressure. Broker lease revision
  deletion models loss; recovery closes/stops the same IDs without new execution.

`src/fault_injection.rs` has runner/store-local counters and semaphore pause guards.
Dropping a guard releases work, including during assertion unwinding. Publication
still means local advisory delivery, not durable intermediate fanout. The terminal
outbox bridge commits and conditionally publishes a retained final status event;
remote readers and intermediate outboxes remain separate work. No sleeps select
race winners; startup uses readiness helpers.

## Broker inventory (baseline 2a00be06)

Observed binary: nats-server 2.11.6. `Cargo.lock` pins async-nats 0.50.0;
workspace features include server_2_10, server_2_11, ring, jetstream, kv,
object-store and websockets. `multi_replica_inventory.rs` exercises stable message
IDs, expected-last-subject-sequence rejection, accepted message TTL headers,
per-subject DiscardNew and finite-byte rejection against that broker. This does
not test TTL expiry or deduplication beyond its window.

| Resource / budget | Current evidence |
|---|---|
| Session metadata | `nats_session_metadata/store.rs::ensure`: `harnx_sessions`, File, history 1, configured replicas; live stream asserts max_age=0 and no finite max_bytes/max_message_size. Server payload limit is 1,048,576 bytes. |
| Transcript | `nats_session_log.rs`: `SESSION_<sha256(storage_key)>`, subject `sessions.{storage_key}.log`, Limits retention, 120-second duplicate window; no max_age or finite byte limit configured. |
| Worker lease | `nats_lease.rs`: `harnx_leases`, `sessions/{storage_key}/lock`, TTL 30s / renew 10s / tombstone 3600s. A2A must use a distinct scoped key; this task does not change worker locking. |
| Input | `input_map.rs::InputLimits`: default 65,536 bytes for rendered data/inline files; text has no matching data-part cap; new admission separately checks serialized input against half the broker/KV ceiling. |
| Metadata extensions | `nats_session_metadata.rs`: 65,536 bytes per namespace, 262,144 bytes total. The separate context authority value isn't a metadata extension. |
| Index | `store/a2a.rs::put_a2a_task_index`: serialized size checked against min(stream max message size, server payload). `store_nats_index.rs::store_index_size_limit_clear_error_does_not_write_task` uses a 512-byte bucket to exercise overflow. |
| Existing large-output fixture | `support.rs::Script::Large`, `runner.rs::runner_large_stream_materializes_text_only_for_readers`: 69 chunks of 1,024 bytes (EVENT_CAPACITY=64 plus 5), 70,656 output bytes. Final record repeats output in artifact, status message and assistant history, so budgeting only one text copy is unsafe. |
| Probe only | Test stream `A2A_CAPABILITY_PROBE`: Limits, max_bytes=4096, max messages/subject=1, DiscardNew per subject. A 4097-byte message is rejected and retained predecessor remains. Deleted at test end. Not a proposed production capacity. |

Shared reservations use `a2a/first-messages/{scope-hash}` without independent TTL.
Every update uses a same-document outbox and conditional task subject publication.
The event stream is 128 MiB Limits/DiscardNew with explicit per-subject DiscardNew,
128 messages/subject and checkpoint-covered cleanup retaining 65 confirmed events.
The durable registry and bounded sweep recover missing owners without HTTP traffic.
`operations/` tests exact 224-KiB answer output, full pending authority, byte/subject
backpressure, predecessor-safe cleanup, coordinated GC and service permissions.
See [A2A operations](../../../../docs/a2a-operations.md) for current budgets and
honest deployment/load gaps; the older 69-KiB fixture isn't a release capacity claim.

## Bootstrap, cleanup and permissions

`src/bootstrap.rs::Bootstrap::new` loads config, selects frontend-local or named
cluster routing, opens JetStream, ensures session metadata with R1 locally or
cluster replicas, then builds Store/Runner/Backend once per cluster. Harness startup
uses that same metadata ensure path, independent backend constructors and a shared
worker. It doesn't call the private CLI bootstrap itself; existing e2e tests cover it.

Worker `nats_worker/daemon.rs` schedules `remote_session_cleanup.rs` hourly only
when `cleanup_remote_sessions_days` is positive (unset/0 disables GC). GC elects a
lease holder, then `nats_admin.rs::delete_remote_session_by_key` deletes transcript,
worker lease, invocation journal, metadata prefix and session-owned blobs/plans.
`SessionMetadataStore::purge_session_prefix` removes `sessions/{storage_key}/a2a/*`.
The inventory test purges concrete A2A keys and preserves a similarly named sibling.
Coordinated GC also removes the scoped A2A lease and global event/reservation/registry
resources, after exact revision-fenced readiness checks. Unallocated winning
identities and pending outboxes block deletion. Late first publication after GC
requires a retained authority tombstone before cleanup, not absence alone.
`store/index.rs::DANGLING_TASK_GRACE_PERIOD` is 300s, not a session-retention TTL.

The restricted-account test bootstraps metadata, CAS-updates an index, rejects a
stale CAS and reads it through `get_a2a_task_index`'s leader API. It allows publish
on `$JS.API.INFO`, `$JS.API.STREAM.>`, `$JS.API.CONSUMER.>`, `$JS.ACK.>` and
`$KV.harnx_sessions.>`, subscribe on `_INBOX.>`. A disallowed publication produces
an observed permissions-violation event. `$JS.API.INFO` is required during KV ensure;
omitting it failed the probe. This is a tested metadata profile, not a least-privilege
profile for the entire runtime. STREAM API access is broader than one bucket.

Current `operations/nats-permissions.conf` and `operations/permissions.rs` verify
backend/worker resource APIs with denied-subject events. The stronger
`multi_replica/operations/permissions.rs` starts a real restricted worker, runs
one turn through a restricted backend, closes a missing admission with no prompt,
and performs production session GC through the worker account. The old metadata
probe alone doesn't prove that lifecycle. Task-event readers have zero server
consumers; KV watches and worker queues have separate existing consumers.

NATS wildcard tokens can't split slash-containing KV keys, so the fixture grants
trusted services broad KV token access rather than claiming per-path lease ACLs.
Replace sandbox authentication and test each installed tool/hook profile before
deployment. Application principal/export ACL tests remain separate and unchanged.

## Distributed supervision tests

`multi_replica/supervision/` exercises exact remote cancellation, stale T1 requests
while T2 runs, accepted cancellation before prompt admission, completion before
frontend publication, and a lost cancel acknowledgement superseded by terminal CAS.
`recovery.rs` covers restart before metadata, a claimed context before task
allocation, two sweepers passing 40 unrelated registry records,
worker-lease-independent takeover, and an already-waiting local
client returning the successor's terminal task before the former publisher resumes.
No recovery test admits a missing prompt. Counters assert zero executions for closed
reservations or one execution for the interrupted/completed invocation.

`broker.rs` freezes the real test NATS process with SIGSTOP, observes an uncertain
cancel result and retained intent/stop checkpoint, then heals with SIGCONT. It also
kills/restarts NATS on the same storage and TCP port, reconnects clients, and settles
from the retained registry without incoming HTTP. This is a broker-wide outage,
not selective packet filtering of one backend connection.

`e2e/supervision.rs` starts the real A2A binary, SIGKILLs only that owner, observes
worker lease renewal afterwards, and lets the real default 30-second scoped lease
expire. A fresh independent runner's background sweep settles Failed without an
HTTP request and without another worker execution. Terminal checks use point reads,
not a request-triggered reconciler. Process cleanup remains owned by the test sandbox.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection -E 'test(supervision)' --stress-count=20
```

Distributed streaming has separate handoff/outbox tests below. Global registry,
reservation and event retention/permissions need the separate retention work before
unrestricted scale-out; no fixed failover latency is asserted.

## Durable streaming tests

`multi_replica/streaming/` uses two independent backends and real HTTP/SSE. Six
handoff cases publish intermediate and terminal events after watermark capture,
after snapshot capture and after reader creation. Snapshot-first assembly must be
exactly `Hello world` and terminate, with one worker execution. Other cases cover
independent subscribers, reconnect, remote blocking waiters, real published-event
ack suppression, stage/checkpoint receipts superseded by cancellation, delayed old
committed publication after takeover and expired duplicate window, recovered answer
replacement before terminal, bounded slow-reader lag, and missing intermediate or
terminal events removed during handoff. Readers create zero server consumers.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection -E 'test(multi_replica::streaming)' --stress-count=20
```

The 100 ms broker duplicate-window minimum is tested on pinned NATS 2.11.6. The
expiry wait doesn't choose a race winner; barriers and confirmed broker state do.
Global cleanup, permissions and production stream capacity remain separate checks.
