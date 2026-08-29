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

### JetStream Resources
Harnx automatically manages the following JetStream resources:
- **KV Bucket**: `harnx_leases` — session leases with a tombstone marker
  after release (not a bucket-wide TTL). This is the split-brain guard:
  every durable write is fenced on the lease's KV revision, and a worker
  that loses its lease aborts. If this bucket can't survive a node loss,
  neither can a session mid-turn on that node.
- **KV Bucket**: `harnx_sessions` — canonical session state. Each session uses
  `sessions/{id}/meta` for immutable identity plus CAS-updated title, variables,
  overrides, and extensions, and `sessions/{id}/activity` for frequently
  renewed lifecycle timestamps. No expiry. The `sessions/{id}/read/{viewer}`
  prefix is reserved for future unread cursors.
- **KV Bucket**: `harnx_tool_registry` — tool server discovery, with a
  per-registration TTL.
- **KV Buckets**: `harnx_hook_registry` and `harnx_hook_expectations` — hook
  server discovery and its fail-closed fallback routes. Only the copies
  opened by the standalone `harnx-hookset-server` binary carry a TTL; the
  worker daemon's own copy of the same buckets does not set one.
- **Streams**: `SESSION_<id>` (Subject: `sessions.{id}.log`) stores only the
  durable append-only conversation history. Agent identity, settings, rendered
  prompts, and titles do not belong in this stream.
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
directory, installation, or binary changes. Health checks do not proactively
restart a running child for those changes. Crash recovery retains the same
worker ID and consumer route but starts a new PID.

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
- **Resuming Sessions**: Clients attach to an existing `session_id`. Multiple clients can attach to the same session simultaneously (Multiplayer Mode).

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

Clients communicate with the active worker over specific NATS subjects:
- **Cancel**: `sessions.{id}.control` — Deliver a cancel command to the worker.
- **Set Pending**: Update the pending user message without triggering execution.

Semantics are consistent because the authoritative state is derived from the durable JetStream log.

## Multi-Client Support

Multiple clients can attach to a single session:
1.  Clients replay the **durable history** from the JetStream stream.
2.  Clients subscribe to **live advisory events** on `sessions.{id}.events` for real-time updates (streaming chunks, tool progress).

Late-joining clients automatically converge to the same state by replaying the durable log.

A bounded history snapshot can legitimately end at `ToolCalls` while the lease
holder is still executing the tool. Read-only observers sample the session
lease before loading history (and again afterward if it was initially free):
while either sample is active, replay preserves that tail as pending and AG-UI
emits the assistant tool call without a result. Only a lease-free replay turns
a trailing call into an interrupted result. Replay also ignores redundant
`ToolResults` for IDs that already completed, so old recovery artifacts do not
appear as current orphan warnings.

## Failover & Safety

### Leases & Fencing
Harnx uses a renewable CAS (Compare-And-Swap) lease in NATS KV:
- **TTL**: ~30 seconds.
- **Renewal**: Every ~10 seconds.
- **Fence Token**: The KV revision of the lease. Every write to the durable log is gated by this token.

If a worker loses its lease (e.g., network partition), it immediately aborts. This prevents "split-brain" scenarios where two workers think they are active.

### Resume & Idempotency
When a worker resumes an interrupted session:
- **Idempotent Tools**: Tools marked with `idempotent_hint` or `read_only_hint` in MCP are re-run if their result was lost.
- **Non-idempotent Tools**: If a result is missing for a non-idempotent tool, Harnx synthesizes an "interrupt-error" result to prevent accidental double-execution of side effects.

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
user row.

## Cleanup

Session logs, leases, canonical metadata, and attachment blobs persist in
JetStream until explicitly deleted. Deletion purges the transcript stream,
lease, every KV key under `sessions/{id}`, and every attachment object owned by
the session. The periodic remote-session cleanup uses the same deletion path.

```bash
harnx session delete <session_id> --cluster local
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

### Tracing
Workers participate in OpenTelemetry distributed tracing when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Trace context propagates across process boundaries over NATS message headers for sub-agent activations and tool executions. See [OpenTelemetry Tracing](tracing.md).

## TLS Support Note
Harnx supports TLS and mTLS for NATS connections. While token authentication and config-based TLS have been verified, automated PKI-backed integration tests for live TLS handshakes are ongoing.

Config-based TLS (`tls`/`tls_cert`/`tls_key`/`tls_ca` in `nats_servers/<cluster>.yaml`)
covers the client and worker session connection. It does **not** cover tool/hook
discovery — see
[Independently Deployed Tool and Hook Servers](#independently-deployed-tool-and-hook-servers)
for the separate `HARNX_NATS_TLS*` env vars that connection needs.
