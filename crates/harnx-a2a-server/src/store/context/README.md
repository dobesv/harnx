# Context authority primitives

The runner and HTTP admission path now claim contexts and use fixed runtime tickets.
First-message reservations precede session effects; follow-up admission checks the
active document, not a local slot or task index. Don't activate these APIs on a
context with a live legacy writer. Rollout must drain old turns and exclude mixed
old/new writers. Foreground/background recovery, remote cancellation and a
per-update durable outbox support safe context reuse. Independent readers consume
committed events. See `../../runner/README.md`.

## Ownership and writes

`NatsSessionLease::acquire_scoped(params, "a2a")` uses
`sessions/{storage_key}/a2a/lock` in `harnx_leases`. `acquire` and
`acquire_for_execution` still use `sessions/{storage_key}/lock`; worker activity
refresh and execution binding are unchanged. Scoped acquisitions don't refresh
worker activity or bind an execution. Scope accepts one nonempty alphanumeric,
underscore or hyphen segment.

`prepare_context_claim(ContextIdentity, lease, operation_id)` verifies the scoped
key/bucket and renews the prospective lease, then reads the authority from the KV
stream leader. Successful CAS of `sessions/{storage_key}/a2a/context` establishes
the owner, not the lease check. Epoch increments inside that document. The stable
lease *acquisition* revision orders claimants; the moving renewal revision is not
an owner epoch. A delayed prepared claimant cannot overwrite a later document CAS.
A newer lease holder can take over while preserving every active work identity.

Owner mutations use `ContextVersion` (boot ID, epoch, exact task ID or idle state,
and bucket revision). `prepare_context_update` validates those against the document
it reads. Mutation closures receive only `ContextState`, not owner/epoch/receipt.
`commit_context` writes that same document at the immutable predecessor revision.
No lease-check-then-unfenced-task-write path exists. Reads don't infer ownership
from local runner handles.

Boot IDs and operation IDs are trusted internal identities, not HTTP parameters.
Callers must resolve export/principal binding before these storage-key APIs, as
with other trusted store methods. Use the store and lease from the same broker.

## Ambiguous acknowledgements

Retain the exact `ContextWrite` until commit is resolved. It fixes the predecessor,
operation ID and canonical full-document SHA-256 digest, including work identities.
Its fields are private; callers cannot deserialize or edit a prepared ticket.

After any failed CAS acknowledgement, `resolve_context_write` makes a leader read.
A matching receipt and digest proves this exact operation committed. Repeating its
CAS returns the original revision without another write. Reusing its ID with a
different payload/predecessor fails. If another operation or owner replaced the
receipt, resolution fails closed with a conflict; it does not prove the old operation
never applied. Inspect the current authority/work identities during later recovery.
Never reprepare/rebase an uncertain operation or replay its runtime side effects.
The receipt is bounded (latest operation), not an unbounded operation journal.

`prepare_context_release` CAS-clears only the owner. It retains epoch, lease claim
ordering and active work so a successor can recover. It never deletes the document.
A deleted/purged authority returns `AuthorityError::Deleted`; a claimant cannot
reset its epoch on a retained tombstone. Deleted sessions need a new session identity.

## Active state and projections

The document includes active `TaskRecord`, message ID/fingerprint, stable invocation
and prompt IDs, fixed log predecessor/admission phase, exact cancellation intent,
logical publication cursor/subject predecessor and pending outbox event identity.
Runtime fixed admission, exact interruption and durable publication use these fields.
Task cursor now serializes with a zero default for old terminal records.

Message identity and admission ticket stay immutable within a task. Snapshot changes
increment task-local revision inside the prepared CAS. Once terminal, the snapshot
(including its final cursor) is immutable. A task can be retired/replaced only after
stop confirmation, archive/message/final-event checkpoints and no pending event.
New task snapshots start at revision 1; archived/legacy terminal IDs can't be reused.

`archive_context_terminal` projects only a leader-read committed terminal snapshot
into `sessions/{storage_key}/a2a/archive/{uuid}` with create-only CAS. Any replica,
including a former owner, can finish this projection. Retries must match existing
content; they cannot overwrite it. Setting the archive checkpoint validates actual
archive content before the owner CAS. Stop/message/final-event checkpoints require
durable runtime/projection/publication proofs. Isolated schema tests set checkpoints explicitly; runner integration tests
exercise their actual adapters.

`get_task` prefers the active authority, then immutable archive, then legacy terminal
records. Activated contexts reject legacy task create/update paths. Index repair
re-reads authority rather than trusting the supplied snapshot, and cleanup recognizes
active documents. Index writes remain repairable projections, never ownership proof.
`watch_task` watches the context key for activated contexts (legacy task key otherwise),
so terminal waiters don't wait on an unwritten legacy key.

Authority/archives share session retention. Deletion takes the distinct scoped
lease, refuses unsettled work and revision-purges authority before removing its
log, global first-message identity, registry and event subject. Checkpoint-covered
history cleanup retains subject predecessors. See `../../../../../docs/a2a-operations.md`.
Single-document payloads are checked against stream/server limits before publication;
missing server limit metadata falls back to 1 MiB, matching runtime conventions.

## Verification

`tests/it/context_authority*` uses two independent stores and real isolated JetStream.
CAS races prepare both tickets before either commit. Expiry waits for a broker KV TTL
marker; renewal loss waits for the real lost-watch. Feature-gated faults suppress a
successful context acknowledgement and pause between broker CAS and ack resolution.
Restricted-account tests forbid DIRECT.GET while exercising authority leader reads.
No sleeps choose the interleaving.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-a2a-server --features fault-injection -E 'test(context_authority)' --stress-count=20
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-runtime -E 'test(nats_lease)'
```

## One-event outbox

Each emitted update stages snapshot + cursor + immutable envelope in one owner CAS.
Pending publication freezes snapshot and prevents another event. Conditional task
subject publication uses its original predecessor and stable commit ID; acknowledgement
repair reads retained identity, never rebases. Same-owner stage/clear retries inspect
stable envelope/checkpoint after a superseding cancellation receipt. A stale owner
can publish an already-committed envelope but can't mutate successor authority.
Authoritative readers capture global watermark before snapshot and inspect confirmed
publication progress to detect retention gaps, including a missing terminal event.
See `../../runner/README.md` for handoff and retention boundaries.
