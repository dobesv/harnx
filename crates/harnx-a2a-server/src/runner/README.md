# Coordinated admission

Admission uses shared reservations, a scoped A2A lease and the CAS context document.
The worker still acquires `sessions/{storage_key}/lock` independently. Don't acquire
that worker lease from A2A. The A2A key is `sessions/{storage_key}/a2a/lock`.

## First and follow-up messages

Authorization and target binding precede fingerprint checks. Deduplication precedes
Busy or terminal-target rejection. Invalid new input doesn't allocate a session.

For a first message, the candidate allocates IDs in memory and takes its scoped
lease **before** the global create-only reservation. The reservation key hashes a
structured `(cluster, export, principal user ID, messageId)` tuple. It retains the
fingerprint, original message, context/task IDs, runtime invocation/prompt/closure
IDs and creation time. Only the winner creates metadata, binding and transcript.
A loser releases its unused candidate lease and follows the winner's IDs.

The candidate lease covers the initialization window even before a context exists.
Missing metadata/authority is not proof of owner loss while that lease is held.
Followers wait at most five seconds per request and report an initialization error
if still unresolved. The default lease TTL is thirty seconds; a timeout is not
permission to replace the reservation. Retry the same messageId. Bootstrap starts background recovery from the durable registry, without a request.

Follow-ups read the active document's retained identity before the repairable message
mapping, Busy or terminal-target checks. A scoped candidate then CAS-claims the
context, repairs a settled predecessor if needed, captures one runtime predecessor,
and CAS-reserves all task identities before prompt append. Contending candidates
follow the same identity or return the existing Busy error for distinct work.

Local gates, LRUs and live handles aren't authority. Live presentation and local
completion handles require a matching owner fence; a stale cached snapshot can't
replace a recovered task. Active task mutations use `OwnedTask::update`, which
checks the lease and CASes the same context document's owner/task/revision.
Legacy task CRUD is refused on activated contexts.

## Request memberships and stored owners

Request-facing admission, reads, cancellation, streams and waiters carry the full
`RequestIdentity`. Current groups and roles grant scopes; only `Principal` supplies
the immutable owner and first-message dedup scope. A changed membership is checked
before retained identity or fingerprint lookup on each new request. Group and role
names never become owners, even if a name equals another user's ID.

Background recovery has no request memberships. `stored_session.rs` checks the
stored principal against the binding's owner, export, agent, cluster and schema,
then settles retained work through the existing fenced ticket. It doesn't construct
a request identity with empty memberships or ask ACLs to authorize a historical
caller. Revoked access must stop new requests without blocking closure or exact
interruption of already admitted work. Missing active bindings aren't recreated;
only a verified winning first reservation can initialize missing metadata.

Recovery registrations keep the binding's original owner when an admin admits a
follow-up, not that admin's current identity. Only owner/export/allocation facts are
persisted, never request groups, roles, headers or credentials. `multi_replica/auth/`
tests this split with revoked rules, paused first initialization and prompt append,
stale owners, role-admin follow-ups and independent replica retries.

## Recovery never admits missing work

Foreground retries/Get/New Send and background supervision may acquire the expired owner's scoped lease and
CAS-take over. Recovery restores the original ticket, checks durable completion,
then closes its fixed predecessor or interrupts the exact admitted prompt. It never
calls append/activation to repair an absent prompt. A first-message crash before
allocation reconstructs a non-executable task from the reservation, then closes it;
that isn't prompt replay. Runtime completion inspection doesn't activate work.

Only confirmed closure/interruption/completion permits terminal settlement. Broker
uncertainty leaves the same identity unresolved and returns an error. Completion
wins over owner-loss failure when the worker already wrote its durable result.
Local failure retries reload only the same owner/task before repairing persistence.
Scoped interruption can't cancel a later invocation. Tool side effects aren't rolled
back by stopping a turn.

## Remote cancellation and background recovery

Any authorized replica can CAS a `CancelIntent` into the active authority. Intent
names the task and immutable runtime invocation. It can't be removed or retargeted.
The owner observes it independently of HTTP connections and appends a fenced scoped
interrupt. An expired owner's successor closes an unadmitted fixed ticket or stops
that exact prompt. The runtime log orders completion against interruption; a durable
completion wins even if A2A terminal publication hasn't finished. Cancel replies
require terminal authority; uncertain broker reads/appends return the existing
internal error, leaving intent for retry. A delayed T1 cancel can't interrupt T2.

Create-only `a2a.registry.{storage-hash}` records precede first-message reservation
and context effects. They retain export/agent/principal and original allocation; an
initialization sweep checks the shared reservation winner before reconstructing a
non-executable task. Registry records survive backend restart. Scans use leader
`STREAM.MSG.GET` next-by-subject, at most 32 records per pass, with a five-second
registry lookup budget and one-second pause between passes. Runtime settlement
uses its bounded broker operations, not a fixed whole-transcript deadline that
could starve a long session on every pass. A failed
entry is revisited on a later pass; no HTTP request is needed. These bounds limit
work, not failover latency. NATS outages and lease TTL can delay settlement.

Supervision runs only for exports configured on that backend. Keep an export
configured while it has unresolved tasks. Worker leases aren't A2A ownership proof.
Retirement still requires durable closure/stop plus archive, mapping and terminal
publication. Tool side effects aren't rolled back and interrupted work isn't replayed.
Session GC removes global discovery and first-message identities only after those
retirement proofs. Unused losing allocation hints have separate grace-based cleanup.

## Durable publication and independent readers

Working status, coalesced artifact updates and terminal status each CAS-commit the
complete task snapshot, durable `stream_seq` and one `PendingEvent` together. The
pending envelope freezes commit ID, task sequence, exact subject predecessor and
unchanged public update payload. No next event or snapshot change is allowed until
pending publication resolves. Artifact coalescing stays at 100 ms; there is no
separate two-second KV snapshot throttle or in-memory-only stream cursor. Runtime
text coalesces in a bounded inbox instead of an unbounded event queue; overflow
fails explicitly rather than dropping tokens. Inbox and accumulated text use the
existing dynamic NATS payload ceiling (1 MiB fallback), while serialized snapshot
checks still enforce the real metadata budget.

`HARNX_A2A_TASK_EVENTS` uses `a2a.tasks.{storage_key}.{task_uuid}` subjects. Publish
only committed envelopes with stable message ID and expected last subject sequence.
Leader retained-identity inspection resolves missing acknowledgements outside the
broker duplicate window too. A former owner can finish its committed publication,
including after a successor event, but can't CAS new state or checkpoint over a
successor. Superseded stage/clear receipts follow stable envelope or exact subject
checkpoint; they never reapply a delta or replay runtime work. Recovery drains a
pending event before terminal settlement and publishes recovered answer replacement
before terminal status. Archive, mapping and final-event proof still gate retirement.

Every HTTP stream and coordinated terminal waiter uses this handoff:

1. Authoritative global stream watermark **G first**.
2. Fresh authoritative task snapshot and cursor **n second**.
3. Independent task-filtered reader starting **G + 1 last**.

Never capture a newer watermark after the snapshot. Send the snapshot first,
discard task sequences <= n, then require contiguous later task sequences. A reader
uses leader `STREAM.MSG.GET` next-by-subject queries and a private 64-event delivery
buffer. It creates no JetStream consumer and shares no queue with another subscriber.
Idle reads poll at 100 ms. Drop cancels only that reader, never admitted execution.
Blocking results use the same reader with bounded persistence-failure reconciliation;
legacy records retain their KV-wait fallback.

A missing sequence interrupts with a reconnect error. If no next event exists,
compare confirmed publication progress with delivered task sequence and reread after
that checkpoint, so a publish between reads isn't mistaken for retention loss.
Missing terminal events are errors too, not hung or silently completed streams.
Broker failures and slow-reader lag produce the same explicit interruption. Reconnect
needs no public cursor: it starts from a fresh snapshot with exact accumulated text.

Stream retention is Limits/DiscardNew, no age or message-count eviction, configured
metadata replica count, and a finite 128 MiB byte cap. Explicit per-subject
DiscardNew caps each task at 128 messages. The same authority CAS retains 65
confirmed sequence cutoffs; cleanup preserves the latest predecessor and never
purges a pending event. Terminal cleanup uses a 30-second reader grace and durable
retirement proofs. Full capacity keeps pending authority and blocks context reuse.
Session deletion revision-fences settled authority and removes global identities,
registry and event subjects with the scoped lease. See
[`docs/a2a-operations.md`](../../../../docs/a2a-operations.md) for measured limits,
permissions, purge seq=1 behavior, capacity gaps and drain rollout.

## Upgrade and verification

Drain legacy active turns and exclude mixed writers. Old terminal records remain
readable, but a live legacy context can't be activated by new admission. Runtime
`admission_closed` readers must be deployed before writers emit that entry.

`tests/it/multi_replica/admission.rs` uses independent backends and real broker/worker
counters. Pauses cover initialization, reservation, append, mapping and activation;
lease revision deletion models loss. Retried missing work produces no LLM request.
`multi_replica/supervision/` and `e2e/supervision.rs` cover traffic-independent
recovery, actual owner SIGKILL and broker restart. `multi_replica/operations/`
covers retention, GC and service permissions. Separate-process route-alternating
deployment/load verification remains end-to-end work.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection -E 'test(multi_replica)' --stress-count=20
```
