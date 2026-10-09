# A2A replica operations

A2A backends share context authority, message identities and task events in NATS.
Any replica can admit, read, cancel or stream a task without sticky routing.
The worker execution lease remains separate from the A2A context lease.

## Provisioning and limits

Verified dependency pair: **NATS Server 2.11.6 / async-nats 0.50.0**. The lease
bucket uses the server's message-TTL and limit-marker features. Don't infer support
from the older minimum version for conditional publication alone.

Startup provisions metadata, leases and `HARNX_A2A_TASK_EVENTS`, then validates
its policy. Unsafe existing streams fail startup; harnx doesn't silently rewrite
retention settings or evict data to fit a smaller limit.

| Setting | Selected policy |
| --- | --- |
| Storage / replicas | File, configured cluster replica count (local mode R1) |
| Retention / discard | Limits / DiscardNew, `discard_new_per_subject=true` |
| Global byte cap | 134217728 bytes (128 MiB) |
| Per-subject message cap | 128, rejects new messages rather than dropping predecessors |
| Active checkpoint window | 65 confirmed events, plus at most one pending event |
| Message age / message TTL | No automatic expiry; `max_age=0`, `allow_message_ttl=false` |
| Event payload | Broker ceiling, normally 1048576 bytes (1 MiB) |
| Serialized authority | Smaller of fresh KV-stream and broker ceilings, minus 1024 bytes for CAS headers |
| Serialized new input | At most half the authority ceiling before header reserve (512 KiB at default broker limit) |
| Answer text | At most 224 KiB of JSON-encoded text at default ceiling; reduced for large retained input or a lower KV/broker ceiling |
| Reader delivery buffer | 64 events per subscriber; zero JetStream task-event consumers |

Answer sizing reserves space for four answer copies (artifact, history, status,
pending response) and authority/input overhead. JSON escape bytes count, not just
UTF-8 bytes. The complete serialized document is still checked before CAS. A model
result beyond the answer budget fails the A2A task explicitly; it isn't silently
truncated or replayed. Durable runtime history remains available through normal
session APIs. The existing 64 KiB `--max-data-part-bytes` setting controls rendered
data and inline-file parts, **not arbitrary text messages**.

Set `HARNX_A2A_EVENT_MAX_BYTES` to a positive byte count **before initial stream
creation** to choose a different cap. It doesn't resize an existing stream. Raise
an existing `max_bytes` through JetStream administration, preserving every other
validated setting. Don't lower byte or subject limits below retained data, or
change to DiscardOld, MaxAge or per-message TTL to relieve pressure.

A byte budget is an admission/backpressure boundary, not a promise that every
workload fits. Include active event history, retained terminal answers, message
headers and replication disk cost when sizing it. Metadata, transcript and blob
stores retain their existing independent policies; the 128 MiB cap bounds task
events, not total cluster disk use.

## Checkpoint cleanup and session GC

Every event commits snapshot, logical cursor and one outbox envelope in the same
context CAS. A pending event is never evicted to make room. Rolling cleanup uses
only persisted confirmed sequence cutoffs and preserves the latest subject
predecessor. It never uses a moving stream-wide `keep=N` cutoff.

**NATS 2.11.6 treats filtered purge `seq=1` as a full purge.** Harnx skips that
cutoff. For `seq>1`, purge removes messages before the frozen cutoff, not the
cutoff itself. A delayed or repeated purge can't remove newer publication.

A bounded background event scan visits at most 32 messages per pass. After a
terminal event's 30-second reader grace and positive stop/archive/mapping/final
publication proofs, it compacts that task to its final event. This applies to
archives from earlier turns too. Grace is not a delivery guarantee: slow readers
or retention gaps receive an interruption/reconnect error instead of silent loss.
HTTP disconnect doesn't cancel admitted work; dropping a reader stops only its
private pump. No durable subscriber consumers need manual teardown.

Worker session GC runs hourly only when `cleanup_remote_sessions_days` (or
`HARNX_CLEANUP_REMOTE_SESSIONS_DAYS`) is positive. Default is disabled. Session
retention keeps live message identities, including the global first-message
reservation. There is no independent short deduplication TTL.

Deletion acquires the distinct A2A scoped lease, validates the exact context
revision and revision-purges authority before deleting the transcript. It refuses
unallocated winning reservations and unresolved admission, stop, cancellation,
projection or outbox work. An expired A2A lease alone isn't proof of safe deletion.
Periodic GC skips such work even when the worker execution lease has disappeared.

After fencing, deletion removes the session prefix, task event subject,
first-message winner, recovery registry and scoped lease, alongside the existing
transcript/journal/blob cleanup. Registry goes last so partial cleanup can retry.
KV purge markers retain fencing evidence without retained message payloads. A delayed
first event (predecessor zero) can land after whole-subject GC and broker dedup
expiry; the bounded event sweep removes it only with a leader-confirmed authority
tombstone. It cannot resurrect task authority.
Deleted contexts must use a new session identity; don't reuse a purged storage key.
Unused losing allocation hints can be pruned after the existing 300-second grace,
but only without metadata, authority, lease or a matching winning reservation.

Explicit deletion uses the existing command:

```sh
harnx delete session <session-id> --agent <agent> --cluster <cluster>
```

If deletion is refused, settle the original task or restore NATS capacity and
retry. Don't remove the reservation/registry manually to bypass the refusal.

## Permissions

The executable test profile is
[`nats-permissions.conf`](../crates/harnx-a2a-server/tests/it/operations/nats-permissions.conf).
It exercises backend provisioning/CAS/leader reads/conditional publication,
actual worker startup and one execution, non-executable admission closure, worker
purge APIs, and negative denied subjects. Validate its syntax with the pinned server:

```sh
nats-server -t -c crates/harnx-a2a-server/tests/it/operations/nats-permissions.conf
```

**This is a sandbox profile, not production credentials.** Remove `no_auth_user`
and the anonymous admin, replace known test passwords with your authenticated
service identities, enable TLS and restrict network access before deployment.
Tool-specific subjects and arbitrary installed hook/tool profiles need their own
permission tests; this fixture covers the text-turn backend/worker lifecycle.

Backend additions include task-event STREAM CREATE/INFO/MSG.GET/PURGE,
`a2a.tasks.>`, metadata/lease KV APIs and watches, transcript APIs,
`cluster.*.sessions.notify`, and metadata/read-state invalidations. Worker GC needs
event-stream INFO/PURGE, scoped-lease KV access and global registry/reservation
purges in addition to its existing transcript/journal/blob permissions. Backend
stream DELETE and worker-queue consumers are denied; worker event publication is
denied. There are no task-event consumer APIs in the reader path. KV watches and
worker activation consumers are separate existing consumers.

KV keys contain slashes within one dot-delimited NATS subject token. NATS `*`
doesn't match part of that token, so this profile cannot enforce separate worker
and A2A lease paths with slash wildcards. Broad KV access is for trusted services,
not end users. Application ownership fences and the existing principal/export
AccessRules remain required. Shared-broker permissions aren't per-user task ACLs.

## Full capacity and diagnostics

On DiscardNew rejection, the same pending identity remains durable. No next event
or context reuse is allowed until it resolves. Free checkpoint-covered history or
GC only settled expired sessions; otherwise raise the event budget. Retry the
original message ID. Never rebase an event onto a missing predecessor, clear its
outbox manually, or replay a missing prompt. Broker outages can delay all confirmed
recovery.

Enable `harnx-a2a-server --metrics-addr :8456` to export bounded-label diagnostics:

- `harnx_a2a_owner_lost_total`: successful takeover of unresolved retained work.
- `harnx_a2a_pending_age_seconds{phase="admission|cancel|outbox"}`: observed ages
  of pending work, not completion latency or an exact queue-size gauge. Older
  records without timestamps use their retained snapshot time.
- `harnx_a2a_sweep_lag_seconds`: elapsed time between completed registry scan
  cycles, including scan work and idle delay.
- `harnx_a2a_events_purged_total{phase="checkpoint|terminal|deleted"}`: messages
  actually removed by safe cleanup.
- `harnx_a2a_operations_total{op="publish|recovery|sweep",outcome="ok|error"}`: operation
  attempts. Check broker event-stream bytes/message count and pending ages together.

No session, task, principal or subject is a metric label. Structured recovery and
reader-interruption logs contain those identities for incident correlation.

## Upgrade without mixed writers

1. Deploy runtime readers that understand `admission_closed` before enabling its
   writers. Old readers must not encounter that transcript variant.
2. Stop routing new turns to old A2A backends. Drain admitted turns to durable stop,
   archive, mapping and final event publication. HTTP connection drain alone isn't
   task drain. Safely close unresolved old admission before context reuse.
3. Stop all old A2A writers. Don't mix process-local and coordinated writers, or
   old/new retention implementations writing the same contexts.
4. For an older event stream lacking per-subject DiscardNew, first verify all
   retained subjects are settled and checkpoint-compact terminal history while
   preserving each predecessor. Only then provision the validated policy. Never
   lower the subject cap below retained counts. Startup fails until this is done.
5. Start coordinated backends on the same NATS account/cluster and export set,
   verify permissions and recovery diagnostics, then scale behind ALB/ingress.
   Keep exports configured until their unresolved work settles.

Recovery is eventual and lease-TTL-based (default A2A lease 30 seconds, renewal
10 seconds), with **no fixed failover SLO**. The successful context CAS is the
ownership fence, not wall-clock expiry alone. Broker uncertainty extends recovery.
Interrupted work is never automatically replayed. Already completed tool side
effects are not rolled back.

## Measured scope and capacity gaps

Task7 tests use Linux light sandbox, actual NATS 2.11.6 file storage and R1. The
224 KiB answer case emits 69 chunks through a real worker and independent reader,
retains 65 events at about 478 KiB, and completes in about 9.5 seconds (100 ms
coalescing plus test barriers). Checkpointed authority is about 690 KiB; the full pending terminal authority
measured **919805 bytes**. A separate real CAS succeeds at **1047552
serialized bytes**, rejects larger values without revision change, and verifies a
lowered 128 KiB KV limit is read freshly. Tiny-capacity and 128-message subject
saturation probes reject new publication, preserve predecessors and recover after
safe purge. These are regression/load probes, not a production throughput benchmark.

Not measured: sustained concurrent sessions/subscriber fanout, R3 latency/disk
amplification, cross-zone network faults, deployment ALB drain behavior, and every
installed tool/hook's permissions. Raw task readers currently poll at a 100 ms
idle cadence, so broker request load grows with subscriber count. Run deployment
load tests and choose budgets for your retention horizon before claiming capacity.
