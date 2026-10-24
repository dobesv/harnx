# Fixed runtime admission

Coordinated frontends use `NatsSession::prepare_fixed_admission`,
`append_fixed_prompt`, `resolve_fixed_admission` and `close_fixed_admission`.
Ordinary runtime admission keeps its existing steering and worker lease behavior.
The A2A runner uses these APIs after its owner-checked context CAS. Shared first-message reservations and foreground recovery preserve the ticket; background supervision restores the same ticket. Remote event readers are separate work.

## Persist before append

1. Allocate stable invocation, prompt and closure IDs. Invocation ID must be a
   KV-safe alphanumeric/underscore/hyphen segment. Use the agent-scoped storage key,
   not the public local session ID.
2. Capture a ticket before runtime admission. Persist it in the frontend's owner-
   checked same-document CAS. Only the CAS winner may call `append_fixed_prompt`.
3. Retain every ticket field. A retry uses the original ticket and original content.
   Never call prepare again to repair a missing or uncertain prompt.

Task2's A2A `AdmissionState` carries invocation ID, prompt ID, `fixed_predecessor`
and optional closure ID. Set closure ID before the reservation CAS. Restore with
`FixedAdmissionTicket::from_parts(storage_key, invocation_id, prompt_id, closure_id,
fixed_predecessor)`; missing closure ID is a recovery error, not permission to mint
another identity. Alternatively persist the serialized runtime ticket directly.
`ContextDocument.local_id` is the public local ID, not the storage key.

The immutable runtime `InvocationAdmission` saves the fixed ticket and original
content before executable append. Prompt binding is create-only. Reusing an
invocation with different fixed fields or content fails, including after completion.
Ordinary prompt append refuses fixed admissions, so ordinary repair can't rebase them.

## Resolve or close, never replay

Both prompt and `AdmissionClosed` append with the SAME predecessor through
`NatsSessionLog::append_fenced`. Each uses a stable publication ID. Every outcome,
including duplicate acknowledgement, conflict and uncertain acknowledgement, is
resolved against durable raw identity and original content. Transcript edits don't
change the original admission. An unavailable broker is an error, not proof of closure.

- `Pending`: no successor exists. An explicit owner append may still compete;
  recovery calls close, never append.
- `Admitted { prompt_sequence }`: prompt won. `fixed_prompt_handle` returns the
  same invocation/prompt for observation through `follow_admitted_prompt`.
  Recovery must inspect durable completion or interrupt this exact invocation.
  **Admitted does not mean stopped.** Don't mark a stop checkpoint merely because
  close returned Admitted, and don't automatically activate interrupted work.
- `Closed { closure_sequence }`: closure won. No executable prompt exists.
- `Fenced { sequence }`: unrelated retained entry won the predecessor. Missing
  prompt cannot land. No closure is appended at a newer tail.

Admission-head checks recognize Closed/Fenced unbound reservations as terminal,
allowing another invocation instead of leaving the session busy. A closure is
neither Cancel nor Error: it doesn't cover another pending prompt or cancel tools.
It is ignored by conversation reconstruction and live rendering and doesn't shift
logical history indices. Raw history queries expose `admission_closed`.

Admission resolution requires the retained transcript. Missing streams, a removed
successor, or an empty stream with an advanced sequence fail closed. Append/close
use open-existing-only handles; they cannot recreate a GC-deleted transcript.
Session GC must retire frontend authority with its log and must not reuse the same
storage identity. Partial retention that removes admission evidence requires an
explicit migration/settlement policy, not recovery replay.

## Observe approval progress

`NatsSession::fixed_prompt_progress(&ticket)` reads one retained log snapshot and
validates the saved admission against it. It requires an admitted ticket; pending,
closed, fenced, or mismatched tickets return an error. It doesn't append or activate
work. `PromptProgress` distinguishes:

- `Running { last_decision }`: no terminal or pending approval. The optional
  `HitlDecisionReceipt` records the exact request, decision sequence, answer, and note.
- `InputRequired(PendingHitlApproval)`: the invocation is suspended for approval.
- `Terminal(NatsTurnResult)`: durable completion, cancellation, or worker failure.

These types and `ApprovalRef` are exported from `harnx_runtime` and
`harnx_runtime::nats_session`. Build a durable reply target with
`pending.approval_ref(ticket.invocation_id())`. `PendingHitlApproval` and
`ApprovalRef` support JSON serialization and equality comparisons.

Scoping starts at the ticket's immutable root prompt and ends at its first raw
terminal proof. Fixed admission prevents a successor invocation before that proof.
Both the request and its `ToolCalls` round must be after the root prompt. Checking
only the request sequence could attach a late request from an older round. Raw
terminal proof wins over any remaining approval and over approvals in later
invocations, even after transcript edits or rewind. Pending requests and decision
receipts respect effective transcript mutations and are joined within their own
round, so a reused provider tool call ID doesn't inherit an older decision.
Tool name and arguments come from persisted `ToolCalls`, not unpersisted hook edits.

## Follow one approval segment

`NatsSession::follow_admitted_prompt_until_boundary` takes an existing
`AppendedPrompt`, the same `Arc<dyn AgentEventSink>` and optional cancel receiver
as `follow_admitted_prompt`, and `FollowBoundaryOptions`. It returns public
`FollowBoundary::InputRequired(PendingHitlApproval)` or
`FollowBoundary::Terminal(NatsTurnResult)`. Both boundary types are exported from
`harnx_runtime` and `harnx_runtime::nats_session`. No confirmation subject is
accepted: activation always passes `None`, so the worker defers tool approval.

For a new A2A segment, append the fixed prompt once, obtain its handle with
`fixed_prompt_handle(&ticket)`, and follow with `FollowBoundaryOptions::default()`.
On `InputRequired`, save the approval reference and publish the challenge. Don't
close admission, interrupt the invocation, or treat suspension as a completed turn.

After delivering a decision for Q1, reconstruct the handle from the **same ticket**
and call the same follow method with:

```rust
FollowBoundaryOptions {
    after_request_seq: Some(q1.request_seq),
    run: original_run_options,
}
```

`after_request_seq` is a checkpoint watermark, not an event-log offset. Decisions
are excluded by durable request derivation, so a decided Q1 can't reappear. A
still-undecided request at or before the watermark is returned again rather than
silently skipped. A next approval already written before attachment is returned
from history. Terminal proof always wins, even below the watermark. Don't use this
field as permission to target a newer request with an older reply.

Attachment subscribes before reading history. Typed follow then refreshes durable
facts before activation, including when reusing an admission-time subscription.
An already-pending approval or terminal result needs no activation. Otherwise it
activates the original prompt and follows until a durable boundary. It reuses the
legacy event loop, generation/cancel fencing, logical-sequence decoration, and
final buffered-event drain. `follow_admitted_prompt` retains its existing return
value and compatibility behavior.

Text arrives at the supplied sink as `AgentEvent::Model(ModelEvent::MessageChunk)`
throughout each live segment, including a resumed segment. Reuse the publisher's
sink/inbox and preserve accumulated artifact text between segments. Like the
legacy follower, this API doesn't replay historical text or make advisory chunks
durable. A publisher attaching after output was already emitted must recover
persisted `ToolCalls.text`/assistant messages and reconcile its artifact; terminal
`NatsTurnResult.response` contains the final assistant response, not all prior
segment text. The approval watermark doesn't deduplicate artifact output.

A typed `Terminal` requires a durable terminal entry or confirmed cancel receipt.
Log failures, a closed event stream without a boundary, or an orphaned worker
without terminal proof return an error, not a synthetic successful completion.
The existing `RunTurnOptions` are carried in `FollowBoundaryOptions.run`; following
never admits a new prompt or creates a fresh run/deadline.

## Decide an exact approval

Call `NatsSession::decide_hitl_approval_for(&reference, approved, note)` with the
saved `ApprovalRef`, not a fresh reference selected during retry. It returns public
`HitlDecisionOutcome` (exported alongside the progress types):

- `Applied`: the request was pending when inspected, an activation was published
  for its original invocation, and a matching durable decision was then observed.
  This includes recovery after a lost acknowledgement and a concurrent same-answer
  decision. It proves the answer, not which caller wrote it.
- `AlreadyApplied { approved, note }`: the same answer was already durable, the
  request hasn't been superseded, and the invocation is non-terminal. Activation
  is published again before returning. The original note is returned; retry notes
  don't replace it. Idempotency compares the boolean answer, not the note.
- `Conflicting`: this exact request has the opposite durable answer. A conflict
  found at entry doesn't activate work. A conflict found after a delivery race
  can follow an activation already published for that original invocation.
- `Stale`: invocation identity doesn't match, the request was removed or replaced,
  a different approval is current, or the invocation is already terminal. A stale
  reference found at entry neither activates nor sends a decision. If replacement
  happens during delivery, the earlier activation is still pinned to the original
  prompt; no retry retargets the replacement request.

Terminal proof and a newer pending request win over historical decisions at API
entry. After an attempt starts, a matching commit for that exact request can still
return `Applied` even if the worker has since finished or reached the next gate.
Recovery should query `fixed_prompt_progress` after every outcome before projecting
current state. Broker, log, activation, and unresolved delivery failures return an
error rather than inventing an outcome. Activation publishes existing work; it
neither appends a prompt nor guarantees a worker has already resumed.

The tool-ID-only `decide_hitl_approval` API remains for web callers. It selects the
current generation once and uses the fenced path, but keeps its historical boolean
idempotency after completion. Its already-decided live path now reactivates work.

Workers check optional `ControlCommand::HitlApprovalDecision.request_seq` against
the current request and use the same snapshot's stream tail for the lease-fenced
CAS append. A replaced generation or intervening write can't inherit the decision.
Missing `request_seq` retains legacy tool-ID routing; old receivers ignore the new
field. Wire compatibility does **not** add fencing to old workers. Upgrade workers
before enabling exact-reference decisions in coordinated frontends.

## Deployment and tests

`AdmissionClosed` is a new durable transcript entry. Older readers reject it.
Deploy readers that understand it before enabling writers. A2A still requires a
hard cutover excluding legacy writers when Task4 activates coordinated admission.

Tests use real isolated JetStream, explicit pauses before fixed publication and
suppressed acknowledgements after real successful publication. New API tests fail
if nats-server is unavailable. No sleeps select race outcomes.

```sh
PROTOC=/usr/bin/protoc HARNX_TEST_SANDBOX=light cargo nextest run -p harnx-runtime -p harnx-a2a-server --features harnx-a2a-server/fault-injection -E 'test(nats_fixed_admission) | test(context_authority::fixed_admission)' --stress-count=20
```
