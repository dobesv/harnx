# Tool Confirmation Guide

Use middleware `confirm` to pause a tool call for an approve/deny decision. It runs
inside the worker, including unmanaged/cloud workers; no hook-server process is
needed. The script decides what approval runs and what denial returns to the model.
Legacy `PreToolUse` approvals still work for existing configurations and sessions.

## Quick start

Add middleware globally in `config.yaml`, in agent Markdown front matter, or in
a tool-server YAML file. This is the confirmation from the shipped demo agent:

```yaml
middleware:
  - name: confirm-time
    match: '^time_get_current_time$'
    bash: |
      if confirm --id time 'Manual approval required'; then
        next --id dispatch
      else
        deny 'Time lookup denied by the user.'
      fi
```

`match` is a regex against the tool name. Use anchors for an exact tool, or `.*`
to match every call. The default `on_error: deny` fails closed on script errors.
A denial here is an error result with the script's text, not a script failure.
Use an `if` branch: a bare Bash `confirm` that returns false follows `set -e`.

The same list works in an agent's front matter alongside `model` and `use_tools`.
Global, tool-server/package and agent entries compose in that order. See the
[Middleware Guide](middleware-guide.md) for ordering, overrides, Python, helper
tool allowlists and timeouts.

### Confirm an Atlas handoff

Handoffs are tool calls too. Pantheon's Daedalus agent ships this exact policy:

```yaml
middleware:
- name: confirm-atlas-handoff
  match: '^atlas_session_handoff$'
  bash: |
    if confirm --id handoff 'Hand off this plan to Atlas for execution?'; then
      next --id dispatch
    else
      deny 'Atlas handoff denied by the user.'
    fi
```

Approval reaches handoff dispatch; denial returns `Atlas handoff denied by the
user.` as an error result without starting Atlas. Handoff activation waits until
all calls in the round finish, including siblings still awaiting input.

## What suspends and resumes

1. The model requests a tool. Existing preparation and legacy PreToolUse policy
   run before middleware.
2. With no accepted answer, `confirm` stops the interpreter and requests a modal
   interaction tagged `harnx.kind: confirm`. Printing JSON cannot forge it.
3. The worker persists the private checkpoint and request, settles or durably
   classifies all in-flight siblings, then writes `SuspensionReady`. Frontends
   don't show requests before that manifest exists.
4. The frontend submits the interaction ID, exact request sequence and response.
   The owning worker durably accepts it before resuming. Identical retries are
   idempotent; conflicting, stale and mismatched replies are rejected.
5. Only the answered call reruns, from the beginning of its middleware chain.
   Completed siblings are restored, not dispatched again. Unanswered calls wait.
   One final `ToolResults` entry combines all results in original order before the
   model continues.

Suspension survives frontend reconnects and worker moves. Only one request is
shown at a time, in the latest manifest's order. An unknown interaction kind
shows **not supported by this client** and permits denial only.

The script timeout includes time spent in `next`, but not human response time:
the interpreter isn't running while suspended. The invocation's execution deadline
keeps counting. Approval doesn't reset or extend it, and a late response can't
revive a cancelled or completed round.

### Answers, step keys and saved results

`confirm --id handoff ...` returns success for approval and failure for denial
(Python `confirm(..., id='handoff')` returns `True`/`False`). `answer handoff`
(Python `answer('handoff')`) reads the accepted response as
`{interaction_id, action, data, note?}`, or exits 1 / returns `None` if absent.
Answers come only from runtime-accepted user responses, never tool arguments or
saved values. Notes aren't automatically added as user messages; a script can
read them through `answer` and decide what to return.

Use explicit `--id` / Python `id=` keys when reruns can skip or reorder `next`
calls or confirmations. Unlabelled operations use separate 1-based ordinals in
each frame; loop iterations need distinct keys. Keys identify logical steps, not
cached executions: **every `next` is fresh**, including after approval.

For a post-tool question such as "Keep this result?", use `save`/`saved` to avoid
running the tool again on resume. Save the result envelope before requesting input
and restore it on the next attempt; this keeps its host-owned `ref` and media.
The [tested Bash and Python save/restore example](middleware-guide.md#keep-a-downstream-result-without-running-the-tool-twice)
shows the complete pattern and a denial that says the command already ran.
Stash is bounded to 256 KiB per frame and the checkpoint to 8 MiB per call.

## Answer in each frontend

### Serve and web

The web pending card shows the middleware message, Approve/Deny and an optional
note. It hydrates after reconnect. It submits JSON-RPC
`session/interaction_response` to the owning agent/session URL with params
`{interaction_id, request_seq, response: {action: 'approve' | 'deny', data: {}, note?}}`.
Use the mandatory ID and positive sequence from the displayed request, not a tool
call ID alone. The response is `{applied: true, outcome: 'applied' | 'already_applied'}`.

AG-UI CUSTOM `pending_interaction` carries
`{interaction_id, request_seq, tool_call_id, kind, message, pending_count}`;
`pending_count` counts further queued requests. A null value clears it. Session
state uses `awaiting_approval` with interrupt type `interaction`. Rejections are
visible on the card; RPC error `-32004` has message `interaction response rejected`
and an outcome such as `stale`, `wrong_sequence` or `conflicting`. Refresh rather
than retargeting a rejected answer to a newer request. Existing session access
checks apply. See [serve interaction protocol](../crates/harnx-serve/README.md#middleware-confirm-interactions).

### TUI

Middleware requests use the **Middleware confirmation** tray during a local turn
or when attaching to a NATS session. It opens only after durable suspension is
ready and shows one request at a time.

- Type or paste an optional note. `Shift+Enter` or `Alt+Enter` adds a newline.
- Press `Enter` to approve after two seconds without typing, or `Ctrl+D` to deny.
- Use `PageUp`/`PageDown` to scroll a long message.
- Press `Ctrl+C` to interrupt through the normal session cancellation tray.

The note is stored in `InteractionResponse.note`, not queued as a separate user
message. Rejected answers appear in the tray or transcript. A request answered
elsewhere disappears on durable refresh. Closing the frontend doesn't itself
approve or deny the durable request; reconnect to answer remaining work.

For direct child sessions awaiting approval, press lowercase `a` on the
selected root child row, root detail pane, or opened child header displaying
`⏸ Awaiting input: <message>  a/answer` to answer directly without navigating away.

### A2A

A2A parks the **existing task** at `TASK_STATE_INPUT_REQUIRED` with a text challenge
showing the middleware message. Blocking sends return there and streams close;
the task isn't terminal. Reply with `SendMessage` or `SendStreamingMessage`,
preserving `contextId` and normally `taskId`. Context-only replies answer that
context's current challenge; task-only replies also work. Mismatched IDs fail.

The first whitespace-delimited word selects the decision, case-insensitively:

| Decision | First words | Example |
| --- | --- | --- |
| Approve | `y`, `yes`, `go`, `approve` | `yes audit note` |
| Deny | `n`, `no`, `reject`, `deny` | `no deny note` |

Trailing `.`, `!` and `,` on the first word are accepted. Remaining text becomes
the interaction's optional note. For middleware, approval and denial notes stay
in the response; the script controls whether they're returned to the model.
(Legacy hook denial notes are included in the synthetic blocked-tool result.)
Only text parts count as decisions. Unrecognized words, data parts and mixed
text/data messages restate the challenge without calling the model or answering.
There is no structured JSON answer format.

Keep the reply's `messageId` on retries. Its receipt is bound to the exact
challenge, so a duplicate can't answer a later request. Use a new ID for a new
answer. Replies can reach any replica and resume the same invocation; decisions
made from web/TUI are also discovered. `CancelTask` works while parked.
See [A2A interactive approval](../crates/harnx-a2a-server/README.md#interactive-tool-approval)
for wire examples and server-version deployment constraints.

### ACP

ACP maps confirm to `session/request_permission` with **AllowOnce** and
**RejectOnce** options. Explicit dialog cancellation denies; a transport
disconnect leaves the request pending for reconnect/resume. An unsupported kind
offers only rejection. ACP's permission outcome has **no free-form note field**,
so ACP responses store no note. Session cancellation prevents a late AllowOnce
from reviving the turn. See [ACP permissions](../crates/harnx-acp-server/README.md).

### CLI

In non-interactive command-line mode (`harnx prompt`), sub-agent progress displays
pending interactions for visibility but does not support interactive answering
([#1865](https://github.com/dobesv/harnx/issues/1865)):

- A nonterminal `SubAgentProgress` carrying `pending_interaction` formats with
  status `awaiting` and the bounded message (up to 512 UTF-8 bytes):
  `[sub-agent] awaiting agent=<agent> session=<id> elapsed=... in=... out=... cached=... tools=... message=<msg>`
- Terminal events (`done`, `failed`, `cancelled`) are never rendered as awaiting,
  even if a pending interaction was previously displayed.
- CLI is strictly display-only: it provides no input prompt or command to answer
  child session approvals. Answer pending requests through the Web UI, TUI, or by
  submitting an exact decision to the child session's JSON-RPC endpoint.

## Direct child sub-agent confirmations

When a sub-agent pauses awaiting human approval (either legacy hook approvals or
middleware confirmations), direct child pending interactions are surfaced to
parent frontends.

### Web row and TUI interaction

- **Web UI**: Parent session sub-agent rows display pending interactions for
  direct child sessions. Users can approve or deny legacy hook approvals and
  middleware confirmations directly from the parent row without navigating away.
- **TUI**: Press lowercase `a` (`a/answer`) to answer a pending interaction
  for a direct child. Active when focused on a selected root child row, viewing
  passive root detail, or from the first opened child's header. Displays the
  marker `⏸ Awaiting input: <message>  a/answer`. For legacy `ask` or
  confirmation, users can approve, deny, or enter notes. An unsupported kind
  directs the user to open the child session; no answers are accepted from the
  parent view.
- **CLI**: Display-only ([#1865](https://github.com/dobesv/harnx/issues/1865)).
  CLI nonterminal progress reports `awaiting` with the bounded message; no
  interactive answering is supported from CLI mode.

The TUI uses one confirmation modal at a time. A child confirmation waits behind
any foreground modal, then rechecks that the displayed request is still pending.
Parent-row notes are audit notes on the child's decision, not user messages queued
in the parent's composer. `Ctrl+C` in a child modal interrupts the child, never the
parent turn. The normal foreground legacy modal keeps its existing queued-message
behavior.

### Exact-generation semantics and ApprovalRef

Provider tool-call IDs alone are not globally unique across turns or retries.
To prevent answering a superseded or subsequent request that reuses the same
tool-call ID, legacy approvals use exact-generation semantics.

The parent row sends the complete `ApprovalRef`:
- `invocation_id`: Child invocation ID.
- `tool_round_seq`: Tool round sequence number.
- `request_seq`: Durable approval request sequence number.
- `tool_call_id`: Tool call ID.

Post this displayed reference to the **child's** session URL using
`session/hitl_decision_exact`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session/hitl_decision_exact",
  "params": {
    "request": {
      "invocation_id": "...",
      "tool_round_seq": 10,
      "request_seq": 11,
      "tool_call_id": "..."
    },
    "approved": true,
    "note": "Reviewed the request"
  }
}
```

Normal agent and session access checks run against the child before any decision.
For middleware `confirm` summaries, submit `session/interaction_response` to the
child URL with the displayed `interaction_id` and `request_seq`.

### Exact RPC outcome mapping

`session/hitl_decision_exact` returns `{applied: bool, outcome: string}` with
strict outcome semantics:

| Outcome | `applied` | Meaning and UI behavior |
| --- | --- | --- |
| `applied` | `true` | Decision was accepted and applied to the exact generation. Clear the row's pending state. |
| `already_applied` | `true` | Idempotent retry. If recorded `approved` matches the submitted decision, treat as success; if conflicting, show that it was answered differently elsewhere. Includes recorded `note`. |
| `conflicting` | `false` | A different decision was already accepted for this request; show the recorded answer. Includes recorded `approved`. |
| `stale` | `false` | The request is no longer pending (e.g. turn completed, timed out, or replaced by a new round). Newer requests that reuse the same tool-call ID remain untouched. |

A missing or unrecognized outcome (such as a bare `{applied: true}` without an
`outcome` field) is treated as an error.

### Older servers: NO fallback to legacy method

If `session/hitl_decision_exact` returns JSON-RPC method not found (error `-32601`
or unknown method) because the server is running an older binary without
exact-generation support, frontends **MUST NOT** fall back to
`session/hitl_decision`.

Legacy `session/hitl_decision` resolves by `tool_call_id` against whatever
request is active on the child session, introducing a race condition where a
parent answer could approve an unintended newer request. Instead, the UI directs
the user to open and inspect the child session directly.

### No grandchild propagation

Pending interaction summaries are strictly limited to direct children (`depth = 1`).
If a child agent delegates to a grandchild agent, that grandchild's pending
interaction is owned and observed by the child session; it is **never**
propagated up to the parent or grandparent session. Neither frontends nor
progress reporters perform recursive descendant traversal.

### Parent stays running

The child owns the request. When a child suspends waiting for user input, the
parent session does **not** suspend or pause. The parent stays running, retains
its own worker lease, and continues its turn or observation loop. Frontends send
answers directly to the child session; the parent agent doesn't answer for you.

### Synchronous delegation and summary freshness

Synchronous `{agent}_session_prompt` returns a non-error blocked handle
(`status: "blocked", terminal: false`, containing `pending_interaction`) when a
child requires human input, allowing callers to inspect the pending request and
later resume observation with `{agent}_session_wait` after the user answers.

Child pending summaries are re-derived from the child's durable log on live
progress updates and during durable catch-up. Reconnects and refreshes hydrate
authoritative state. Changes publish on the next durable poll without waiting for
a heartbeat interval. A successful parent-row answer clears only that displayed
request generation; a newer pending request is left untouched.

## Inner scripts and operator consent

Middleware wraps model calls, inner calls from the model's Bash tool and operator
calls. An inner-script call reaching `confirm` returns
`interaction required, can't be answered inside a script`, even under
`on_error: allow`. It doesn't suspend: replaying the whole model script could
repeat earlier effects. Ask the model to call the protected tool directly.

Operator **root** calls (`harnx call tool`, TUI `.call tool`) auto-approve confirm
under explicit operator consent. Denial and argument/result rewriting still apply.
Other interaction kinds fail without an operator UI. Don't confuse this with
legacy hook `ask`: legacy operator Bash inner calls auto-approve `ask`, but
middleware inner calls can't suspend. Middleware's own `tools:` helpers bypass
the chain to prevent recursion, while retaining admission and cancellation checks.
Operator consent doesn't grant approval to child agent sessions. Control tools
such as `*_session_handoff` aren't available inside model scripts.

## Recovery and deployment safety

An **interrupted** or **outcome unknown** result means effects may already have
happened. If a worker crashes after a physical dispatch starts but before the
middleware reaches its next durable boundary, recovery doesn't rerun that
unfinished attempt. A saved leaf reply isn't a final middleware result; even a
crash after the reply but before `save` must not redispatch it. Check external
state before manually retrying. Completed outer results and siblings are reused.
`save`/`saved` isn't an exactly-once transaction with external tools.

Deploy **readers before writers**: every worker, serve, A2A, ACP and TUI reader
must contain the T2.1 record readers/guards before enabling a T2.2 durable worker.
**Rollback below T2.1 is unsupported once any T2.2 worker has run.** A T2.1 worker
can read but parks T2.2-owned rounds and pending marked handoffs until a newer
worker resumes them. Keep `harnx_interaction_checkpoints` reachable from every
resuming worker. Store read errors block whole-round recovery instead of allowing
redispatch; missing referenced checkpoints produce interrupted/unknown errors.

**Drain in-flight turns when upgrading a middleware-running Phase 1 or T2.1
worker to T2.2**, before changing its middleware. Those older workers left no
round-start/attempt evidence, so removing or renaming middleware could hide prior
effects. Rounds started by T2.2 carry the marker and don't require that drain for
mixed T2.1/T2.2 ownership. See [rollout and crash details](middleware-guide.md#cluster-rollout-and-rollback)
for limits, retention and recovery classification.

## Legacy hook approvals

Existing `hooks:` entries returning `permissionDecision: ask` still use the legacy
HITL path. Its durable readers stay supported for old sessions. New policies
should use middleware; don't change old session records to interaction records.
The legacy serve method `session/hitl_decision` is unchanged and must not receive
middleware responses.

The legacy TUI modal shows full arguments as multiline YAML and, when available,
a rendered transcript template (`Ctrl+F` toggles). `PageUp`, `PageDown` and the
mouse wheel scroll. `Enter` approves after two seconds idle; `Ctrl+D` rejects;
`Ctrl+C` rejects and interrupts, restoring text as a draft. Unlike middleware
notes, a nonempty legacy note is durably queued as a user message before the
confirmation reply. If enqueue fails, the modal keeps the draft for retry.
Legacy hook `deny` remains enforced for model and operator calls.
See the [Hooks Guide](hooks-guide.md) for compatibility details.

## Demo and verification

The shipped `demos/config/agents/tool-confirm-agent.md` is tested for both
approval and denial. The tape approves a time lookup with `Enter`, then denies
another with `Ctrl+D`. Render it with `./demos/render.sh tool-confirm` (output:
`demos/out/tool-confirm.gif`).

Configuration examples above are read from this guide and compared with the
shipped agents by integration tests; both approval and denial execute the real
middleware script. Daedalus's shipped confirmation also runs through the TUI and
A2A worker harnesses. The middleware guide's examples run verbatim in the runtime
test suite.

Related: [Agent Guide](agent-guide.md), [Middleware Guide](middleware-guide.md),
[Configuration Guide](configuration-guide.md).
