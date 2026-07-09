# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- add GitHub auth proxy hook (`harnx-proxy-auth`): persistent hook binary that acts as an HTTPS MITM proxy, injecting configurable auth headers for matching URLs into `bash_exec`/`bash_spawn` tool environments (closes #531)

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
- Surface MCP server transport failures and subprocess churn as user-visible notices, including reconnect warnings and child stderr/closure signals, so ACP/TUI users can see MCP restarts and deaths without digging through logs. Fixes #990.
- Add an optional `agents:` list to NATS cluster config (`nats_servers/<cluster>.yaml`). Declared remote agents appear as `name@cluster` in `--list-agents`/shell completion, and assistant-role entries also appear in the interactive picker. Static config only — no network calls.
- Add server-side multipart attachment uploads with `cid:` reference prompt plumbing for AG-UI sessions.
- harnx-serve and the AG-UI web client now support composing and injecting pending messages while an agent run is active. The server queues prompts sent via `session/prompt` (returning `Enqueued`) and consumes them on the next tool round, emitting a `pending_message_consumed` `CUSTOM_EVENT`. The web client uses this event to clear its queued message UI indicator reliably.
- Adds opt-in background GC for remote sessions stored in NATS KV. Enable via `cleanup_remote_sessions_days` config field or `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS` environment variable. When set, runs hourly to purge stale session index entries across all configured NATS clusters.
- The web client now allows uploading attachments using `assistant-ui`'s native attachment UI. Images and files are transparently uploaded and their CID references are piped through the JSON-RPC `session/prompt` mechanism to the server.
- Surface previously-silent errors in the AG-UI web UI: agents-list and sessions-list fetch failures (#983) now show inline error text, and message-send failures (#987) show an inline composer-area error.

#### Fix #988: ACP server now creates per-session context once instead of forking config per-prompt, preventing MCP subprocess respawn on every turn.

**Problem:** The ACP server re-derived config per prompt via `fork_prompt_config` → `use_agent_by_name` → `reinit_managers_for_agent`, which unconditionally rebuilt McpManager. This killed and respawned all MCP subprocesses (bash, fs, time, plans) on every prompt, breaking `read_exec_log` and degrading performance.

**Solution:** Introduced `SessionContext` that holds a forked `GlobalConfig` (with its own `McpManager`/`AcpManager`) set up once at session creation. The `HarnxAgent.sessions` map now stores `Arc<SessionContext>` instead of bare concurrency primitives.

- `new_session`: Fork config once, call `use_agent_by_name` + `use_session`, store `Arc<SessionContext>`.
- `prompt`: Look up `Arc<SessionContext>`, reuse its stored config (no per-prompt fork).
- `cancel`: Works identically via `SessionContext.abort_signal`/`cancel_notify`.
- Lazy resume: Sessions on disk but absent from memory are rebuilt on-demand via `get_or_build_session`.
- Idle reaper: Sessions idle > 15 minutes are evicted (reusing `session_actor.rs` pattern), reaping only when not holding the `prompt_lock`.

This mirrors the NATS worker's correct per-session config pattern from `daemon.rs::execute_session`.

#### High-availability distributed agent execution backed by NATS JetStream.

This feature enables a distributed mode where thin clients (TUI, CLI, ACP) communicate with backend workers over NATS. Key capabilities include:
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
- Thread explicit per-session working directories through harnx runtime so in-process runs keep tool execution, hooks, and persisted session metadata bound to each session's own working directory while preserving CLI and ACP fallback to process cwd when no per-session directory is set.
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

- The --acp <agent> flag has been removed from the harnx
binary. Use the standalone harnx-acp-server <agent> binary instead.
- derive client name from filename stem and ignore in-file name field (#824)

#### Move ACP support out of the `harnx` binary and fix same-package pantheon delegation.

- **Breaking:** The `harnx --acp <agent>` flag has been removed (no backward compatibility). Serving an agent over ACP is now done with the standalone `harnx-acp-server <agent>` binary. Internally generated ACP subagent servers are auto-registered to spawn `harnx-acp-server <agent>` instead of `harnx --acp <agent>`; the server binary is resolved as a sibling of the running `harnx` executable, falling back to `harnx-acp-server` on `PATH` (#550).
- **Fix:** Same-package pantheon delegation no longer drops the package prefix on spawn. Auto-registered ACP servers now keep the package-qualified spawn target (e.g. `pantheon/aristarchus`) in their `args`, independent of the per-agent display-name rewrite that exposes same-package peers under their bare name. Package-loaded `acp_servers/*.yaml` whose `args` reference the bare server stem are normalized to the qualified `<package>/<name>` target. This fixes delegation between two agents in the same package once same-named top-level agents are removed (#804).

#### Client names are now derived from the YAML filename stem instead of a `name:` field in the file contents.

- A client defined in `clients/<name>.yaml` is named `<name>` (extension stripped, verbatim — no lowercasing). For package clients the name is `<package>/<stem>`.
- A `name:` field inside a client spec is now ignored (silently skipped); the filename is the sole source of the client name. There is no migration — rename the file if you relied on a differing `name:` field. This mirrors how MCP/ACP server specs are already named (#823).
- The provider-default fallback (e.g. defaulting an unnamed client to `openai`) has been removed; dynamic clients created from a `provider:model` selection are still named after their provider.

### Features

- auto-whitelist Go caches and support arbitrary $VAR expansion (#800)
- extract ACP server and fix same-package delegation (#810)
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
- Add a heap-usage guard to catch the intermittent runaway-allocation OOM (#842). The `harnx` and `harnx-acp-server` binaries install a global allocator that tracks live heap and, the instant it crosses a ceiling, captures a backtrace (whose top frames are the runaway allocation site), writes it to stderr and the log file, then aborts — before the process exhausts the machine and is OOM-killed with no stack. It is armed by default at 4096 MiB; set `HARNX_HEAP_LIMIT_MB` to change the ceiling, or `HARNX_HEAP_LIMIT_MB=0` to disable it. When disarmed it is a passthrough to the system allocator.
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
- add LLM read timeout so stalled requests fail clearly instead of false ACP idle timeout (#874) (#887)
- Fix false ACP idle timeout in session_prompt by resetting the timer on any subprocess liveness.
- Grant the `harnx_agent_session_history_read` tool to every bundled agent configured with a `compaction_agent`, so they can search their pre-compaction session history after a compaction.
- Fix the log file filling with NUL bytes (#880). harnx and the `harnx-acp-server` sub-agents it spawns share one `HARNX_LOG_PATH`; opening it with `File::create` gave each process an independent, truncating file offset, so concurrent writers clobbered each other and the kernel zero-filled the gaps. The log is now opened in append mode (`O_APPEND`), so every write lands atomically at end-of-file and concurrent processes interleave cleanly at line granularity. The file is no longer truncated per run. Each process also logs a `harnx start: v… build=<git sha> pid=… level=… log=…` line at startup so a shared log can be attributed per PID and the running build is verifiable.
- Add a configurable read (inactivity) timeout for LLM provider HTTP requests so stalled responses fail with a clear error instead of hanging until the ACP idle timeout; raise the default ACP idle timeout to 600s as a backstop.
- Remove the `inputs`/`outputs` parameters from the bash MCP tools (`bash_exec`/`bash_spawn`). The sandbox no longer narrows project roots per call — roots always get read+write+exec — fixing `cargo` build failures in sub-agents (#850). Legacy calls that still pass `inputs`/`outputs` are accepted and ignored.
- Clarify generated tool descriptions for session delegation and agent handoff so agents understand session context semantics (#837). The `session_prompt` ACP tool now states that continuing a prior conversation requires passing the `session_id` returned by an earlier prompt call, and that omitting it starts a new empty session with no prior context. The `handoff` tool description is corrected to reflect that the target agent always starts fresh — prior conversation history is intentionally cleared, and only the `prompt` argument carries context. Documentation-string changes only; no behavior changes.
- Fix an infinite retry loop in the TUI when a queued message failed to send. Errored messages are now restored as editable drafts instead of being automatically replayed.
- Add diagnostic instrumentation for the intermittent out-of-memory crash (#842). The TUI event loop now runs a low-overhead memory watchdog that, once per second, logs (at `warn`) a snapshot of process RSS, transcript item count and text size, and the event-channel backlog whenever RSS crosses a doubling threshold — plus a warning when a single tick drains an abnormal number of events (a flooding producer). Compaction now logs when it starts and finishes (with duration) and flags a compaction triggered while another is still running. These surface in the harnx log file, so the next occurrence shows whether the growth is in the transcript/event path or elsewhere. Enable logging (set a non-`off` log level; `info` captures compaction detail) to collect it.
- Simplify TUI streamed-assistant-text accumulation. Streamed text now coalesces into a single transcript block per unbroken run; an interleaving item (tool call, tool result, notice, source heading) ends the run so the following text starts a fresh block below it. This replaces the previous per-line splitting and the index-based bookkeeping (`streaming_assistant_idx`) with a single open/closed flag and a "look at the trailing item" rule, removing a fragile multi-branch loop.
- Fix `cargo xtask install` failing with "Text file busy" (ETXTBSY) when a target binary is currently running. The installer now copies to a temp file and atomically renames it over the destination, matching the old `cp -f` behaviour so install works without stopping existing harnx processes.

#### Fix package agents losing their delegation tools when activated directly (#826).

When a package agent (e.g. `pantheon/atlas`) was activated through the async
`Config::use_agent` path — used by `--agent`, handoff `switch_agent`, and the
ACP server — the MCP/ACP managers were never re-scoped to the agent's package.
They stayed in the global scope left by `Config::init`, so every package server
was emitted with a `<package>__` prefix. Two visible symptoms resulted:

- The agent's own same-package ACP delegation tools were emitted as
  `<package>__<peer>_session_prompt` instead of the bare `<peer>_session_prompt`
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
- fix(acp): isolate concurrent sub-agent session prompts so they no longer clobber each other's active session or event sink (#783)
- Remove the left/right borders from the transcript detail viewer so copying multi-line content no longer captures vertical `|` border characters. The top and bottom borders are retained for the title and footer separation.
- Fix duplicated assistant text where streamed message chunks were followed by a repeated full final message (TUI streaming + ACP server hardening). Fixes #671.
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
- Fix README install instructions to list all installable binaries instead of only three, and link each binary to its documentation. Added per-crate READMEs for `harnx-serve`, `harnx-acp-server`, `harnx-mcp-fs`, `harnx-mcp-time`, `harnx-mcp-hooks-proxy`, `harnx-proxy-auth`, and `harnx-sandbox-exec` (in `harnx-sandbox-common`). Also wired the previously-omitted `harnx-k8s-creds` binary into the release workflow, Docker image, and `argc install` so it ships and installs alongside `harnx-aws-creds`, and added the missing `harnx-mcp-hooks-proxy` binary to the Docker image.
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
- add ACP (Agent Client Protocol) support (#67)
- support Anthropic OAuth tokens with Bearer auth for Claude client (#89)
- wire --acp server to real agent execution (#87)
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
- split clients, mcp_servers, and acp_servers into separate subdirectory files (#260)
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
- resolve ACP server hang and MCP transport death on Ctrl+C (#104)
- update rust crate inquire to 0.9.0 (#111)
- update rust crate fancy-regex to 0.17.0 (#109)
- update rust crate schemars to 0.9 (#121)
- update rust crate scraper to 0.26.0 (#124)
- set stdin to null to prevent command hangs (#158)
- cancel ACP sub-agent session on Ctrl+C to stop stale output (#163)
- improve streaming output chunk boundaries (#191)
- correct transcript scrolling when content is wrapped (#202)
- update rust crate ratatui-textarea to 0.9.0 (#209)
- persist exec output logs for recovery (#218)
- isolate spawned bash processes in groups/jobs (#219)
- enable word wrap on input textarea (#223)
- show full error chain in ACP delegation and tool error messages (#228)
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
- resolve file-backed agent variables before ACP session start (#331)
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
