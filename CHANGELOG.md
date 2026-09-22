# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- add GitHub auth proxy hook (`harnx-proxy-auth`): persistent hook binary that acts as an HTTPS MITM proxy, injecting configurable auth headers for matching URLs into `bash_exec`/`bash_spawn` tool environments (closes #531)

## 0.34.0 (2026-09-22)

### Breaking Changes

- HARNX_INSTANCE_ID is now HARNX_SERVER_SCOPE. It is set
automatically in normal use; set it explicitly only when deploying tool
or hook servers independently of a worker.

* feat(worker): add --manage-servers instead of inferring topology from the cluster key

Three gates decided whether the worker launches its own tool and hook
servers by comparing the cluster key to __local__, so pointing a worker at
any other cluster silently left it with no tools and no hooks. Make it an
explicit flag and let a worker discover independently deployed servers
under a configured scope.

Also fixes two silent-degradation gaps from the previous task's review:
a failed NATS tool-registration discovery now logs a warning naming the
scope instead of vanishing, and two comments describing a worker-owns-this
framing the project discarded (a scope can belong to an independently
deployed set with no worker at all) are reworded.

* fix(worker): skip local NATS resolution when there is nothing to spawn

start_local_tool_servers had no servers.is_empty() short-circuit, unlike
start_global_hooks's hooks.entries.is_empty() check, so a manage_servers
worker with nothing configured still resolved a local NATS server (and,
with no broker address handed down, spawned a real shared nats-server
child) on every call. agent_hook_start_config had the same gap at the
per-activation level: it ran the identical resolution once per turn
regardless of whether the active agent had any hooks to launch.

Add the missing empty checks, mirroring the hooks path. This is what was
driving the stress-run flakiness in unrelated tests that switched to
- Replace filesystem roots and per-tool extra path flags with shared explicit allow paths and opt-in batches. Existing tool-server YAML and sandbox-run invocations must migrate to the new flags and environment variables.
- **Breaking:** `HARNX_INSTANCE_ID` is renamed to `HARNX_SERVER_SCOPE`; the old name is no longer read at all. If you set `HARNX_INSTANCE_ID` by hand anywhere (a deployment manifest, a wrapper script, an independently deployed tool/hook server pod), rename it to `HARNX_SERVER_SCOPE`. What happens if you don't depends on the process: `harnx-toolset-server`, `harnx-hookset-server`, `harnx-mcp-bridge`, and a worker run without `--manage-servers` all fail to start, since none of them ever fell back to minting a scope of their own. A worker run *with* `--manage-servers` is unaffected either way — it always mints its own scope for the tool/hook servers it launches and never reads this variable. It is set automatically in normal use; set it explicitly only when deploying tool or hook servers independently of a worker.

#### Local runs no longer fail with "local worker did not publish readiness within 15s" when a tool server is slow or misconfigured. The worker announces readiness before it starts tool servers, hooks and sub-agent toolsets, and the front-end now waits with backoff and a progress notice instead of a fixed deadline. Worker, tool-server and hook-server output is captured to `harnx_worker.log` in the state dir so a server that dies during startup explains itself.

Hook servers take their command as trailing arguments after `--` and run it directly instead of through a shell, matching what the hooks guide already documented. A hook that needs pipes, redirection or variable expansion asks for a shell explicitly: `-- sh -c '...'`.

#### Standardize MCP Streamable HTTP options across the `time`, `bash`, `fs`, `grep`, and `plans` tool servers. Each server now accepts `--mcp-http`, `--host`, and `--port`, with documented default ports. The plans server no longer accepts `--http`, which is a breaking change for users of that flag.

The MCP adapter now reports unknown tools as JSON-RPC `-32602 invalid_params` instead of `method_not_found`.

#### feat(nats): migrate sub-agent delegation to NATS agent sessions and remove ACP.

Sub-agents now run as standard NATS agent sessions via worker-hosted toolsets rather than stdio ACP child processes. Agents are defined exclusively by `agents/*.md` files (Markdown system prompt with YAML front matter).

Key changes:
- ACP (Agent Client Protocol) and `acp_servers/*.yaml` configuration have been removed.
- For every configured agent, the worker automatically registers four NATS tools: `{agent}_session_new`, `{agent}_session_prompt`, `{agent}_session_load`, and `{agent}_session_cancel`.
- Tool responses for `{agent}_session_new` and `{agent}_session_prompt` include a structured `{ agent, session_id }` marker (`sub_agent`), and an early `SubAgentStarted` event is published on the parent session stream (`sessions.{parent_id}.events`) so user interfaces can attach to `sessions.{session_id}.events` for real-time live event streaming.
- Sub-agent turns route via standard NATS JetStream WorkQueue subjects and acquire distributed KV locks, enabling worker-agnostic execution in multi-worker deployments.

### Features

- introduce AgentEvent::SubAgent for structural routing (#1233)
- add natural-writing style guidance to agent prompts (#1249)
- route local front-ends to worker via shared NATS broker (#1250)
- add instance-scoped tool servers over NATS with time pilot (#1274)
- generalize tool-server bootstrap with config-driven lifecycle (#1284)
- add native harnx-vercel-grep-server MCP server (#1277)
- migrate fs and bash MCP servers to run bridged over NATS (#1299)
- migrate sub-agents to NATS agent sessions and delete ACP (#1306)
- convert fs to a toolset server and rename to harnx-fs-tools (#1310)
- add core hooks-over-NATS infrastructure and dual dispatch (#1314)
- launch and dispatch hooks over NATS (#1324)
- remove inline hook dispatch and migrate proxy-auth to NATS (#1325)
- make hook config command-only with supervisor nonces (#1224) (#1330)
- convert bash/plans/grep servers to native NATS toolsets (#1224) (#1339)
- harmonize fs and bash allowlists and deprecate roots (#1224) (#1343)
- propagate tool _meta over bridge, relocate shared utils, fix release.yaml (#1224, #1349) (#1352)
- remove direct MCP path (McpManager + mcp_servers/), delete harnx-mcp (#1224) (#1353)
- split the NATS worker into its own harnx-worker binary (#1401)
- run tool and hook servers as independent deployments (#1415)
- reconcile repository knowledge (#1460)
- standardize log outputs across all binaries (#1461)
- make one-shot prompts explicit (#1496)
- add frontend-affine local NATS workers (#1508)
- add shell command template support for MCP tools (#1546)
- add canonical NATS session metadata (#1545)
- make agent handoffs durable and monitor sub-agents (#1552)
- support embedded jaq expressions (#1567)
- add OpenTelemetry distributed tracing (#1577)
- retain tool-observed execution context (#1580)
- add opt-in Prometheus metrics endpoint (#1558) (#1593)
- add --healthz-addr readiness endpoint to server binaries (#1614)
- report per-invocation progress (#1619)
- wait for pull request stability (#1624)
- add cached-token cost accounting (#1637)
- add per-invocation timeout and token budget controls (#1644)
- emit canonical provider label on LLM metrics (#1749)
- regenerate session titles mid-loop during tool execution (#1754)
- add ChatGPT subscription auth via Codex client (#1769)
- add Kubernetes sandbox tool gateway (#1793)
- refresh agent models and complete provider fallbacks (#1806)
- make advisory events durable and unify HITL handling (#1797)
- make session cancellation durable and hierarchical (#1817)
- add connection coordinator and test suite (#1841)
- harden retry and timeout policy (#1851)
- replace the execution-control gate with log-fenced interruption (#1949)
- move session garbage collection into worker daemon (#2000)
- support remote agent@cluster agents (#1999)
- ship Web UI assets to container deployments (#1998)
- support the WebSocket transport for mTLS-gated load balancers (#2022)
- restore Bedrock catalog coverage and adopt Kimi K3 (#2027)
- add HARNX_NATS_SERVER cluster-client mode (#2028)
- let a client name the model catalog it inherits (#2030)
- Add per-invocation timeout and token budget controls (`--timeout-secs`, `--token-budget`) to CLI one-shot prompts and sub-agent tool calls (`{agent}_session_prompt`). The two limits end the turn differently: a timeout interrupts it with a durable `Cancel`, while an exhausted token budget is caught worker-side at a round boundary and ends the turn with an error. Either way the invocation returns a synthesized explanation alongside machine-readable termination details, leaving the session consistent for same-session retries. Interactive TUI and Web UI paths remain unbounded by design.
- Let foreground bash commands and command templates run for 24 hours by default, accept a per-call timeout override with zero meaning unlimited, and terminate cleanly when cancelled.
- Add embedded jaq expressions to the generic hook server, use them for concise tool-confirmation examples, and require approval before Daedalus hands a plan to Atlas.
- Add an explicit `prompt` CLI subcommand and require `--` before root-level prompt text, provide sober one-shot output with a `--final-only` mode, and always show the generated session resume command after standard one-shot runs.
- Add bounded Kubernetes and sandbox MCP waits, cancellation-aware lifecycle polling, jittered retry policy, and categorized gateway metrics.
- Add opt-in --healthz-addr flag exposing a /healthz readiness endpoint across the 15 supported long-running server binaries.
- Run configured hooks fully over NATS. Adds a generic `harnx-claude-compatible-hook-server` that runs `claude-command` and `claude-command-persistent` hooks over NATS, a native NATS hook mode for `harnx-proxy-auth`, a `hooks:` field on tool-server configs, and a worker-side supervisor that launches configured hooks scoped by where they're defined (global, tool-server, or agent). NATS now dispatches lifecycle, prompt, stop, and tool-use events that have runtime call sites; `InstructionsLoaded` and `CwdChanged` are supported by the protocol but aren't fired by the runtime yet. PreToolUse context injection and Ask approvals work over NATS. The inline dispatch path remains as a fallback for now.
- Add a native Kubernetes Agent Sandbox gateway with ambient per-session routing and inherited sub-agent context.
- Add a `provider` label to the `harnx_llm_tokens_total` and `harnx_llm_cost_dollars` metrics carrying the canonical backend kind (`openai`, `claude`, `bedrock`, `openai-compatible`, …), resolved from the selected model's configured client. The existing `client` label is unchanged; `provider` is additive and empty (`""`) when the client cannot be resolved. Note for dashboard operators: adding a label starts new Prometheus time series, so series that existed before the upgrade stop updating; queries that don't group by `provider` are unaffected.
- Interruption is now one durable `Cancel` entry in the session log. Ctrl+C returns control as soon as that append is acknowledged; workers watch their own session stream, abort model and tool calls, and cancel running tools, hooks and sub-agents through the tool protocol. Interrupted tool calls get placeholder results and a runtime note so the model knows why they ended. The execution-control KV bucket (`harnx_execution_control`) is no longer used and can be deleted; the "unconfirmed cancellation" and "resume anyway" flows and the `--resume-anyway` flag are removed. Tool protocol is v5: deploy workers, tool servers, hook servers and frontends together.
- `harnx-mcp-bridge --list-tools -- <command>` starts a wrapped MCP server, prints the tools it advertises, and exits without touching NATS. Reaching the listing proves the child spawns, completes the MCP handshake and answers `tools/list`; when it does not, the error distinguishes a child that failed to spawn, one that died during startup, and one that never finished the handshake — three cases a registration timeout in the worker cannot tell apart. `--name` is now only required when actually serving over NATS.
- feat(nats): add instance-scoped Core NATS tool invocation and the `harnx-time-tools` pilot. NATS tools coexist with existing stdio tools during migration, and configured tool and sub-agent children now inherit local broker credentials by design. References #1224.
- Make the shared-local-nats-server front-end/back-end split the only local execution path. TUI, one-shot CLI, serve, and ACP sessions now run every local turn front-end → NATS → worker; the old in-process local path is removed. This architectural change enables future work on tool servers and sub-agents over NATS (Phase 2). References issue #1224.
- feat(nats): add `harnx-mcp-bridge`, a generic MCP→NATS bridge that wraps any stdio MCP server and re-exposes its tools over NATS. Migrate the `plans` tool server to run over NATS via the bridge; the `harnx-plans-tools` binary still works standalone as an MCP server (`--mcp-stdio`) for external MCP clients. References #1224.
- feat(nats): migrate roots-free MCP servers (`context7`, `exa`, `fetch`, `grep`, `plans-github`, `wet`, `dev`) to run over NATS via `harnx-mcp-bridge`. Their configurations move from `mcp_servers/` to `tool_servers/`; `fs` and `bash` remain stdio pending roots support. References #1224.
- Tool and hook server registrations now expire: the registry and expectations buckets carry a 90s TTL (three refresh intervals), so a registration can no longer outlive the process that published it and grow the bucket without bound. Servers also deregister themselves on graceful shutdown, including SIGTERM/Ctrl+C — independently deployed tool/hook server pods have no parent supervisor to clean up after them, and Kubernetes terminates pods with SIGTERM. Without the SIGTERM wiring, a terminated pod's registration would keep being routed to until the TTL expired.
- NATS cluster config accepts `replicas` to set the JetStream replica count for buckets harnx creates: the tool/hook registries (including the hook expectations bucket), session leases (`harnx_leases`), and the session index (`harnx_sessions`). Buckets created before `replicas` was set (or before it was raised) now get their live replica count reconciled up to match, alongside the existing TTL reconcile; a reconcile that a cluster can't satisfy is logged and skipped rather than stopping harnx from starting. Reconcile only ever raises a bucket's replica count, never lowers it, since a caller that doesn't know the cluster's actual configured value could otherwise silently downgrade an already-correctly-replicated bucket. A brand-new bucket requested with a replica count the cluster can't provide still fails to create, by design — this only changed the fix-in-place path for buckets that already exist.
- Tool and hook servers accept TLS settings for their NATS connection.
- feat(nats): declare NATS tool servers in `tool_servers/*.yaml` across user and package configuration directories instead of using a hardcoded server list. Tool servers are lazy-spawned based on the active agent's `use_tools` patterns, and a missing or crashing server emits a UI warning while worker execution continues. The `time` tool now ships as `harnx-time-tools` under `tool_servers/`. References #1224.
- Add OpenTelemetry distributed tracing. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export OTLP traces covering agent turns, LLM calls with token count attributes, and cross-process tool calls; off by default.
- Add opt-in Prometheus /metrics endpoint via --metrics-addr.
- Make agent handoffs durably activate independent target sessions and restore fullscreen TUI monitoring for nested sub-agent transcripts.
- Remove the direct MCP integration path. External MCP servers are now added only as `tool_servers/` entries that run through `harnx-mcp-bridge`; the old top-level `mcp_servers/` config directory, `McpManager`, and the `harnx-mcp` crate are gone. Existing `mcp_servers/*.yaml` files are no longer loaded — declare external stdio MCP servers as `tool_servers/*.yaml` launching `harnx-mcp-bridge` instead (see the configuration guide). Tool call/result templates (`_meta.call_template`/`_meta.result_template`) are preserved for bridged tools.
- Remove hook config fields `type`, `event`, `matcher`, and `timeout`. Hooks now use a command-only model: the `command` field specifies a hook server binary (e.g., `harnx-claude-compatible-hook-server --event <E> --matcher <M> [--persistent] -- <child>` for generic hooks, or `harnx-proxy-auth ...` for native hooks that self-declare their event/matcher).
- Remove inline runtime hook dispatch so hooks run fully over NATS. Delete the `harnx-mcp-hooks-proxy` crate and launch bash proxy-auth injection as a co-located NATS hook.
- Session garbage collection moved from the `harnx` CLI into the worker daemon, so headless `harnx-serve` + `harnx-worker` deployments now collect expired sessions; workers log a warning when retention (`cleanup_remote_sessions_days`) is unset.
- Add YAML-defined shell command templates with typed input schemas and sandboxed execution for harnx-bash-tools.
- Give every frontend its own supervised local worker while preserving the shared local NATS broker, session history, events, and session leases. Local activations now target the owning frontend's worker, including nested sub-agents, and `harnx-worker` replaces `--cluster __local__` with the frontend-managed `--session-scope __local__` mode. Worker diagnostics now also require `--session-scope __local__` instead of a configured `--cluster`.
- Show per-invocation sub-agent progress, token and tool metrics, elapsed time, and CLI proof-of-life status across the TUI, Web UI, and one-shot CLI.
- Session titles now regenerate during the tool-call loop in addition to turn end, triggered by token growth (`title_update_threshold`) or an optional time interval (`title_update_interval_secs`). The title agent also sees the current turn's in-progress thinking and tool calls.
- Show elapsed time for running tool calls that exceed 5s across TUI, Web UI, and CLI, while suppressing the timer for sub-agent launcher calls.
- Track tool-observed repository and branch context across local and remote sessions, and use it to rank and enrich interactive session picker rows without exposing worker paths.
- Add a responsive full-width TUI exit tray that shows the outcome of an interrupt and exits once it is accepted, holding on a static state with a retry when the request fails. Add direct cancellation controls for a selected child session, and carry an interrupt through tools, hooks and nested sub-agents.
- Prompt before quitting the TUI while the agent is still working. Ctrl+D, `.exit`, and picker exits now open a confirmation modal when a turn is in flight: Ctrl+D exits without interrupting (work continues or resumes on reopen), Ctrl+C durably interrupts the session and then exits, and Esc stays. The modal copy reflects how the session runs (remote, local worker owned by this client, or owned by another client). Ctrl+C waits for the interrupt to be durably accepted before shutting down, so the interrupt can't be lost to the local worker being torn down. If the interrupt fails to land, the TUI says so and leaves the choice to you — Ctrl+C retries, Ctrl+D exits anyway, Esc returns to the editor — and an exit that leaves a failed interrupt behind prints a warning to stderr. Idle exit is unchanged.
- Allow `harnx-serve` to read its web asset directory from `HARNX_WEB_ASSETS` and warn at startup when the resolved directory has no `index.html`.
- `harnx-worker --session-scope __local__ --diagnose` starts this configuration's tool servers, reports which ones registered and how many tools each advertises, and exits without serving sessions. It applies the same selection and startup the worker uses, so it shows what a real run would do — including servers pulled in from packages the active agent cannot use — without racing a front-end that exits after its turn.
- `harnx-worker` takes `--manage-servers` to launch its own tool and hook servers. Without it, the worker discovers independently deployed servers under `HARNX_SERVER_SCOPE`.

#### Restore Bedrock coverage in the weekly `models.yaml` refresh, and move the

reasoning-heavy package agents onto Kimi K3 as their Bedrock fallback.

The updater recognised only LiteLLM's `bedrock` provider tag, but most of the
live Bedrock catalog is tagged `bedrock_converse` — the very API the Bedrock
client calls. A second filter then admitted only `us.`, `zai.` and `minimax.`
model ids. Between them the refresh yielded 18 legacy models, so every Bedrock
entry the packages actually select had been added by hand and its price frozen
ever since. Selecting on id shape rather than on a list of known vendors brings
the count to 117 and picks up Qwen3, Kimi K2.5, GLM 4.7, DeepSeek V3.2,
Nemotron, gpt-oss and the Claude 5 family on Bedrock. Capability flags that a
curated entry asserted and LiteLLM omits, such as vision on Llama 4, now
survive a refresh rather than silently switching off.

Two figures the registry states and the AWS model cards contradict are now
pinned with a citation: GLM 4.7 Flash caps output at 4K rather than the
reported 128K, and MiniMax M2.5 takes 196K of context rather than 1M. Both
were wrong in the direction that makes harnx ask for more than the model
accepts.

Kimi K3 (`us.moonshotai.kimi-k3`) is added by hand because LiteLLM carries no
Bedrock listing for it yet, and becomes the Bedrock fallback for Oracle, Plato,
Hephaestus, Daedalus, Sisyphus, Melpomene, Momus, Polyhymnia and Zosimus. It is
the only frontier-class open-weight model on Bedrock, and its 1M context and
vision close gaps the other Bedrock choices cannot. It costs about three times
GLM 5 on input and five times on output, which is acceptable only because it
sits last in every chain selecting it; the mid-tier agents keep GLM 5.

AWS documents Kimi K3 rejecting a Converse request that replays earlier
reasoning. That does not reach harnx: the model returns reasoning without a
signature, so the existing signature gate already omits the block, and both
packages reach Bedrock through the OpenAI-compatible endpoint, which never
replays reasoning at all. Both paths were verified with two-turn tool-calling
sessions against a live account.

#### Add cached-token cost accounting. Every provider now normalizes token usage to

the OpenTelemetry-subset convention (`input_tokens` includes cache tokens;
cache-read and cache-write are subsets), and cost is computed with a single
formula that prices uncached input, cache-read, cache-write, and output
separately. Cache prices (`cache_read_price`/`cache_write_price`) are
auto-generated from LiteLLM. Prometheus gains `harnx_llm_tokens_total{type=cache_read|cache_write}`
and cache-inclusive `harnx_llm_cost_dollars`; OTel spans gain
`gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`,
and `harnx.gen_ai.cost.usd`. A per-model `cache_accounting: subset|disjoint`
flag lets an OpenAI-compatible proxy fronting a disjoint backend be priced
correctly.

#### Replace NATS transcript headers and denormalized session indexes with canonical KV session metadata and activity records, including redacted metadata HTTP APIs.

This is a hard protocol cut: pre-upgrade NATS sessions are not migrated and
must be cleared before upgrading all frontends and workers together.

#### Add a `codex` client type that authenticates with a ChatGPT Pro/Plus/Team subscription instead of a metered `OPENAI_API_KEY`.

After you run the official `codex` CLI's `codex login` once, harnx reads the OAuth credentials from `~/.codex/auth.json`, refreshes the access token automatically when it expires, and sends requests to OpenAI's Codex backend using the Responses API. Configure it with a `clients/codex.yaml` file (`type: codex`) and select models like `codex:gpt-5`. The client reuses OpenAI's built-in model catalog, so new models arrive automatically as you update harnx. Tokens are held in memory only — harnx never writes back to `auth.json`, so it won't interfere with the Codex CLI. See `docs/providers.md` for setup. This uses the same first-party client path as the Codex CLI and depends on endpoints OpenAI hasn't published as a stable public API, so treat it as best-effort for personal subscription use.

#### Sub-agent delegations now record a durable start entry in the parent session log

carrying the child session_id, so a parent agent can resume or inspect a sub-agent
session even when the delegation is interrupted before returning. This adds a new
`sub_agent_started` transcript entry; in multi-instance clusters, deploy readers
that understand it before workers that write it.

#### Let a client name the model catalog it inherits, instead of deriving it from

the filename.

Client configs gain `model_catalog:`, naming a provider block in the shared
`models.yaml`:

```yaml
type: openai-compatible
model_catalog: bedrock
api_base: https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1
```

Until now the filename decided, and for `openai-compatible` clients it did so
by prefix. That made the file's name load-bearing in ways that were easy to
trip over: `deepseek-proxy.yaml` inherited the DeepSeek catalog whether or not
that was intended, and a sensibly-named `aws-prod.yaml` inherited nothing at
all, leaving its models with no context limits, prices or capabilities. The
field also works for native client types, so a `claude` client can borrow a
different block without being renamed.

The filename fallback is unchanged and still applies when `model_catalog` is
absent, so existing configurations keep working. Naming a catalog that does
not exist logs a warning and inherits nothing rather than quietly falling back
to the filename, so a typo surfaces instead of substituting models nobody
asked for; explicitly listed `models:` entries still apply.

The shipped package clients, the example configs and the provider and package
docs now name their catalog.

#### Stop the front-end and worker broker split for local sessions by making the deployment model unambiguous. When `HARNX_NATS_SERVER` is unset, front-ends (`harnx`, `harnx-serve`) always self-host a local broker and worker for `__local__` sessions and ignore operator `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` for local session routing. This fixes issue #2021, where front-ends given external NATS credentials connected their session side to the external cluster while the spawned worker ran against an elected pod-local broker, causing sessions to silently hang.

If you set `HARNX_NATS_URL`/`HARNX_NATS_TOKEN` on `harnx-serve` (or the CLI/TUI) to reach an external NATS cluster, that no longer joins the cluster — the front-end self-hosts a local broker. Set `HARNX_NATS_SERVER=<name>` and add `nats_servers/<name>.yaml` (which can use `${HARNX_NATS_URL}`/`${HARNX_NATS_TOKEN}`).

#### feat(nats): introduce core hooks-over-NATS protocol and worker dual dispatch.

Adds the `harnx-hookset` protocol crate and `harnx-hookset-server` daemon for NATS hook registration and request/reply execution. Worker dual dispatch runs NATS hooks alongside existing inline hooks, while hook supervision and config migration are deferred to future slices. References #1224.

#### MCP servers now return recoverable tool failures and argument validation errors as `CallToolResult` with `is_error: true` instead of JSON-RPC protocol error frames (`Err(ErrorData)` / `McpError`), allowing client agents to self-correct without terminating the session (#1862).

Note for client authors (such as kagent or other MCP client consumers): domain failures and argument validation errors are now returned as `CallToolResult` with `is_error: true` rather than JSON-RPC error frames (`Err(ErrorData)` / `McpError`). Client agents receive the error as tool result content and can self-correct instead of encountering a fatal protocol exception. JSON-RPC error frames are reserved for protocol violations, unknown tool names, and broken transport state.

#### feat(nats): convert `bash`, `plans`, and `grep` to native toolset servers with binaries named `harnx-bash-tools`, `harnx-plans-tools`, and `harnx-grep-tools`.

These three tool servers now implement the `Toolset` trait and run directly, dropping the `harnx-mcp-bridge` wrapper process. `--mcp-stdio` mode is retained on all three for backward compatibility. `harnx-mcp-bridge` stays as the adapter for external stdio MCP servers (fetch/exa/context7). The bash sandbox, git-snapshot history, proxy-auth hook, and the plans retention loop are unchanged. References #1224.

#### feat(nats): convert `fs` to a toolset server and rename crate/binary `harnx-mcp-fs` → `harnx-fs-tools`.

The `fs` tool server now implements the `Toolset` trait and runs directly, removing the `harnx-mcp-bridge` wrapper process. `--mcp-stdio` mode is retained for backward compatibility. References #1224.

#### feat(nats): migrate `fs` and `bash` MCP servers to run bridged over NATS via `harnx-mcp-bridge`. Add `--default-root-cwd` to `harnx-mcp-fs` and `harnx-bash-tools` to seed allowed roots from process CWD with `$HOME`-ancestor protection. Export ambient `HARNX_PACKAGE_DIR` to NATS tool servers so bundled hooks resolve when wrapped in `harnx-mcp-hooks-proxy`.

**Behavior change**: Running fs/bash from `$HOME` (or a `$HOME`-ancestor directory) now denies access with a warning — the `$HOME`-ancestor guard blocks the CWD default. To allow operations from `$HOME`, pass `--root $HOME` explicitly. References #1224.

#### Support the NATS WebSocket transport (`ws://`, `wss://`) in `nats_servers/<cluster>.yaml`

and `HARNX_NATS_URL`, so harnx can reach a broker behind an HTTP load balancer such as an
AWS ALB that requires x509 client certificates. `tls_ca` and `tls_cert`/`tls_key` can now
also be used together, and a new `ignore_discovered_servers` setting controls whether the
peers a clustered broker advertises are added to the connection's server pool.

#### A worker launching its own tool servers now starts them per session based on the session's agent, instead of one fixed set at startup. A session whose agent uses different tools than the worker's own config gets the right servers, and a worker with several agents no longer pays for tool servers a given session never calls. A server with no active session using it lingers briefly (to survive back-to-back sessions reusing it) before it actually stops.

One consequence: a tool server that failed to register no longer gets retried by a background loop for the lifetime of the worker process. It now retries the next time some session's agent asks for it — so a server fixed while a long-running session is already active will not come back for that session, only for a new one.

#### Add OTLP/gRPC trace export to `harnx-telemetry` alongside the existing HTTP exporter. Honors `OTEL_EXPORTER_OTLP_PROTOCOL` and `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` (`grpc` or `http/protobuf`).

**Operator note**: Explicit endpoints like `http://localhost:4318` are preserved as configured. The default port 4317 applies only when no endpoint is configured and `OTEL_EXPORTER_OTLP_PROTOCOL=grpc`. If `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` was previously configured in an environment pointing to an HTTP-only OTLP collector on port 4318, trace export will now attempt gRPC to port 4317. Update endpoint configuration to match the desired collector transport.

#### Support remote `agent@cluster` agents in `harnx-serve` (the HTTP API and Web UI), matching the CLI and TUI.

Agents declared in `nats_servers/<cluster>.yaml` now appear in `GET /v1/agents` (respecting the `role: assistant` picker filter) and can be addressed over HTTP as `/v1/agents/sisyphus%40shared`. Sessions against a remote agent are created, prompted, streamed, cancelled and resumed through the server, with turns running on a worker in the target cluster. A server that only talks to a remote cluster no longer starts a local broker or worker. A declared but unreachable cluster now returns a clear transport error naming the cluster instead of a misleading 404, and remote agents no longer vanish from the listing when their config can't be loaded as a local file. The internal `::__status=` marker no longer leaks into user-facing error messages.

#### Split session inspection into action-entity CLI commands and TUI parity:

- `harnx info session <agent> <id> [--format text|yaml|json]` now displays session metadata only (behavior change).
- `harnx dump session <agent> <id> [--format text|yaml|json] [--follow]` dumps the session transcript (with JSONL for json format, and live streaming via `--follow`).
- Renamed `harnx session delete` to `harnx delete session` and `--list-sessions` to `harnx list sessions`.
- Added `.dump session` and updated `.info session` in the TUI overlay.

#### Standardize logging across every binary. `HARNX_LOG_LEVEL` now configures all of

them (default `info`) and is inherited by subprocesses; `HARNX_LOG_FORMAT=json`
switches every process to one JSON object per line. The `harnx` CLI and TUI log
to `<state dir>/harnx.log` (was `harnx_runtime.log`, a name bug), overridable
with `HARNX_LOG_PATH`. Servers and subprocesses always log to stderr, and a
parent that logs to a file redirects their output there — so the worker and its
tool and hook servers land in the front-end's log instead of a separate
`harnx_worker.log`. `harnx-pkg`, `harnx-proxy-auth`, `harnx-sandbox-run`,
and `harnx-mcp-remote` previously ignored `HARNX_LOG_LEVEL` entirely;
`nats-server` output was discarded.

#### Rename the native time toolset from `harnx-time-server` to `harnx-time-tools` and rename the Git history library from `harnx-mcp-history` to `harnx-git-history`.

Remove the redundant `harnx-mcp-time` server; use `harnx-time-tools --mcp-stdio` for standalone MCP. Remove `harnx-mcp-plans-github` and its internal `harnx-mcp-plans-core` library and `harnx-mcp-plans-hermetic` test binary.

This release includes breaking container packaging changes: the `harnx-mcp-time` GHCR image is no longer published, and `ghcr.io/dobesv/harnx-mcp-plans` is now `ghcr.io/dobesv/harnx-plans-tools`. The plans Dockerfile and CI/release jobs now use the `harnx-plans-tools` name.

#### Fire `SessionStart` hooks from the worker and drop `SessionEnd` support.

`SessionStart` was dispatched by the CLI/TUI frontend, which never has a NATS
hook provider, so the event went nowhere and only logged "NATS hook provider
unavailable". The worker now fires it once per session, on the activation that
creates the session, where the hook servers it launched are reachable. Any
`additionalContext` a `SessionStart` hook returns is injected into the first
turn.

`SessionEnd` is removed. Only the frontend knows a session ended, and worker
activations happen per turn, so there was no place to fire it correctly. Hooks
registered for `SessionEnd` no longer match any event.

#### The NATS worker is now a separate `harnx-worker` binary, and the `harnx worker`

subcommand is gone. Run `harnx-worker --cluster <key>` where you used to run
`harnx worker --cluster <key>`.

Front-ends spawn the worker for the local cluster, so `harnx-worker` must be
installed alongside `harnx` / `harnx-serve` — releases publish it as its own
archive, and it ships in the Docker image. Discovery checks
`HARNX_WORKER_BIN`, then a sibling of the running front-end, then `PATH`.
`HARNX_BIN` no longer plays a part in it.

### Fixes

- resolve title agent at top level for package agents (#103) (#1164)
- surface background title-generation failures for broken agents (#1172)
- accept content param, reject unknown params, and nest plans as GitHub sub-issues (#1181)
- set require_max_tokens for adaptive-only Opus base models (#1238)
- apply agent variable defaults to restored sessions (#1258)
- update dependency @assistant-ui/react to v0.14.28 (#1270)
- update dependency @assistant-ui/react-markdown to v0.14.7 (#1272)
- update dependency @assistant-ui/react-ag-ui to v0.0.46 (#1271)
- update dependency @assistant-ui/react-syntax-highlighter to v0.14.3 (#1273)
- update dependency @assistant-ui/react to v0.14.29 (#1300)
- update dependency @assistant-ui/react-markdown to v0.14.8 (#1302)
- update dependency @assistant-ui/react-ag-ui to v0.0.47 (#1301)
- update dependency @assistant-ui/react-syntax-highlighter to v0.14.4 (#1303)
- update dependency @assistant-ui/react to v0.15.0 (#1304)
- update dependency @assistant-ui/react to v0.15.1 (#1313)
- update dependency @assistant-ui/react-ag-ui to v0.0.49 (#1327)
- wire tool discovery and restore package tool-naming (#1360)
- dispatch SessionStart from the worker and drop SessionEnd (#1369)
- surface worker turn failures instead of hanging the client (#1371)
- name the unsupplied variable instead of failing to render (#1372)
- unblock local worker startup and run hook commands as argv (#1376)
- install a logger in spawned server processes (#1377)
- report failing request patches and fix the effort aliases (#1378)
- stop passing the worker's NATS identity to wrapped servers (#1379)
- update dependency @assistant-ui/react to v0.15.2 (#1380)
- update dependency @assistant-ui/react-ag-ui to v0.0.50 (#1383)
- update dependency @assistant-ui/react to v0.15.4 (#1384)
- grant allowlist paths as written, resolve only when checking (#1386)
- move to the useAui hooks in assistant-ui 0.15 (#1393)
- decouple Command.name from usage hints to fix tab completion (#1201)
- update dependency @assistant-ui/react to v0.15.5 (#1407)
- update dependency @assistant-ui/react-ag-ui to v0.0.51 (#1408)
- update dependency @assistant-ui/react to v0.15.8 (#1411)
- update dependency @assistant-ui/react-markdown to v0.14.9 (#1412)
- update dependency @assistant-ui/react-ag-ui to v0.0.52 (#1418)
- update dependency @assistant-ui/react to v0.15.9 (#1417)
- update dependency @assistant-ui/react-markdown to v0.14.10 (#1422)
- update dependency @assistant-ui/react-ag-ui to v0.0.53 (#1421)
- update dependency @assistant-ui/react-syntax-highlighter to v0.14.5 (#1423)
- update dependency @assistant-ui/react to v0.15.12 (#1424)
- update dependency @assistant-ui/react to v0.15.13 (#1432)
- load sessions from NATS (#1430)
- stop update_models regenerating the broken effort patches (#1427)
- publish advisories in emission order (#1444)
- let the shared local server pick its own port (#1442)
- reap nats-server on startup failure and stop blocking the runtime (#1440)
- flush queued advisories before reporting a turn's outcome (#1443)
- align assistant UI dependencies (#1452)
- update dependency @assistant-ui/react to v0.15.14 (#1454)
- update dependency @assistant-ui/react-ag-ui to v0.0.54 (#1455)
- persist local sessions exclusively in NATS (#1451)
- make web asset setup reliable and concise (#1475)
- update dependency @ag-ui/client to v0.0.58 (#1479)
- keep managed servers alive across thread retirement (#1477)
- restore custom markdown rendering for tool calls (#1487)
- finalize events after complete agent turns (#1490)
- expose sub-agent session tools (#1492)
- harden sub-agent tool registration (#1494)
- harden model resolution and local worker reuse (#1497)
- abort orphaned turn task on session actor drop (#1502)
- stop the worker re-feeding a turn its own user messages (#1506)
- update dependency @assistant-ui/react to v0.15.15 (#1511)
- update dependency @assistant-ui/react-markdown to v0.14.11 (#1512)
- preserve compacted message order (#1513)
- prevent terminal probes from stopping workers (#1518)
- update dependency @assistant-ui/react-ag-ui to v0.0.56 (#1524)
- synchronize concurrent clients reliably (#1526)
- update dependency @assistant-ui/react to v0.15.16 (#1523)
- clean up stale direct MCP references (#1530)
- update dependency @assistant-ui/react-markdown to v0.14.12 (#1532)
- regenerate provider-owned model aliases (#1533)
- recover workers after local broker exit (#1538)
- persist session attachments in object storage (#1556)
- clear activity from durable turn end (#1562)
- refresh tools after server activation (#1574)
- recover queued multi-round turns (#1575)
- preserve leased tool calls for observers (#1578)
- initialize local metadata during discovery (#1581)
- clean up agent routes after turns (#1590)
- recover activation stream after broker stalls (#1594)
- update dependency @ag-ui/client to v0.0.59 (#1598)
- use lease-backed subagent liveness (#1599)
- show sub-agent session notes (#1608)
- update dependency @assistant-ui/react to v0.15.17 (#1612)
- update dependency @assistant-ui/react-ag-ui to v0.0.57 (#1616)
- update dependency @assistant-ui/react-markdown to v0.14.13 (#1617)
- keep CA temp dir alive for full proxy lifetime (#1623)
- widen transcript area and switch to divider message layout (#1632)
- route worker tool approvals to confirmation modal (#1638)
- recover stalled local workers (#1639)
- propagate malformed OpenAI responses tool arguments (#1643)
- show sub-agent prompt tool final reply in parent transcript (#1645)
- print Web UI URL on startup (#1650)
- fix agent handoff confirmation rendering and turn routing (#1653)
- use canonical agent name in subagent tool call templates (#1654)
- render tool name and arguments for command templates (#1660)
- map native toolset errors to recoverable instead of fatal (#1661)
- sync multi-client busy state and trailing tool calls (#1663)
- update dependency @assistant-ui/react-ag-ui to v0.0.58 (#1699)
- update dependency @assistant-ui/react-markdown to v0.14.14 (#1702)
- update dependency @assistant-ui/react to v0.15.18 (#1698)
- gate Renovate updates with Mergify (#1745)
- use short session IDs for sub-agent and handoff sessions (#1748)
- suppress replica count warning when requested equals current (#1755)
- grant exec on the Corepack cache by default (#1766)
- record sub-agent session ID durably on delegation start (#1768)
- honor default() filter in tool templates on error results (#1779)
- show full tool result in transcript detail view (#1781)
- normalize token usage output to single per-turn line (#1784)
- accept successful streams without a content type (#1808)
- prevent incompatible reasoning signatures when switching providers (#1816)
- address durable session review follow-ups (#1819)
- prepare pinned pnpm from web project (#1836)
- scope tool discovery cache by execution (#1840)
- bound NATS discovery cache growth (#1846)
- recover sessions after unconfirmed interrupts (#1849)
- recover active sessions across local broker failover (#1857)
- recover sub-agent progress and keep cancellation responsive (#1864)
- wait indefinitely for TUI tool approval (#1870)
- recover durable tool invocations after restart (#1875)
- bootstrap native pnpm before invoking sandbox shim (#1892)
- surface server-side run failures (#1899)
- prevent session transcript name collisions (#1894)
- update dependency @assistant-ui/react-ag-ui to v0.0.59 (#1912)
- update dependency @assistant-ui/react to v0.15.19 (#1907)
- update dependency @assistant-ui/react-markdown to v0.14.15 (#1913)
- stop leaking llama-server processes on exit (#1916)
- restore terminal title updates during a session (#1974)
- run the interrupt request on its own task (#2007)
- decide interrupts from the log's last entry (#2015)
- say why the local worker never became ready (#2020)
- stop frontend/worker broker split for local sessions (#2031)
- update dependency @assistant-ui/react-ag-ui to v0.0.60 (#2040)
- update dependency @assistant-ui/react-syntax-highlighter to v0.14.6 (#2044)
- update dependency @assistant-ui/react-markdown to v0.14.16 (#2042)
- Add durable handoff sequence markers and AG-UI session attach boundaries so clients can reject stale handoff navigation events.
- Keep AG-UI lifecycle events balanced when attaching to active local or remote sessions. Open text, tool, step, and thinking segments now close before run terminals, including after local broadcast lag and remote poll-based completion. Fixes #1043, #1837, and #1830.
- Apply agent variable defaults when restoring sessions so newly added variables with defaults do not cause strict-mode template rendering errors.
- Templated bash tools now render the tool name and supplied arguments in the TUI/CLI transcript instead of just the tool name (#1630).
- Fix macOS bash command timeouts failing with "Operation not permitted" when a process group has already exited. Continue cleanup after failed SIGTERM and tolerate kill errors for exited processes while preserving process identity checks.
- Bridged MCP tools now keep their `_meta.call_template` and `_meta.result_template`, so custom tool call/result templates render again for tools reached through `harnx-mcp-bridge` (#1349). `ToolSpec` gained an optional `meta` field carrying the tool's `_meta`, and the bridge, toolset-server adapter, and NATS tool provider thread it end to end.
- Keep the user's prompt in session history when a model turn is cancelled before its response is accepted. Persist input before model work without accepting late model or tool output.
- Clean up agent hook routes after each completed NATS-backed turn so stale fail-closed hooks cannot block later tool calls.
- Accept successful Codex subscription streams with a missing Content-Type header instead of discarding completions and falling back to an API-key provider. Preserve HTTP status and retry hints for non-JSON streaming errors, and show underlying causes in retry warnings without dumping invalid-stream response bodies.
- Fix web UI showing an alert icon on an in-flight tool call when a session is opened in a second tab. A pending (running, result-less) tool call now renders the pending spinner instead of the amber alert used for approval interrupts.
- Keep managed tool and hook servers alive when Tokio retires the runtime thread that requested their launch.
- Deliver queued TUI follow-ups at the next tool round and make interrupted multi-round sessions resume once without orphan-repair loops or a stuck busy state.
- Route interactive tool approval requests from NATS workers back to the TUI instead of automatically denying them.
- Fix auth-proxy CA bundle paths going stale. The CA temp dir was dropped when `build_runtime` returned, deleting `ca.pem` while the proxy kept running. This left `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, and similar variables pointing at a missing file, which caused tools such as `gh`, `curl`, and Git to report TLS errors. The CA temp dir now lives for the full proxy lifetime.
- Stop the TUI busy spinner after a completed turn when its durable completion boundary arrives just after the live response.
- Expose package-relative sub-agent session tools to agents and wait for their initial NATS registrations before starting the first turn.
- Emit one final model/turn boundary after the complete agent loop and keep attached TUI and web clients in sync with shared session activity.
- Fix Web UI parity for queued messages (#1741) and tool-approval confirmations (#1742). Queued messages in the web UI can now be viewed, edited (restored to composer), or cancelled. Tool approval and session handoff confirmations now display in the web UI, accept approve/deny decisions, and reappear for clients that reconnect while an approval is pending. Mid-turn injection timing for queued messages and NATS tool-approval fanout redesign remain deferred.
- Stop the NATS worker from re-feeding a turn's own user messages back into itself. The header-insert migration re-maps a headerless session's leading user block onto the migration's log seq, which sits above the turn's seed cursor, so the mid-round injection callback read the prompt the turn was already answering as a new message. Injected text was also persisted as a fresh user log entry, so every injection guaranteed another one on the next tool round and left a leftover message the end-of-turn drain ran as another turn — the TUI kept spinning and queued replies never started a loop. Worker turns no longer send their prompt to the model twice either.
- Keep `harnx dump session --follow` tailing when a transient JetStream history refresh fails.
- Fix the agent-handoff tool confirmation modal so the allow/deny options stay visible, the input preview is markdown-rendered and no longer clipped, and the prompt renders as a full-width panel.
- Prevent test environment races from overwriting user configuration, preserve embedded model metadata alongside custom client models, and validate shipped agent model references. Model-list APIs now evaluate the current client configuration on every call and return owned values. Add the missing Gemini client used by the coding package's compaction agent.
- Return successful tool-approval decisions when a worker's acknowledgement is lost but the decision is already durable. Repeating the same approval or denial is idempotent and does not execute the tool twice.
- Interrupt requests and prompt appends read only the session log's last entry before their fenced append instead of the whole transcript, so their latency no longer grows with the session's length. The worker's interrupt hint check and its tail lookup for fenced appends read the same single entry.
- Show pending tool responses when a remote session still has an active worker lease.
- Fence live session events against interruption so buffered output from a turn that was already stopped cannot change the TUI's current turn, while committed transcript history stays visible.
- Stop leaking `llama-server` subprocesses on Linux. The process registry kept each server alive in a `static`, which Rust never drops at process exit, so `kill_on_drop` never fired and every exit stranded a running server. Servers are now tied to their parent by the kernel and exit with it. macOS and Windows have no equivalent parent-death signal and still strand a server on exit.
- Initialize canonical session metadata when opening the local TUI session picker so a fresh broker shows an empty picker instead of an availability error.
- The MCP bridge no longer passes the worker's NATS identity (`HARNX_INSTANCE_ID`, `HARNX_NATS_URL`, `HARNX_NATS_TOKEN`) to the server it wraps. The bridge is the process that registers over NATS; everything below it speaks MCP on stdio. Leaking those let a descendant conclude it had been launched by a worker and switch protocols — a sandbox shim running `harnx-proxy-auth` as a stdio hook did exactly that, then served NATS instead of answering the stdio handshake, so the wrapped server was never launched and the bridge timed out after 30s. Servers wrapped in a sandbox shim now start under a worker as they already did from a shell.
- Keep tool calls shown as running when the Web UI monitors a session whose worker lease is active, and ignore redundant persisted results for calls that already completed.
- Support file and pasted-text attachments in NATS-backed TUI and web sessions.
- Route `harnx-serve` session discovery and restoration through the canonical NATS session store instead of legacy session YAML files.
- Create session transcript and cluster activation streams with the configured NATS replica count.
- Keep nested and concurrent agent tool calls attached to their own session execution.
- Keep local sessions exclusively in NATS and restore their transcripts in the TUI across restarts and session switches.
- Plans, bash, and grep tool servers now return filesystem and validation errors as recoverable tool results instead of fatal errors, so a failed tool call no longer halts the agent session.
- Preserve chronological message order when a compacted session keeps one live message.
- Fix provider switching and fallback replaying incompatible reasoning signatures, which caused HTTP 400 responses and wedged affected sessions. Tool calls now retain reasoning provenance, request builders apply destination-specific import handling, and imported anonymous calls receive request-local correlation IDs. Fixes #1804.
- Keep TUI tool-confirmation routing alive for queued continuation turns and deny promptly when the frontend detaches.
- Recover frontend-owned local workers when the shared local NATS broker's owning frontend exits.
- Recover local sessions when a worker process remains alive but stops responding, confirm cancellation durably across worker replacement, and stop orphaned sub-agent rows from spinning indefinitely.
- Keep workers available for pending session reactivation after transient NATS activation-stream heartbeat failures.
- Refresh NATS tool discovery after activating on-demand tool servers so the first model request includes their declarations.
- Remove stale direct-MCP command completions and document package tool-server patching.
- Stop logging a spurious "declining to lower" replica-count notice when a NATS KV bucket already has exactly the requested number of replicas. Only a request that would genuinely lower the count is logged now; an equal request is a silent no-op.
- Prevent local workers from being stopped by inherited terminal theme queries, and include the worker PID in slow-start notices for easier diagnostics.
- NATS tool discovery now reaches the LLM schema, package-aware `<server>_<tool>` naming is restored through one worker `ServerIdentity` module, and package/config identities prevent the #1350 server collision.
- Restore automatic terminal and browser title updates during a session. Title and compaction completion events emitted from detached maintenance tasks now reach the owning session's event sink instead of being lost to the worker's process-global sink.
- Show token usage once per completed turn instead of after every model call. The turn-end usage line now sums the whole tool loop, renders on its own line in the CLI (no longer appended to streamed text), and uses the same 📥/📤/💾 format as the status bar. Removes the inconsistent inline per-call usage lines in both the TUI and CLI.
- Keep session actors responsive to commands during periodic cancellation status refreshes from NATS, and prevent delayed poll results from overwriting newer command state.
- Abort a session actor's in-flight turn task when the actor stops, including on panic. Dropping the actor requests cancellation through `JoinHandle::abort()`, so a pending write may be dropped and a replacement actor may overlap until the old task terminates. This bounds the double-writer window instead of eliminating it; a strict single-writer guarantee would require a registry-side join or actor-mediated writes.
- Announce web approval notices and errors to screen readers and prevent approval buttons from submitting enclosing forms. Consolidate session usage context and document durable approval and cancellation behavior.
- Scope sessions by the exact agent name and local session ID, allowing different agents to reuse IDs such as `review-12345` independently. Use SHA-256 storage and stream names to preserve case sensitivity on all filesystems. Require an explicit agent for session commands. Start fresh sessions after upgrading; earlier stream names are not migrated. Remove unavailable or incorrectly named tool suggestions from sub-agent errors and runtime notes while retaining the child session ID.
- Keep successful persistent-hook startup output out of warning logs, make proxy-auth stdout strictly JSONL when launched as a persistent hook, and stop printing AWS credential bearer tokens.
- The `sub_agent_progress` custom event now carries an optional `title` field when the sub-agent session has a title, enabling UIs to display meaningful names for delegated tasks.
- Recover completed sub-agent status and counters in attached TUI sessions while the parent continues working, including across compaction. Preserve queued live events and handoffs during recovery, and preserve live counters when start events are repeated.
- Recover the shared local NATS broker in the background while preserving its endpoint and surviving workers. Share bounded read/CAS recovery, lease-safe retries and acknowledged tool/hook replies across the runtime. Report a sub-agent whose completion cannot be established instead of waiting on it indefinitely, and restore missed sub-agent completion from durable results. Restart all local frontends and update worker/tool/hook binaries together to enable the recovery contract.
- Reword the sub-agent `session_prompt` tool and `session_id` parameter descriptions so agents stop inventing custom session IDs. The old copy suggested "an unused ID such as review-12345 to create that exact session", which led models to make up an ID for every delegation and risked reusing sessions unexpectedly. The descriptions now lead with omitting `session_id` to get a generated ID (the default for a new delegation), explain that continuing a session requires the exact ID returned by a prior `session_prompt`/`session_new` call, and warn not to invent an ID.
- Sub-agent delegation and handoff now use short session IDs instead of full UUIDs.
- TUI: show a packaged sub-agent's canonical name (e.g. `@ pantheon/momus`) in the delegation tool-call line instead of the sanitized tool-name form (`@ pantheon__momus`).
- Restore the custom markdown rendering of tool calls for the bash, fs, plans, time, and sub-agent tool servers. Their native toolsets never advertised the display templates, so the TUI fell back to a generic YAML dump of the arguments.
- TUI transcript detail view now shows the full, untruncated tool result the agent sees. Previously pressing ENTER on a tool call only re-showed the collapsed user-facing summary; it now includes all content parts (including assistant-audience text) that were hidden or truncated in the inline row.
- Fix TUI Ctrl+C reporting "timed out appending the interrupt" on long sessions. The interrupt request now runs as its own task instead of being advanced one broker round trip per render tick, so it is no longer paced by the render loop. Broker read time still grows with the session's length.
- Fix the TUI incorrectly switching to idle state while a prompt task is still active. Sub-agent events are now represented using a structural `AgentEvent::SubAgent` variant, replacing out-of-band source tracking. When a nested sub-agent completes or fails, its output is rendered under its own source heading without clearing the main task's busy spinner.
- Let long-running sub-agent calls wait for lease-backed completion without implicit idle or one-hour deadlines, while detecting unavailable NATS tool servers through their registrations.
- Web UI: send subsequent messages via JSON-RPC session/prompt and follow the AG-UI stream instead of a streaming run per message; remove the client-local queue.
- Show running, completed, and failed sub-agent sessions in the Web transcript and open their child transcripts with browser-history navigation.
- Replace the web UI placeholder favicon with the Harnx icon and adapt its colors to the browser's light or dark theme.
- Show server-side run failures, including missing local worker binaries, in the Web UI and log their full cause in harnx-serve.
- Surface worker session failures in the UI instead of hanging, and load file-backed agent variables in the NATS worker.
- The NATS worker fails with the variable's name and description when an agent declares a variable nobody supplied, instead of an opaque template error.

#### Fix ACP `session_prompt` failing with a bare "Invalid params" when a sub-agent model passes an empty or made-up `session_id`.

Empty or whitespace-only `session_id` values are now treated as omitted and start a new session instead of being forwarded verbatim. Unknown session IDs (in both `prompt` and `cancel`) now return an actionable error telling the model to use a real ID or omit it to start a new session. The `session_prompt` tool and `session_id` parameter descriptions now spell out how to continue a conversation versus start a new one, and warn against inventing session IDs.

#### Sandbox allowlist entries now keep the symlinks they were written with. A relative entry is still made absolute against the working directory, but nothing beyond that is resolved; symlinks are followed only when checking a path against a grant.

Grants were previously canonicalised at insertion, which had two consequences. It widened a grant to wherever a symlink pointed, so allowing a link could hand over its target. And it lost the path callers actually use: on merged-`/usr` systems `/lib64` collapsed into the `/usr/lib64` entry already present, so the sandbox never mounted `/lib64` and every dynamically linked binary failed to start, because loaders are named absolutely as `/lib64/ld-linux-x86-64.so.2`. That surfaced as `bash_exec` failing every command with `sandboxing failure: No such file or directory`.

A leading `~` in an allowlist entry now resolves against the home directory too. Config files and tool-server arguments are read without a shell, so `--allow-read ~/.config/foo` arrived literally and was treated as relative to the working directory. `~user` is left alone, since resolving another account's home would need a passwd lookup.

#### Fix the web build against `@assistant-ui/react` 0.15, which removed the `useThread`, `useMessage`, `useComposerRuntime` and `useThreadRuntime` hooks.

State reads move to `useAuiState`, which takes a selector over one combined state object, so `useThread(s => s.isRunning)` becomes `useAuiState(s => s.thread.isRunning)` and `useMessage(s => s.role)` becomes `useAuiState(s => s.message.role)`. The two runtime handles come off `useAui()` instead, as `aui.composer` and `aui.thread`, and keep the same methods.

#### Bootstrap pnpm's native executable through Corepack before using a sandboxed pnpm

shim during `cargo xtask install`, preventing read-only cache errors after pnpm
version updates.

#### Package managers launched through Corepack now run inside the sandbox without extra flags. `~/.cache/node/corepack` is granted exec by default.

Corepack spawns the pinned package manager straight out of that cache. Through pnpm 11 it spawned a script and ran it via `node`, which the existing exec grant on `node` covered. pnpm 12 ships a native binary instead, and `~/.cache` is a read+write default with no exec, so the download succeeded and the spawn failed with `Could not run the pnpm binary at ~/.cache/node/corepack/v1/pnpm/<version>/pnpm-native: EACCES`. Until now the only way past it was to patch your own shim.

The cache is listed under exec rather than read/write/exec on purpose. A more specific grant replaces the one it sits inside, so the exec entry also revokes the write this subtree used to inherit from `~/.cache`: sandboxed code can run the cached package manager but can no longer replace it with something the host will execute later. That makes this a tightening of the default posture, not a relaxation.

Two consequences worth knowing. Corepack can no longer download a *new* package manager version from inside the sandbox, so run `corepack install` on the host after changing a `packageManager` field. And defaults are skipped when the path does not exist, so on a machine that has never run Corepack the first sandboxed invocation still fails and the next one succeeds. Grant `--allow-rwx ~/.cache/node/corepack` if you would rather let the sandbox fetch releases itself.

#### Drop the `fs4` dependency and lock files through `std::fs::File` instead.

`File::try_lock` and `File::unlock` were stabilised in Rust 1.89, and the inherent methods shadow `fs4`'s extension trait, so the crate was already being bypassed at every call site. Contention and I/O errors now arrive as one `TryLockError`, and a small helper keeps them apart: another process holding the lock makes this one a follower, while an I/O error has to surface rather than be read as a lost election.

#### Make committed-handoff navigation, token usage, sub-agent progress, and

tool-approval state durable in the session log so every client recovers them by
replay. Previously these were advisory-only fan-out events, so a client that
wasn't attached when they fired (reconnect, a second client, a backend restart,
or a different harnx-serve process) never saw them.

- Handoff, usage, and sub-agent start are recorded as durable session-log
  entries and rehydrated on attach; clients dedupe live vs. hydrated events by
  marker id.
- Tool approvals are now a worker-owned durable protocol. The lease-holding
  worker is the single writer of the approval request and decision; any
  harnx-serve routes a decision to it, and lease+fence gives single-winner
  semantics (no double-apply under concurrent or duplicate submissions). Pending
  approvals survive reconnect and backend restart, and the web and TUI clients
  use the same worker-routed path. The web approval UI now reviews one tool call
  at a time.
- Loading pre-change sessions still works (additive schema).

#### Keep web and TUI clients synchronized on canonical session IDs, live session activity, and complete reloaded assistant replies. Prevent hydration from duplicating externally submitted prompts, keep title maintenance inside its session lease, distinguish message roles, report unavailable session discovery, and recover damaged tool transcripts with visible warnings.

Keep the web installer lockfile and Assistant UI dependency family aligned so frozen installs and production builds succeed.

#### Fix request patches for the Opus 4.7/4.8 effort aliases and the `gpt-5.6-*:high` aliases, which silently did nothing. jaq (unlike jq) won't create a missing parent object for a nested path assignment, so `.body.output_config.effort = "high"` failed at runtime and took the rest of the patch with it — those models were sent `temperature`/`top_p` and no thinking or effort config.

A failing request patch is now reported as an error naming the patch source, the expression, and the jaq message, instead of only a `warn!` that needed debug logging to see.

#### Say why the local worker died when it never becomes ready.

`LocalWorkerSupervisor` gave up after three worker exits with `local worker
exited 3 times without becoming ready:` and nothing after the colon. The exit
status was available and only logged, and the message tailed a log file that a
process without a configured logger never opens — so every test binary, and
every embedder that logs nothing, reported a startup failure with no evidence in
it.

The message now names each exit status and quotes what the worker printed. When
the parent has no log file to share, the supervisor captures the worker's output
into a temporary file of its own rather than discarding it.

#### Fix a panic on every TLS connection to a NATS broker that did not set `tls_ca`, including

plain `tls: true` and the client-certificate mTLS configuration. Harnx now builds the rustls
config itself, naming the crypto provider, instead of leaving async-nats to resolve a
process default that this workspace makes ambiguous.

#### Make `cargo xtask install` disable Corepack's pnpm download prompt so web asset

installation can run unattended, and synchronize the pnpm lockfile with the
current workspace overrides and package manifest. Keep web builds concise by
hiding Vite's per-asset size report while preserving build warnings.

#### Refresh package agent models by workload and provide Gemini, Claude, Codex,

OpenAI API, and non-Anthropic Bedrock fallbacks for every agent, including
compaction. Prefer Codex immediately before the equivalent OpenAI API model.

Use GPT-6 Astra at maximum effort for Oracle and Plato, with Claude Fable 5.1
as their Claude alternative. Move Atlas and general Gemini workers to Gemini
3.8 Flash, keep Opus 4.8 for Sisyphus and Daedalus, and use cheaper models for
routine work and compaction. No package agent selects newer Opus versions.

Package-qualified OpenAI-compatible clients now inherit shared provider model
metadata. Add the Bedrock GLM/MiniMax and direct Gemini 3.8 entries, and generate
Astra/Fable reasoning aliases with the required request settings. Preserve
Gemini function-call IDs through tool-result replay and configure Fable 5.1
to tolerate thinking invalidated by conversation compaction. Packages
require harnx 0.34.0 or a development build containing these changes.

#### Make `cargo xtask install` prepare the pnpm version pinned by the web project

with Corepack, and run every pnpm command from `web/` so Corepack always finds
that version before building the web UI.

#### Fix promptless AG-UI run showing idle when remote worker is active

When a Web UI client opens a promptless `/run` against a session whose local `SessionActor` is `Idle` but a remote NATS worker holds the lease, the AG-UI endpoint now follows the remote worker's advisory stream instead of terminating immediately with a synthetic `RUN_FINISHED`. This prevents the Web UI from showing an idle (Send button, no spinner) state while a remote worker is actively processing a turn.

The remote-follow path:
- Emits `RUN_STARTED` (exactly one)
- Hydrates history snapshot (MessagesSnapshot)
- Attaches to the session's NATS advisory stream and translates events to AG-UI frames
- Terminates with `RUN_FINISHED` when a matching durable `TurnEnd` is observed; sustained lease absence is handled separately as crash detection
- Handles worker crash (lease absent for sustained interval with no `TurnEnd`) by forcing finish
- Handles race condition (turn ended between lease sample and stream attach) by checking for existing `TurnEnd` and finishing immediately

#### The OpenAI Responses parser now surfaces an error when a streamed tool call has non-empty but malformed JSON arguments, instead of silently replacing them with an empty object `{}`.

Both the non-streaming (`openai_extract_responses`) and streaming (`responses_finalize_tool_call`) paths previously did `serde_json::from_str(..).unwrap_or_else(|_| json!({}))`, so a truncated or invalid arguments buffer produced a tool call with no arguments rather than reporting the failure. They now propagate the parse error with the tool name and raw arguments, matching the chat parser. An empty arguments string still defaults to `{}`.

#### Update `rmcp` to 3.x.

The MCP server trait now returns `CallToolResponse`, an enum whose other variants cover elicitation and long-running tasks. Every server here answers in one step, so tool dispatch moved to its own method returning a plain `CallToolResult` and `call_tool` converts. `ListToolsResult` gained the SEP-2549 caching fields and is built with `ListToolsResult::with_all_items`, which fills them in and marks the result complete. `Meta` is now `MetaObject`, and `StreamableHttpServerConfig::with_stateful_mode` is `with_legacy_session_mode`.

#### Print the Web UI URL on `harnx-serve` startup and label the API endpoints as POST-only.

Startup previously advertised only the `/v1/embeddings` and `/v1/rerank` endpoints, which are POST-only and can't be opened in a browser, and never showed the Web UI URL served from `/`. The startup banner now leads with the Web UI URL and derives the advertised host/port from the socket's real bound address, so wildcard binds (`0.0.0.0`, `::`) map to loopback and ephemeral ports (`:0`) resolve to the actual port.

#### Spawned server processes — tool servers, hook servers and the MCP bridge — now install a logger, so the diagnostics they already emit reach the worker log instead of being discarded. The MCP bridge forwards its wrapped child's stderr line by line, which previously went nowhere: a `context7` or `exa` server failing on a missing API key produced no output anywhere. The bridge also logs the command it is starting and the tool count once ready, so a server still initialising is identifiable rather than silent. Set `HARNX_LOG_LEVEL=debug` to see the wrapped child's own output.

A tool server that has not registered but whose process is still running is now reported as possibly still starting, naming the log to look in, rather than as having failed to start.

#### Fix spurious 503s from `harnx-serve` when a request lands on a session whose actor is reaping itself.

An idle session actor stops after 5 seconds and removes itself from the registry. It used to do that without regard for callers, so a request that had already picked up its handle sent commands into a closed channel and got `session actor unavailable` or `session actor dropped ... reply` back as a JSON-RPC 503 — for a session that was perfectly resumable. The reap now happens atomically with the liveness check: an actor only removes itself while the registry holds the last handle to it, so no in-flight request can be talking to it, and otherwise it waits another interval. Handing out a handle also treats an entry whose channel is closed as no actor at all and spawns a replacement, so a session survives an actor task dying on its own (a panic) instead of failing every later request for that key.

#### Stop the non-interactive CLI from printing raw `[event] LogSeqAssigned { seq: N }` debug lines on stderr during `prompt` runs.

`SessionEvent::LogSeqAssigned` is persistence bookkeeping: the TUI patches transcript rows with the assigned log sequence so edit/delete/rewind can target the right entry, but the CLI makes no use of it. It had no explicit match arm in the CLI event sink, so it fell through to the `[event] {other:?}` debug catch-all and printed once per log write. It's now dropped silently, matching how the sink already ignores other internal-only events.

#### Fix automatic session-title generation when running a package agent (e.g. `pantheon/sisyphus`).

A globally-configured `title_agent` was resolved relative to the active agent's package, so a top-level `title-agent` was looked up as `<package>/title-agent` and never found — title generation was silently disabled. Global title agents now resolve at the top level, while an agent's own `title_agent` frontmatter still resolves package-relative.

Also surface title-generation failures instead of failing silently: a new `TitleGenerationFailed` event is shown in the TUI, CLI, server (AG-UI), and web client, carrying the full error chain. Background title-agent output is isolated from the main transcript. Adds a `.title` command to view the current title and guard state and `.title generate` to (re)generate on demand, shows the title in `.info session`, and adds logging to the title-generation path.

#### Keep TUI tool approvals pending until the user responds instead of automatically denying them after 30 minutes. Cancellation and frontend shutdown still end the approval wait.

Preserve synchronous confirmation support on both current-thread and multithreaded Tokio runtimes.

#### Recover pending tool invocations automatically after a worker restart. Tool servers can return saved replies, retry idempotent operations, or reattach sub-agent turns without duplicating their prompts. Fix a lease-release race that could delay restart recovery and false cancellation failures when completed execution records are pruned concurrently. This upgrades the internal tool protocol; restart all frontend, worker, and tool-server instances together.

Preserve completed child results and tool-call order during replay, and verify the original logical tool-server identity and current worker ownership before redispatch.

Reduce worker stack usage during session activation to prevent stack overflows exposed by nested sub-agent execution on Windows debug builds.

#### Stop logging spurious `template error in tool '...' result_template: undefined value` warnings (#1537).

Tool call/result display templates rendered under MiniJinja's Lenient undefined mode, which still raises on attribute/index access into an undefined intermediate. The shared plans result template `{{ result.content[0].text | default('') }}` walks into `result.content`, which is absent on recoverable-error results (`{"is_error": true, "error": ...}`), so it raised before `default('')` could apply and every plans/time tool logged a warning on its error path. Templates now render with Chainable undefined behavior so `default()` is honored; syntax errors and other hard failures still surface.

## 0.33.4 (2026-07-23)

### Features

- Add OpenAI `/v1/responses` support so gpt-5.6 reasoning models work with function tools and `reasoning_effort` (blocked on `/v1/chat/completions`). New reasoning-level model aliases `gpt-5.6-sol:high|max` and `gpt-5.6-terra:high|max` route to `/v1/responses` via a new `endpoint` model field, with cross-turn reasoning replay (`reasoning.encrypted_content` via `thought_signature`), `store: false` default overridable through a new `patches.responses` client-config key.

### Fixes

- include failing expression and input kind in runtime error logs (#1088)
- forward model errors via harnx:error meta instead of plain text (#964) (#1128)
- Fix Gemini requests failing with a 400 "Role 'function' is not supported" error. Tool-result turns are now sent with the `user` role, which is the only valid container for `functionResponse` parts (Gemini accepts only `user`/`model` roles). Newer Gemini endpoints reject the previously-tolerated `function` role.

## 0.33.3 (2026-07-21)

### Features

- bring web UI to TUI parity with GFM markdown and collapsible tool cards (#1031)
- fix and harden hook-based auth injection (Jira/GitHub) + startup handshake (#1050)
- render system prompt at request time with tool and model awareness (#1055)
- add automatic LLM-driven session title generation (#1069)
- add cross-process file locking for local sessions (#1077)
- implement agent handoff for Web UI and NATS worker (#1109)
- serialize concurrent file mutations to prevent corruption (#1122)
- Add AG-UI tool summary custom events and context token usage metadata for live and restored sessions.
- Serialize concurrent mutations in the filesystem MCP server to prevent corruption from parallel edits. Same-file edits (write, edit, insert, re_replace) are now serialized via per-file locks, while `rollback_file` takes an exclusive repository-wide lock so it cannot interleave with concurrent edits to other files in the same repository.
- Render agent system prompts at request time with current tool and model context instead of storing them in session transcripts.

#### feat(proxy-auth): send resolved `vars` to executable hooks on each request

Executable (`--hook <path>` / inline shebang) hooks now receive a `vars` object
on every JSONL request containing the resolved, non-secret context that jq hooks
reference as jaq variables — the `fake_*` sentinels and `temp_file_root`. Real
secrets are deliberately excluded (a hook already inherits proxy-auth's process
environment, so putting them in the payload would only widen the logging
surface).

This lets a hook write files into proxy-auth's own per-instance temp dir
(`--fs`'s `$temp_file_root`) — unique per proxy and auto-deleted on exit — and
agree with a sibling `--env` on the path, instead of guessing a shared location.
`example_config/jira-auth-hook.py` uses `vars.temp_file_root` to place its
synthetic acli config exactly where `--env` points `ACLI_CONFIG_DIR`, fixing
`acli` auth in the sandbox (the previous `\($temp_file_root)/harnx-fs-acli`
rendered as `/harnx-fs-acli` because `$temp_file_root` is empty without `--fs`).
The hook also gained verbose per-request tracing (method + host + path +
injection decision) when `HARNX_JIRA_LOG_FILE` is set.

#### feat(hooks): structured notice channel + failure surfacing to the UI

Hooks can now surface messages to the active UI (TUI/CLI/serve) two ways:

- **Structured channel (live hooks):** a persistent hook prints a standalone
  JSONL line `{"notice": {"level": "error"|"warning"|"info", "message": "…"}}`
  on stdout (no request `id`). harnx recognizes it and posts an
  `AgentEvent::Notice`. `harnx-proxy-auth` forwards such lines from its exec
  sub-hooks, so a nested hook (e.g. `jira-auth-hook.py`) can report an internal
  error even while it keeps running.
- **Dead-child fallback:** when a persistent hook process fails to launch or
  exits unexpectedly, harnx emits an Error notice with the child's captured
  stderr tail (deduped per command within 30s).

`jira-auth-hook.py` uses the structured channel to report auth-init failures
(e.g. keyring/config problems) instead of failing silently.

#### feat(hooks): inject `$HARNX_PACKAGE_DIR` into hook processes

Every hook command now runs with a `HARNX_PACKAGE_DIR` environment variable set
to the directory of the package that owns the hook (for hooks defined by a
packaged MCP server), falling back to the config directory for hooks defined
outside a package. This lets packages bundle helper scripts alongside their
config and reference them without hardcoding an absolute path, e.g.
`harnx-proxy-auth --hook $HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py`.

#### feat(commands): add `.info env` to inspect the harnx process environment

`.info env` lists the environment variable **names** harnx (and therefore its
hooks and MCP servers) inherit — values hidden. `.info env <NAME>` prints a
single variable's value. Useful for diagnosing hook/proxy problems (e.g. is
`DBUS_SESSION_BUS_ADDRESS` present, is a token var set) without dumping secrets.

Also adds `example_config/probe-auth-hook.py`: a standalone script that drives a
`harnx-proxy-auth` exec hook (e.g. `jira-auth-hook.py`) directly, showing its
debug/init state and the masked Authorization header it would inject per host.

#### feat(session): cross-process file locking for local filesystem sessions

Processes sharing a local session file are now serialized via a per-session `.yaml.lock` file (`std::fs::File::lock`).
A second process shows "Waiting for session lock…" in the transcript, then acquires
the lock when the first goes idle, reloads the session from disk to pick up entries
written by the prior holder, and proceeds. Session file writes (`save`, `append_event`,
`ensure_log_file`) no longer truncate or drop entries under concurrent access, and
sequence numbers are re-derived from the file while the lock is held to avoid stale
caches.

#### feat(commands): add `.info mcp [server]` diagnostics

Print an MCP server's resolved command, args, env, roots, connection status,
child PID, and — crucially — the exact `command` string of each configured
hook plus the **live PID of any running persistent hook** (e.g. the
`harnx-proxy-auth` process). Seeing the hook command verbatim and its PID makes
it easy to spot YAML-folding/argument-dropping problems or a hook that never
spawned. With no server name, lists all running servers with status and PID.

#### Add a startup message to the harnx-proxy-auth exec-hook protocol. After a hook

prints `READY`, the proxy sends `{"event": "startup", "vars": {...}}` and the
hook may respond with an `env` map that is injected into the sandboxed command
(and write files to `temp_file_root`) before the first request runs. The bundled
`jira-auth-hook.py` now initializes eagerly at startup.

#### feat(session): automatic session title generation

Sessions now get a short, LLM-generated title after the first exchange and
periodically as they grow (every `title_update_threshold` tokens, default
50,000). Configure the generator via `title_agent` (global in `config.yaml` or
per-agent in front matter); leave it unset to disable. Titles are stored as
append-only `Title` log entries, surfaced in local and remote (NATS) session
listings and the serve API, and can be set manually with `.set title <text>`
(which freezes automatic regeneration). Do not use a reasoning model as the
title agent.

#### feat(ui): show the generated session title in the terminal and browser tab

The automatically generated session title now sets the terminal window title in
the TUI and the browser tab title in the web UI (as `harnx — <title>`), updating
live as the title is (re)generated or set with `.set title`. Adds an
`example-title-agent` and `title_agent` / `title_update_threshold` settings to
the example configuration.

### Fixes

- only prefetch selector-matching MCP servers at agent init (#1029) (#1030)
- overhaul transcript history navigation seq mapping (#1032)
- emit streamed notifications in order, not per-chunk tasks (#1038)
- sandboxed acli auth — write synthetic token as YAML !!binary (#1052)
- update dependency @assistant-ui/react to v0.14.27 (#1105)
- update dependency @assistant-ui/react-ag-ui to v0.0.45 (#1106)
- update dependency @assistant-ui/react-markdown to v0.14.6 (#1107)
- Polish web chat UI with breadcrumb navigation, flatter composer styling, auto-growing input, cleaner token status display, and a real queue for submit-during-run behavior.

#### fix(example): jira-auth-hook.py injected auth on the wrong Atlassian host

acli `jira` data calls authenticate to `api.atlassian.com/cli/<cloud_id>/…`
with the api_token; it separately POSTs to `as.atlassian.com/api/v1/batch`
**unauthenticated** (`Basic BLANK`). The hook was matching `*.atlassian.com`,
so it forced the real token onto the `as.atlassian.com` batch call — which
Atlassian rejects there — aborting acli before it reached the working
`api.atlassian.com` data call ("unauthorized").

Now the hook injects only for the hosts acli authenticates to: `api.atlassian.com`
and the configured site. `as.atlassian.com` is left untouched. (Verified against
a capture of a working interactive `acli jira project list`.)

#### fix(example): sandboxed acli authenticates again — synthetic token written as YAML `!!binary`

`jira-auth-hook.py` wrote the synthetic acli config token as a plain string.
acli stores its token as an encrypted SecretStore blob it expects as a YAML
`!!binary` scalar (the YAML parser base64-decodes it before acli decrypts).
With a plain string, acli failed to decrypt and aborted with "failed to
retrieve authenticated status" **before** ever calling `api.atlassian.com`, so
the proxy's on-the-wire token swap never ran and the sandboxed `acli` reported
unauthorized. This restores the `!!binary` format (originally fixed in the
inline `bash.yaml` config, dropped when the logic moved into the hook) across
all three `jira-auth-hook.py` copies.

The hook now also sources the token per platform automatically — `secret-tool`
on Linux and `security find-generic-password` (login keychain) on macOS —
instead of assuming `secret-tool`; `HARNX_JIRA_TOKEN_CMD` still overrides it.
The Jira docs recipes now use `jira-auth-hook.py` directly rather than an inline
config that re-serialized the token as a plain string.

#### fix(packages): bash.yaml proxy-auth hook silently dropped all args after the first

The `harnx-proxy-auth` hook command in `packages/{pantheon,coding}/mcp_servers/bash.yaml`
is a folded (`>-`) YAML scalar. Its jq `then`/`end` lines were indented deeper
than the `--hook` they belonged to, so YAML preserved those as **literal
newlines between arguments**. Because the hook runs via `sh -c`, each newline
was a command separator — `sh` executed `harnx-proxy-auth --hook '<first hook>'`
and discarded everything after it (`--hook` for api.github.com, `--env`, and
`--hook …/jira-auth-hook.py`), reporting `sh: --hook: not found`.

Result: GitHub API (`api.github.com` Bearer) auth was never injected, the acli
config dir was never set, and the Jira auth hook never ran (hence no log file).
Fixed by aligning the jq continuation lines with `--hook` so the scalar folds
to a single space-separated command; verified all arguments now reach
`harnx-proxy-auth`.

#### fix(example): robust config parsing, lazy init, and diagnostics for jira-auth-hook.py

- **Config parsing**: use PyYAML when available, else an indentation-agnostic
  line parser. The old hand-rolled parser required list items indented exactly
  `  - ` and silently parsed **zero profiles** for other (valid) layouts,
  producing `profile matching current_profile not found` and no auth injection.
- **Lazy init**: read the acli config + keyring on the first Atlassian request
  instead of at startup, retrying on failure — so the hook never touches the
  keyring before it's ready and a transient miss isn't cached for the process's
  lifetime.
- **Diagnostics**: step-by-step logging (never the token), optional
  `HARNX_JIRA_LOG_FILE`, a full traceback on failure, and a `/jira-auth-hook/debug`
  endpoint reporting `initialized`, `target_hosts`, and the captured `error`.
- Fall back to `ATLASSIAN_EMAIL` when the profile has no email (was producing a
  blank Basic-auth username).

## 0.33.2 (2026-07-09)

### Features

- add unified error handling for streaming events across LLMs (#908)
- add static remote agent catalog to cluster configuration (#929)
- achieve remote agent tool parity and fix thin-client N… (#930)
- implement remote session enumeration protocol (#938)
- wire remote control surface for agent@cluster (#915) (#956)
- add opt-in background GC for remote sessions (#960)
- implement AG-UI protocol server support (#966)
- add AG-UI follow-up features and fixes (#1005)
- Add AG-UI Phase 2 control plane to harnx-serve: a per-(agent,session) actor with a tokio::broadcast event bus, a JSON-RPC 2.0 control endpoint (`session/get|prompt|cancel`), and a subscription-style SSE endpoint that emits a MESSAGES_SNAPSHOT on join then streams live events to all subscribers (with ~15s keep-alive). Dropping an SSE connection no longer stops a run — only `session/cancel` aborts, and cancellation persists partial state. The SSE run POST now inspects only the last message and drops the previous reconcile/empty/multi-message 400s.
- Add AG-UI protocol server support to harnx-serve (content-negotiated /v1/agents tree with SSE run, session enumeration, history, and durable message ids).
- Add #984 assistant-role filtering to the `/v1/agents` server endpoint.
- Add an optional `agents:` list to NATS cluster config (`nats_servers/<cluster>.yaml`). Declared remote agents appear as `name@cluster` in `--list-agents`/shell completion, and assistant-role entries also appear in the interactive picker. Static config only — no network calls.
- Add server-side multipart attachment uploads with `cid:` reference prompt plumbing for AG-UI sessions.
- harnx-serve and the AG-UI web client now support composing and injecting pending messages while an agent run is active. The server queues prompts sent via `session/prompt` (returning `Enqueued`) and consumes them on the next tool round, emitting a `pending_message_consumed` `CUSTOM_EVENT`. The web client uses this event to clear its queued message UI indicator reliably.
- Adds opt-in background GC for remote sessions stored in NATS KV. Enable via `cleanup_remote_sessions_days` config field or `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS` environment variable. When set, runs hourly to purge stale session index entries across all configured NATS clusters.
- The web client now allows uploading attachments using `assistant-ui`'s native attachment UI. Images and files are transparently uploaded and their CID references are piped through the JSON-RPC `session/prompt` mechanism to the server.
- Surface previously-silent errors in the AG-UI web UI: agents-list and sessions-list fetch failures (#983) now show inline error text, and message-send failures (#987) show an inline composer-area error.



#### High-availability distributed agent execution backed by NATS JetStream.

- **Durable Persistence**: Session logs are stored as append-only streams in NATS JetStream.
- **High Availability**: Multiple workers can provide failover, using a NATS KV-based lease for single-active-worker mutual exclusion and fence tokens to prevent stale writes.
- **Thin Client Driver**: Automatic routing for `agent@cluster` agent references, separating client-side UI/tooling from backend execution.
- **Live Event Fan-out**: Real-time streaming of model chunks and status updates to multiple connected clients for multiplayer visibility.
- **Control Plane**: Remote cancellation and pending message management across the NATS cluster.
- **Security**: Support for NATS token authentication and mTLS.
- **Operations**: New `harnx worker` command, session management tools, and comprehensive HA documentation.

#### Enables remote session enumeration for NATS-backed agents in the TUI session picker, CLI `--list-sessions`, and shell completion. 

Previously, remote (`agent@cluster`) sessions were invisible to enumeration tools unless they existed in the local session directory. This change introduces a NATS KV-backed session index (`harnx_sessions`) that workers populate upon session activation and refresh during lease renewal. Clients now automatically route enumeration requests to this remote index when a remote agent is in context, with graceful degradation and timeouts to ensure local operations remain responsive even during NATS connectivity issues.

#### Wire the remote control surface for NATS-backed `agent@cluster` sessions into the TUI, mirroring existing local-session operations.

Remote sessions can now be resumed from the session picker (the picked session id is threaded into the thin-client turn instead of always starting a new session), cancelled with Ctrl+C (publishes `ControlCommand::Cancel` to the session's NATS control subject, fire-and-forget), and retracted/edited with the existing `d`/`e` keybindings (routed to the thin-client `retract_user_message`/`edit_user_message`, converting the displayed index to the JetStream user-message sequence). Local-agent execution paths are unchanged. CI now installs `nats-server` on Linux so the NATS integration tests run.

### Fixes

- prevent infinite loops and panics in scrolling widget rendering (#907)
- use leader-authoritative read for mid-turn injection decision points (#917) (#928)
- resize height cache instead of underflowing when items shrink (#952)
- don't restore the panic hook while unwinding (#954)
- surface mid-stream streaming LLM errors instead of stopping silently (#963)
- keep transcript visible after compaction (#904) (#967)
- forward EXA_API_KEY through the sandbox for the exa MCP server (#973)
- harnx-serve now maps `AgentEvent::Status` to an AG-UI `CUSTOM_EVENT` (name `status`, payload `{ "text": string }`), emitted within run boundaries, so web clients can surface agent status the way the TUI does. Previously these status updates were dropped.
- harnx-serve now streams AG-UI step, compaction, plan, status, and usage events so clients can observe turn boundaries, transcript compaction, and plan updates live.
- harnx-serve now streams AG-UI thinking events from model thought chunks and preserves assistant prose plus tool-result entries in AG-UI history snapshots for tool-call turns.
- Fix the Exa MCP server so web search works when `npx`/`node` is wrapped by a harnx sandbox. The configs previously set `EXA_API_KEY: "$EXA_API_KEY"`, but harnx does not expand `$VAR` in MCP `env:` values and the sandbox scrubs the child environment — so the server received no usable key and returned `API key must be provided`. They now use `HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY`, which `harnx-sandbox-run` honors to forward the real host value. Also documents both footguns (literal `env:` values; sandbox env stripping) in the configuration guide, environment-variables, sandbox-run, and FAQ docs.
- Fix #985 by percent-decoding encoded agent names in AG-UI server routes.
- Fix server-mode log filter default (`harnx::serve` → `harnx`) so logs from `harnx_*` crates are captured. Correct `.env` precedence to standard dotenv semantics: the ambient/inherited environment always wins and the `.env` file only fills in variables that are not already set (previously `.env` unconditionally overrode inherited variables, silently clobbering operator-set values like `HARNX_LOG_LEVEL`). Fixes #989.
- Fix a crash where a panic in the TUI aborted the process (and dumped core) instead of exiting cleanly. Restoring the terminal's panic hook while a panic was already unwinding triggered a fatal double-panic ("panic in a destructor during cleanup"); the guard now skips hook restoration when it is dropped during unwinding, so the original panic is reported and the terminal is restored normally.
- Fix promptless session join returning empty MESSAGES_SNAPSHOT when session has persisted history (issue #959).
- Emit tool-result events to sinks for recoverable tool execution errors, so AG-UI/CLI/TUI subscribers see terminal tool-call events for failed tool runs.
- Fix infinite loop and potential panics in scrolling widget by limiting render attempts and using safe indexing.
- Fix a crash when scrolling the transcript after compaction. The scrolling widget's per-width height cache assumed the number of items only ever grows; when compaction shrank or blanked the transcript, an internal length calculation underflowed — panicking in debug builds and, in release builds, wrapping into an unbounded allocation that could exhaust memory. The cache now resizes to the current item count.
- Fix silent stop with no output when a streaming LLM response returns a mid-stream error (issue #905).
- Fix AG-UI tool approval resume handling so browser resumes can omit original prompt text, resumed batches must cover every pending interrupt, and mixed approved/deferred tool rounds preserve results for every emitted tool call.
- Release binaries now ship with line-table debug info and are no longer stripped, so crash backtraces — including the heap-guard abort trace and panic backtraces — resolve to real function names and line numbers instead of `<unknown>`. This makes crash reports from release builds actionable out of the box, at the cost of a somewhat larger binary.
- Remove `harnx --serve` from core CLI. Use standalone `harnx-serve` binary for HTTP server mode instead.
- Removed harnx-serve legacy chat-completions proxy, playground, and arena endpoints so AG-UI is sole interactive surface, while preserving configured tools for AG-UI sessions.
- Fix session-scoped AG-UI RPC routing so web prompts and cancels no longer 404.
- fix: thin client now waits for assistant reply to current NATS turn instead of returning early on transient Idle state, and returns no stale prior response on abnormal turn termination

## 0.33.1 (2026-06-23)

### Fixes

- allow file-ioctl on macOS so TUIs can enter raw mode (#897)

#### Fix interactive TUIs (claude, gemini, bash readline) failing inside `harnx-sandbox-run` on macOS.

birdcage 0.8.1's default Seatbelt profile omits `(allow file-ioctl)`, which causes `tcsetattr` to return EPERM inside the sandbox. As a result, every TUI launched via `harnx-sandbox-run` (or any consumer of `harnx-sandbox-common`) silently loses raw mode: arrow keys leak as literal `^[[A`/`^[OB`, terminal DA1 responses appear in input fields, and trust/confirmation prompts become unnavigable.

birdcage's public `Exception` API only grants path/env/network exceptions — there's no surface for adding operation-level rules like `file-ioctl`, so the macOS sandbox path is now implemented in-tree as `harnx_sandbox_common::macos_sandbox::MacSandbox`. The new profile mirrors birdcage's macOS rule generation (identical deny-then-allow ordering, identical subpath escaping) with one extra line in the default header: `(allow file-ioctl)`. Linux continues to use birdcage unchanged.

## 0.33.0 (2026-06-20)

### Breaking Changes

- derive client name from filename stem and ignore in-file name field (#824)



#### Client names are now derived from the YAML filename stem instead of a `name:` field in the file contents.

- A client defined in `clients/<name>.yaml` is named `<name>` (extension stripped, verbatim — no lowercasing). For package clients the name is `<package>/<stem>`.
- The provider-default fallback (e.g. defaulting an unnamed client to `openai`) has been removed; dynamic clients created from a `provider:model` selection are still named after their provider.

### Features

- auto-whitelist Go caches and support arbitrary $VAR expansion (#800)
- add llama-server LLM provider for local GGUF models (#817)
- support per-model GGUF configuration and HuggingFace auto-download (#821)
- view compaction result details (#828)
- auto-whitelist Homebrew prefix and fix /usr/local defaults (#831)
- externalize image attachments to content-addressed files (cid refs) (#843)
- flattened-text summarization keeping recent turns verbatim (#846)
- harnx_agent_session_history_read tool (#851)
- configurable keep-recent/truncation knobs on the compaction agent (#857)
- instrument the intermittent OOM (#842) with a memory watchdog (#864)
- add automatic session garbage collection and cleanup (#868)
- implement provider-side upload-by-reference for attachments (#871)
- grant session-history tool to compaction-enabled agents (#879)
- add info agent and info session commands to CLI and TUI (#886)
- Add `llama-server` provider for managing local llama.cpp subprocesses over Unix domain sockets.
- Add upload-by-reference attachment encoding for the Gemini and Anthropic providers. Historical image attachments stored as `cid:` references are now uploaded once to the provider Files API (Gemini File API / Anthropic Files API) and reused across turns via an in-memory cache (keyed by content id, with expiry where the provider sets one), instead of re-inlining base64 every turn. Falls back to base64 inline content when upload is unsupported or fails. OpenAI remains base64-only because the Chat Completions API cannot reference uploaded images by file id. Backends without a Files API (Vertex, Bedrock, Ollama, etc.) continue to use base64. No change to the on-disk transcript format.
- Add `harnx info` subcommand to CLI and improve `.info` in TUI to inspect fully-rendered agent configurations and session states.
- Add automatic cleanup of inactive sessions (#847). A new opt-in config key `cleanup_inactive_sessions_days` automatically deletes inactive session transcripts and their attachments after a configurable number of days. Activity is based on filesystem mtime; unset or 0 disables cleanup. Runs once at startup and hourly thereafter in all modes (TUI, CLI, serve); best-effort and fault-tolerant.

#### Auto-whitelist Go build caches and support arbitrary `$VAR` expansion in sandbox whitelist paths.

- The bash sandbox now grants read+write (but not execute) access to `GOMODCACHE` and `GOCACHE` when those environment variables are set, and forwards both to the sandboxed process. This fixes `go build`/`go test` failing with `read-only file system` when a custom cache location is configured. Caches hold source, `.a` archives, and build logs only — no executables — so execute access is intentionally withheld.
- Sandbox whitelist arguments (`--extra-read`/`--extra-write`/`--extra-exec`/`--extra-rwx`) now expand arbitrary `$VAR` references from the environment in addition to the existing pseudo-vars (`$GIT_ROOT`, etc.). A leading `$NAME` or `$NAME/...` resolves to the environment value; unset variables are left literal. Pseudo-vars still take precedence. The home-directory exposure guard continues to apply at the call sites.
- Deduplicated the Go/toolchain default-path logic so `harnx-sandbox-run` and `harnx-sandbox-common` share one implementation. As part of this, the `.exists()` gating was dropped for toolchain env-relative paths (`CARGO_HOME`/`GOROOT`/`GOPATH`/`GOBIN` and the new cache vars): they are now whitelisted unconditionally when the variable is set, since cache directories often don't exist on first run.

#### Auto-whitelist the Homebrew install prefix in the default sandbox allowlist so Homebrew-managed tools work without manual overrides.

- The Homebrew prefix is granted read+execute (never write) by default. The location is resolved dynamically: `HOMEBREW_PREFIX` is honoured when set, otherwise a compile-time platform default is used (`/opt/homebrew` on macOS, `/home/linuxbrew/.linuxbrew` on Linux). No runtime OS detection is performed.
- Fix a `/usr/local` static-path oversight: `/usr/local` is now readable on both Linux and macOS, and `/usr/local/lib` is now executable on Linux (macOS already had it) so dynamically linked binaries can load their dylibs (#818).

#### Updated `llama-server` provider to support per-model GGUF configuration and HuggingFace auto-download.

- Models in `models[]` now specify their own `model_path`, `hf_repo`, and tuning knobs (`ctx_size`, `n_gpu_layers`, `threads`, `extra_args`, `socket_path`).
- Added support for HuggingFace auto-download via the `-hf` flag in `llama-server`.
- Model source resolution precedence: `model_path` (local) -> `hf_repo` (HuggingFace) -> model `name` as the HuggingFace repo spec.
- Multi-model support: one provider config can now serve multiple models, each in its own lazily-spawned `llama-server` subprocess.

### Fixes

- install over running binaries without ETXTBSY (#798)
- scope managers to package on async agent activation (#826) (#832)
- stop leaking unfiltered tool list into agent system prompt (#863)
- preserve whitespace-only streaming chunks (#867)
- append-mode log file (#880) + heap-usage guard for the #842 OOM (#881)
- Grant the `harnx_agent_session_history_read` tool to every bundled agent configured with a `compaction_agent`, so they can search their pre-compaction session history after a compaction.
- Remove the `inputs`/`outputs` parameters from the bash MCP tools (`bash_exec`/`bash_spawn`). The sandbox no longer narrows project roots per call — roots always get read+write+exec — fixing `cargo` build failures in sub-agents (#850). Legacy calls that still pass `inputs`/`outputs` are accepted and ignored.
- Fix an infinite retry loop in the TUI when a queued message failed to send. Errored messages are now restored as editable drafts instead of being automatically replayed.
- Add diagnostic instrumentation for the intermittent out-of-memory crash (#842). The TUI event loop now runs a low-overhead memory watchdog that, once per second, logs (at `warn`) a snapshot of process RSS, transcript item count and text size, and the event-channel backlog whenever RSS crosses a doubling threshold — plus a warning when a single tick drains an abnormal number of events (a flooding producer). Compaction now logs when it starts and finishes (with duration) and flags a compaction triggered while another is still running. These surface in the harnx log file, so the next occurrence shows whether the growth is in the transcript/event path or elsewhere. Enable logging (set a non-`off` log level; `info` captures compaction detail) to collect it.
- Simplify TUI streamed-assistant-text accumulation. Streamed text now coalesces into a single transcript block per unbroken run; an interleaving item (tool call, tool result, notice, source heading) ends the run so the following text starts a fresh block below it. This replaces the previous per-line splitting and the index-based bookkeeping (`streaming_assistant_idx`) with a single open/closed flag and a "look at the trailing item" rule, removing a fragile multi-branch loop.
- Fix `cargo xtask install` failing with "Text file busy" (ETXTBSY) when a target binary is currently running. The installer now copies to a temp file and atomically renames it over the destination, matching the old `cp -f` behaviour so install works without stopping existing harnx processes.

#### Fix package agents losing their delegation tools when activated directly (#826).

When a package agent (e.g. `pantheon/atlas`) was activated through the async
`Config::use_agent` path, package managers stayed in the global scope left by `Config::init`, so every package server
was emitted with a `<package>__` prefix. Two visible symptoms resulted:

- Same-package delegation tools used `<package>__<peer>_session_prompt` instead of the bare `<peer>_session_prompt`
  its `use_tools` allow-list references, so they were filtered out and the agent
  could not delegate.
- Same-package MCP tools leaked in under both `<package>__*` and sibling-package
  namespaces (e.g. `coding__*`) instead of their bare same-package names.

The intermittency depended on which activation path ran: the synchronous
`use_agent_obj` path already scoped the managers, while the async `use_agent`
path did not. `use_agent` now mirrors `use_agent_obj` and re-scopes the managers
to the incoming agent's package before the agent's tools are snapshotted.

#### Fix two TUI rendering issues:

- Tool-use confirmation prompts (`PreToolUse` hooks returning `ask`) now render as a native ratatui modal instead of an `inquire` terminal prompt that collided with the alternate-screen TUI, producing garbled, interleaved output (#695). Answer with `y` to allow; `n`/`Esc`/`Enter` deny.
- The agent welcome banner no longer prints a dangling `v` when an agent has no `version` set — the header now reads `# agent-name` instead of `# agent-name v`.

## 0.32.5 (2026-06-10)

### Fixes

- remove side borders from transcript detail viewer (#779)
- deduplicate streamed text and repeated final message (#784)
- isolate concurrent sub-agent session prompts (#783) (#787)
- resolve package-relative agent delegation naming (#788)
- only initialize MCP servers whose tools match use_tools (#793)
- Remove the left/right borders from the transcript detail viewer so copying multi-line content no longer captures vertical `|` border characters. The top and bottom borders are retained for the title and footer separation.
- fix(mcp): only initialize MCP servers whose tools match the agent's `use_tools` selectors, so unused servers no longer connect at startup or emit spurious "failed to connect" warnings (#790)
- Fix package-relative agent resolution for delegation tools (`_session_prompt`, etc.); tool names now match the slash-free, package-relative scheme used for handoffs. Fixes #709.
- Replace the argc-based `install` task with a Rust `xtask` crate. Use `cargo xtask install` (optionally with `--debug` or a list of bin names) to build and install harnx binaries from a local checkout. The bin list is discovered automatically from cargo metadata. Fixes #792.

## 0.32.4 (2026-06-09)

### Features

- add opt-in Streamable HTTP transport for time and plans MCP servers (#706)
- improve bash exec output markdown formatting (#716)
- support project-root pseudo-vars and tilde in paths (#720)
- isolate Linux sandbox from host keyring and restore acli via proxy (#769)
- Add opt-in Streamable HTTP transport (MCP spec 2025-03-26) to the `harnx-mcp-time` and `harnx-mcp-plans` servers. Pass `--http` to serve MCP over HTTP at `/mcp` instead of stdio, with `--host` (default `0.0.0.0`) and `--port` (default `3000`) to control binding. Stdio remains the default, so existing usage is unchanged. The plans server's background cleanup loop continues to run in HTTP mode when retention is enabled.

#### Close a Linux sandbox security gap where sandboxed processes could reach the host DBus session bus and read every OS keyring secret via the Secret Service. The default exec allowlist no longer includes top-level `/run` or `/var/run` — these are replaced with a least-privilege list of specific `/run` subpaths (`systemd/resolve`, `resolvconf`, `NetworkManager`, `current-system`, `opengl-driver`, `opengl-driver-32`, `udev`), explicitly excluding `/run/user` and `/run/dbus`. The `XDG_*` environment passthrough (in both the bash MCP server and the standalone `harnx-sandbox-run` runner) is now a deny-by-default whitelist of the XDG Base Directory Specification variables (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_BIN_HOME`, `XDG_DATA_DIRS`, `XDG_CONFIG_DIRS`); `XDG_RUNTIME_DIR` and desktop-session/seat variables are no longer forwarded, so DBus clients can no longer locate the session bus.

`harnx-proxy-auth` gains generic, product-agnostic primitives:

- `--load-yaml`/`--load-json`/`--load-raw <name>=<path>` — load a file at startup and expose it as the jaq variable `$<name>` in all jaq contexts (`--hook`, `--env`, `--fs`). A missing or malformed file yields `null` rather than aborting.
- `--load-exec <name>=<command>` — run a shell command at startup (`sh -c`, inheriting the proxy's host env) and expose its captured stdout as the jaq string variable `$<name>`. Any failure (non-zero exit, empty output, missing tool) yields `null`. The captured value is treated as a secret and never logged.
- `--fs <jaq>` — a transformer (like `--env`) whose output object maps paths (relative to `$temp_file_root`) to string file contents, written into a private `0700` temp dir the proxy creates and cleans up on exit/SIGTERM/SIGINT. Path traversal is rejected.
- `$temp_file_root` — a jaq binding holding the path of that temp dir (empty when no `--fs` is present).

The `coding` and `pantheon` packages' bash MCP servers use these to restore Atlassian CLI (`acli`) functionality inside the sandbox without keyring access: the proxy self-sources the real API token from the host OS keyring (`secret-tool` on Linux, `security` on macOS) via `--load-exec`, selects the active profile (the one whose `cloud_id:account_id` matches `current_profile`), synthesizes a private `jira_config.yaml` containing that profile's real `cloud_id`/`account_id` but a fixed sentinel token, points `acli` at it via `ACLI_CONFIG_DIR`, and the MITM proxy replaces the `Authorization` header with the real credentials before forwarding — only for requests to `api.atlassian.com` or the active profile's exact site, so credentials are never forwarded to unrelated `*.atlassian.net` tenants. This works from just `acli jira auth login` on the host — no `ATLASSIAN_API_TOKEN`/`ATLASSIAN_EMAIL` environment variables and no manual keyring extraction are required. `--extra-read ~/.config/acli` is no longer needed. The injection no-ops cleanly when the user is not logged in (the keyring lookup yields `null`).

#### Harden the sandbox default whitelist: directories on `PATH` or holding host-executed binaries (`~/.nvm`, `~/.cargo/bin`, `~/.pyenv`, `~/.rye`, `~/.mono`, `~/.local/share/{claude,opencode,pipx}`) are now **read+execute only**, and package-manager caches (`~/.npm`, `~/.yarn`, `~/.cargo/registry`, `~/.cargo/git`, `~/.bun/install/cache`, `~/.local/share/{pnpm,uv}`) are **read+write only**. No `$HOME` directory is granted write+execute by default. This closes a sandbox-escape vector where a compromised sandboxed process could plant a malicious executable in a writable directory that the user later runs on the host.

Privileged operations that install or self-update executables (`cargo install`, `nvm install`, `pyenv install`, `rye sync`, `pipx install`, `claude update`, `opencode` self-update) now require explicit write access — pass `--extra-rwx <path>` (or set `HARNX_BASH_EXTRA_RWX` for the bash MCP server), or run them outside the sandbox.

A custom `CARGO_HOME` now also receives the same defaults as the standard `~/.cargo`: read access to its root (for `config.toml`/credentials), read+exec for `bin`, and read+write for the `registry` and `git` download caches.

#### This release introduces project-root pseudo-variables for sandbox path configuration in `harnx-sandbox-run` and `harnx-mcp-bash`. You can now use `$GIT_ROOT`, `$GIT_COMMON_DIR`, `$NODE_PROJECT_ROOT`, `$CARGO_ROOT`, and `$GO_ROOT` in both CLI flags (`--extra-*`) and environment variables (`HARNX_BASH_EXTRA_*`).

These variables are resolved at startup against the current working directory. If you are not inside a matching project (e.g., you use `$GIT_ROOT` while not in a git repository), the path is silently skipped. For security, any path that resolves to your home directory or an ancestor of it is also dropped.

Additionally, `harnx-mcp-bash` now correctly applies the home-directory guard to all extra paths provided via flags or environment variables, matching the security behavior of `harnx-sandbox-run`.

### Fixes

- harden default whitelist with least-privilege split (#735)
- make cleanup test deterministic on Windows (#760)
- show session compaction in transcript instead of stdout spinner (#778)
- Show session compaction in the transcript instead of a stdout spinner. Previously, triggering compaction (the `.compact session` command or automatic compaction) drew a spinner directly to stdout, which corrupted the TUI input area and left an uncleared line. Compaction now emits `CompactingStarted` / `CompactingCompleted` / `CompactingFailed` session events that the TUI renders as transcript entries and the CLI renders via a managed spinner. Manual compaction also guards against running concurrently with automatic compaction.
- Fix a Windows CI flake in the `harnx-mcp-plans` cleanup test. `cleanup_deletes_stale_plan_but_keeps_fresh_plan` relied on millisecond-scale sleeps finer than Windows filesystem timestamp resolution, causing the fresh plan to be deleted too. The test now sets the stale plan's mtime explicitly via `filetime` and uses a generous retention margin, making it deterministic across platforms.
- Improve bash exec output formatting for markdown rendering in kagent (#713).
- Rewrite the "AI Agent Wrapper Scripts" section of `docs/sandbox-run.md` to use a PATH-prepended shim directory (`${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin`). The shims are named after the real commands (`claude`, `gemini`, `node`/`yarn`/`npm`/`npx`/`pnpm`), each stripping its own directory from `PATH` before exec'ing the real tool inside a tailored birdcage sandbox, using the project-root pseudo-variables. Replaces the old `claude-sb`/`gemini-sb` recipe (#575).
- Update the `ratatui` crate to 0.30.1.

#### Fix agent handoff from a package agent resolving to the wrong agent. When a package agent (e.g. `pantheon/daedalus`) handed off a session to a same-package agent via a bare `_session_handoff` tool (e.g. `atlas_session_handoff`), the handoff incorrectly targeted the top-level `atlas` instead of `pantheon/atlas`. Handoff targets are now resolved relative to the active agent's package.

Handoff tool names are also now generated with package-namespaced, schema-valid spelling instead of containing a raw `/` (which is rejected by provider function-name schemas): same-package peers use the bare name (`atlas_session_handoff`), cross-package peers use `pkg__agent_session_handoff`, and top-level agents addressed from within a package use `__agent_session_handoff`. The engine decodes these via an exact lookup table so package and agent names containing underscores remain unambiguous.

## 0.32.3 (2026-05-29)

### Features

- add --env sentinel env vars for bash tool calls (#672)
- automate models.yaml updates via LiteLLM registry (#678)

### Fixes

- rename .changesets to .changeset so knope consumes them (#688)
- don't request roots from clients lacking the capability (#692)
- ingest LiteLLM bare-keyed first-party models (#696)
- MCP servers (`harnx-mcp-fs`, `harnx-mcp-bash`) no longer send `roots/list` requests to clients that did not advertise the `roots` capability (#690). Such clients can't answer the request, so the servers now keep their CLI-provided roots instead.

## 0.32.2 (2026-05-27)

### Features

- add automatic background cleanup of inactive plans (#656)
- add shebang support for non-bash interpreters (#659)

### Fixes

- propagate errors for invalid jq expressions in package patches (#651)
- remove duplicate gemini-3.1-flash-lite entries in provider catalogs (#663)

## 0.32.1 (2026-05-25)

### Features

- add env parameter to bash_exec and bash_spawn (#534)
- add hook mutation support for tool calls (#537)
- add scripted GIF rendering for TUI and web UIs (#541)
- support pulling packages from private OCI registries (#543)
- add AWS credential chain support to Bedrock provider (#545)
- support show_timestamps and show_sequence_numbers in config.yaml (#555)
- add pantheon and coding example agent packages (#547)
- add harnx-aws-creds for AWS container credential injection (#560)
- add harnx-proxy-auth persistent hook for GitHub authentication (#567)
- Add hooks configuration to MCP server configs (#578)
- support client patching from packages (#583)
- add Kubernetes credentials gateway hook (#598)
- add stdio MCP proxy with tool hook interception (#607)
- add MCP server mode to harnx-aws-creds and harnx-proxy-auth (#617)
- add gemini-3.5-flash and gemini-3.1-flash-lite to model registry (#645)

### Fixes

- grant write access to /dev/shm on Linux for Chrome/Puppeteer (#529)
- change bash_spawn display from `> command` to `$ command &` (#552)
- include package agents in picker and add harnx-pkg binary (#574)
- CodeRabbit auto-fixes for PR #574 (#576)
- preserve package-scoped qualifier when loading agents from CLI (#584)
- strip package namespace prefix from env var lookup (#614)
- prevent $HOME exposure via write-path ancestor walk and over-broad roots (#619, #503) (#620)
- .info session no longer dumps session transcript (#627)
- use_tools whitelist bypassed in select_tools (#624) (#631)
- proxy auth hook misses host for tunnelled HTTPS requests (#629)
- list individual tools instead of toolset aliases and wildcards in package agents (#638)
- suppress noisy sandbox-run log for non-existent paths (#643)
- extract knope sync command to a shell script (#650)

## 0.32.0 (2026-05-13)

### Breaking Changes

- remove all default session related features and configurations (#477)

### Features

- markdown table rendering with widget architecture and render cache (#351) (#486)
- add package management system and runtime integration (#490)
- add word wrap and multi-line row support to markdown tables (#495)
- surface hook-blocked tool calls in TUI transcript (#356) (#496)
- open agent/session picker for bare .agent/.session commands (#508)
- add insert and re_replace tools (#511) (#514)
- add surgical editing and unified diff output (#516)

### Fixes

- track I/O task JoinHandle to detect unexpected subprocess exit (#71) (#488)
- sort session picker by last updated time instead of created time (#504)
- scroll history into view on first navigation press (#509)
- resolve duplicate exit_session call preventing resume hint (#522)

## 0.31.0 (2026-05-07)

### Breaking Changes

- remove all default session related features and configurations (#477)

### Features

- markdown table rendering with widget architecture and render cache (#351) (#486)

## 0.30.1 (2026-05-06)

### Features

- built-in MCP servers, roots support, REPL tool management (#31)
- support Anthropic OAuth tokens with Bearer auth for Claude client (#89)
- file-sourced agent variables, prompt cleanup, and comprehensive docs (#96)
- add `harnx-mcp-todo` as a file-based todo management MCP server (#112)
- add `exa` and `wet` MCP servers for enhanced web search and ext… (#118)
- add support for plan association and management in `harnx-mcp-todo` (#132)
- session name templates, session resilience, and REPL improvements (#134)
- improve tool result display formatting with audience-based filtering (#142)
- add idle-aware timeout, propagate all session updates, improve truncation hints (#146)
- display token usage after LLM responses and in spinner (#157)
- show agent name and session ID in spinner, usage line, and initial prompt (#164)
- replace use_tools matching with glob patterns (#165)
- show sub-agent activity, tool calls, and token usage in REPL (#166)
- support Gemini 2.0 thought signatures and thinking blocks (#172)
- add key, dependencies fields and plan_get_todo tool (#174)
- append-only session logging with always-save default (#225)
- model fallback and retry with exponential backoff (#261)
- submit pending message after tool round (#267)
- replace convert_time with flexible timestamp conversion utility (#279)
- persist compaction_agent in session logs for customizable context compression (#288)
- update built-in models list with new models and remove deprecated ones (#284)
- honour server Retry-After hint in 429 responses (#329)
- MiniJinja templating for system prompts (#344)
- MiniJinja templating for MCP tool call/result display (closes #340) (#349)
- add git-backed local history and rollback (#353)
- restrict and curate child process environment (#381)
- improve sandboxing and tilde expansion (#376) (#377) (#379) (#388)
- markdown rendering for tool templates and assistant messages (#389)
- indicate result truncation in tool summaries (#338)
- unify exec/spawn/wait/terminate response metadata fields (#401)
- render history diffs as syntax-highlighted markdown (#402)
- expand default sandbox paths for common toolchains (#405)
- implement parallel tool call dispatch via two-phase model (#411)
- implement Session History Editing Phase 2 (#428)
- improve tool call display and output appearance (#429)
- transcript detail view and navigation fixes (#442)
- improve session management and enforce mandatory selection (#456)
- add transcript navigation UX and fullscreen browsing mode (#460)
- replace harnx-mcp-todo with harnx-mcp-plans (#462)
- replace browsing mode detail panel with full transcript view (#465)
- print session resume instructions on exit (#417) (#464)

### Fixes

- Remove unused dead code (#3)
- update in-memory agent name after save (#62)
- improve unresolved patch variable error message with actionable guidance (#88)
- update rust crate inquire to 0.9.0 (#111)
- update rust crate fancy-regex to 0.17.0 (#109)
- update rust crate schemars to 0.9 (#121)
- update rust crate scraper to 0.26.0 (#124)
- set stdin to null to prevent command hangs (#158)
- improve streaming output chunk boundaries (#191)
- correct transcript scrolling when content is wrapped (#202)
- update rust crate ratatui-textarea to 0.9.0 (#209)
- persist exec output logs for recovery (#218)
- isolate spawned bash processes in groups/jobs (#219)
- enable word wrap on input textarea (#223)
- update rust crate reqwest to 0.13.0 (#114)
- update rust crate bincode to v3 (#127)
- persist sub-agent sessions to disk (#235)
- stabilize status output and attachment transcript (#239)
- sync shell completions with current CLI (#244)
- support batch embeddings (#241)
- update rust crate reedline to 0.47.0 (#113)
- add support for flattening `nullable` schemas in JSON conversion (#254)
- preserve semantic transcript events and surface nested delegated tool calls (#253)
- fix shift key and key repeat in kitty terminal (#251) (#257)
- fix session handoff output ordering in transcript (#262)
- eliminate sub-agent activity duplication and rendering artifacts (#270)
- preserve model_fallbacks in sessions (#273)
- show attachments in transcript on direct submit and dot-command (#237) (#275)
- Up/Down arrow keys navigate history only when input is blank or in preview mode (#281) (#304)
- persist tool calls and results as separate log entries (#308)
- persist sessions to agent-scoped directory (#323)
- fix scroll dead-zone and tall-item content clipping (#264) (#325)
- anchor front-matter regex and propagate YAML parse errors (#56) (#327)
- echo thinking block + signature on multi-turn tool calls (#328) (#330)
- correct lines_above for partial-bottom tall items so scroll moves the right way (#333)
- write response_text before forwarding to close prompt-task race (#341)
- fix input token count missing cache_creation_input_tokens (#337)
- populate shared_variables before session render and report undefined variable (#346)
- echo thinking blocks on streaming multi-turn tool calls (#347) (#348)
- resolve flakiness in nested sub-agent activity test (#370)
- drain forwarder before teardown to stop dropping nested events (#378)
- propagate session/cancel to nested sub-agents on Ctrl-C (#358) (#382)
- drop empty-key dotfile lines and extend allowlist for Windows (#383)
- render MCP MiniJinja templates in TUI and CLI tool events (#386)
- surface MCP startup errors with stderr context in TUI (#394)
- show history diffs in tool output (#398) (#399)
- guard against divide-by-zero in need_rows_local when columns=0 (#409)
- don't treat 4+-space-indented backticks as code fences (#408)
- handle CTRL-C and CTRL-D in non-TUI terminal mode (#413)
- CodeRabbit auto-fixes for PR #405 (#412)
- agent/session picker — filter, correct sessions, ESC new session (#439)
- lazy-discover tracked repos so edit diffs survive empty initial_roots (#443)
- use triple-tick code fences for bash tool call display (#445)
- suppress blank diff blocks when no files changed (#444) (#447)
- use short IDs for sessions created via use_session(None) (#459)
- ESC, Ctrl+C, and Ctrl+D now exit pickers (#467) (#468)
- correctly serialize tool-call rounds in session rewrite (#471)
