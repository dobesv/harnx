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
