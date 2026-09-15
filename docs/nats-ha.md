# High-Availability Deployment (NATS Mode)

Harnx supports a distributed, high-availability (HA) mode backed by [NATS JetStream](https://nats.io/). In this mode, the agent tool loop runs in a dedicated **worker** process, while **clients** (TUI or CLI) connect to the worker via a NATS cluster.

## Architecture Overview

Harnx NATS mode decouples the execution of an agent from the user interface:

- **Clients**: The TUI and CLI processes that post user messages and render events. They do not execute tools.
- **NATS Cluster**: The central nervous system. Uses JetStream for a durable session log and Key-Value (KV) for leader election (leases).
- **Workers**: Daemon processes that execute the agent loop. Persistent-cluster
  workers compete for activations. Local workers are frontend-owned and receive
  targeted activations. In both cases, only one worker holds a session lease at
  a time.

## Prerequisites

- **NATS Server 2.11+**: Requires JetStream and Key-Value support.
- **Configuration**: A NATS cluster configuration file in your `nats_servers/` directory.

## Standing up NATS

For development, a single NATS server with JetStream is sufficient:

```bash
nats-server -js
```

For production HA, run a NATS cluster with at least 3 nodes and set `replicas: 3`
in the cluster's `nats_servers/<cluster_key>.yaml` (see the production example
below) so the buckets harnx creates survive losing a node.

### Session identity

A session is identified by its exact agent name and local session ID within a
cluster. For example, `alpha/review-12345` and `beta/review-12345` have independent
transcripts, metadata, leases, execution generations, cancellation, and attachments.
Both components are case-sensitive. Commands require an explicit agent:

```sh
harnx --agent alpha --session review-12345 prompt "Review this change"
harnx info session alpha review-12345
harnx delete session review-12345 --agent alpha --cluster local
```

The TUI command is `.info session <agent> <id>`; it does not infer the selected
agent. Agent-specific subagent tools already identify the agent in their toolset.

`harnx_core::session_identity::session_key` hashes the JSON tuple
`[agent, local_id]` to a lowercase SHA-256 key. Internal inline agents use JSON
`null`, which is distinct from every named agent. All broker protocols use this
key: transcript subjects, metadata/activity, leases, execution and parent references,
invocation journals, controls, events, and attachment ownership. In the resource
and protocol descriptions below, `{id}` and internal `session_id` fields refer to
this storage key. Public metadata, tool results, and handoff targets retain the
readable local ID and its agent. Hook and tool-confirmation payloads also retain
the readable ID; their internal execution references carry the storage key. Derive a key once at the public boundary; never
hash a key again as though it were a local ID or resolve a bare ID across agents.

Earlier unscoped sessions are not migrated. Restart sessions after upgrading and
upgrade clients and workers together.

### JetStream Resources
Harnx automatically manages the following JetStream resources:
- **KV Bucket**: `harnx_leases` — session leases with a tombstone marker
  after release (not a bucket-wide TTL). This is the split-brain guard:
  every durable write is fenced on the lease's KV revision, and a worker
  that loses its lease aborts. If this bucket can't survive a node loss,
  neither can a session mid-turn on that node.
- **KV Bucket**: `harnx_sessions` — canonical session state. Each session uses
  `sessions/{storage_key}/meta` for immutable identity plus CAS-updated title,
  variables, overrides, and extensions, and `sessions/{storage_key}/activity`
  for frequently renewed lifecycle timestamps. No expiry. The
  `sessions/{storage_key}/read/{viewer}` key
  stores session-level unread state (see [Session Unread State](#session-unread-state)):
  - `viewer="default"` — the only viewer in current use; session-level (global) unread.
  - Value shape: `{ "last_attention_seq": u64, "last_read_seq": u64, "manual_unread": bool }`
  - `is_unread = (last_attention_seq > last_read_seq) || manual_unread`
  - Monotonic: `last_read_seq` never moves backward; manual unread is a separate flag.
  - Workers bump `last_attention_seq` on `TurnEnd` (final message) and `HitlApprovalRequested`.
  - Invalidation subject: `harnx.session.{storage_key}.read.invalidated` (separate from metadata invalidation).
- **KV Bucket**: `harnx_tool_registry` — tool server discovery, with a
  per-registration TTL.
- **KV Buckets**: `harnx_hook_registry` and `harnx_hook_expectations` — hook
  server discovery and its fail-closed fallback routes. Only the copies
  opened by the standalone `harnx-hookset-server` binary carry a TTL; the
  worker daemon's own copy of the same buckets does not set one.
- **Streams**: `SESSION_<sha256(id)>` (Subject: `sessions.{id}.log`) stores only the
  durable append-only conversation history. Agent identity, settings, rendered
  prompts, and titles do not belong in this stream. Hash the exact, case-sensitive
  storage key to lowercase hexadecimal with `stream_name_for_session`. JetStream
  uses stream names as directory names, so simply preserving case still aliases
  distinct IDs on case-insensitive filesystems. The fixed-length digest also
  avoids filename length limits. Subjects carry the storage key; user-visible
  local IDs remain unchanged.
  Earlier stream names are not migrated; start fresh sessions after upgrading.
- **Object Store**: `harnx_attachments` stores binary attachment payloads under
  session-scoped object names. Conversation entries contain only `cid:`
  references; workers hydrate the matching blobs into their local
  content-addressed cache before calling a model.
- **Persistent activation streams**: `WORK_NOTIFY_<cluster>` captures
  `cluster.<cluster>.sessions.notify` with cluster-shared work-queue dispatch.
- **Local activation stream**: `LOCAL_WORK_NOTIFY_V2` captures
  `session_scope.__local__.workers.*.sessions.notify` with interest retention
  and one exact durable consumer per frontend worker ID.

All of the KV buckets and the attachment object store above are created with
the `replicas` count from the cluster's config (`None` means 1, no HA). Set it
to 3 to match a 3-node cluster; a mismatch between the two is what leaves a
bucket unable to tolerate a node loss.

For NATS sessions, a local `cid:` file is only a cache entry. New local or
inline payloads are uploaded before their `cid:` is appended to the transcript;
a worker can backfill a legacy local-only blob, but fails the turn if the blob
exists in neither place. HTTP(S) attachment URLs remain external references and
are intentionally not copied into the object store.

**A bucket that has never existed before is created, not reconciled**, so
`replicas` above what the cluster can actually provide (e.g. a production
`replicas: 3` config pointed at a single-node dev server, before any of
these buckets exist) makes creation fail outright, and harnx will not start
against that cluster. This is intentional: failing loudly on a
misconfiguration is better than silently running at `replicas: 1` while an
operator believes they have HA. It only affects buckets that don't exist
yet — a bucket created earlier at a lower `replicas` and later pointed at a
higher one gets raised in place instead.

**Reconcile only ever raises `replicas`, never lowers it.** Some callers
(the hourly remote-session GC lease, for one) don't necessarily know the
cluster's actual configured value at the point they touch a bucket; if
reconcile lowered on request, one of those callers could silently downgrade
an already-correctly-replicated bucket's fault tolerance. Genuinely scaling
a bucket down requires recreating it.

### Tool-observed session context

Harnx records where its bash and filesystem tools actually ran. This lets the
interactive session picker answer questions such as "which session was working
on this repository and branch?" even when the session ran on a remote worker.
The frontend's own current directory is used only to search the picker; it is
never saved as evidence of where a session ran.

#### What a tool server observes

After a supported tool returns a result, the tool server records:

- its configured workspace and an effective context directory;
- the containing Git worktree and symbolic branch, when present;
- every network Git remote, normalized to a credential-free identity such as
  `github.com/acme/app`; and
- the observation time.

Credentials, URL query strings and fragments, trailing `.git`, and transport
syntax are removed from remote identities. Local paths and `file://` remotes
are not treated as portable repository identities. The tracking remote is
primary when Git identifies one; otherwise `origin`, then the first remote in
deterministic order, is primary.

The protocol currently calls that context directory `working_directory`, but
its exact meaning depends on the tool server:

- For bash `exec` and `spawn`, it is the command's resolved per-call working
  directory, which may differ from the bash server process's own directory.
  `wait` and `terminate` reuse the directory recorded for the spawned process.
- For a filesystem file target, it is the file's parent directory. For a
  directory target, it is that directory. A search without an explicit path
  uses the server's default search root. This is target context, not the
  filesystem server process's actual working directory. Target-derived context
  is what allows one filesystem server to report several repositories under a
  shared workspace.

Bash attaches an observation after `exec`, `spawn`, `wait`, `terminate`,
`rollback_file`, and template calls; `read_exec_log` does not attach one.
Filesystem `read`, `write`, `edit`, `insert`, `re_replace`, `ls`, `grep`,
`find`, and `rollback_file` calls attach one. Observing after execution is
important: a bash `exec` that checks out another branch reports the new branch.
Repository and branch fields are present only if the effective target is in a
Git worktree; a detached worktree has no symbolic branch.

The observer checks only the effective target and its ancestors. It does not
parse shell commands or scan arbitrary descendants. Therefore, `git clone
child` run from a non-repository parent does not discover `child` immediately;
a later tool call that targets `child` does.

#### How the observation reaches the session record

The observation uses the reserved, versioned namespace
`dev.harnx.execution_context`. It moves through five steps:

1. **The caller opts in for this tool call.** A direct MCP call includes the
   namespace in request `_meta`:

   ```json
   { "_meta": { "dev.harnx.execution_context": true } }
   ```

   A NATS `ToolRequest` uses the equivalent additive capability:

   ```json
   { "capabilities": ["dev.harnx.execution_context"] }
   ```

   Harnx's NATS provider currently opts in on every tool call. A direct MCP
   client chooses whether to opt in for each call.

2. **The tool runs and the server observes its actual target.** Filesystem
   canonicalization and Git commands run on the async runtime's blocking pool,
   so they do not stall request or picker workers. The server awaits that work
   and temporarily adds the observation to the successful tool result:

   ```json
   {
     "_meta": {
       "dev.harnx.execution_context": {
         "version": 1,
         "observed_at": "...",
         "workspace_root": "...",
         "working_directory": "...",
         "repository": {
           "worktree_root": "...",
           "branch": "feature/picker",
           "remotes": [
             {
               "name": "origin",
               "repository": "github.com/acme/app",
               "primary": true
             }
           ]
         }
       }
     }
   }
   ```

3. **The transport attests the source.** Harnx adds the server scope and
   identity, tool name, call ID, and worker receipt time. For NATS, the
   receiving provider uses the route and call it actually used instead of
   trusting provenance supplied inside the result.

4. **Harnx removes the private result metadata immediately.** The observation
   is carried separately on an in-memory, non-serialized `ToolResult` field.
   Post-tool hooks, model input, UI events, and the transcript see only the
   ordinary tool result; they never receive the context `_meta` or worker
   paths.

5. **Harnx persists it after the ordinary tool result is durable.** First the
   `ToolResults` entry is appended to the session transcript. Only after that
   append succeeds does Harnx merge the observation into
   `sessions/{id}/meta`, under the `dev.harnx.execution_context` extension. A
   failed transcript append therefore cannot leave context claiming that a
   tool result was recorded. Unmatched or duplicate results that are excluded
   from the durable entry are also excluded from the context merge. An
   ordinary metadata merge failure produces a warning without changing the
   already-completed tool call. HA worker writes remain protected by the
   session lease fence.

For example, suppose a remote bash server runs `git checkout feature/picker`
in `/srv/work/app`. Its result temporarily carries `/srv/work/app`, branch
`feature/picker`, and repository `github.com/acme/app`. Harnx strips that
private metadata from the visible result, durably records the normal command
result, and then updates the session extension. Later, opening the picker from
the same repository and branch ranks that session highly and may display
`github.com/acme/app @ feature/picker`; it never displays `/srv/work/app`.

#### Retention, privacy, and mixed versions

The extension contains at most 16 current execution locations per session; it
is not a log of the last 16 tool calls. An exact repeat is a no-op session
metadata write, but the current server still performs the observation and the
opted-in response still carries it. This favors detecting branch changes and
new target repositories without keeping caller-specific state in a shared tool
server. A later observation replaces the retained entry for the same primary
repository, or for the same canonical worktree within one tool-server scope.
Non-Git observations from the same scope and workspace also replace one
another. This updates branch and path state instead of accumulating stale
entries. If a 17th unrelated location is added, the distinct or changed
context with the oldest transport-attested worker receipt time is evicted; a
skewed tool-server clock cannot control retention.

Raw workspace paths and transport provenance remain in the private canonical
extension, but redacted HTTP session metadata removes them and exposes only an
optional repository-and-branch view. Picker rows likewise display only safe
repository identities and branches. Generic extension replace and delete APIs
cannot modify this reserved namespace.

Capability negotiation makes rolling upgrades safe. A new server strips the
context from its response unless the caller opted in. A new caller accepts a
response from an old server with no context. In either mixed-version case the
tool call still succeeds; the picker simply falls back to its other retained
contexts and normal recency ordering. Malformed observations are also stripped
and logged instead of failing the tool call.

### Private tool routing context

Trusted native tool servers can keep small, model-hidden routing values in the
reserved `dev.harnx.tool_context` session-metadata extension. Values are
versioned and updated with the same compare-and-swap loop as other mutable
session metadata. Generic extension APIs cannot replace or delete this
namespace, and redacted HTTP session metadata omits it entirely.

The NATS tool server receives the invoking session ID and call ID through
`ToolInvocationContext`; these are infrastructure context rather than tool
arguments generated by the model. A server can resolve an omitted routing
argument from the session and update the binding after an explicit lifecycle
operation. `harnx-k8s-sandbox-tools` uses this for its ambient sandbox ID.

When a sub-agent creates a new session, Harnx snapshots the parent's complete
tool context into the child initializer. The child can therefore use the same
routing state immediately and then diverge independently. Resume operations do
not overwrite an existing child's context. During a rolling deployment, update
workers before relying on inheritance; older workers preserve the unknown
extension but do not copy it into new sub-agent sessions.

## Configuration

Harnx looks for NATS cluster definitions in `nats_servers/<cluster_key>.yaml`. The filename (stem) is used as the cluster key.

### Example: Development (Plaintext)
`nats_servers/local.yaml`:
```yaml
url: "nats://localhost:4222"
```

### Example: Production (Token + TLS)
`nats_servers/prod.yaml`:
```yaml
url: "nats://nats.example.com:4222"
token: "${NATS_TOKEN}"
replicas: 3   # JetStream replica count for buckets harnx creates; defaults to 1
tls: true
tls_cert: "/etc/harnx/client-cert.pem"
tls_key: "/etc/harnx/client-key.pem"
# Note: tls_ca + client cert is NOT supported; use trusted certs or drop tls_ca.
```
*Note: Environment variable expansion `${ENV_VAR}` is supported in all fields.*

## Running Workers

A worker is its own binary, `harnx-worker`. It joins a cluster and waits for
session assignments.

```bash
harnx-worker --cluster local --worker-id worker-1
```

- `--cluster`: The key from `nats_servers/`.
- `--worker-id`: (Optional but recommended) A stable identity for the worker.
- `--manage-servers`: Launch this worker's own tool and hook servers as child
  processes. Without it, the worker discovers independently deployed servers
  under `HARNX_SERVER_SCOPE` instead — see
  [Independently Deployed Tool and Hook Servers](#independently-deployed-tool-and-hook-servers)
  below.

For the default local cluster you don't run this yourself: `harnx` and
`harnx-serve` spawn `harnx-worker` themselves, always with `--manage-servers`.
They look for it at `HARNX_WORKER_BIN` first, then next to the running
front-end, then on `PATH` — so normally the worker just has to be installed
alongside the front-end.

When developing with `cargo run --bin harnx-serve`, build the worker too:
`cargo build -p harnx-worker` (or `cargo build --workspace`). Cargo does not
build the separate worker binary when it builds only the server. An explicit
`HARNX_WORKER_BIN` must point to an existing binary; a missing override is an
error, even if another worker is installed on `PATH`. Relative overrides are
resolved from the server's working directory.

If a Web UI message appears in the transcript without an agent response,
check the displayed run error and the server's `session run failed` log entry.
The prompt is saved before worker startup, so a saved message alone does not
prove a worker was activated.

`--cluster __local__` is rejected. The reserved name identifies shared local
session state on the frontend side, but local worker execution uses a separate
frontend-managed `--session-scope __local__` mode and an exact worker target.
Persistent workers continue to use `--cluster`; their topology and
cluster-shared dispatch are unchanged.

### Shared local state, frontend-owned execution

Every local frontend owns one worker child with a generated `local-<uuid>` ID.
Two frontends therefore share the same local broker, session logs, advisory
events, session list, and lease bucket while retaining different execution
environments. An idle prompt wakes only the submitting frontend's child. If
another child already holds that session's lease, the holder consumes the new
durable messages at a tool boundary or final drain; the targeted wakeup remains
available in case the holder releases before seeing them.

The worker inherits its owning frontend's environment and current directory.
Restart the frontend after intentional configuration, environment, working
directory, installation, or binary changes. Before reusing a running child,
the frontend waits for a fresh readiness heartbeat; a child whose process still
exists but whose event loop has stopped is replaced. Health checks do not
restart a responsive child merely for input changes. Crash and stall recovery
retain the same worker ID and consumer route but start a new PID.

Frontend and worker executables may be built separately as long as their local
readiness protocol is compatible. Build SHA is diagnostic and does not decide
admission or affinity. A local protocol upgrade is a hard cutover: restart all
local frontend and worker processes. Legacy local activation streams,
consumers, and `worker.lock` files are simply left inert. Canonical session
metadata is also a hard protocol boundary: transcripts created with legacy
embedded headers/title rows or without `sessions/{id}/meta` are rejected rather
than repaired.

On a persistent cluster, you can run multiple workers for redundancy. If the
active worker for a session dies, another persistent worker will acquire the
lease and resume execution. Local redundancy instead comes from the owning
frontend respawning its worker on the same targeted route.

### Independently Deployed Tool and Hook Servers

By default a worker launches its own tool and hook servers as child processes
and assigns them a scope. To run them as their own containers instead, give
every process the same `HARNX_SERVER_SCOPE` and leave `--manage-servers` off:

```bash
# Tool server container
HARNX_NATS_URL=nats://nats:4222 HARNX_NATS_TOKEN=… \
  HARNX_SERVER_SCOPE=shared harnx-time-server

# Worker container
HARNX_NATS_URL=nats://nats:4222 HARNX_NATS_TOKEN=… \
  HARNX_SERVER_SCOPE=shared harnx-worker --cluster prod
```

**`--cluster prod` alone is not enough for the worker container.** `--cluster`
only tells the worker which `nats_servers/<cluster>.yaml` to use for the
*session* connection (leases, session log, control plane). Discovering tool
and hook servers is a separate connection that never reads that file — it
always resolves from `HARNX_NATS_URL`/`HARNX_NATS_TOKEN` (and, on a TLS or
mTLS cluster, `HARNX_NATS_TLS`, `HARNX_NATS_TLS_CERT`, `HARNX_NATS_TLS_KEY`,
`HARNX_NATS_TLS_CA`) in the worker's own environment. A worker pod must carry
these env vars *in addition to* `--cluster`, even when `prod.yaml` already
has the same URL and TLS settings — otherwise the worker connects fine for
sessions but can't discover any tool or hook server, or (on a TLS cluster)
can't discover them at all because that connection falls back to plaintext.

Both sides must carry the same scope value. A mismatch is not an error — the
worker finds no servers and logs that it searched an empty scope.

Servers deployed this way must not depend on the worker's filesystem: each
container has its own. `harnx-fs-tools` and `harnx-bash-tools` are therefore
not suitable for this mode.

A shared scope is reachable by every worker holding cluster credentials. The
instance header on each request records which worker called, but is not
checked by the server, so NATS account permissions are the enforcement
boundary.

Each worker also serves its own sub-agent toolset in-process. When several
workers share a scope they all register that toolset under the same key and
join one queue group, so any worker may serve any sub-agent request.

## Using Remote Agents

To use an agent via NATS, append `@cluster` to the agent name:

```bash
harnx -a coder@local
```

This works from the CLI and TUI.

- **New Sessions**: Canonical metadata and activity are reserved before the
  first user row is appended. The worker creates a lease only when activated.
- **Resuming Sessions**: Clients attach with an explicit agent and local session ID. Multiple clients can attach to the same session simultaneously (Multiplayer Mode).

Workers load agent identity from canonical metadata on every activation. Named
agents are re-read from disk and then overlaid with persisted session variables
and explicit overrides; inline sessions keep their raw instruction template in
metadata. A client publishing directly to `sessions.{id}.log` must initialize
metadata first. Supported frontends do this through `NatsSession` or the HTTP
session-creation API.

## Agent Catalog (Static Discovery)

To make remote agents discoverable in shell completion and interactive pickers, you can declare them in your cluster configuration.

Add an `agents:` list to `nats_servers/<cluster>.yaml`:

```yaml
url: "nats://nats.example.com:4222"
agents:
  - name: atlas
    description: "Main orchestrator"  # Reserved for future use / stored only
    role: assistant                # Optional: 'assistant' (default) or 'subagent'
  - name: critic
    role: subagent
```

### Discovery Behavior

- **Naming**: Agents appear as `name@cluster`. For example, `name: atlas` in `prod.yaml` surfaces as `atlas@prod`.
- **Filtering**:
    - **Shell Completion**: All agents appear in `--list-agents` and tab-completion regardless of role.
    - **Assistant Picker**: Only agents with `role: assistant` (the default) appear in interactive assistant selection menus. `subagent` entries are excluded from the picker.
- **Static Config**: This is purely local configuration. Harnx does not perform network calls to discover or list these agents.

## Control Plane

Cancellation is coordinated by `harnx-execution-control`, shared by the runtime,
tool servers, and hook servers. Its file-backed, replica-aware JetStream KV bucket
`harnx_execution_control` is authoritative; `sessions.{id}.control` is only a
latency hint. Losing that Core NATS message must not lose cancellation.

### Operation graph and generations

`sessions/{session_id}/current` points to the active execution record at
`sessions/{session_id}/operations/{execution_id}`. A worker activation includes
all queued continuations it consumes. Tool and hook calls are child operations;
a sub-agent session execution is a child of its invoking tool operation. The
transport-attested tool call ID is also the sub-agent invocation ID.

Registration creates a preparing child, then CAS-adds its reference to an
accepting parent. Work starts only after registration and an ancestor preflight.
Cancellation CAS freezes new children and prompt reservations. Owners watch the
operation and ancestor chain, so cancellation survives a requester or
intermediate owner disappearing. Direct child cancellation only travels down;
gated tool replies carry a typed interrupted outcome, not a recoverable tool failure.

Every activation and cancellation latency hint carries an execution ID. A stale
child row supplies `expected_execution_id` and cannot cancel a later invocation
that reuses the same session. Terminal generations never reopen. Retry preserves
the execution and cancellation IDs, increments the attempt, and refreshes the
five-second progress deadline. Missing ancestors or graph cycles fail closed.
Session owners use the lease fence; tool and hook owners use fresh invocation
owner identities. A replacement server's routing name alone cannot authorize
it to claim or acknowledge an invocation still owned by another process.

### Logical stop model (#1878, Stage 1)

Execution control now exposes separate logical and physical dimensions:

- `LogicalState::{Preparing, Running, Completed, Interrupted}`. An accepted
  `StopDecision { cancellation_id, accepted_at, reason }` makes the exact
  generation `Interrupted`, permanently. Cleanup and cancellation retries cannot
  reopen it. `can_replace_generation()` permits `Completed` or `Interrupted`.
- `CleanupState::{Pending, Confirmed, Unconfirmed}` reports owner/child shutdown.
  Neither an accepted stop nor `cancel_recorded` confirms physical cleanup.
  `cancel_recorded` tracks transcript projection coverage only.

The views derive from the persisted stop decision and existing lifecycle facts;
there are no duplicate mutable state fields for old writers to leave stale.
`OperationState` remains the compatibility lifecycle. Its terminal predicate is
not broadened: `can_prune()` still requires legacy completion (including its
explicit abandonment override), never logical interruption alone.

Retirement CAS-compacts a physical record on its original generation key, keeping
its identity, parent lineage, and stop decision until session deletion. Installing
another generation does not remove that evidence. `ExecutionStore::get` returns
no physical operation for a retired node; `stop_decision` and `is_stop_fenced`
resolve stops through both live and retired ancestry. Missing lineage is an error,
not permission to replay. Retired generation IDs cannot be reused.

`accept_interrupt` is the initial single-record state API. Production
`request_cancel` bridges interruption to the tree-wide gate below. Stages 2-7
establish the output, recovery and cleanup fences; Stage 8 enables frontend
return on acceptance. A negative stop query is a snapshot, not authority for a
later write. No transcript variant changes in this protocol.

### Tree-wide commit gate (#1878, Stage 2)

`harnx-execution-control::gate` provides the commit mechanism. Stage 3 opts tool
invocations and their lineage into it through an activation CAS (below).
`open_gate` alone initializes new logical authority; it is not a live-graph or
lease-GET import.

`ExecutionContext` captures the session generation, stable gate root, operation,
operation owner and generation-owner fence at creation. `commit_if_admissible`
and `interrupt(scope, cancellation_id)` serialize on the same per-tree key:
`sessions/{root.session_id}/gates/{root.execution_id}/head`. The library persists
an immutable candidate with its exact context, action/payload, previous head and
expected KV revision before CAS-publishing it. Candidates that lose CAS are not
committed or consumable. Even a positive idempotency retry CAS-validates its
snapshot, so stale `Running` reads cannot authorize post-stop work.

Actions include child registration plus exact input (`StartWork`), exact typed
output (`CommitOutput`), committed reply consumption (`ConsumeReply`), physical
cleanup progress (`CleanupUpdate`), and conditional historical projection
(`ProjectCommitted`). Generation and owner changes use this same gate. Root
interruption changes only its scope record; ancestor traversal fences descendants
without a cancellation walk. A normally completed producer's reply still requires
admissible consumption under the receiving generation.

Receipts prove history, never permission to append arbitrary future data. Use
`committed_action` to reconcile a lost action response after interruption;
`commit_if_admissible` rejects stopped work, including positive retries. Repeating
`interrupt` with the original scope and cancellation ID returns the original stop,
including after G2. `gate_stop` reads retained gate lineage independently of
physical-node pruning.

A persistent crit-bit index bounds individual KV values to 128 KiB (actions to
64 KiB); the anchor does not accumulate the log. `checkpoint_gate` CAS-switches
index epochs before collecting obsolete paths and losing candidates. Historical
payloads/proofs remain session-lifetime while replay and sink retention rules are
not yet installed. `projection_cursor` tracks each named projector. A cursor is
not sink deduplication: adapters must conditionally append the exact commit ID and
recover a crash between append and cursor acknowledgement.

Stages 3-6 must adapt journal/transcript/event writers, recovery and lease/session
lifecycle changes to this authority before enabling gated execution. Separate
lease writes cannot revoke a gate owner; the handover must commit here. Deletion
must stop the affected scope before removing its physical projection. Stage 1
`accept_interrupt` is still legacy-only and must not be mixed with gated output.
Cross-gate session migration is rejected pending an explicit handover protocol.
A GC race may invalidate an old snapshot; readers fail closed and retry rather
than treating missing index nodes as an unfenced generation.

### Journal, cache and replay fencing (#1878, Stage 3)

Tool requests now carry `execution` (producer and receiving `ExecutionContext`),
captured before dispatch. `replay_execution` carries an explicitly created replay
attempt without changing the original request identity. A tool server CAS-transfers
the reserved producer to a fresh owner before claiming physical cleanup ownership.
It keeps that context through handler completion; it never adopts the current
owner when a delayed reply arrives. Concurrent replacement servers cannot reuse
the reservation to execute the same call.

The bridge in `gate/bridge.rs` CAS-marks each physical operation with an immutable
`gate_registration` before materializing its gate registration. `cancel_operation`
serializes with that marker: cancellation before activation prevents import;
cancellation after the marker helps finish registration and commits `interrupt`
on the governing tree. The per-node cancellation write is cleanup bookkeeping,
not logical acceptance. Successful cancellation returns only after the gate stop
is durable. `accept_interrupt` rejects marked operations to prevent mixing Stage 1
per-node acceptance with gate output commits. Session owner handovers and generation
replacement also enter the gate; physical completion can project `FinishWork`.
The compatibility cancellation acknowledgement still waits for cleanup.

`harnx_tool_invocations` stores requests, checkpoint handles and reply history.
Completion stages an immutable content-addressed reply blob, then commits
`CommitOutput { ToolReply }` with its SHA-256 digest. Only that gate slot determines
the winning reply. The journal's `reply`, `reply_commit` and `reply_producer` fields
are a conditional projection, not a second commit boundary. Restart can recover a
committed blob even if the server died before updating these fields. A losing blob
has no committed proof and is never consumed. Blob storage avoids putting large
tool replies into the gate's bounded action records.

Every saved-reply/cache success requires a fresh `ConsumeReply` under the receiving
context. Positive idempotency retries still perform gate CAS. The in-process cache
key includes generation, operation, owner and call identity; its value includes
the exact committed producer and proof. A late cache insertion of previously
committed bytes is harmless: it supplies payload, not authorization. Cache eviction
is not part of the fence. Calls with different operation/call identities no longer
share a result merely because their `Idempotency-Key` headers match.

Recovery resolves original generation/lineage before looking for a reply or
admitting a replay. `AdmitWork` and the replay owner transfer race interruption on
the same gate. A pruned physical operation is acceptable only with retained gate
authority and a valid committed reply. Unknown legacy records fail closed rather
than attaching to the current generation. Retained stop evidence can still yield
an interrupted outcome when legacy metadata is missing. Runtime rejects successful
wire replies without a committed proof in a controlled invocation.

`ToolErrorPayload::Interrupted` and `ToolInvokeError::Interrupted` carry a durable
stop receipt. Runtime preserves `harnx_execution_control::Interrupted` as a typed
`anyhow` error inside the terminal `ToolError::Fatal` carrier. It is never converted
to the model's recoverable `is_error` result. No `SessionLogEntry` variant or
transcript wire format changed. Upgrade worker and tool-server readers together;
protocol v4 acknowledgement and cleanup changes remain Stage 7.

Stage 4 adds the transcript/model/hook/sub-agent boundaries below. Generation-first
recovery and event isolation are described in Stages 5-6. Cleanup supervision and
early return are described in Stages 7-8. A checkpoint handle is still bookkeeping,
not permission to restart work. Cross-gate session migration remains rejected. Gate proofs and blobs
remain until session deletion; standalone clients also need an explicit retention
policy before high-volume use. Stage 8 enables frontend early return and G2
while physical cleanup is still pending.

### Transcript, model and work boundaries (#1878, Stage 4)

`WorkerExecution::claim` activates the gate before constructing the control listener,
per-session config, or append sinks. Direct NATS loop callers activate before
reconstruction. Each captures a `GenerationFence`; late callbacks never resolve a
new current generation. Session worker handover enters the same gate, including
handover for projecting cancellation on a stopped session. Handover changes the
owner, not the retained stop decision. Gate owner fences stay fixed for an attempt;
lease renewals advance the transcript audit revision. Cancellation projection records
that current audit revision without rebinding execution authority.

Worker transcript writes commit `CommitOutput { Transcript }` with exact content
before projection. Large outputs use immutable SHA-256-addressed 64-KiB chunks
under the gate's session namespace; only the gate CAS accepts their digest. Large
work and hook inputs use committed blob references in their admission actions too.
The private projector reads committed decisions in sequence, filters by destination
session, and conditionally appends using the stream tail. Durable `Nats-Msg-Id`
headers prove which commit was appended after a crash; the finite broker dedup
window is not the proof. The gate cursor acknowledges projection, never authorizes
new output. An older projector cannot append G1 output behind G2: G2 first drains
all earlier committed decisions, and later G1 retries find their durable IDs.
Missing stream evidence fails closed. Session streams must retain these headers
until deletion; a future retention policy needs an explicit projection watermark.

This preserves `ToolCalls` → `SubAgentStarted` → `ToolResults` ordering. Existing
reconstruction still queues mid-tool messages to keep tool-use/result adjacency.
HITL output retains its expected-tail condition; a losing conditional projection
returns no append and forces the caller to re-derive state. `RecordCancellation`
is a separate control action: it requires an already stopped, current session
generation and its owner, contains no worker output, and projects `Cancel` in the
same ordered drain. `cancel_recorded` remains coverage/projection bookkeeping.

User input is committed history before model work, not output authorized by a
model response. `NatsSession` reserves the prompt on its generation, appends the
user row, then commits its sequence before activation. Workers retain
`skip_user_log_append` for that already-durable input. The shared in-process
agent loop also prepares input before calling the model; assistant/tool persistence
must not be the first place a user's prompt is saved. Its request-local history
cursor prevents duplicate appends while keeping prompt patches out of durable
history. `Cancel` leaves user rows in history but closes their pending/replay
status. Retained Stage 5 admissions still bind them to their original generation.

Model resolution first rechecks local abort, then commits exact `ModelResponse`
through the gate before assistant persistence, shared conversation mutation, final
emission, or returned tool dispatch. Tool evaluation commits `StartWork` at
handoff and checks admission after handlers and post-hooks. Interrupted outcomes
are terminal for the generation, not synthesized model-visible tool failures;
no normal final/turn-end is emitted. Background metadata (title, settings and
tool-observed execution contexts) uses `SessionMetadata` output in the same drain.
Its metadata CAS records `worker_projection`, preventing an old projector retry
from overwriting newer metadata.

Controlled hooks carry creation-time execution context, claim their invocation
through gate owner transfer/admission, commit the reply, and require `ConsumeReply`
before the caller accepts it. Sub-agent creation checks local cancellation and
commits work admission before allocation/creation, fences `SubAgentStarted` using
the invoking tool's context, and checks again before child activation. Child
session execution joins the parent's gate through the existing activation bridge.
Constructor/local-tool admissions are gate-only work records; physical handler
ownership still belongs to the existing execution graph.

No `SessionLogEntry` variant or field changed. Stream headers are additive. The
optional metadata `worker_projection`, gate action/output kinds, and controlled
hook header/reply shape require a coordinated worker/hook-server deployment.
Protocol v4 acceptance acknowledgements remain Stage 7. This stage does not enable
early return, overlapping generations, or asynchronous cleanup. Generation-first
orphan recovery and pending-prompt adoption are handled by Stage 5 below.
Remaining legacy lifecycle/cleanup adapters are deferred to Stage 7. Live event
envelopes/render isolation are described in Stage 6 below.
Historical committed output may be projected after stop, but only in its original
place before later generation output. New output from that stopped generation is
rejected.

### Recovery and legacy ownership (#1878, Stage 5)

Recovery resolves the original generation before reconstructing a model turn.
Worker claim reconciles an unfinished gate registration, reads retained stop
lineage, and projects a missing `Cancel` before reconstructing model state. A gate
stop is authoritative even when physical cancellation has not reached the worker
or its descendants. Direct loop callers follow the same order; they cannot create
a new generation to resume an old stopped prompt.

Prompt reservations already bind message IDs to an execution before the client
appends them. Those reservations, worker-fence history and gate registration now
survive physical-node retirement on the original operation CAS key. Recovery
follows retained `previous_generation` links, not a key listing of the busy KV
bucket. It backfills a lost prompt sequence acknowledgement from its reserved
message ID.
It does not reserve an uncovered old prompt on a new generation. Missing or
ambiguous ownership requires explicit history repair instead of automatic adoption.

Tool-call ownership comes from the Stage 4 committed transcript proof in the
existing message-ID header. Older rounds can use their original invocation journal
contexts or an unambiguous retained worker fence. An idempotent/read-only hint is
considered only after resolving that authority and passing a gate admission.
Unknown calls fail closed without invoking a tool. `Cancel` closes orphan discovery;
model-history reconstruction can balance the unresolved call in memory, without
appending a late successful `ToolResults` on behalf of the stopped generation.

`AdmitRecovery` is the common CAS boundary before saved-reply consumption or replay.
It validates the receiving owner and original generation and checks the original
operation's retained stop scope. The saved branch still uses `ConsumeReply`; the
replay branch still transfers ownership and uses `AdmitWork`/`StartWork` before
dispatch. All these actions race cancellation on the same gate. `Interrupted`
remains terminal, including through the legacy rerun error path. An exit without
interruption keeps its generation and remains resumable.

A child whose first claim happens after its parent's stop can recover its original
lineage using `RegisterStoppedWork`. This control-only registration starts in
`Interrupted`, carries no work input, and cannot authorize model/tool output.
Retained gate registrations allow ancestry resolution after physical pruning.

New prompt admission reconciles the previous stop projection before replacing the
generation. `RecordCancellation` projection uses its recorded transcript tail as a
conditional append, so a delayed projector cannot cover a newer prompt. Recovery
refuses ambiguous mixed-generation history rather than moving the old stop boundary
past new input. Stage 8 uses this ordered boundary when admitting overlapping generations.

No `SessionLogEntry` variant or field changed. Gate records have additive ownership
fields and new action tags; upgrade workers and tool servers together. Existing
pre-gate history without reliable binding is not granted execution authority.
Live event isolation is described below. Protocol v4 and asynchronous cleanup are
in Stage 7; early return/G2 overlap is enabled by Stage 8. Session-lifetime
ownership retention and recovery scans still need a later retention/indexing policy.

### Live event and UI isolation (#1878, Stage 6)

`AdvisoryEnvelope.execution_id` is the existing execution/operation generation ID.
It wraps every live `AgentEvent`, including `SessionEvent`, model final/error/chunks,
tool completion/progress and turn lifecycle signals. It's optional on the wire
(`serde(default, skip_serializing_if)`), so old payloads still decode. Live consumers
fail closed when the ID is absent. No durable `SessionLogEntry` field or variant changed.

Workers bind their `NatsEventSink` to the creation-time `GenerationFence`. The emitter
stamps the envelope before enqueueing; the publisher never resolves a new identity
for an old event. Enqueue rejects locally stopped producers. The ordered publisher
rechecks the captured generation and retained gate stop before sending buffered
output. Required-event transport/authority errors still reach the flush barrier;
stale-generation discards are intentional, not publish failures.

Subscribers load gate authority after subscribing and before draining the buffered
advisories. Generation mismatch or accepted stop rejects a live event regardless of
`after_seq`. The existing durable sequence filter also applies (the shared observer
uses its attachment boundary, since its recovery cursor doesn't render transcript
rows). The dedicated follower keeps its admitted generation when projecting live
status, sequence assignments and errors. Its post-cancellation final flush drops
stopped or replaced generations before any decoration or sink emission.

The TUI carries the original generation through its own event queue. Prompt events
also carry the prompt task's abort-signal identity, checked like `PromptTaskFinished`.
Shared observers and child monitors carry attachment identity. These checks precede
all live reducers, so an old Final, Completed, Ended, error or progress update cannot
append transcript rows or clear a replacement turn's spinner. No new turn epoch is
introduced: attachment/task pointers only prevent detached readers from updating a
replacement reader; generation identity still comes from the execution gate.

Accepted cancellation receipts immediately populate a local stopped-ID set. Forked
attachments retain that set. Reconnect reloads the durable fence before any buffered
live event is eligible, including when the frontend has lost all local memory. The
stopped set is a rejection cache, never permission for durable output or recovery.
Historical transcript replay remains separate and still displays committed output
from interrupted generations. Durable activity resolves the latest effective user
row's original generation from retained prompt admissions. It can settle its own
stopped turn, but an old TurnEnd cannot become a replacement turn's idle transition.

Remote AG-UI followers now retain the attached prompt's generation through their
output queue (Stage 8 below). Browser-side isolation remains a separate follow-up:
reducers in `ChatProvider.tsx`, `RuntimeSessionSubscriber.tsx` and
`SubAgentSessionNotes.tsx` still need envelope identity and receipt-driven stop
handling for events already in browser queues. No web source changed in Stage 6.

### Acceptance versus shutdown

`NatsSession::request_cancel` bounds durable acceptance to two seconds without
waiting for shutdown or recovery activation. Once its KV CAS succeeds, a slow or
failed activation wake-up cannot turn the accepted request into a persistence
failure. `cancel_pending_turn` returns the receipt's acceptance boolean without
waiting. `cancel_status` and `wait_for_cancel` remain explicit cleanup diagnostics,
not frontend completion APIs. An idle cancel succeeds without appending a
transcript entry.

Status reconciliation propagates an accepted cancellation through registered
descendants. An ownerless operation still in `Preparing` never started work and
is closed immediately; this repairs the interruption race where a sub-agent
session was registered but cancellation arrived before its activation. Other
descendants retain their normal owner cleanup requirements.

The state machine is preparing → running → completed for normal completion, or
cancel_requested → quiescing → cancelled for cancellation. Five seconds without
progress produces **unconfirmed**, which remains nonterminal in the compatibility
lifecycle. Gated backend admission can replace an interrupted generation; the
TUI and CLI no longer wait for cleanup before returning control.
It may later converge to cancelled. Retry can move unconfirmed back to requested.
Closing normal prompt admission does not itself cancel already-registered work.

An operator may explicitly abandon an unconfirmed cancellation to unblock the
session. Abandonment terminalizes the old operation graph and marks its records
with `abandoned: true`; it does not assert that vanished owners completed cleanup,
and their external work may still run. The terminal records reject late owner
updates, and the next prompt installs a fresh execution generation. This override
is generation-scoped and is never available before cancellation becomes
unconfirmed.

The worker signals local abort and writes the existing fenced
`SessionLogEntry::Cancel`. Stage 7 releases execution separately from supervised
cleanup. The compatibility status still converges only after owner/descendant
evidence and transcript coverage. A transcript marker or absent lease alone
is not proof of physical shutdown for a gated execution.

Prompt IDs are allocated before append. A CAS reservation decides whether a
concurrent prompt belongs to the cancelling execution. An admitted message is
appended with that ID and its sequence committed to the reservation. Recovery
looks up unresolved IDs in the log. Frontends must not acknowledge an unreserved
local queue as durable prompt acceptance. A late append beyond an earlier Cancel
marker requires another fenced recovery pass.

Admission subscribes to advisory events before appending and publishing activation.
Its receipt retains that subscription until the frontend follows the admitted
prompt. A fast worker can finish before the frontend's follow task starts;
durable completion drains eligible already-buffered events before closing that turn.
Accepted-stop and generation checks also apply to this final drain.

### Cleanup supervisor and protocol v4 (#1878, Stage 7)

The internal tool protocol is **v4**. Deploy workers, frontends and tool servers
together; registrations with another version are rejected. No durable
`SessionLogEntry` variant changed. The v4 cancellation acknowledgement is:

```rust
struct CancellationAcknowledgement {
    protocol_version: u32, // 4
    generation: OperationRef,
    operation_id: String,
    cancellation_id: String,
    acceptance: CancelAcceptance,
    cleanup: Option<CleanupStatus>,
}
// acceptance (tagged by "kind", snake_case):
// Accepted { stop: StopReceipt } | AlreadyFinished |
// Rejected { reason: String } | Unknown { reason: String }
// cleanup: { state: Pending | Confirmed | Unconfirmed,
//            owner_stopped: bool, remaining: usize, last_error: Option<String> }
```

`stopped: bool` is removed. Control requests include `protocol_version`, the
creation-time `ExecutionContext`, server identity, operation/call IDs and the
stable cancellation ID. Server identity prevents another subscriber on the
shared control subject from answering for this invocation.

The tool server validates identity, cancels its local token, then commits the
scope stop through the gate or proves an existing ancestor stop. It immediately
returns `Accepted { stop }`, normally with cleanup `Pending`. It doesn't wait
for the handler or descendants, or perform a cleanup-status read after obtaining
the receipt. Cleanup bookkeeping follows independently through `CleanupUpdate`.
A response timeout is `Unknown` **acceptance**: retry the same identity to recover
the committed receipt. `Unconfirmed` **cleanup** says nothing about whether the
stop was accepted. Session cancellation depends on root acceptance, never on
these per-tool acknowledgements. Recovery activations and owner notifications
run after the root receipt, not in its return path.

Every worker starts a cleanup reconciler before serving turns. Its task is held
by `WorkerRuntime`, outside individual turns. Retained gate scopes are its durable
queue: startup and periodic scans recover stops even if a worker died between
acceptance and the first wake-up. Watches reduce latency but aren't authoritative.
The reconciler traverses physical resources under each interrupted scope, sends
idempotent generation-bound owner requests with backoff (up to 30 seconds), and
records aggregate `CleanupUpdate` status on that scope's gate. Five seconds
without confirmation produces `Unconfirmed`. Later owner evidence may advance
it to `Confirmed`; confirmed cleanup never reopens. Missing operation metadata,
a missing process handle, an absent lease or a transcript Cancel isn't proof of
shutdown. Recovery-only workers don't claim that a prior worker's resources have
stopped. Old gate records containing only a cleanup label still decode, but a
legacy confirmation without owner evidence is treated as unconfirmed.

Resource owners retain their own handles:

- **Tool invocation:** a server-lifetime supervised task owns the handler. The
  request path owns a reply receiver only. A cooperative handler can stay pending
  forever without delaying acceptance or the interrupted reply. Its cleanup
  budget expires independently. Normal success or handler errors aren't replaced
  by cleanup-derived Fatal errors. `HardOnDrop` remains a promise that dropping
  the owned future stops all per-call work, not merely its reply stream.
- **MCP:** bounded best-effort typed `notifications/cancelled`, close/unregister
  this request's waiter and ignore late responses. No wait for a remote reply,
  and no shared-server kill/restart to cancel one call. Lack of remote termination
  evidence is `Unconfirmed`. The Kubernetes sandbox MCP adapter follows the same
  rule without invalidating its shared session on cancellation.
- **Sub-agent:** fence the original invocation subtree and transfer the owned
  turn/follower handle to supervision. The foreground no longer waits for that
  turn or calls the blocking five-second compatibility wrapper. Child timeouts
  close a child-only admission token, so a late startup cannot create new work
  and a delayed G1 cancellation cannot target G2.
- **Foreground bash:** signal the owned process group, allow bounded TERM grace,
  then escalate to group KILL and reap in the invocation owner task. The leader
  isn't reaped during grace. Linux checks its start-time identity as well as the
  owned child handle; no cleanup request reconstructs a reusable PID. Failed
  identity checks remain unconfirmed rather than signalling an unrelated process.
- **Model:** abort/drop the turn's model future. The lease supervisor doesn't wait
  for the turn's physical drop; it retains the join handle in cleanup. A started
  `spawn_blocking` cannot be aborted. Dropping a `JoinHandle` only detaches it.

The session lease/control supervisor is outside the turn task. Each activation
has a fresh cancellation signal, generation-bound sinks, per-turn config and
completion channels. Interrupted cleanup retains G1-only state, not the active
session slot, and never releases the lease serving G2. Tool-server user keys
include the execution ID; old cleanup cannot release G2's server claim. Shared
servers remain owned while invocation cleanup is unconfirmed. Activation
preparation also runs outside the command dispatcher so slow server startup
can't block other sessions or cleanup.

Dropping a reply cannot undo an already-applied external side effect. Logical
interruption rejects further Harnx output/work; it doesn't promise that arbitrary
external work rolled back. Session deletion is still explicit. Stop, reply and
cleanup evidence remain generation-scoped until retention permits deletion.

### Early return and overlapping generations (#1878, Stage 8)

TUI, one-shot CLI and NATS prompt followers complete interruption on durable
**root acceptance**, not physical shutdown. `NatsSession::request_cancel` returns
only after execution control has committed the stop through the governing gate.
Its `CancelReceipt.cancelled` field means accepted, even when `disposition` still
says requested, quiescing or unconfirmed. It does not mean every resource stopped.
Tool acknowledgements, model future drop, transcript `Cancel`, lease release and
cleanup confirmation are not prerequisites for returning this receipt.

A dedicated follower watches its **admitted execution ID** through retained gate
lineage. The watch stays independently pollable during attachment, activation
and final history reads. Root acceptance returns `NatsTurnResult` with
`was_cancelled: true`, no response and no error; it skips the final transcript
reload and advisory flush. A follower attaching after G2 still resolves G1's stop,
never G2's final response. An incomplete registration means keep waiting, not
accepted cancellation or permission to run. Unreadable authority is an error.
One-shot timeout signals this same generation-bound follower and preserves its
timeout output/exit-code contract. It never sends a second session-current cancel
that could accidentally interrupt G2.

After acceptance the TUI dismisses the cancellation tray and confirmation modal,
clears logical busy state and restores the composer. G2 can be submitted immediately.
The previous frontend follower is aborted and its join is supervised asynchronously;
there is no 500ms drain. Each prompt has fresh abort, event and pending-message
state. Receipt-driven stop and generation/task checks reject late G1 events.
Closing a tool-confirmation route synchronously invalidates its queued requests,
so a delayed G1 modal cannot cover G2's composer.

G2 admission still reconciles G1's ordered stop projection before appending new
input. This is a durable ordering boundary, not a wait for handler shutdown.
Every cancellation projection validates retained prompt ownership, including the
worker/control path. G2 input can exist before G2's worker installs its gate member;
gate-current G1 alone isn't permission to cover that input. Ownership validation
and expected-tail projection CAS fence both sides of this interval.
The worker transfers G1's turn join and resource cleanup to the Stage 7 supervisor
and releases its session lease/active slot independently of G1 physical drop.
G2 runs on the same worker while a cooperative G1 handler, model drop or external
resource remains pending. Only one execution-control lease is held per session;
cleanup has no authority to release G2's lease or mutate G2's state. G1 tool replies,
model results, hooks and advisories retain their creation-time fences.

New worker lease records also carry `execution_id`, preserved by renewal. If a
frontend exits or a worker crashes after acceptance but before releasing its lease,
the next worker reads that lease's original execution stop and revokes only that
exact lease revision. Normal create-CAS then arbitrates replacement ownership;
a renewal or intervening G2 lease defeats the revocation CAS. A pre-registration
cancellation can revoke its bound lease too: the same operation CAS permanently
closed gate registration. Missing execution identity never permits takeover, so
unattributed legacy leases still require expiry. Lease takeover is not output
permission; the replacement must claim its generation/owner through the gate.

Normal success still requires durable `TurnEnd` coverage. An assistant row plus
`Turn::Ended` is no longer a completion shortcut: both can precede the worker's
durable `Error`. Failure cleanup stops leftover child scopes without turning the
root failure into a user interruption. This keeps one-shot terminal errors visible.

No opt-in flag is required for gated generations after the deterministic race
suite passes. Legacy history without gate authority remains fail-closed: it needs
reconciliation or explicit repair, not an invented acceptance proof. Cleanup
`Unconfirmed` cannot revoke an accepted gate stop or block replacement. Acceptance
`Unknown` (for example, a persistence timeout) cannot promise safe retry; retain
the original execution/cancellation identity when reconciling it.

Cleanup continues asynchronously and is recovered from retained stops after a
worker restart or crash immediately after acceptance. Interrupt-and-exit returns
on acceptance for local workers too. Exiting a local frontend still tears down its
owned process tree; unavailable physical evidence remains unconfirmed rather than
blocking the next gated generation. Cancellation cannot roll back external side
effects already applied.

Deterministic coverage includes same-worker G2 execution while G1 model drop is
held at a barrier, a cooperative tool handler held past G2 completion, followers
without a transcript stop marker, and delayed G1 followers/confirmation requests.

Remote AG-UI SSE runs also bind to the generation that owns the attached prompt,
not the session's current generation when an event arrives. A separate retained
stop watch ends G1 even while history reads or a full output channel are blocked.
G2 taking the session lease cannot prolong G1's stream or send G2 content under
G1's run ID. Queue entries retain generation identity until wire emission. The
lifecycle guard runs after that final filter, closing only segments sent to the
client; discarded queued starts cannot produce orphan ends. Unknown legacy
ownership stays history-only and cannot adopt a later generation's live events.
Tests cover a retained stop without any transcript Cancel/TurnEnd, an active G2
lease, channel backpressure, and cancellation before/after a queued lifecycle start.

The Stages 1-7 suites cover journal/model/registration/dispatch races, stale KV,
lost CAS acknowledgements, old high-sequence advisories, crash recovery, pruning,
projection retry and late cleanup. Tests use barriers or explicit phase inputs;
timeouts only bound failures.

### Frontend behavior

The TUI shows a root cancellation tray only until durable acceptance. From any
unresolved phase, `Esc` restores an editable input while keeping the cancellation
workflow alive. A compact status line retains progress, errors and retry hints.
Restoring the editor is separate from admitting a new prompt: `Enter` cannot
submit or queue a prompt while root cancellation is unresolved. Paste is ignored
while the tray hides the editor; after `Esc`, paste edits only the retained draft.
Acceptance clears the tray and submission guard immediately, without waiting for
physical cleanup or auto-submitting the draft. G2 can then be submitted while G1
cleanup continues as described in Stage 8.

When the execution ID is known, `Esc` from `Unconfirmed` or `Failed` abandons
directly without a confirmation modal. The prior-work warning remains in the
tray status. With an unknown ID, `Esc` only restores the editor; it does not
abandon work. `Ctrl+C` retries from these unresolved phases, retaining the
observed session, cluster, execution ID and restored-editor state. It never
retargets a delayed G1 request to the current G2. Repeated keys do not restart an
in-flight request or abandonment. `Ctrl+D` exits immediately.

Explicit abandonment remains available for unresolved historical cancellation;
it permits a new execution instead of reviving the old one. After local
abandonment, the frontend retires its managed worker and tool-server process
tree so the next prompt starts on a fresh worker. Ordinary accepted interruption
needs neither abandonment nor a resume-anyway confirmation.

Child Ctrl+C targets only the viewed or focused invocation whose monitored
execution ID matches, without clearing the root composer's state. Worker
preparation remains outside the two-second durable-acceptance bound, and local
retries retain their frontend-targeted activation route. Read-only attachment
resolves gate acceptance before treating physical cleanup as logical activity;
an unresolved observed cancellation can retry recovery for that same generation.

While requesting interrupt-and-exit, Esc stays in the TUI and keeps cancellation
running; Ctrl+D exits immediately. Acceptance exits automatically unless Esc
cancelled that exit intent. Local worker ownership does not add a shutdown wait.
The Web UI keeps its existing cancellation/abandonment controls; its browser
reducer isolation remains the separate follow-up described in Stage 6.

`session/cancel` accepts optional `expected_execution_id`. Its response retains
`cancelled` and adds `disposition`, `cancellation_id`, `execution_id`,
`requested_at`, `unconfirmed_after_ms`, and `abandoned`. Dispositions include idle,
requested, already_requested, quiescing, cancelled, and unconfirmed. Idle is HTTP
success. `abandoned: true` distinguishes an operator override from confirmed
owner cleanup.
`session/abandon_cancellation` requires `expected_execution_id` and exposes the
same override to browser clients. The Web UI presents separate retry and
confirmed resume-anyway actions. A one-shot CLI prompt can opt in with
`--resume-anyway`; ordinary prompt admission remains fail-closed.
`session/get` reports `cancelling` or `cancel_unconfirmed`, disables `canPrompt`,
and retains `canCancel` for retry. AG-UI `CUSTOM` events named `cancellation_state`
carry operational updates without changing the transcript protocol. Clients also
hydrate from `session/get`; missing live events must not imply idle.

## Multi-Client Support

Multiple clients can attach to a single session:
1.  Clients replay the **durable history** from the JetStream stream.
2.  Clients subscribe to **live advisory events** on `sessions.{id}.events` for real-time updates (streaming chunks, tool progress).

Late-joining clients automatically converge to the same state by replaying the durable log.

### Busy-State Reflection for Remote Workers

When a NATS worker holds the session lease, the Web UI's AG-UI `/run` endpoint
follows a different path than the local-actor case:

- **Local actor running**: the SSE stream follows the `SessionActor` broadcast,
  which emits real-time `AgentEvent`s from the model/tool loop.
- **Local actor idle + remote lease active**: the AG-UI endpoint attaches to
  `SessionEventStream` advisories and binds to the generation owning the
  snapshot's last `User` row. It translates eligible events to AG-UI frames and
  terminates on that generation's durable root stop or matching `TurnEnd` coverage. A
  sustained lease absence (5 consecutive 1s polls with no `TurnEnd`) remains a
  worker-crash fallback, never proof of cancellation or physical cleanup.

The session metadata watch endpoint (`GET .../events`, `session_updates` in the
serve implementation) is separate from the AG-UI `/run` stream and provides
lightweight `session-updated` notifications that trigger client rehydration.
Two distinct endpoints keep the AG-UI run stream finite and properly sequenced
(RUN_STARTED → … → RUN_FINISHED), while the watch channel converges late
observers on the same durable state.

Tool confirmations are **point-to-point** over NATS: the worker sends the
confirmation request to the `ToolConfirmationRoute` subject carried on the
activation (stored as `tool_confirmation_subject`). Only the owning frontend
receives the prompt; other observers see the resulting durable log updates
after approval.

### Trailing Tool Calls: Pending vs. Interrupted

A bounded history snapshot can legitimately end at `ToolCalls` while the lease
holder is still executing the tool. Read-only observers sample the session
lease before loading history (and again afterward if it was initially free):
while either sample is active, replay preserves that tail as pending and AG-UI
emits the assistant tool call without a result. Only a lease-free replay turns
a trailing call into an interrupted result. Replay also ignores redundant
`ToolResults` for IDs that already completed, so old recovery artifacts do not
appear as current orphan warnings.

## Failover & Safety

## Session Unread State

Harnx tracks session-level unread state to surface sessions that require user attention. The unread indicator appears when:

- A model final message (`TurnEnd` with non-zero `through_seq`) was appended by a worker.
- A tool confirmation (`HitlApprovalRequested`) is pending.
- A user explicitly marked the session unread.

The unread state follows a monotonic cursor model stored in NATS KV under `harnx_sessions`:

- **Key**: `sessions/{storage_key}/read/default` (`viewer="default"` — session-level, not per-user). `storage_key` is the SHA-256 identity derived from agent plus local session ID, so equal local IDs owned by different agents don't share unread state.
- **Value**: `{ "last_attention_seq": u64, "last_read_seq": u64, "manual_unread": bool }`.
- **Predicate**: `is_unread = (last_attention_seq > last_read_seq) || manual_unread`.
- **Monotonicity**: `last_read_seq` never moves backward; manual unread is a separate bit.

### Worker Attention Bump

Workers automatically bump `last_attention_seq` when appending attention-producing entries:

- `TurnEnd { through_seq, .. }` where `through_seq > 0` — indicates a final message turn.
- `HitlApprovalRequested { .. }` — indicates a pending tool confirmation.

The bump uses the assigned stream sequence of the appended entry, enabling repair from the log if the bump is lost (e.g., CAS failure, crash before KV write). On worker resume and on server-side session load, `reconcile_attention_from_log` derives the maximum attention sequence from the transcript and repairs the KV entry.

### Mark-Read / Mark-Unread

Users clear unread via presence actions (typing, ESC, CTRL-C, CTRL-D, submit) or explicitly via:

- **TUI**: Press `'u'` or `'U'` in the session picker to toggle unread.
- **Web**: Click "Mark unread" / "Mark read" button on session cards, or use the JSON-RPC methods:

```
POST /v1/agents/{agent}/sessions/{session}/rpc
{ "jsonrpc": "2.0", "method": "session/mark_read", "id": 1 }
{ "jsonrpc": "2.0", "method": "session/mark_unread", "id": 1 }
```

- `mark_read`: Advances `last_read_seq` to `last_attention_seq` and clears `manual_unread`.
- `mark_unread`: Sets `manual_unread = true` without touching the cursor.

Both are idempotent; both publish to the read-invalidation subject.

### Read Invalidation Subject

Mutations to read-state publish a notification to:

```
harnx.session.{storage_key}.read.invalidated
```

This subject uses the same agent-scoped storage key as the read cursor and is separate from `harnx.session.{storage_key}.metadata.invalidated` (which carries the `/meta` revision). Clients subscribe to the read-invalidation subject for live updates:

- **SSE**: The `/v1/agents/{agent}/sessions/{session}/events` endpoint emits `event: read-updated` for read-state changes, bypassing the `after_seq` gate. The active session's `RuntimeSessionSubscriber` calls its `onReadUpdated` callback on receipt.
- **TUI**: Subscribes to `harnx.session.{storage_key}.read.invalidated` for the active session in `session_activity.rs` and refreshes the current session's unread indicator and picker sessions on receipt.

### Client Cache Reconciliation

Read-state updates are advisory; clients must handle missed invalidations:

- **Subscribe-before-snapshot**:
  - **TUI**: `picker_sessions` in `lifecycle.rs` subscribes to the wildcard subject `harnx.session.*.read.invalidated` before taking the session list snapshot, then drains invalidations that arrived during load and refreshes their read-state.
  - **Web**: The web client does not use subscribe-before-snapshot; it relies on list-level refetching.
- **Dirty-set tracking**:
  - **TUI**: Sessions invalidated during the snapshot load are tracked in a dirty-set, then refreshed after the snapshot completes.
  - **Web**: Not used. Web converges via list-level refetch rather than a per-session dirty set.
- **Live invalidation refetch**:
  - **Web**: Active session SSE stream receives `read-updated` events, invoking `onReadUpdated` / `refreshSessions` to refetch the session list.
- **Periodic reconcile**:
  - **TUI**: When the session picker modal is open, the main loop emits a `RefreshSessionList` event every 30 seconds. The event handler in `input.rs` refetches the session list.
  - **Web**: Bounded 30-second periodic reconcile: `useSessionDiscovery` runs an interval triggering `refreshSessions`, and `RuntimeSessionSubscriber` calls `onReadUpdated` every 30 seconds.
- **Reconnect re-snapshot**:
  - **TUI**: On NATS reconnect, the session activity monitor re-subscribes to read-invalidation and re-fetches the current session's read state. The picker refetches on the next periodic reconcile or when the user next opens it.
  - **Web**: SSE `onopen` handler calls `onReadUpdated` on reconnect, triggering a session list refetch.
- **Subscribe-before-snapshot (active session)**: In the TUI, `monitor_session_connection` subscribes to read-invalidation before attaching the session event stream. When a `ReadInvalidation` arrives, it forwards `SessionReadInvalidation` to the UI for the current session.


### Recovery contract and shared implementation

Local broker ownership is temporary; its authenticated endpoint and JetStream
store are persistent. The first owner lets NATS allocate a port. Subsequent
owners bind that same port with the same token, publishing a new owner nonce.
`ports.json` survives shutdown; the exclusive lifetime lock establishes whether
an owner exists. A port conflict fails explicitly rather than moving the broker
and stranding existing clients. Keep this file private (0600); it contains the
local authentication token.

`nats_local_server::LocalBroker` continuously checks ownership, including while
a frontend is idle or awaiting a sub-agent. Both frontend worker supervision and
configuration-backed local connections retain this guard. When an owner exits,
survivors elect one replacement. Existing workers keep their PIDs, environments,
tool servers, and activation routes. Exiting a frontend still shuts down that
frontend's own worker tree; this recovery contract covers surviving frontends.

All production runtime, tool-server, and hook-server connections use
`harnx_nats_common::connect::NatsEndpoint`. It keeps reconnecting to the stable
endpoint with bounded reconnect delay. Reusing the same `async_nats::Client`
preserves Core subscription identity. Socket reconnection alone cannot recover
messages lost while disconnected: Core NATS is at-most-once, whereas durable
state must be read back after reconnect. See the
[NATS reconnect guidance](https://docs.nats.io/learn/resilient-clients/reconnection).

Use these operation-specific patterns when adding a NATS caller:

| Operation | Recovery rule and implementation |
| --- | --- |
| Read durable state | `recovery::read` retries transport reads within a fixed deadline. Decode/validate outside the retry. Never translate an unreadable record into absence. |
| KV cancellation watch | `recovery::kv_updates` restores interrupted watches and supplies periodic wakeups. `ExecutionStore` rereads the current ancestor chain; an advisory is never evidence that work is permitted or cancelled. |
| Append a transcript entry | Retain the same message ID, payload, and CAS expectation through `recovery::retry_until`. Retry timeout/broken-pipe errors; a CAS conflict is authoritative. |
| Mutate execution or session metadata | `cas::update` reads back an ambiguous acknowledgement. Matching persisted bytes confirm the write; an unchanged revision permits retrying the exact CAS. A different value/revision remains unconfirmed. |
| Renew a session lease | Retry the same deduplicated CAS only until the previously confirmed lease deadline. A conflict or expired deadline fences the worker; reconnect never restores lost ownership. |
| Invoke a tool or hook | `rpc::request`, `RequestActivity`, and `ReplyTarget` confirm activity and acknowledge replies. Resend a completed result until receipt, never rerun the handler. A NATS 503 is not a reply receipt. |
| Renew discovery registration | `registry::refreshes` retains a separate pending renewal future so broker latency cannot block serving requests, controls, or shutdown. |
| Observe turn completion | Incremental transcript polling operates independently of live advisories. Durable coverage determines which prompt completed; a prior turn's advisory cannot complete an injected prompt. |
| Consume worker activations | Reopen the durable pull stream after errors/closure; acknowledge according to the activation's durable admission and lease state. |

General recovery operations have a 15-second budget. RPC handler heartbeats renew
the activity deadline, so a healthy long-running handler has no imposed total
duration beyond its configured timeout. Results retain a bounded receipt window.
Loss of activity or exhausted recovery reports **completion unconfirmed**; it
does not prove that a side effect was rolled back or that a descendant stopped.
Cancellation continues to use the durable execution graph and ownership checks.

Deploy the frontend, worker, and tool/hook binaries together for this contract.
Older clients still receive ordinary replies, but an older server cannot provide
the activity receipts expected by a new long-running RPC caller. Restart all
local frontends for the persistent endpoint policy: an old broker owner still
deletes its discovery metadata at shutdown. No transcript variant is added.

Regression coverage lives in `nats_local_server/failover.rs`,
`local_worker_supervisor.rs`, the execution-control/tool completion tests, and
`harnx-nats-common/tests/registry_ttl/recovery.rs`. Failover tests must retain
existing clients and in-flight work while removing the broker owner; reconnecting
a newly constructed client alone does not establish recovery.

### Leases & Fencing
Harnx uses a renewable CAS (Compare-And-Swap) lease in NATS KV:
- **TTL**: ~30 seconds.
- **Renewal**: Every ~10 seconds.
- **Fence Token**: The KV revision of the lease. Every write to the durable log is gated by this token.

If a worker loses its lease (e.g., network partition), it immediately aborts. This prevents "split-brain" scenarios where two workers think they are active.

### Diagnosing sub-agent completion stalls

An assistant message in the child transcript does not prove the parent received
the result. Check the child's `TurnEnd` or lease release, then the parent's
`ToolResults` for that invocation. A terminal result in the parent distinguishes
a missed display update from a blocked invocation. Live progress is advisory;
the session follower repairs missed terminal progress from durable tool results.

Completion polling retains raw history and reads only entries beyond its last
successful cursor. It remains independently pollable while delivering advisories
and cancellation. Each refresh is bounded to 30 seconds; three consecutive failed
reads report that completion is unconfirmed. Similarly, an unbounded tool request
ends after three failed registration checks instead of silently waiting through
a persistent backend outage. These failures do not attest that the child stopped.

`NatsSessionLog::read_range` skips only explicit `NoMessageFound` retention gaps.
Transport and storage errors must fail the read: treating them as missing entries
can hide completion or fabricate an incomplete transcript. Do not advance a
replay cursor after a partially successful read.

### Resume & Idempotency
On activation, the worker repairs pending tool calls before asking the model to
continue. NATS invocations are journaled in `harnx_tool_invocations` before dispatch,
with the original call identity and durable `ToolCalls` sequence. Recovery reissues
that request with `ToolRequest.replay`, attested by the current parent owner.
Dispatch also revalidates the worker's live lease; a graph owner snapshot alone
cannot prove that the lease remains held. Saved replies can be recovered without
a current tool registration or live tool server.
Redispatch must retain the logical server identity and raw tool name; the process
scope may change after restart.
The tool server returns a persisted reply, joins its existing in-memory invocation,
or applies its `Toolset` replay policy. The default permits advertised read-only or
idempotent tools; other tools return an interrupted-operation error. A refusal to
replay does not assert that unknown external work has stopped.
Opting into replay permits overlapping observers: a tool must tolerate repetition
or reattach its durable job even if the prior observer is still alive. A graph
ownership transfer by itself does not provide exactly-once external side effects.

Resumable tools opt into replay and checkpoint a durable job handle before starting
work. Sub-agent tools retain the child session and use the original invocation ID
as its prompt admission ID, so replay follows the admitted turn without appending
another prompt. Prompt recovery uses transcript lookup plus tail CAS, not just the
broker's time-limited message deduplication. Child ID reservations retain the
invocation identity in canonical metadata, closing the crash window before the
checkpoint is written. Durable start entries are also deduplicated by invocation.
The original timeout includes recovery setup time. A completed child's durable
result is recovered before applying an expired deadline.

Replies must be persisted before execution ownership is marked stopped: graph
nodes can be pruned before the parent writes `ToolResults`. Journal records retain
the original server provenance and remain until session deletion. Cleanup follows
transcript/lease removal and retains a deletion tombstone to reject late dispatch.
Tool servers cache the journal handle and reconcile its replica count against the
execution store periodically, outside the request path.
Journaled recovery does not repeat approval or post-use hooks.
Legacy calls without a journal record retain the hint-based retry/interruption
fallback, with lease revalidation after approval and before dispatch.

User messages submitted while a tool is running are durable immediately, so
their physical log entries may appear between the corresponding `ToolCalls`
and `ToolResults`. Replay preserves the logical model order—tool call, tool
result, then queued user—and orphan detection continues across those interleaved
user entries. When the resumed turn completes, its `TurnEnd.through_seq` covers
every queued user already included in replay; a zero-sequence completion
boundary is invalid. These invariants keep repair idempotent and prevent a
completed session from reconstructing as perpetually busy.

Appending a user row and publishing its worker activation are separate broker
operations. Once the append succeeds, its log sequence is authoritative: an
activation failure must be retried for the pending durable turn and must never
fall back to appending the same frontend text again. Session reconstruction and
the worker lease make repeated activation safe while preserving exactly one
user row. Each targeted recovery attempt has a distinct JetStream message ID,
so an activation already acknowledged by a worker that later stalled cannot
suppress the replacement worker's wakeup during the duplicate window.

## Cleanup

Session logs, leases, canonical metadata, and attachment blobs persist in
JetStream until explicitly deleted. Deletion purges the transcript stream,
lease, every KV key under `sessions/{id}`, and every attachment object owned by
the session. The periodic remote-session cleanup uses the same deletion path.

```bash
harnx delete session <session_id> --agent <agent> --cluster local
```

## Observability

### Logs
Workers emit structured logs for:
- Lease acquisition, renewal, and loss.
- Fenced-write rejections.
- Session activation and failover.

### Metrics
Harnx tracks internal counters (exported to logs and future metrics endpoints):
- `active_sessions_per_worker`: Current active loops.
- `lease_acquisitions` / `lease_losses`: Lease churn.
- `fenced_writes_rejected`: Safety triggers.
- `interrupt_errors_synthesized`: Data points on failover impact.

Cancellation transition and quiescence metrics are described in
[Prometheus metrics](metrics.md#cancellation-progress). Inspect the current KV
operation's owner, admissions, children, and blocker when cancellation remains
unconfirmed; an absent lease or a transcript Cancel alone is insufficient.

### Tracing
Workers participate in OpenTelemetry distributed tracing when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Trace context propagates across process boundaries over NATS message headers for sub-agent activations and tool executions. See [OpenTelemetry Tracing](tracing.md).

## TLS Support Note
Harnx supports TLS and mTLS for NATS connections. While token authentication and config-based TLS have been verified, automated PKI-backed integration tests for live TLS handshakes are ongoing.

Config-based TLS (`tls`/`tls_cert`/`tls_key`/`tls_ca` in `nats_servers/<cluster>.yaml`)
covers the client and worker session connection. It does **not** cover tool/hook
discovery — see
[Independently Deployed Tool and Hook Servers](#independently-deployed-tool-and-hook-servers)
for the separate `HARNX_NATS_TLS*` env vars that connection needs.
