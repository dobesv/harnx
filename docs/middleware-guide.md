# Tool middleware

Middleware wraps tool calls with in-process BashKit Bash or Monty Python scripts.
Use it to rewrite arguments, deny a call, transform its result, or call trusted
helper tools. It runs in the worker, including cloud/unmanaged workers without
`manage_servers`; it doesn't launch a hook-server process. When the worker
manages tool servers locally, servers hosting helper tools start automatically on
session activation without listing them in the agent's `use_tools`. On unmanaged
workers, helper servers must run externally and be available to the worker.

Lifecycle `hooks:` are in-process event scripts now; see the [Hooks Guide](hooks-guide.md).
To call an existing Claude hook command from middleware, use the
[Claude compatibility tool](middleware-claude-hooks.md).

## Configure middleware

Add a `middleware:` list to global `config.yaml`, agent Markdown front matter,
or tool-server YAML. This global example injects an environment variable into
`bash_exec` and adds a field to its JSON result:

```yaml
middleware:
  - name: label-bash
    match: '^bash_exec$'
    bash: |
      args=$(tool_input | jq '.arguments | .env = ((.env // {}) + {"MIDDLEWARE_LABEL":"reviewed"})')
      result=$(next --id run "$args")
      respond "$(echo "$result" | jq '.output.reviewed = true')"
    tools: []
    timeout: 60s
    on_error: deny
```

| Field | Meaning |
| --- | --- |
| `name` | Required, nonempty name, unique within each level. Used in overrides and diagnostics. |
| `match` | Required regex matched against the tool name, not its arguments. Use anchors for exact matches. |
| `bash` | Inline BashKit source. |
| `python` | Inline Monty Python source. |
| `script` | Script file with `.sh`, `.bash`, or `.py` extension. |
| `tools` | Helper tool selectors, same syntax as `bash_tools`; empty by default. Starts required servers under local worker management. |
| `fetch` | Exact HTTP(S) host allowlist for this entry, e.g. `[policy.example.com]`. Empty or omitted denies all fetches. See controlled fetch below. |
| `timeout` | Positive duration string, default `30s`. Includes downstream execution time. |
| `on_error` | `deny` (default) or `allow`; see error policy below. |
| `enabled` | Defaults to true. A disable entry has only `name` and `enabled: false`. |

A full entry requires exactly one of `bash`, `python`, or `script`. Unknown
fields are errors. `bash` and `python` are source text, not file paths.
`script` paths are relative to the declaring configuration file's directory;
package declarations resolve from the package root instead. Files must be
readable in the worker's configuration deployment, not just on the frontend.

Order is **outermost first**: global entries, enabled tool-server/package entries
in deterministic load order, then agent entries. Declaration order holds within
each level. For two matching entries A and B, execution is A-before, B-before,
tool, B-after, A-after. Each entry sees the arguments passed to it by its parent.
Tool-server entries join the resolved chain; their `match` controls which calls
they wrap, so use a narrow matcher if they should affect only that server's tools.

A full entry with the same name at a later level — or in a later tool-server
file — replaces the earlier entry without moving its position. A later
`{name: label-bash, enabled: false}` removes it. Disabling an unknown earlier
name is an error, as are duplicate names within one file. The same name across
tool-server files follows the replacement rule, so packages shipping the same
middleware (for example `proxy-auth` on `bash`) resolve to a single chain entry.

## Script operations

Bash commands exchange JSON. Python functions exchange dicts and other JSON values.
Neither language is an OS subprocess: BashKit provides a virtual filesystem and
builtins such as `jq`; Monty is a Python subset, not CPython. Host filesystem,
network and subprocess access aren't implicit. Use configured helper tools for
host operations. Scripts are trusted configuration, not model-authored code.

| Bash | Python | Behavior |
| --- | --- | --- |
| `tool_input` | `tool_input()` | Read call context and effective arguments. |
| `next [--id STEP] [JSON]` | `call_next(args=None, id=None)` | Run the rest of the chain, using current arguments when omitted. Bash also accepts JSON on stdin. |
| `respond JSON` | `respond(result)` | Set the final result envelope. |
| `deny REASON` | `deny(reason)` | Set an error result marked with this middleware's name. |
| Allowlisted tool command | `call_tool(name, args)` | Invoke a helper, bypassing middleware. |
| `confirm [--id KEY] MESSAGE` | `confirm(message, id=None)` | Return success / `True` for an exactly-bound approval, failure / `False` for denial. Without an answer, stop the interpreter and request input. |
| `answer KEY` | `answer(key)` | Read an accepted response in this frame, or exit 1 / return `None` if unanswered. Never reads tool arguments or the stash. |
| `save KEY JSON` | `save(key, value)` | Save bounded JSON in this frame's resume stash; envelopes retain their host-owned snapshots. |
| `saved KEY` | `saved(key)` | Restore a value from an earlier run of this frame, or exit 1 / return `None` if absent. |
| `fetch URL` | `fetch(url)` | GET a URL on this entry's `fetch:` allowlist; return `{status, body}`. |
| `interact [--id KEY] SURFACE_JSON` | `interact(surface, id=None)` | Ask the user to answer an A2UI form. Suspends until answered, then returns `{action, data, note?}`. Bash also accepts the surface JSON on stdin. |

`tool_input` returns `tool`, `arguments`, `tool_call_id`, `session_id`, `agent`,
and `middleware`. Optional context such as session or agent can be null.
It contains no user answers.

Every `next` is a **fresh downstream execution**. Repeating it can repeat side
effects. `--id` / `id` labels a logical step; it is not a cache or deduplication
key. Without a label, steps use their ordinal within that frame.

Only one control operation may be outstanding in a frame. Don't run `next`,
`respond`, `deny`, or helper calls concurrently, including from background Bash
jobs. A concurrent second operation is a contract error. Finish control work
before exiting; don't leave a background dispatch running.

`respond` and `deny` set a result; they don't stop the script. Avoid subsequent
side effects. To deny before any tool runs:

```bash
deny 'not permitted'
```

Python equivalent:

```python
deny('not permitted')
```

A denial after `next` cannot undo the operation. Use denial text that says it
already ran when appropriate.

## Results and refs

`next` returns an envelope with three fields: `output` (the downstream tool's
JSON output), `is_error` (boolean), and `ref` (an opaque result handle).
`respond` accepts that envelope, with an optional `ref`.

Keep `ref` when rewriting a downstream result and preserving its media or private
metadata. Each ref selects exactly one host-owned materialized result snapshot.
For two sequential `next` calls, returning the second envelope carries only the
second call's media, execution context and control metadata. Results aren't merged.

A fabricated result without a ref has no hidden fields. An unknown, cross-call,
or cross-frame ref is a contract error subject to `on_error`. A ref copied from an
earlier attempt isn't valid until its saved envelope is restored through `saved`.
Don't store bare ref strings outside the invocation. Script-authored output cannot
manufacture handoff metadata; an error or denial never activates a handoff. Markdown is
regenerated from effective output, not restored from the snapshot.

This Python rewrite is equivalent to the Bash configuration above. Put it in a
`python:` block instead of `bash:`:

```python
args = tool_input()["arguments"]
args["env"] = dict(args.get("env", {}))
args["env"]["MIDDLEWARE_LABEL"] = "reviewed"
result = call_next(args, id="run")
result["output"]["reviewed"] = True
respond(result)
```

### Error normalization

Recoverable downstream failures reach the script as error envelopes, retaining
partial-result data. Both engine `output.is_error: true` and MCP
`output.isError: true` mark an error. Setting envelope `is_error` to false doesn't
clear an error marker already inside `output`.

If a script sets envelope `is_error: true` and its output has neither marker,
Harnx wraps that output as an `is_error: true` object whose `error` field contains
the authored output. Already-marked MCP errors stay unchanged. To recover from a
recoverable failure, replace the error-marked output with a success output and
set the envelope's `is_error` to false.

Fatal downstream failures, turn cancellation and interruption stop the script and
propagate; scripts can't catch them or override them through `on_error: allow`.

## Defaults, failure policy and timeouts

A successful script without `respond` or `deny` returns its last completed `next`
result. If it never called `next`, it passes through with the original arguments.
Stdout is not a final-result protocol: printing JSON does not call `respond`.
A no-op Bash middleware can be:

```bash
next
```

Python equivalent:

```python
call_next()
```

Nonzero exit, timeout, runtime limit, interpreter error and contract error follow
`on_error`:

- **`deny`:** return an error naming the middleware and failure. If `next` was
  never reached, the tool doesn't run. Completed effects aren't rolled back.
- **`allow`:** first check for unknown outcomes. If any started physical dispatch
  (downstream or helper) has an unknown outcome, return an error saying the outcome
  is unknown. Do not redispatch or use an earlier result. Otherwise return the last
  completed `next` result, or dispatch once with original arguments if none started.

`allow` is for optional middleware, not a security policy that must fail closed.
Explicit `deny` is a result, not a script failure; `allow` doesn't undo it.

The timeout covers the **whole script run**, including waiting inside `next` and
helper calls. Raise it for slow tools. Nested middleware each has its own limit;
an outer deadline can stop the inner chain first. Timeout excluding `next` is not
supported. Cancellation stops running interpreters and pending host dispatch
futures, but cannot undo effects an external tool already performed.

## Confirm and resume

Worker turns persist confirmation requests and checkpoints, then suspend until a
frontend answers. The interpreter stops while waiting, so human response time
doesn't consume its script timeout. The invocation's execution deadline still
counts down. An engine context outside worker turns without an interaction driver
returns `interaction not supported here`.

The [Tool Confirmation Guide](tool-confirmation-guide.md) covers TUI, serve/web,
A2A and ACP responses, including notes and reconnects.

Before dispatching a tool, branch on the user's answer:

```bash
if confirm --id gate 'Proceed?'; then
  next --id run
else
  deny 'not approved'
fi
```

Python equivalent:

```python
if confirm('Proceed?', id='gate'):
    call_next(id='run')
else:
    deny('not approved')
```

With no accepted answer, `confirm` records an interaction tagged
`harnx.kind: confirm`, stops the interpreter, and returns the host checkpoint.
It doesn't wait inside the interpreter. A denial is an ordinary false result:
use an `if` branch, since a bare Bash `confirm` returning 1 follows `set -e`.
Fatal outcomes and suspension bypass `on_error`, even with `on_error: allow`.
Printing interaction-shaped JSON to stdout cannot request or answer anything.

A resume starts the chain from the beginning with accepted responses supplied by
the host, never injected into `tool_input().arguments`. An answer is bound to the
session storage key, round sequence, original tool-call ID, chain version,
middleware name, enclosing `next` step path, interaction key, kind and canonical
payload. Changing the message prompts again. `answer('gate')` returns the latest
accepted response for that key in this frame as
`{interaction_id, action, data, note?}`; an unanswered key returns nothing. It
never suspends and can't turn model-authored arguments into an accepted response.

Use explicit `--id` / `id` values whenever a rerun can skip or reorder `next`
calls or confirmations. Unlabeled operations use separate 1-based ordinals for
`next` and interactions within each frame. If a rerun skips its first `next`,
ordinal 1 now labels a different operation and can select the earlier operation's
answer. Explicit logical keys keep that operation's identity stable. Loop
iterations need distinct keys. Keys don't cache execution.

### Keep a downstream result without running the tool twice

Save the envelope before confirming. On resume, restore it rather than calling
`next` again:

```bash
if saved result > /tmp/r.json; then
  :
else
  next --id run > /tmp/r.json
  save result "$(cat /tmp/r.json)"
fi
if confirm --id keep 'Keep this result?'; then
  respond "$(cat /tmp/r.json)"
else
  deny 'Result discarded. The command already ran.'
fi
```

Python equivalent:

```python
result = saved('result')
if result is None:
    result = call_next(id='run')
    save('result', result)
if confirm('Keep this result?', id='keep'):
    respond(result)
else:
    deny('Result discarded. The command already ran.')
```

`saved` reads the previous attempt's stash, not values newly saved in the current
attempt. Envelopes can be nested inside saved JSON; restoring them reactivates
only their retained refs in this frame. Private media, execution observations
and control metadata stay in host snapshots, not script JSON or interaction
payloads. Nested suspension checkpoints ancestors and completed frames as well
as the frame issuing the request.

Limits are **256 KiB of serialized JSON stash per frame** and **8 MiB per call
checkpoint**, including frame metadata and retained snapshots (with private
observations). Exceeding a limit fails the call rather than returning a partial
checkpoint. Plain `next; confirm 'Keep?'` still executes `next` again on resume.
Neither helper provides exactly-once effects across crashes.

## Interact: A2UI surfaces

`interact` asks the user to answer a form instead of a yes/no question. It uses
the same durable interaction path as `confirm`: without an accepted answer the
interpreter stops and the call suspends, and a resume reruns the chain from the
start with the host-supplied answer. Once answered it returns
`{action, data, note?}`, where `action` is the triggered button action name and
`data` carries the values the user entered.

The surface is a single A2UI `createSurface` message with `"version": "1.0"`
and its components and data model inline:

```bash
surface='{"version":"1.0","createSurface":{"dataModel":{"choice":[],"name":""},"components":[
  {"id":"root","component":"Card","child":"col"},
  {"id":"col","component":"Column","children":["title","choice","name","submit_label","submit"]},
  {"id":"title","component":"Text","text":"Pick a deployment target"},
  {"id":"choice","component":"ChoicePicker","variant":"mutuallyExclusive",
   "options":[{"label":"Staging","value":"staging"},{"label":"Production","value":"prod"}],
   "value":{"@path":"/choice"}},
  {"id":"name","component":"TextField","label":"Release name","value":{"@path":"/name"}},
  {"id":"submit_label","component":"Text","text":"Deploy"},
  {"id":"submit","component":"Button","child":"submit_label","action":{"event":{"name":"deploy",
   "context":{"choice":{"@path":"/choice"},"name":{"@path":"/name"}}}}}]}}'
response=$(interact --id deploy "$surface")
respond "$(echo "$response" | jq '{output: {action: .action, data: .data}, is_error: false}')"
```

Python equivalent:

```python
surface = {
    "version": "1.0",
    "createSurface": {
        "dataModel": {"choice": [], "name": ""},
        "components": [
            {"id": "root", "component": "Card", "child": "col"},
            {"id": "col", "component": "Column",
             "children": ["title", "choice", "name", "submit_label", "submit"]},
            {"id": "title", "component": "Text", "text": "Pick a deployment target"},
            {"id": "choice", "component": "ChoicePicker", "variant": "mutuallyExclusive",
             "options": [{"label": "Staging", "value": "staging"},
                         {"label": "Production", "value": "prod"}],
             "value": {"@path": "/choice"}},
            {"id": "name", "component": "TextField", "label": "Release name",
             "value": {"@path": "/name"}},
            {"id": "submit_label", "component": "Text", "text": "Deploy"},
            {"id": "submit", "component": "Button", "child": "submit_label",
             "action": {"event": {"name": "deploy",
                "context": {"choice": {"@path": "/choice"}, "name": {"@path": "/name"}}}}}
        ]
    }
}
response = interact(surface, id="deploy")
respond({"output": {"action": response["action"], "data": response["data"]}, "is_error": False})
```

Surfaces are validated at request time against the A2UI v1.0 Basic Catalog
subset harnx supports: **Text, Button, ChoicePicker, TextField, Card and
Column**. Anything else fails with an error naming the problem, before any
suspension. The rules:

- `createSurface.components` is required and must contain a component with
  `id: "root"`; child references must name defined components.
- At least one `Button` must carry an `action.event.name`, so the surface can
  be answered. Renderer function-call actions (`action.functionCall`) are
  rejected.
- Input component values (`TextField.value`, `ChoicePicker.value`) must be
  data-model bindings like `{"@path": "/name"}` so an answer can carry the
  user's input back. Bind `{"@path": ...}` values in the button's
  `action.event.context` to receive them in `data`.
- Function calls (`{"@call": ...}`) are rejected everywhere: harnx implements
  no A2UI catalog functions, including inside `@checks` conditions.
- Unknown components, properties or fields are rejected (the schema is strict),
  as are wrong envelope versions.

The runtime stamps what it owns: a fresh `surfaceId` per attempt (surface IDs
are unique for a renderer's lifetime, and identity ignores them), the Basic
Catalog `catalogId` when the surface doesn't declare one, and
`harnx.kind: "a2ui"` in `createSurface.metadata.extensions`. Changing any
user-visible surface text or action semantics produces a new interaction and a
new prompt, exactly as changing a `confirm` message does. `answer KEY` works
for `interact` keys like `confirm` keys.

### How an answer maps into `data`

Every frontend answers through the same acceptance check, and the same
mapping produces `data` for the same form and answer. When the user triggers
an action:

- Every input component contributes `data["<path>"]` — its value binding's
  data-model path without the leading `/`. A binding `{"@path": "/choice"}`
  lands at `data.choice`. A `TextField` contributes a string; a
  `ChoicePicker` an array of the selected option `value`s. An untouched input
  contributes the surface's declared `dataModel` value at that path (else `""`
  or `[]`).
- Each entry of the chosen button's `action.event.context` is resolved per
  A2UI semantics — a `{"@path": ...}` binding against the current input
  values (falling back to `dataModel`), a literal unchanged — and lands in
  `data` under its context key.
- `deny` is dismissal, not an action: it carries no data.

Acceptance rejects, without appending: an action the surface doesn't declare
(other than `deny`), a `data` key that is neither an input binding nor a
context entry of the chosen action, a non-string `TextField` value, a
`ChoicePicker` value that isn't an array of declared option values, or more
than one selection where the variant is exclusive. A renderer may omit
untouched inputs. Non-graphical frontends render the same surface as text
(`harnx_core::interaction::text::render`) and parse a one-line answer
(`...text::parse`, grammar: `<action> [key=value ...]`) into this same
response.

The Web UI (`harnx-serve`) and TUI (`harnx-tui`) render `a2ui` interactions as modal forms supporting the pinned Basic Catalog subset (Text, Button, ChoicePicker, TextField, Card, Column). Operators can inspect fields, select choices, edit inputs, and submit any declared button action (or deny). In the TUI, Tab navigates between form fields, buttons, and an optional note, while arrow keys edit text or navigate choices. If server-side acceptance rejects a response (for example, undeclared actions or mismatched input types), the rejection message is shown and the operator can correct values and resubmit.

ACP has no form widgets, so it renders the same surface with the shared text fallback and offers one `AllowOnce` option per declared button action (labelled by the button text, option ID `action:<event name>`) plus Reject, which denies. ACP's permission reply carries only an option ID — it cannot collect input values — so each bound input is answered with its declared default through the shared response builder, and the dialog text tells the user so. Frontends that lack even that fallback show a "not supported by this client" message and offer deny-only dismissal; a `deny` action reaches the script as `{action: "deny", data: {}}`. The CLI answers pending interactions from any session without a frontend: `harnx interact list <agent> <session>` prints them with their exact identity and `harnx interact answer ... --id <id> --action <action> [--data KEY=VALUE ...] [--data-json <json>] [--note <text>]` submits through the same durable acceptance, including structured input values the ACP dialog cannot collect. Inner bash-tool calls fail with the same "can't be answered inside a script" error as `confirm`, and operator root calls fail outright: operators have no form UI, and only `confirm` auto-approves.

### A2A form answers

A2A status messages carry the pending `a2ui` surface as a DataPart with
`mediaType: "application/a2ui+json"` (A2A 1.0's field name, not `mimeType`) plus
the shared readable text fallback. Only the current pending head is exposed.
Answer on the same task/context with either:

- One DataPart containing an A2UI v1.0 `action` envelope. Put raw input values
  in `action.metadata.extensions.harnx_response.data` and an optional operator
  remark in `...harnx_response.note`. Copy `surfaceId` from the challenge and
  include the action's `name`, `sourceComponentId`, RFC3339 `timestamp`, and
  resolved `context`. The server calls `build_data` to rebuild canonical data
  from the declared surface, not client-supplied context.
- Text `<action> [key=value ...]`, for example
  `deploy choice=prod name=rel-1`, or `deny` to dismiss. Text replies split on
  whitespace; use a DataPart for TextField values containing spaces.

Invalid input returns a visible correction with the same `INPUT_REQUIRED`
challenge and no interaction response in the journal. Send the corrected answer
with a new `messageId`; retries of an accepted ID remain bound to the original
interaction and request sequence, even across replica recovery. Confirmation
interactions still use yes/no text. See the
[A2A wire format and full reply example](../crates/harnx-a2a-server/README.md#a2ui-form-interactions).

## Tool-produced interactions (`input_required`)

A native tool (a `Toolset` served over NATS, or an in-process toolset behind
the same server) can suspend its own call asking the user for input, without
any middleware involved. The tool opts in by declaring the metadata on its
spec (`ToolSpec::with_input_required()`, the `harnx:input_required` meta key)
and returning `Err(ToolInvokeError::InputRequired(ToolInputRequired::...))`
from `invoke_with_context` instead of a result:

- `ToolInputRequired::confirm(key, message, request_state)` — an
  Approve/Deny question, rendered by every frontend's confirm UI.
- `ToolInputRequired::a2ui(key, surface, summary, request_state)` — an A2UI
  v1.0 form, validated and stamped with a fresh surface id exactly like a
  middleware `interact` surface before it becomes visible.

The challenge rides the tool wire beside the arguments
(`ToolReply::input_required`, MRTR SEP-2322 `input_required`), and older
callers that do not understand it see a visible recoverable error
(`INPUT_REQUIRED_COMPAT_ERROR`) instead of a silent success. The runtime's
durable layer turns it into the same D6 interaction middleware uses: the
producer is infrastructure-derived (`tool:<server>/<tool>`, e.g.
`tool:____probe/challenge`), the key is the tool's own request key, and the
rest of the identity — session, tool round, tool call, chain version,
enclosing middleware step path — is unchanged. A tool challenge inside a
middleware frame therefore has a distinct identity from the middleware's own
interactions, and every frontend renders it with the same records and kinds
it already handles.

The tool's opaque `request_state` (bounded to 64 KiB serialized; keys to 256
bytes) is checkpointed in the call's durable interaction state, so it
survives worker moves and replica recovery. When the user answers, the
runtime re-dispatches the original call with the accepted response keyed by
the challenge's key plus the latest state, delivered through
`ToolInvocationContext::input_responses` and
`ToolInvocationContext::request_state` — never merged into the model-authored
`arguments`.

A tool may challenge again after an answer: each challenge is a new request
with its own key and state, and the rerun receives only the latest
challenge's responses and state. A challenge from a tool that did not declare
`harnx:input_required` degrades to an error result instead of suspending.
Calls from inner scripts and operators have no interaction driver and fail
closed the same way, and replaying a journaled challenge returns a "not
available" error result instead of answering.

### MCP tools (`input_required` over the bridge)

A tool on a bridged external MCP server participates in the same flow — no
middleware or per-tool configuration is involved. The bridge connects with
`server/discover` and negotiates the MCP 2026-07-28 revision (MRTR, SEP-2322)
when the server offers it; older servers keep working over the legacy
`initialize` handshake and their tools simply cannot suspend.

The MRTR capability is server-level: when the wrapped server negotiates the
revision, every listed tool of that server is stamped `harnx:input_required`
and may return `input_required` from `tools/call`. The bridge maps the result
onto the native challenge path:

| MCP (MRTR) | harnx |
|---|---|
| one `elicitation/create` request in `inputRequests` | one `ToolInputRequired` challenge; the `inputRequests` map key is the challenge key |
| empty `requestedSchema` | `confirm`, rendered by every frontend's confirm UI |
| `requestedSchema` properties (documented subset) | A2UI form: `string` properties become text fields, `enum` properties become choice pickers; other types are rejected |
| `requestState` (opaque string) | the challenge's `request_state` (JSON decoded when possible, the raw text otherwise) |
| `inputResponses` on the re-call | the user's answers, beside the unchanged `arguments` |

Kinds harnx cannot show — sampling (`createMessage`), `roots/list`, URL
elicitation — and results with several requests or none fail with a clear
recoverable error, never a silent success. Responses encode as ElicitResult
(`accept`/`decline`, optional `content`) for third-party servers, with the
original harnx action and note preserved under `_meta` so a harnx↔harnx hop
loses nothing.

The same round cap applies to native and bridged tools alike: a tool that
keeps challenging without finishing is cut off after
10 rounds (`MAX_TOOL_INPUT_ROUNDS`, tracking rmcp's
`DEFAULT_MRTR_MAX_ROUNDS`) with a visible error result instead of suspending
forever.

## Non-modal user-audience surfaces

Completed tool results can carry user-audience A2UI surfaces alongside normal
output. Unlike modal `interact` or `input_required` interactions, non-modal
surfaces do not suspend the tool round or block the turn. They are presented
to the user inline alongside the conversation, and multiple surfaces can remain
active at once while the user continues chatting.

Native toolsets return surfaces via `ToolResult.user_surfaces`. External MCP
tools return embedded resources with `type: "resource"`, MIME type
`application/a2ui+json`, and `annotations.audience` containing `"user"`.
The engine extracts and validates each surface envelope against the shared
A2UI Basic Catalog subset, persists it in the round's durable `ToolResults`
log entry, and strips it from model-visible output. The model never sees the
raw surface structure.

Each published surface is identified by its `surface_id` (from
`createSurface.surfaceId`) and `publish_seq` (the sequence of the enclosing
`ToolResults` entry). Frontends can only act on surfaces that exist in a
durable `ToolResults` entry.

### Visibility routing

Buttons on a non-modal surface declare their execution and visibility behavior
in `metadata.extensions.harnx`:

    {
      "harnx": {
        "visibility": "note",
        "tool": {
          "name": "deploy_release",
          "arguments": {"target": "staging"}
        }
      }
    }

The `visibility` field controls where action output goes:

- `invisible`: The action is recorded in the durable session log
  (`SessionLogEntry::SurfaceAction`). It produces no model input and does not
  start or join a turn.
- `note`: A passive context line is injected into the model's hook context
  at the next model call. It does not start a new turn. This is the default
  visibility when a direct `tool` is declared.
- `turn`: The action's user message starts a new turn when the session is idle,
  or joins the running turn (steering) when a turn is active. This is the
  default visibility when no direct `tool` is declared.

### Direct-tool vs conversational routing

When an action declares `tool: { "name": "...", "arguments": {...} }`, the
action executes directly through the operator tool execution path. The session's
effective tool allowlist and configured middleware apply. If the tool is not
permitted for the session, the submission is rejected with `unauthorized_tool`
and nothing is executed or recorded. Action `data` keys override declared base
arguments.

When no tool target is declared, the action is purely conversational and routes
its text to the model according to the chosen visibility. No synthetic model tool
calls are generated anywhere in the process.

Direct tools run before the `SurfaceAction` entry is appended so the tool's output
summary can be quoted in the action note (execute-then-record). If a worker crashes
after running the tool but before the record lands in the log, no action is recorded,
and a subsequent retry will run the tool again (the execute-then-record crash window).

At-most-once tool execution is owner-local: the active worker serializes submissions
for the same `(surface_id, publish_seq)` generation using an in-memory gate,
ensuring the tool runs once while concurrent retries queue and classify from the
resulting record. However, the idle path (used when no active worker holds the session
lease) is not serialized across processes. Two idle clients submitting at the same
time, or an idle client racing a newly started worker, can each run a non-idempotent
tool once before the first record commits (the durable log still accepts only one
action under first-wins CAS).

### First-wins identity and retry semantics

A published surface generation `(surface_id, publish_seq)` accepts at most one
action under first-wins semantics. The durable identity of an action is
`(action, data)`.

- If the exact same `(action, data)` is resubmitted, the system returns
  `already_applied` (an idempotent success).
- If an identical `(action, data)` is resubmitted with a different note or
  `user_message`, it is still classified as `already_applied`, and the second
  note is discarded.
- If a submission attempts a different action or different data on an
  already-answered generation, it is rejected with outcome `conflicting`.

### Note contract

Clients must pass user-typed remarks as `user_message` (e.g. `user_message`
in `session/surface_action` or `harnx surface act --note`). The backend reads
`user_message` to construct model-visible context and ignores `response.note`.
This keeps model-facing input explicit. When `user_message` is omitted, the
runtime generates fallback summary text (e.g. `[surface action '<action>']`).

### Obscured text fields are visual masking only

In A2UI surfaces, a `TextField` with `variant: obscured` causes frontends to
mask input characters visually (like a password field). This is visual masking
only. Obscured values are stored as plain text in durable `SurfaceAction.data`,
can appear in generated action summaries, and are passed as plain arguments on
CLI invocations. Do not pass real passwords, tokens, or private keys through
surfaces.

### Frontend and protocol support

- **Web UI & TUI**: Render non-modal surfaces inline with the conversation
  without blocking the composer. Users can open an action form, fill inputs,
  and submit actions while other work proceeds.
- **CLI**: `harnx surface list` and `harnx surface act` inspect and submit
  actions against published surfaces.
- **A2A and ACP**: Do not render or accept actions for non-modal surfaces yet.
  Neither protocol provides an out-of-band user-audience channel. Streaming
  surfaces into A2A task messages or ACP session prompts would leak private
  user surfaces into model input, and their modal channels (`input_required`,
  `request_permission`) suspend turns, which non-modal surfaces intentionally
  avoid. Non-modal surfaces are omitted from A2A and ACP streams without
  failing tasks. Users can inspect and act on them from Web, TUI, or CLI.

## Controlled fetch

`fetch URL` (Bash) and `fetch(url)` or `fetch(url=...)` (Python) use the worker's
HTTP client, not BashKit's network builtins. `http`, `curl` and `wget` remain
unavailable, even when a fetch allowlist is configured. `fetch` is a reserved
middleware operation name and cannot also name a helper tool.

`fetch:` accepts a list of exact ASCII DNS names or IP literals. DNS names are
case-insensitive; use punycode for international names and brackets for IPv6,
e.g. `['[::1]']`. No schemes, ports, paths, wildcards or trailing dots are accepted
in the list. Subdomains aren't included: `example.com` doesn't grant
`api.example.com`. Matching uses the parsed URL host, never a substring.
Only this entry's list applies; another entry's grants and `tools:` don't add
network permissions. Replacing an entry replaces its fetch grants too. Changing
grants changes the chain version and invalidates old confirmations.

**A host grant covers HTTP and HTTPS on every port and path of that host.**
Allowlisting a host is an outbound data grant: request paths and query strings
(which can carry tool inputs or session context) travel to allowlisted destinations.
Prefer HTTPS over HTTP to protect request data in transit.

Fetch intentionally permits loopback and private networks for internal policy
services. A DNS grant trusts that hostname's DNS answers, including internal
addresses; don't allowlist domains controlled by untrusted parties or assume
this is a public-IP-only fetch tool. Link-local, unspecified, multicast,
broadcast and known cloud metadata addresses (`169.254.0.0/16`,
`100.100.100.200`, `[fd00:ec2::254]`) are blocked even when allowlisted.
DNS answers are checked as a whole before connecting, including IPv4-mapped
IPv6; a mixed answer containing a blocked address fails. Use worker network
policy as well if destinations need stricter restrictions.

Requests are **GET only**, with no custom headers or request body. URL userinfo
(`user:password@host`) is rejected. Redirects aren't followed, proxies from the
worker environment aren't used, and requests aren't retried automatically.
TLS certificate verification stays enabled. No cookies or credentials are
borrowed from the worker's other clients.

Each request has fixed limits: **10 seconds total** for DNS, connection, headers
and body, and **256 KiB (262144 bytes)** of response body. A shorter enclosing
middleware timeout or cancellation wins. The size cap checks both declared
content length and streamed bytes; oversize responses fail rather than truncate.
Automatic decompression is disabled. The body must be UTF-8 (no charset
conversion); binary, compressed or invalid UTF-8 bodies fail.

Success returns `{"status": 200, "body": "ok"}` as JSON on Bash stdout or a
Python dict. Any HTTP status, including 3xx/4xx/5xx, is a response, not a runtime
failure; check `status` in the script. URL validation, denied hosts, network errors,
timeouts and size/encoding failures stop the script and follow `on_error`.
Bash `set +e` / `||` and Python `try`/`except` cannot override that host failure.
The default `deny` returns an error naming the middleware and failure without
calling the tool. `allow` can pass through, so don't use it for required policy
checks. Fetch responses don't become the tool result unless passed to `respond`.

Use fetch only for read-only, idempotent endpoints. Fetch isn't journaled as a
tool dispatch: it can repeat on resume or crash recovery. `save`/`saved` can retain
a response across a committed confirmation boundary, subject to stash limits,
but don't guarantee exactly-once HTTP effects. Use an allowlisted helper tool
with dispatch tracking for side-effecting requests.

This Bash entry allows the tool only when a policy endpoint returns `200` and
`ok`. Replace the URL and matcher for your deployment:

```yaml
middleware:
  - name: fetch-policy-bash
    match: '^probe_run$'
    fetch: [policy.example.com]
    bash: |
      policy=$(fetch 'https://policy.example.com/allowed')
      if echo "$policy" | jq -e '.status == 200 and .body == "ok"' > /dev/null; then
        next
      else
        deny 'Policy endpoint did not allow this call'
      fi
```

Python equivalent:

```yaml
middleware:
  - name: fetch-policy-python
    match: '^probe_run$'
    fetch: [policy.example.com]
    python: |
      policy = fetch('https://policy.example.com/allowed')
      if policy['status'] == 200 and policy['body'] == 'ok':
          call_next()
      else:
          deny('Policy endpoint did not allow this call')
```

`middleware_fetch` tests execute both configurations against a local server and
cover allowlist denial, streamed size limits, header/body timeouts and disabled
network builtins. Run `CARGO_BUILD_JOBS=8 cargo nextest run -p harnx-runtime middleware_fetch`.

## Which calls are wrapped

- **Model calls:** yes.
- **Calls inside the model's Bash script tool:** yes. Wrapping only the outer
  `bash` tool would let inner calls bypass policy; matching inner tools are wrapped
  too.
- **Operator root calls (`harnx call`):** yes. Operator consent doesn't bypass
  middleware denial or rewriting.
- **Lifecycle hook tool calls:** yes. Calls made by hook scripts pass through
  configured tool middleware so policy and credential injection stay effective.
  See the [Hooks Guide](hooks-guide.md#calling-tools).
- **Middleware's allowlisted helper calls:** no. They use the inner-call machinery
  with middleware disabled, preventing recursion. They still obey tool admission
  and cancellation. Helper calls don't publish separate outer-call UI events.

Declare helpers with `tools:`; an unlisted tool is unavailable. Bash helper
commands use the same generated flags and `--json` input as the Bash script tool.
Python uses `call_tool(name, args)`. Tool selectors don't grant host capabilities
beyond the selected tool's own permissions.

An inner-script call reaching `confirm` returns
`interaction required, can't be answered inside a script`, even with
`on_error: allow`. It cannot suspend and replay the model's whole script.
Operator root calls auto-approve `confirm` under scoped operator consent;
`answer` can read that approval. Other interaction kinds still fail on
operator calls — `interact` names the reason and points at `harnx interact
answer` for sessions, where a pending interaction can be answered from the
CLI.

### Helper tool servers and startup

When the worker daemon manages tool servers locally, servers hosting middleware
helper tools start automatically on session activation. You do not need to list
helper tools in the agent's `use_tools` (or the global `use_tools` fallback) just
to start their servers.

- **Active middleware sources:** The worker collects `tools:` selectors from
  every middleware entry active for the session: global `middleware:`,
  middleware declared in enabled tool-server files, and agent front matter.
  Entries removed by a later disable entry (`enabled: false`) and middleware in
  disabled tool-server files do not contribute selectors.
- **Selector matching:** Selectors follow standard `use_tools` matching rules,
  including concrete tool names, toolsets, package-qualified names, and
  wildcards (`*`). For example, `tools: ["*"]` starts every configured server.
  Matching uses the active agent's package context, identical to execution-time
  tool resolution.
- **Match patterns ignored for startup:** Tool-server selection ignores
  middleware `match` regexes. Sessions may call matching tools at any point, so
  required servers start on session activation and stay ready.
- **Server lifecycle:** Servers started for middleware are reference-counted
  and linger after turns and sessions exactly like servers started for agent
  `use_tools`.
- **Unmanaged workers:** Automatic startup applies only when the worker daemon
  manages tool servers locally. On unmanaged or cloud workers where tool servers
  run externally, helper servers must run independently and be available over NATS.
- **Lifecycle hooks:** In-process event hooks (`hooks:`) also declare helper
  tools via `tools:` and start their servers on session activation. See the
  [Hooks Guide](hooks-guide.md#calling-tools).

## Deployment and crash limits

BashKit and Monty execute inside the worker process. Limits and cancellation
handle ordinary script failures, but **a Monty panic, abort or native crash can
terminate the worker**. This isn't process isolation for untrusted Python.
Deploy reviewed scripts and keep an external worker supervisor.

### Durable suspension and siblings

A worker writes the private checkpoint before `InteractionRequested`. It settles
or durably classifies in-flight siblings, then writes `SuspensionReady` before
showing any request. Each finalized call gets its own `ToolCallCompleted` before
its terminal UI event. Frontends show one unresolved request at a time in the
latest manifest's order, which needn't match request append order.

Responses are fenced, first-wins writes to `InteractionResponse` before the call
reruns. They must name the interaction ID and exact request sequence. Identical
retries succeed without a second append; conflicting, stale, wrong-sequence and
post-completion replies are rejected. Tool arguments, stdout and the stash can't
approve a request.

Only the answered call reruns, from the outermost middleware with its last
committed checkpoint and accepted responses. Completed siblings don't run their
middleware, confirmation, or provider again. Pending unanswered calls stay
waiting. The worker merges all results in original call order into one
`ToolResults` entry before calling the model. Handoff activation is round-level:
Atlas can't start while a sibling still awaits a response.

### Clearing or resetting a suspended session

`.empty session` and `.reset session` append `Clear`. This abandons every earlier
unfinished tool round, including pending confirmations and selected handoffs that
haven't dispatched. A new prompt runs normally; recovery doesn't resume or repair
those rounds. Old confirmation replies are rejected, and their approvals don't
authorize new calls, even if a tool call ID is reused.

Clear doesn't undo side effects that already happened or stop an operation already
running outside the worker. Check external state before manually retrying a call.
Private checkpoints and dispatch records remain stored until session deletion;
Clear doesn't purge them. While the Clear boundary remains in the effective log,
these older records are inert: guards and recovery select only post-Clear rounds,
whose higher log sequence numbers keep their checkpoints separate. New rounds
still get the normal ownership checks and fresh confirmation requests.

### Crash recovery and unknown outcomes

There is no interpreter continuation or automatic replay of `next`. Recovery
reuses completed outer results. An interrupted attempt with no physical dispatch
intent can restart from its previous committed interaction boundary. If an
unfinished attempt started any downstream or allowlisted helper dispatch, recovery
ends that call with an **interrupted, outcome unknown** error rather than rerunning
its script. Even a completed leaf reply isn't a completed middleware result.
A crash after the reply but before `save` or the next durable confirm boundary
therefore doesn't repeat the operation; the diagnostic can say the downstream
tool completed but the middleware didn't finish.

An unknown-outcome error means an operation might already have happened. Check
external state before manually retrying a side-effecting call. `save`/`saved`
prevents repeats across committed suspension boundaries, not exactly-once effects
across every crash window. A missing referenced checkpoint also ends the call as
interrupted, never a fresh dispatch based on missing evidence.

Changing the resolved middleware chain invalidates old approvals. If no physical
dispatch started in any attempt of that call, the worker discards the stale
request and reruns fresh, prompting again. If a downstream or helper dispatch
already started, it returns a middleware-changed error warning that the operation
may already have happened. Keep policy updates separate from unfinished work.

### Cluster rollout and rollback

Running a tool call under any non-empty `middleware:` chain enables durable
interaction mode in the worker. This writes new `SessionLogEntry` variants
(`ToolCallCompleted`, `InteractionRequested`, `SuspensionReady`, and
`InteractionResponse`) to the session log. Similarly, non-modal A2UI surface
actions write `SessionLogEntry::SurfaceAction`. The writers of `SurfaceAction`
are `harnx surface act`, the TUI surface action modal, the `harnx-serve`
`session/surface_action` JSON-RPC method, and the worker control path. Older
harnx binaries hard-reject transcripts containing unknown entries. In
multi-instance deployments sharing a NATS cluster, follow a reader-first rollout:

1. **Upgrade every session-log reader and control client first.** Upgrade workers,
   `harnx-serve`, `harnx-a2a-server`, ACP, and any TUI or CLI instances reading
   shared logs before enabling middleware on workers or submitting surface actions.
   Older readers cannot read sessions once new entries are written. Frontends and
   API clients must also upgrade before workers because the surface-action control
   protocol uses an early `Pending` acknowledgement: older clients cannot
   deserialize `Pending`, drop the acknowledgement, and fall back to idle execution
   after 40 seconds, which can run a slow direct tool a second time while the new
   worker is still executing it. Deploy confirm-capable and surface-capable frontends
   before expecting them to answer confirmation requests or act on surfaces. Readers
   across the cluster must be upgraded before producers write any new entries.
2. **Hold back middleware writers until readers are upgraded.** Any non-empty
   `middleware:` list starts writing durable entries. The updated `pantheon`
   and `coding` packages ship middleware chains for proxy auth (`packages/{pantheon,coding}/tool_servers/bash.yaml`)
   and Daedalus handoffs (`packages/pantheon/agents/daedalus.md`).
   To prevent workers from writing new entries during the rollout window:
   - Keep previous package versions pinned on workers, or
   - Remove the `middleware:` blocks from `packages/{pantheon,coding}/tool_servers/bash.yaml`
     and `packages/pantheon/agents/daedalus.md` (and any other declaring file) before deploying package files to workers.
   - Do not try to disable middleware with global `middleware:` entries
     such as `{name: confirm-atlas-handoff, enabled: false}` in `config.yaml`.
     The configuration loader evaluates global entries first, before agent definitions
     exist, and errors with "cannot disable an unknown earlier entry". Later definitions
     in agent front matter would re-add the entry anyway. Remove the entry directly
     at its declaring source instead.
3. **Understand confirmation and proxy-auth fallback without middleware.**
   - Pinned packages containing legacy command hooks fail config loading in
     this release. A rollback needs an older binary with its matching package.
     Saved legacy approvals remain readable and answerable in this release. Never
     delete session logs or checkpoint stores to unblock or roll back a session.
   - If the updated `pantheon` package is deployed with the `middleware:` block
     removed from `daedalus.md`, `atlas_session_handoff` has neither hooks nor
     middleware. Handoffs to Atlas execute directly without prompting for confirmation.
   - If `middleware:` is removed from `tool_servers/bash.yaml`, commands run
     without proxy-auth environment injection unless environment variables are
     configured manually.
4. **Drain in-flight turns before upgrading workers running middleware.** Workers
   from older releases leave no round-start or attempt-start records in the checkpoint
   store. Without those records, crash recovery must classify calls using the current
   middleware chain: matching calls end interrupted with unknown outcome. Finish
   or cancel in-flight turns before changing worker binaries or configuration.
5. **Keep the private checkpoint store reachable.** Workers lazily create the NATS
   JetStream bucket `harnx_interaction_checkpoints` on the first durable tool round.
   Every worker that may execute or resume the session must have reachability and
   JetStream permissions to create and access this bucket. Store errors block recovery
   of the whole round; confirmed absence differs from a read failure. Do not delete
   the store to unblock a session. The principal deleting sessions or running
   orphan-stream GC also needs stream-info and purge permission on
   `KV_harnx_interaction_checkpoints`.
6. **Rollback constraints.** Newer workers write a round-start record before dispatch.
   A reader-capable worker parks that work until a writer-capable worker resumes it,
   and parks marked post-results handoffs awaiting dispatch. Rolling back below the
   reader-capable version is unsupported once any durable interaction entry,
   `SurfaceAction`, or round-start record exists in the session. Rollback below
   this release is one-way once any `SurfaceAction` exists in a transcript.

Private store retention is bounded to **64 MiB per call**, with **8 MiB per stored
value** (the script checkpoint and stash have the tighter limits above). Records
remain until the round's `ToolResults` commits, then are removed; unfinished
rounds are cleaned up with the session. Preserve this store alongside session logs
through failover. It contains snapshots and execution observations that must not
be published as transcript or model input.

Middleware emits tracing events for start, finish and failure with its name.
Legacy process `PostToolUse` hooks no longer run from middleware: since lifecycle
`hooks:` became in-process event scripts (T3.4), no legacy tool hook servers can
be configured, so each `next` dispatch fires middleware only.

## Example tests and timing

`crates/harnx-runtime/tests/it/middleware/documentation.rs` reads this guide's fenced
examples verbatim and executes every Bash/Python script, including the YAML script.
It also uncomments and executes the middleware example in
`example_config/config.yaml`. The tests use echo tools instead of launching a
shell on the host. Existing middleware integration tests cover nesting, refs,
helper allowlists and both error policies.

An ignored `middleware_noop_latency` integration test compares an empty chain,
`next`, and `call_next()` using the real runtime context and engine with an
in-process echo provider. From the repository root, run
`CARGO_BUILD_JOBS=8 cargo nextest run -p harnx-runtime --test it middleware_noop_latency --run-ignored only --nocapture`.
It reports warm per-call mean, median and p95, plus the difference from the empty
chain. It excludes context construction, model latency and NATS transport; it's
not an end-to-end cloud latency measurement. Timing has no pass/fail threshold.
