# Kubernetes Sandbox Tool Gateway

`harnx-k8s-sandbox-tools` lets centrally operated Harnx workers run filesystem
and shell tools inside per-workload Kubernetes Agent Sandboxes. It is designed
for the common topology where several agents may work in one sandbox, while a
given session normally stays attached to one sandbox.

## Architecture

```text
Harnx worker -- native tool request --> harnx-k8s-sandbox-tools
                                           |
                                           | Kubernetes API: create/wake/hibernate
                                           | HTTP MCP: bash_* / fs_*
                                           v
                                    Agent Sandbox pod
                                    +------------------+
                                    | agentgateway:8080|
                                    |  |- bash stdio MCP
                                    |  `- fs stdio MCP
                                    +------------------+
```

Only the central gateway connects to the Harnx NATS cluster. Sandboxes have no
NATS credentials and no Harnx worker or model client. This keeps scheduling,
model configuration, logs, metrics, and session ownership centralized while
putting only the stateful execution boundary in the sandbox.

The gateway is an independently deployed native tool server. Give it and every
worker the same `HARNX_SERVER_SCOPE`, NATS connection variables, and JetStream
replica count as described in [NATS HA deployment](nats-ha.md#independently-deployed-tool-and-hook-servers).
The process publishes three registrations (`bash`, `fs`, and `sandbox`) and is
ready only after all three are visible.

Leave worker-managed tool servers disabled for this deployment. In particular,
do not also launch a local `bash` or `fs` registration in the shared scope:
duplicate agent-visible names make routing ambiguous and would reintroduce host
filesystem access on the coordinator worker.

## Tools and routing

The gateway preserves the standard Harnx tool names and schemas:

- `bash_exec`, `bash_spawn`, `bash_wait`, `bash_terminate`,
  `bash_read_exec_log`, and `bash_rollback_file`
- `fs_read`, `fs_write`, `fs_edit`, `fs_insert`, `fs_re_replace`, `fs_ls`,
  `fs_grep`, `fs_find`, and `fs_rollback_file`
- `sandbox_connect`, `sandbox_status`, and `sandbox_release`

Every proxied bash/fs schema has an optional `sandbox_id`. Resolution order is:

1. a non-empty explicit `sandbox_id` on that call;
2. the private sandbox binding on the invoking Harnx session; or
3. a recoverable error telling the agent to call `sandbox_connect`.

An explicit override affects one call and does not replace the ambient binding.
`sandbox_connect` creates or attaches, waits until the sandbox is usable, and
then binds it to the invoking session. The binding lives in the private,
versioned `dev.harnx.tool_context` session-metadata extension. It is excluded
from redacted HTTP metadata and model-visible tool arguments.

New sub-agent sessions receive a snapshot of their parent's tool context, so a
delegated implementation or review agent starts in the same sandbox without
having to repeat the ID. Existing child sessions keep their own snapshot if a
parent later connects elsewhere. Several independent sessions can deliberately
bind to the same sandbox.

### Migrating from Tartarus

The gateway preserves Tartarus's sandbox lifecycle and bash/filesystem tool
capabilities, but it is not a drop-in MCP endpoint. Update agent definitions
and prompts for these intentional native-Harnx contract changes:

- `get_sandbox_status` is named `sandbox_status`.
- The optional status wait argument is `timeout_secs` instead of `timeout`.
- `sandbox_status` is observational and may report `hibernated`; unlike
  Tartarus status lookup, it does not wake the sandbox, extend its TTL, or
  update its activity timestamp.
- `sandbox_id` is optional after `sandbox_connect` binds the invoking Harnx
  session, including for newly created sub-agents. Explicit IDs still provide
  a one-call override.
- `sandbox_connect` gets the invoking session from attested native-tool
  context. The Tartarus `session_id` and `app_name` arguments and their
  `kagent/*` claim annotations are not used.

Tartarus's Prow, media publishing, report, plan, and session-storage tools are
outside this gateway's scope and need separate Harnx services or toolsets.

Tartarus and this gateway can use the same existing sandbox images and claims
during a migration when they target the same namespace and template. Both use
the Agent Sandbox CRDs, the `kagent/last-activity` annotation, and streamable
HTTP MCP at port `8080`, path `/mcp`. Claim naming does not collide: Tartarus
uses Kubernetes-generated `sandbox-*` names, while this gateway derives a name
from the Harnx tool-call ID.

Do not run both idle watchers against the same namespace. Each watcher scans
every claim rather than only claims created by its own gateway, so both may
patch the same sandbox and the shorter configured idle timeout effectively
wins. Prefer one active watcher during a rolling migration; use separate
namespaces if both lifecycle owners must remain active. Likewise, do not issue
concurrent release or destroy operations for the same claim.

Image compatibility depends on the in-sandbox MCP contract, not which gateway
created the claim. The image must expose the expected `bash_*` and `fs_*` names
and accept the schemas advertised by the gateway. Tartarus loads a schema
snapshot from deployment configuration, while this gateway compiles schemas
from its Harnx bash/fs crates, so upgrade the gateway and sandbox image together
when those tool schemas change. Harnx session bindings are private NATS metadata
and are not visible to Tartarus; calls through Tartarus still need its explicit
or kagent-derived sandbox context.

## Sandbox MCP endpoint

Each sandbox must expose streamable HTTP MCP at port `8080`, path `/mcp`.
[agentgateway](https://github.com/agentgateway/agentgateway) can host the two
Harnx stdio MCP servers with this configuration:

```yaml
binds:
  - port: 8080
    listeners:
      - routes:
          - backends:
              - mcp:
                  statefulMode: stateful
                  targets:
                    - name: bash
                      stdio:
                        cmd: harnx-bash-tools
                        args:
                          - --mcp-stdio
                          - --allow-rwx
                          - /workspace
                          - --no-sandbox
                    - name: fs
                      stdio:
                        cmd: harnx-fs-tools
                        args:
                          - --mcp-stdio
                          - --allow-rwx
                          - /workspace
```

Run the sandbox container with `/workspace` as its working directory. Disabling
the bash process sandbox here is intentional only when the Kubernetes pod is
itself the security boundary. The filesystem server remains limited to
`/workspace`. Agentgateway prefixes target names, producing the `bash_exec` and
`fs_read` names the central gateway calls.

The gateway forwards Harnx execution-context capability metadata through MCP,
so repository/branch observations made inside the sandbox still update the
central session picker. W3C trace context is forwarded as well. Cancellation is
sent to the in-sandbox MCP request without closing unrelated concurrent calls.
The gateway retains one stateful MCP session per sandbox so process handles from
`bash_spawn` remain valid for later `bash_wait`, log, and terminate calls. The
session is replaced when the pod IP changes and closed on `sandbox_release`.

Run one gateway process per Harnx server scope while the sandbox uses stdio MCP
targets. Multiple processes would establish independent agentgateway sessions
and therefore independent bash process registries. This does not limit the
number of Harnx workers, agents, or sessions that can share the gateway. A
future highly available deployment should use an in-sandbox network MCP server
whose process state is shared independently of the frontend connection, or add
sandbox-affine routing to the native tool protocol.

## Lifecycle behavior

`sandbox_connect` without an ID creates a `SandboxClaim` from the configured
template. The claim name is derived from the Harnx tool-call ID, making a
redelivered creation request converge on the same claim. Optional `repos` are
cloned through `bash_exec` after the sandbox is ready; each result reports its
path, checked-out branch, or an error. Clone destinations must be below
`/workspace`, and authentication/network-shaped failures are retried up to
three times.

Before every proxied bash/fs call, the gateway:

1. loads the claim and best-effort extends a nearly expired shutdown time;
2. scales a hibernated backing `Sandbox` from zero to one replica;
3. waits for the claim's `Ready=True` condition and a pod IP; and
4. updates the last-activity annotation.

`sandbox_status` is observational: it neither wakes the sandbox, extends its
TTL, nor updates activity. With `timeout_secs`, it polls until ready, deleted,
hibernated, a recognized terminal error, or timeout.

`sandbox_release` defaults to hibernating (`replicas: 0`) while retaining
storage and TTL. `destroy: true` deletes the claim and its storage according to
the claim's `Delete` shutdown policy, and clears a matching ambient binding.

An in-process watcher scans claims every 15 minutes by default and hibernates
running sandboxes whose last activity is older than 15 minutes. A two-minute
creation grace period prevents it from racing new claims. Hibernation also
closes the gateway's pooled MCP session for that sandbox.

## Gateway configuration

All flags have environment-variable equivalents:

| Flag | Environment | Default |
| --- | --- | --- |
| `--sandbox-namespace` | `SANDBOX_NAMESPACE` | `agent-sandboxes` |
| `--sandbox-template` | `SANDBOX_TEMPLATE` | `formative-buildbox` |
| `--default-ttl-minutes` | `DEFAULT_TTL_MINUTES` | `4320` (72 hours) |
| `--sandbox-scan-interval-minutes` | `SANDBOX_SCAN_INTERVAL_MINUTES` | `15` |
| `--idle-timeout-minutes` | `IDLE_TIMEOUT_MINUTES` | `15` |
| `--auto-extend-threshold-hours` | `AUTO_EXTEND_THRESHOLD_HOURS` | `48` |
| `--auto-extend-ttl-hours` | `AUTO_EXTEND_TTL_HOURS` | `72` |

The binary also accepts the shared `--metrics-addr` / `HARNX_METRICS_ADDR` and
`--healthz-addr` / `HARNX_HEALTHZ_ADDR` options. In addition to the standard
tool metrics it emits `harnx_sandbox_wakes_total` and
`harnx_sandbox_hibernations_total{reason="release|idle"}`.

## Kubernetes permissions

Use a namespaced service account. The gateway does not use pod exec and needs
no pod permissions:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: harnx-k8s-sandbox-tools
  namespace: agent-sandboxes
rules:
  - apiGroups: ["extensions.agents.x-k8s.io"]
    resources: ["sandboxclaims"]
    verbs: ["create", "get", "list", "patch", "delete"]
  - apiGroups: ["agents.x-k8s.io"]
    resources: ["sandboxes"]
    verbs: ["get", "patch"]
```

If the Deployment and sandboxes use different namespaces, bind this Role to
the gateway service account with a `RoleBinding` in `agent-sandboxes`.

## Network and credential boundary

NetworkPolicy should allow gateway egress to the Kubernetes API, central NATS,
telemetry endpoints, and sandbox pods on TCP 8080. Sandbox ingress on TCP 8080
should accept only the gateway pod selector/namespace. Sandbox egress may allow
source control, package registries, and approved proxies, but does not need
central NATS. Do not mount NATS tokens, model-provider credentials, or the
gateway service-account token into sandbox pods.

The sandbox ID is a routing hint, not an authorization credential. NATS account
permissions, gateway Kubernetes RBAC, namespace isolation, and NetworkPolicy
remain the security boundary.

## Failure and retry semantics

The gateway retries MCP connection establishment after waking a sandbox, but
does not automatically replay a tool call after it may have been submitted.
Handler and Kubernetes errors are recoverable tool results so the agent can
inspect or retry them. A failed stateful MCP connection is discarded before the
error is returned, so a later agent-directed retry establishes a clean session.
Cancellation remains fatal to the current tool invocation.
