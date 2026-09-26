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
below) so the resources harnx creates survive losing a node.

### Session identity

A session is identified by its exact agent name and local session ID within a
cluster. For example, `alpha/review-12345` and `beta/review-12345` have independent
transcripts, metadata, leases, interruption state, and attachments.
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
  local IDs remain unchanged. Honours the cluster's configured `replicas` count
  at creation.
  Earlier stream names are not migrated; start fresh sessions after upgrading.
- **Object Store**: `harnx_attachments` stores binary attachment payloads under
  session-scoped object names. Conversation entries contain only `cid:`
  references; workers hydrate the matching blobs into their local
  content-addressed cache before calling a model.
- **Persistent activation streams**: `WORK_NOTIFY_<cluster>` captures
  `cluster.<cluster>.sessions.notify` with cluster-shared work-queue dispatch.
  All cluster workers bind to one shared durable pull consumer; per-worker
  durables would have overlapping filters and be rejected by the work-queue
  stream. The session lease deduplicates dispatch. Honours the cluster's
  configured `replicas` count.

  Rollout note: clusters that ran an older build have a stale `worker-<id>`
  durable per worker on this stream. Those durables are inert once workers move
  to the shared consumer and self-clear after the 1h inactive threshold. If a
  rollout must not wait, delete the old `worker-*` consumers on
  `WORK_NOTIFY_<cluster>` after the last old worker stops.
- **Local activation stream**: `LOCAL_WORK_NOTIFY_V2` captures
  `session_scope.__local__.workers.*.sessions.notify` with interest retention
  and one exact durable consumer per frontend worker ID. Intentionally R1
  (single-node by design; frontend-local).

The following JetStream resources honour the cluster's configured `replicas`
count (`None` means 1, no HA):

- **Honour `replicas`**:
  - All KV buckets (`harnx_leases`, `harnx_sessions`, `harnx_tool_registry`,
    `harnx_hook_registry`, `harnx_hook_expectations`, and
    `harnx_tool_invocations`)
  - Attachment object store (`harnx_attachments`)
  - Session transcript streams (`SESSION_<sha256(id)>`)
  - Cluster activation stream (`WORK_NOTIFY_<cluster>`)
- **Do not honour `replicas` (by design)**:
  - Local activation stream (`LOCAL_WORK_NOTIFY_V2`) — intentionally R1
    (single-node by design; frontend-local)

Set `replicas` to 3 to match a 3-node cluster; a mismatch between the two leaves
resources unable to tolerate a node loss.

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

Because KV buckets are created at startup at the configured `replicas` count,
a cluster that cannot support the count fails loudly at startup before any
session transcript stream is created. Per-session streams are created lazily on
first write at the same replica count; if the cluster later loses placement
capacity, stream creation surfaces the error rather than silently falling back
to R1.

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
replicas: 3   # JetStream replica count for resources harnx creates; defaults to 1
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
  HARNX_SERVER_SCOPE=shared harnx-time-tools

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
`HARNX_NATS_TLS_CA`, plus `HARNX_NATS_IGNORE_DISCOVERED_SERVERS` when that is
set explicitly) in the worker's own environment. A worker pod must carry
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

This works from the CLI, the TUI, and `harnx-serve` (the HTTP API and Web UI).
Over HTTP the `@` in the agent name is percent-encoded, so `sisyphus@shared`
is addressed as `/v1/agents/sisyphus%40shared`; a package-qualified remote
agent combines both, e.g. `/v1/agents/coding%2Fcoder%40shared`. A remote agent
runs its turns on a worker in the target cluster, and a server addressing only
remote agents never starts a local broker or worker.

### Running a front-end as a cluster client

To connect a front-end (`harnx` CLI/TUI or `harnx-serve`) to an existing NATS
cluster instead of self-hosting a local broker and worker, set `HARNX_NATS_SERVER`:

```bash
export HARNX_NATS_SERVER=remote
```

The front-end reads connection settings from `nats_servers/<name>.yaml` (in this
example, `nats_servers/remote.yaml`) and runs as a pure client:

- It does not start a local `nats-server`, worker, or tool/hook servers.
- No local chat model or `clients/` directory is required. The model requirement
  belongs to the worker that runs the agent loop, not to the front-end. When the
  local catalog is empty, `setup_model` logs an informational message and
  proceeds without selecting a model; remote agents resolve their models on the
  target worker. Local named-agent activation fails with a clear error at the
  worker's model guard (`ensure_named_agent_has_model` in
  `nats_worker/daemon_session_exec.rs`).
- Bare agent names (for example, `assistant`) automatically route to that named
  cluster as if addressed as `assistant@remote`.
- Local (`__local__`) agents are unavailable. Addressing a local agent returns
  an error directing you to use `<agent>@<cluster>` or unset `HARNX_NATS_SERVER`.
- When `HARNX_NATS_SERVER` is unset (the default), the front-end self-hosts a
  local broker and worker, ignoring operator `HARNX_NATS_URL` and `HARNX_NATS_TOKEN`
  for `__local__` routing.

Configuration files in `nats_servers/` support `${VAR}` environment variable
expansion. You can pull transport credentials from the environment while
keeping cluster topology in the file:

`nats_servers/remote.yaml`:
```yaml
url: "${HARNX_NATS_URL}"
token: "${HARNX_NATS_TOKEN}"
```

Note that `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` configure transport credentials
for workers, standalone tool/hook servers, or `${VAR}` expansion in cluster configs.
Setting them on a front-end does not join an external cluster on its own; use
`HARNX_NATS_SERVER` with a corresponding `nats_servers/<name>.yaml` file.

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
    - **Assistant Picker**: Only agents with `role: assistant` (the default) appear in interactive assistant selection menus, including the TUI picker and the Web UI agent list served by `GET /v1/agents?role=assistant`. `subagent` entries are excluded from the picker.
- **Static Config**: This is purely local configuration. Harnx does not perform network calls to discover or list these agents.

## Interruption

Stopping a turn is one durable append. A `Cancel` entry in the session log
terminates the turn in progress at its sequence, and nothing else — no side
bucket, no lease state, no in-memory flag — gets a say. Any process that can
read the log can see that the turn was stopped, which is what makes an
interrupt survive a worker crash, a frontend restart and a broker reconnect
without a coordination protocol between them.

`sessions.{id}.control` still carries an `Interrupt` message, but only as a
latency hint so a live worker reacts in milliseconds instead of on its next
read. A worker that receives one re-reads the log and acts only if a `Cancel`
is really there. Losing that message costs nothing.

### The model

A turn starts with the first user `Message` after the previous terminator and
ends at the next one; a *terminator* is a `TurnEnd`, an `Error` or a `Cancel`.
A `Cancel` at sequence `S` ends the turn running at `S` and is that turn's
terminator — an interrupted turn never gets a `TurnEnd`. User messages queued
before `S` stay in history but never run as a turn of their own; they are input
for whatever the user does next. A tool call is complete once some `ToolResults`
entry carries an output for its call id, real or placeholder, and complete calls
are never cancelled and never replayed.

`harnx_core::session_reconstruct::TurnStatus` is the one classification every
component derives from the log rather than from each other: `Idle`;
`InterruptedPendingWindUp { cancel_seq, cancellation_id, orphans }`, meaning the
last terminator is a `Cancel` and some earlier `ToolCalls` has no results
anywhere in the log; or `InFlightResumable { orphans }`, meaning a user message
follows the last terminator.

A `Cancel` carries a fence token (0 when the writer holds no lease, the normal
case for frontends), an optional `cancellation_id`, and an optional
`requested_by` label. The cancellation id is minted per interrupt request and
is stable only within that request: it doubles as the append's `Nats-Msg-Id`,
so the internal retries of one conflict loop are deduplicated by it. Pressing
Ctrl+C a second time is a new request and mints a new id. Older entries that
have only the fence token still deserialize.

### Fenced appends

Every turn append by a worker or a frontend sets `Nats-Expected-Last-Sequence`
to the tail the writer last observed, plus a stable `Nats-Msg-Id` so a lost
publish acknowledgement is recognised rather than written twice.
(`append_event_async` and the worker's unfenced path remain for lease-less
callers that own no turn and so have no tail to fence against.) `append_fenced`
returns either the sequence it landed at or a `Conflict` carrying every entry
that appeared past the expected tail, so the loser of a race decides by reading
what actually happened instead of retrying blindly.

| Writer | On conflict |
| --- | --- |
| Worker, turn output (assistant `Message`, `ToolCalls`, `ToolResults`, `TurnEnd`, `Error`) | A newer `Cancel` ended the turn: abandon the entry with a `TurnInterrupted` error and wind up. Everything else is legitimate tail movement — queued user input, a HITL decision from the control listener, a sub-agent or handoff marker written through another handle on the same lease — so re-read, advance the expected tail and retry, up to 8 rounds. |
| Worker, wind-up `ToolResults` | Keep going even past a second `Cancel`: the log still owes those calls a result. A conflicting `ToolResults` that already answers any of the same calls IS this wind-up, whoever wrote it, so its sequence is adopted rather than a second answer appended. Byte equality would not do: two workers closing the same round out stamp their own timestamps, and a reply that reached the journal in between turns a placeholder into a real result. The publish also carries a message id derived from the session and the `Cancel`'s sequence, so a replacement worker racing the original is deduplicated by the broker before the tail check runs at all. |
| Frontend, user `Message` | Advance the tail and retry. A `Cancel` in between just makes the message part of the next turn. |
| Frontend, cascade, or worker aborting locally, `Cancel` | A terminator already in the tail settles it. If that terminator is a `Cancel` carrying our own `cancellation_id`, our acknowledgement was lost and the append counts as accepted. Otherwise retry at the new tail. |

Worker appends additionally require a held lease and carry its fence token. The
lease keeps two workers from owning one session; the compare-and-swap closes the
check-then-write race against a `Cancel` that lands mid-append.

### Requesting an interrupt

`nats_session::interrupt_session` is the only way in, used by the TUI, the web
UI through serve, the one-shot CLI and the sub-agent tool. It reads the log's
last entry, decides, and returns an `InterruptOutcome`: `Idle` when no user
message follows the last terminator, so there is no turn to stop and nothing is
appended; `Accepted { cancel_seq }` when the `Cancel` landed; or
`AlreadyInterrupted { cancel_seq }` when one already terminates this turn.
Calling again with the same `cancellation_id` is harmless. The last entry is
all it reads: a `TurnEnd` or `Error` means idle, a `Cancel` means already
interrupted, and anything else is treated as a turn in progress and fenced on
that sequence, so a completion that lands in between rejects the append and is
re-read from the conflict. An idle session whose last entry is a mutation or
control entry therefore gets a stray `Cancel`, which the protocol tolerates;
that is the price of keeping the transcript's length off the interrupt's path.

### Failover vs user cancellation

Worker session execution distinguishes **failover** from **user cancellation**
because confusing them causes work loss:

- **User cancellation** (`AbortSignal` with `aborted_ctrlc()` /
  `aborted_ctrld()`): writes a durable `Cancel` to the session log and
  requires a held lease so the `Cancel` is fenced. Turn disposition is terminal
  ACK — the session is done and no replacement worker will resume it.
- **Failover** (`AbortSignal` with `aborted_failover()`, or lease lost):
  local execution aborts **without** writing `Cancel`. Turn disposition is
  non-terminal NAK (`Nak(None)`) so a replacement worker resumes from the
  journal. Never send terminal ACK once failover is latched.

The `FinishCause` enum (`execution_control.rs`) encodes this distinction:
`UserCancelled` vs `Failover(Shutdown|LeaseLost)`. Only `UserCancelled` and
`Completed{settled:true, has_queued_input:false}` produce terminal ACK.

**Remote tool calls must not be cancelled on failover.** When a worker receives
a failover abort signal, `NatsToolProvider::invoke_tool`
(`nats_tool_provider.rs:276-280`) checks `!abort.aborted_failover()` before
publishing a remote cancellation. Abandoning only the local wait lets the
replacement worker recover the tool result from the invocation journal. A
remote cancel would journal an interruption the replacement mistakes for the
tool's outcome.

After an accepted append it fires two best-effort wake-ups and waits on neither:
the control hint above, and a **wind-up activation** targeting
`requested_seq = cancel_seq`. A live worker settles that activation as already
running; if no worker holds the lease, one activates, finds
`InterruptedPendingWindUp` and closes the turn out. That is how an interrupt
crosses a dead worker without anyone walking a tree of operations — each
session's own worker cancels its own calls, and the sub-agent tool's cancel
interrupts the child session the same way one level down.

### Latency contract

Interruption is complete for the user the moment the `Cancel` append is
acknowledged. Nothing waits for delivery, wind-up, child sessions or tool
processes to die.

- The editor is available again as soon as the outcome returns. A steering
  message typed immediately lands after the `Cancel` and starts the next turn.
  Exit-after-interrupt proceeds on the same acknowledgement.
- The append is bounded at two seconds. A timeout or failure is reported as a
  failed interrupt the user can retry, never silently swallowed — the log is the
  authority, so an append that did land is found by the next history read even
  if its acknowledgement was lost.
  The TUI runs the whole request on its own task and only checks the join
  handle from its render loop, so the budget is spent on the broker rather than
  on 80 ms render ticks.
- To see where a slow interrupt's time went: the cancellation id is a UUIDv7,
  so its timestamp is when the request was minted. Compare it with the `Cancel`
  entry's stream time (`nats stream get SESSION_<sha256 of the storage key>
  <seq> -j`) and the frontend's log lines, which carry its pid: `abort signal
  received`, `interrupt appended`, and `attached to session` for the attach
  that precedes the TUI's request.
- If the owning worker is dead, the wind-up waits for its lease to expire (30 s
  by default) and then runs on the next activation, which is NAKed with delay
  until the lease is free. There is no lease-revocation shortcut, and the user
  is not kept waiting for it either way.
- In local mode the worker and its tool servers die with the frontend
  (parent-death signal on Linux; macOS and Windows still leak processes). In HA
  mode workers are daemons and see the `Cancel` live through their watchers.

### Restart ordering and attach

Interrupting a root session and exiting leaves child sessions whose own logs
still look resumable, and their activations may be redelivered before the root's
wind-up has cancelled them. Two guards make the order irrelevant.

**Ancestor check.** A sub-agent session records `parent: { session_id,
tool_call_id }` in its metadata at creation. Before rolling an
`InFlightResumable` turn forward, the worker reads the parent's log
(`nats_worker/ancestor_check.rs`): if the parent's `ToolCalls` for that call id
is followed by a `Cancel`, or the call already has a result, nothing is waiting
for the child's answer, so it does not resume — it interrupts itself with the
parent's cancellation id and winds up instead. The walk follows parent links
upward, at most 32 levels, and fails closed: a parent log it cannot read is
never taken for "the parent is still waiting". As implemented, that failure
ends the child's turn with an `Error` entry rather than being retried — the
activation is not NAKed for another attempt, so a transient read failure
costs the child its turn.

**Idempotent wind-up cancels.** When the root is activated, its wind-up resends
a cancel for every call with no journal reply, catching any child that slipped
through. The window is one activation wide.

Local worker ids change across restarts, so an activation addressed to a dead
local worker is terminated as misrouted. Frontends therefore re-derive
`TurnStatus` when they open or attach to a session and republish the pending
activation to their own worker (`NatsSession::republish_pending_activation`):
wind-up for `InterruptedPendingWindUp`, resume for `InFlightResumable`. In HA
mode the durable work stream redelivers unacknowledged activations by itself.

### What the worker does

**Session watcher** (`nats_worker/session_watcher.rs`). Each activation runs an
ordered push consumer on the session's own stream, starting just after the tail
observed at activation and independent of whatever the turn is awaiting.
Sequences this worker wrote itself are skipped. A user `Message` only sets a
pending-input flag, because the turn loop reloads the tail at each tool round
and again at its drain check; queued input is folded in there without the
watcher having to interrupt anything. A `Cancel` snapshots the calls in flight,
hands the cancel publishes to a detached task so they survive the watcher being
torn down, then fires the abort signal. Its cursor advances per message, so a
broker hiccup resumes just past the last one delivered rather than replaying
and re-cancelling.

**Wind-up** (`nats_worker/wind_up.rs`). A `Cancel` ends the turn but leaves the
log owing a result for every call the turn had already made, and a transcript
whose last `ToolCalls` is unanswered cannot be replayed to a model. The lease
holder looks each interrupted call up in the invocation journal by session, tool
round and tool-call id — the row is stored under the wire call id, and the
transcript's tool-call id is matched against what the row records — then appends
exactly one `ToolResults` entry: a call that finished while the interrupt was
travelling wrote its result to the journal rather than the log, so that real
output is recovered and used; every other call gets the placeholder
`{ "error": "tool call interrupted by user", "cancellation_id": … }`. It emits
`TurnEvent::Interrupted`, which frontends treat like `Ended` for busy state, and
the lease is released once it is done. No `TurnEnd`, no model call.

Cancels are resent on the way through. While a wind-up is still owed, every
attempt sends an idempotent cancel for each interrupted call the journal has no
reply for — the previous worker may have died between cancelling some calls and
writing their placeholders, and a remote tool may still be running. Once the
`ToolResults` entry is durable, nothing is resent again. A tool that finishes
after that point still records its reply in the journal, where the first reply
written wins, but no one reads it: the transcript already answers that call, and
the turn it belonged to is over.

A journal read that fails abandons the whole wind-up rather than writing a
placeholder over a result it simply could not see. Nothing is appended, so the
turn stays `InterruptedPendingWindUp` and the next activation does the wind-up
over again. That is safe because the reverse case is also handled: once a
wind-up has appended its `ToolResults`, the orphans are answered, so a second
pass reconstructs `Idle` and writes nothing.

The model is told why the round ended. A `Cancel` renders into model context at
its log position as `[Runtime note] The user interrupted this turn. Incomplete
tool calls above were cancelled.`, the same way a sub-agent start does. With the
per-call placeholders beside it, that is enough for the model to make sense of a
tool round that came back with no real answers.

**Activation.** `activation_preflight.rs` winds an interrupted turn up before
anything else may run for that session. A pure wind-up acknowledges the
activation and stops there; a wind-up with a steering message queued behind the
`Cancel` continues into the turn loop, which now sees an idle session with
pending input. An activation whose `requested_seq` names a `Cancel` already
wound up is acknowledged as covered.

### Cancelling tools, hooks and sub-agents

Cancellation travels one level at a time through the tool protocol, never by
inspecting a tree. `ControlMessage::cancel(server, session_id, call_id,
cancellation_id)` goes to the server's control subject
(`harnx.v1.{instance}.tools.control`, or
`harnx.v1.{instance}.hook.{server}.control` for hooks) and is answered with a
`CancelAcceptance`: `Accepted`, `AlreadyFinished`, `Rejected { reason }`, or
`Unknown { reason }` meaning the cancel may have committed and should be retried
with the same id. Acknowledgements are for logging; no caller blocks on one.

Each tool decides what cancel means — bash kills a process group, MCP sends
`notifications/cancelled`, the sub-agent tool interrupts its child session, a
third-party tool stops whatever remote work it started. A tool that begins
durable work first records a **checkpoint**, its own resume handle, in the
invocation journal under `(session_id, call_id)`, and the framework hands that
checkpoint back on every later cancel or replay so the tool can act after any
process restart.

A tool server that receives a cancel for a call it is not running does not
shrug. It reads the journal row: a recorded reply answers `AlreadyFinished`, no
row at all answers `Rejected { "unknown call" }`, and otherwise it builds an
invocation from the stored checkpoint and calls the toolset's own `cancel`
(`harnx-toolset-server/src/control.rs`). That **orphan cancel** path is what
stops a call after the process that started it is gone. Cancellation is
idempotent throughout: a call may be cancelled any number of times, with the
same or different cancellation ids, and each answer is the same.

Hooks use the same mechanism. The hook server keeps no journal, so a call id it
is not currently running is simply `AlreadyFinished` — nothing durable is left to
consult once the future is gone. The worker-side hook provider registers
controlled hook calls in the same in-flight registry as tools, so one interrupt
path reaches both.

Sub-agents are an ordinary tool whose checkpoint holds the child session id.
Both the live cancel and the orphan `cancel` read that checkpoint and call
`interrupt_session` on the child with `requested_by = "parent:<session_id>"`.
Each of those is a fresh interrupt request with a cancellation id of its own —
the cascade is not one id travelling down a chain, and nothing correlates the
levels by id. The one place a parent's id is reused is the ancestor check: a
child that finds its parent's invocation interrupted appends its own `Cancel`
carrying the cancellation id it read off the parent's log. The child's worker —
live, or revived by the wind-up activation — then cancels its own calls in
turn, and that recursion is the entire cascade. `SubAgentStarted` in the parent
log stays a UI and model-context note; control never reads it.

Tool calls made with no parent session (the direct stdio client) get no
checkpoint, no control subject and no orphan-cancel path. Nothing owns a session
log for them, so there is no interrupt to carry.

### Live events

Advisory events are fenced by sequence, not by identity. `LiveEventState`
records the `Cancel`'s sequence through `accept_interrupt`, and `should_render`
drops any envelope whose `after_seq` is below it. A worker that has not yet seen
the `Cancel` cannot have appended anything at or after it, so that comparison
removes exactly its stale in-flight chatter.

### Upgrading from the execution-control gate

The internal tool protocol is **v5**, and the two server kinds fail differently
on a mismatch. A tool server that registers under another version fails
discovery loudly with "incompatible tool protocol … upgrade workers and tool
servers together", so none of its tools load at all. A hook server still
registers under its own unchanged protocol version and keeps working, but the
cancel control messages are v5, so a mismatched one answers `Rejected` and
quietly becomes uninterruptible. Deploy workers, tool servers, hook servers and
frontends together.

The `harnx_execution_control` KV bucket is never created or read again.
Operators delete it once:

```bash
nats stream rm KV_harnx_execution_control
```

Nothing is migrated. Existing session logs stay readable: old `Cancel` entries
carrying only a fence token deserialize fine, and old orphan `ToolCalls` are
still repaired on load with lost-response results. Session metadata written
while worker commits went through the old ledger carries a `worker_projection`
key; it is accepted on read so those records still load, nothing consumes it,
and it is never written again.

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
  `SessionEventStream` advisories from the snapshot's last `User` row onward. It
  translates eligible events to AG-UI frames and ends the stream on the
  terminator that covers that row — a `TurnEnd` or a `Cancel`.

Worker death and task failure produce `RUN_ERROR` on both paths. The local-actor
path wraps its turn task in panic supervision (`session_actor.rs:281`); the
remote-follow path confirms lease absence before reporting loss
(`ag_ui_remote_follow.rs:557`). Actor-owned turns additionally enforce a 60s
acquisition deadline before reporting an unclaimed run as failed
(`nats_session.rs:89`).

The session metadata watch endpoint (`GET .../events`, `session_updates` in the
serve implementation) is separate from the AG-UI `/run` stream and provides
lightweight `session-updated` notifications that trigger client rehydration.
Two distinct endpoints keep the AG-UI run stream finite and properly sequenced
(RUN_STARTED → … → RUN_FINISHED), while the watch channel converges late
observers on the same durable state.

Tool confirmations are **point-to-point** over NATS: the worker sends the
confirmation request to the `ToolConfirmationRoute` subject carried on the
activation (stored as `tool_confirmation_subject`). Only the owning frontend
receives the prompt. If the frontend includes a message with its decision, it
commits that user row to the session stream before replying; the worker then
places it after the real or blocked tool result. Other observers see the
durable log updates after the decision.

### Trailing Tool Calls: Pending vs. Interrupted

A bounded history snapshot can legitimately end at `ToolCalls` while the lease
holder is still executing the tool. Read-only observers sample the session lease
before loading history (and again afterward if it was initially free): while
either sample is active, replay preserves that tail as pending and AG-UI emits
the assistant tool call without a result. Only a lease-free replay turns a
trailing call into an interrupted result. Replay also ignores redundant
`ToolResults` for IDs that already completed, so old recovery artifacts do not
appear as current orphan warnings.

An interrupted turn does not stay in that state for long. The worker's wind-up
appends one `ToolResults` entry answering every call the `Cancel` cut off, using
the tool's real reply where the invocation journal has one and the
`tool call interrupted by user` placeholder otherwise, so a replayed transcript
ends with a complete tool round rather than a guess about a dangling call.

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

- **SSE**: The `/v1/agents/{agent}/sessions/{session}/events` endpoint emits `event: read-updated` for read-state changes, bypassing the `after_seq` filter. The active session's `RuntimeSessionSubscriber` calls its `onReadUpdated` callback on receipt.
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
| Append a transcript entry | Retain the same message ID, payload, and CAS expectation through `recovery::retry_until`. Retry timeout/broken-pipe errors; a CAS conflict is authoritative. |
| Mutate session metadata | `cas::update` reads back an ambiguous acknowledgement. Matching persisted bytes confirm the write; an unchanged revision permits retrying the exact CAS. A different value or revision leaves the outcome unknown. |
| Renew a session lease | Retry the same deduplicated CAS only until the previously confirmed lease deadline. A conflict or expired deadline fences the worker; reconnect never restores lost ownership. |
| Invoke a tool or hook | `rpc::request`, `RequestActivity`, and `ReplyTarget` confirm activity and acknowledge replies. Resend a completed result until receipt, never rerun the handler. A NATS 503 is not a reply receipt. |
| Renew discovery registration | `registry::refreshes` retains a separate pending renewal future so broker latency cannot block serving requests, controls, or shutdown. |
| Observe turn completion | Incremental transcript polling operates independently of live advisories. Durable coverage determines which prompt completed; a prior turn's advisory cannot complete an injected prompt. |
| Consume worker activations | Reopen the durable pull stream after errors/closure; acknowledge according to the activation's durable admission and lease state. |

General recovery operations have a 15-second budget. RPC handler heartbeats renew
the activity deadline, so a healthy long-running handler has no imposed total
duration beyond its configured timeout. Results retain a bounded receipt window.
Loss of activity or exhausted recovery reports that the call's outcome is
**not known**; it does not prove that a side effect was rolled back or that a
descendant stopped. Interruption never depends on that answer: the session log
alone decides whether a turn ended.

Deploy the frontend, worker, and tool/hook binaries together for this contract.
Older clients still receive ordinary replies, but an older server cannot provide
the activity receipts expected by a new long-running RPC caller. Restart all
local frontends for the persistent endpoint policy: an old broker owner still
deletes its discovery metadata at shutdown. No transcript variant is added.

Regression coverage lives in `nats_local_server/failover.rs`,
`local_worker_supervisor.rs`, `harnx-runtime/tests/interruption_fencing.rs`, and
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
reads report that completion is not known. Similarly, an unbounded tool request
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

## Manual Compaction

Manual compaction summarizes older conversation history into a single summary entry while preserving recent turns and active context.

In NATS mode, compaction execution routes to the worker holding the session lease:
1. The client appends a durable `CompactRequest` entry to the session log stream (or submits `session/compact` via JSON-RPC). Frontends check the tail first: if already in flight or recently compacted, skip and report status.
2. The leased worker detects the request, executes the configured `compaction_agent` (or the default summarizer), appends a durable `CompactResult`, and publishes progress advisories.
3. Manual and automatic compaction serialize via the `compressing` flag. If already compacting, the worker skips redundant execution and reports `AlreadyCompacted`.
4. If the transcript has too few uncompacted messages or tokens to warrant summarization, the worker yields `Unchanged` (`Nothing to compact`). This is treated across all interfaces as a neutral success (exit 0 / status message), not an error.

Log layout: `CompactRequest` → `Compress` marker → re-logged suffix messages → `CompactResult`. The `Compress` marker is not the tail entry; suffix messages follow it so replay can reconstruct the transcript without stored indices. The `CompactResult` receipt is the deterministic final entry for manual compaction.

### Triggering Manual Compaction

- **CLI**:
  ```bash
  harnx compact session <agent> <session> [--timeout <seconds>]
  ```
  Submits the request, streams progress advisories to stdout, and blocks until finished (default 60s timeout). Exits 0 on success or when there is nothing to compact.
- **TUI**:
  ```
  .compact session
  ```
  Triggers compaction for the active session. The TUI displays progress notifications and updates the transcript once the worker commits the result.
- **Web UI**:
  Select **Compact session** from the session dropdown menu next to the session ID in the header. Shows a spinner while compaction runs, followed by a status message if there was nothing to compact.

## Cleanup

Session logs, leases, canonical metadata, and attachment blobs persist in
JetStream until explicitly deleted or collected. Manual deletion purges the
transcript stream, lease, every KV key under `sessions/{id}` (including read
and unread state), tool-invocation journal entries, and attachment objects owned
by the session:

```bash
harnx delete session <session_id> --agent <agent> --cluster local
```

There is no separate control-plane state to reap: interruption leaves nothing
behind but log entries, which the same deletion removes.

### Automatic Session Garbage Collection

In multi-node deployments, session garbage collection runs from the
`harnx-worker` daemon rather than the CLI or `harnx-serve`. Every running worker
participates in hourly leader election using the `session_metadata_gc` KV lease
on `harnx_leases`. The winning worker checks a durable epoch-hour marker key
(`session_metadata_gc/last_run_epoch_hour`) before scanning, guaranteeing that
only one worker per cluster performs a cleanup pass per wall-clock hour even
when worker startup times are staggered.

Each worker only collects expired sessions in its own cluster
(`daemon.connection_key()`). If a cluster runs `harnx-serve` or other frontends
with zero workers deployed, no automatic garbage collection runs for that
cluster until at least one worker joins.

Automatic collection uses the exact same deletion path as manual cleanup. In
the normal case it removes the transcript stream, lease, all `sessions/{id}`
metadata keys (including read and unread tracking), tool-invocation journal
entries, and attachment objects. Workers also verify that candidate sessions
are inactive before deletion, skipping any session with an active or
reactivated lease.

Retention is controlled by `cleanup_remote_sessions_days` in `config.yaml` or
the `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS` environment variable. Configure the
same value on every worker in a cluster because the worker elected for an hour
applies its own retention setting:

- **Unset (default)**: Automatic expiry is disabled. Session data grows
  unbounded until removed manually. When unset, workers log a `WARN` at startup
  advising that GC is disabled and explaining how to configure retention.
- **`0`**: Explicitly disabled. Workers log an `INFO` notice at startup and
  skip GC passes without acquiring the election lease or setting marker state.
- **`n > 0`**: Enabled with an `n`-day retention period.

The expiration threshold is measured from the session's last activity timestamp,
falling back to its creation timestamp when no activity record exists.

### Stream Retention Decision (`max_age`)

Harnx deliberately avoids configuring a JetStream `max_age` backstop on session
transcript streams. Message age does not equal session inactivity: an automatic
stream-level cutoff would ignore active leases, truncate resumable conversation
history on long-lived sessions, and leave orphaned records behind in the
metadata KV store, invocation journal, and attachment buckets. Session-aware
garbage collection in the worker daemon is the sole authoritative mechanism for
expiring inactive sessions cleanly across all storage layers.

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

See [Prometheus metrics](metrics.md) for the exported metric families. To look
into an interruption, read the session log: the `Cancel` entry and the
`ToolResults` that answers its orphaned calls are the whole record of what
happened and when.

### Tracing
Workers participate in OpenTelemetry distributed tracing when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Trace context propagates across process boundaries over NATS message headers for sub-agent activations and tool executions. See [OpenTelemetry Tracing](tracing.md).

## TLS Support Note
Harnx supports TLS and mTLS for NATS connections, over both the TCP protocol
(`nats://`, `tls://`) and WebSocket (`ws://`, `wss://`). Token authentication,
config-based TLS and the WebSocket transport are verified; automated PKI-backed
integration tests for live TLS handshakes are ongoing, so the mTLS handshake
itself is exercised by configuration rather than end to end.

Harnx builds the rustls config for these connections itself. Leaving it to
async-nats resolves rustls' process-default crypto provider, which this
workspace makes ambiguous by linking both `ring` and `aws-lc-rs` — that
resolution panics. One case is deliberately not covered: a `nats://` URL with
no TLS settings, pointed at a server that demands TLS in its INFO. Say
`tls: true` (or use `tls://`) for such a cluster.

Config-based TLS (`tls`/`tls_cert`/`tls_key`/`tls_ca` in `nats_servers/<cluster>.yaml`)
covers the client and worker session connection. It does **not** cover tool/hook
discovery — see
[Independently Deployed Tool and Hook Servers](#independently-deployed-tool-and-hook-servers)
for the separate `HARNX_NATS_TLS*` env vars that connection needs.
