# Changelog

All notable changes to the `coding` agent package will be documented here.
## 0.4.6 (2026-09-25)

### Fixes

- format tool calls in session dumps (#2100)
- install rustls crypto provider before startup TLS (#2105)

## 0.4.5 (2026-09-25)

### Features

- add native harnx-exa-tools server for Exa web search (#2079)

## 0.4.4 (2026-09-24)

### Fixes

- speed up macOS binary builds (#2081)

## 0.4.3 (2026-09-24)

### Fixes

- limit Linux ARM build resource use (#2077)

## 0.4.2 (2026-09-23)

### Fixes

- restore Windows x86 and Web UI assets (#2073)

## 0.4.1 (2026-09-23)

### Fixes

- support Agent Sandbox v1 APIs (#2067)

## 0.4.0 (2026-09-22)

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
- Prevent test environment races from overwriting user configuration, preserve embedded model metadata alongside custom client models, and validate shipped agent model references. Model-list APIs now evaluate the current client configuration on every call and return owned values. Add the missing Gemini client used by the coding package's compaction agent.
- Replace filesystem roots and per-tool extra path flags with shared explicit allow paths and opt-in batches. Existing tool-server YAML and sandbox-run invocations must migrate to the new flags and environment variables.

#### Convert the pantheon and coding `bash.yaml` proxy-auth hooks to the command-only

config model. Hook entries now specify only `command` (plus optional
`status_message` and `async`); the native `harnx-proxy-auth` hook self-declares
its event and matcher, so those fields are no longer set in the package config.

## 0.3.4 (2026-07-23)

### Fixes

- include failing expression and input kind in runtime error logs (#1088)
- forward model errors via harnx:error meta instead of plain text (#964) (#1128)

## 0.3.3 (2026-07-21)

### Features

- bring web UI to TUI parity with GFM markdown and collapsible tool cards (#1031)
- fix and harden hook-based auth injection (Jira/GitHub) + startup handshake (#1050)
- render system prompt at request time with tool and model awareness (#1055)
- add automatic LLM-driven session title generation (#1069)
- add cross-process file locking for local sessions (#1077)
- implement agent handoff for Web UI and NATS worker (#1109)
- serialize concurrent file mutations to prevent corruption (#1122)

### Fixes

- only prefetch selector-matching MCP servers at agent init (#1029) (#1030)
- overhaul transcript history navigation seq mapping (#1032)
- emit streamed notifications in order, not per-chunk tasks (#1038)
- sandboxed acli auth — write synthetic token as YAML !!binary (#1052)
- update dependency @assistant-ui/react to v0.14.27 (#1105)
- update dependency @assistant-ui/react-ag-ui to v0.0.45 (#1106)
- update dependency @assistant-ui/react-markdown to v0.14.6 (#1107)

## 0.3.2 (2026-07-09)

### Features

- add unified error handling for streaming events across LLMs (#908)
- add static remote agent catalog to cluster configuration (#929)
- achieve remote agent tool parity and fix thin-client N… (#930)
- implement remote session enumeration protocol (#938)
- wire remote control surface for agent@cluster (#915) (#956)
- add opt-in background GC for remote sessions (#960)
- implement AG-UI protocol server support (#966)
- add AG-UI follow-up features and fixes (#1005)

### Fixes

- prevent infinite loops and panics in scrolling widget rendering (#907)
- use leader-authoritative read for mid-turn injection decision points (#917) (#928)
- resize height cache instead of underflowing when items shrink (#952)
- don't restore the panic hook while unwinding (#954)
- surface mid-stream streaming LLM errors instead of stopping silently (#963)
- keep transcript visible after compaction (#904) (#967)
- forward EXA_API_KEY through the sandbox for the exa MCP server (#973)
- Fix the Exa MCP server so web search works when `npx`/`node` is wrapped by a harnx sandbox. The configs previously set `EXA_API_KEY: "$EXA_API_KEY"`, but harnx does not expand `$VAR` in MCP `env:` values and the sandbox scrubs the child environment — so the server received no usable key and returned `API key must be provided`. They now use `HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY`, which `harnx-sandbox-run` honors to forward the real host value. Also documents both footguns (literal `env:` values; sandbox env stripping) in the configuration guide, environment-variables, sandbox-run, and FAQ docs.

## 0.3.1 (2026-06-23)

### Fixes

- allow file-ioctl on macOS so TUIs can enter raw mode (#897)

## 0.3.0 (2026-06-20)

### Breaking Changes

- derive client name from filename stem and ignore in-file name field (#824)

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

### Fixes

- install over running binaries without ETXTBSY (#798)
- scope managers to package on async agent activation (#826) (#832)
- stop leaking unfiltered tool list into agent system prompt (#863)
- preserve whitespace-only streaming chunks (#867)
- append-mode log file (#880) + heap-usage guard for the #842 OOM (#881)
- Grant the `harnx_agent_session_history_read` tool to every bundled agent configured with a `compaction_agent`, so they can search their pre-compaction session history after a compaction.

## 0.2.4 (2026-06-10)

### Fixes

- remove side borders from transcript detail viewer (#779)
- deduplicate streamed text and repeated final message (#784)
- isolate concurrent sub-agent session prompts (#783) (#787)
- resolve package-relative agent delegation naming (#788)
- only initialize MCP servers whose tools match use_tools (#793)

## 0.2.3 (2026-06-09)

### Features

- add opt-in Streamable HTTP transport for time and plans MCP servers (#706)
- improve bash exec output markdown formatting (#716)
- support project-root pseudo-vars and tilde in paths (#720)
- isolate Linux sandbox from host keyring and restore acli via proxy (#769)

#### Close a Linux sandbox security gap where sandboxed processes could reach the host DBus session bus and read every OS keyring secret via the Secret Service. The default exec allowlist no longer includes top-level `/run` or `/var/run` — these are replaced with a least-privilege list of specific `/run` subpaths (`systemd/resolve`, `resolvconf`, `NetworkManager`, `current-system`, `opengl-driver`, `opengl-driver-32`, `udev`), explicitly excluding `/run/user` and `/run/dbus`. The `XDG_*` environment passthrough (in both the bash MCP server and the standalone `harnx-sandbox-run` runner) is now a deny-by-default whitelist of the XDG Base Directory Specification variables (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_BIN_HOME`, `XDG_DATA_DIRS`, `XDG_CONFIG_DIRS`); `XDG_RUNTIME_DIR` and desktop-session/seat variables are no longer forwarded, so DBus clients can no longer locate the session bus.

`harnx-proxy-auth` gains generic, product-agnostic primitives:

- `--load-yaml`/`--load-json`/`--load-raw <name>=<path>` — load a file at startup and expose it as the jaq variable `$<name>` in all jaq contexts (`--hook`, `--env`, `--fs`). A missing or malformed file yields `null` rather than aborting.
- `--load-exec <name>=<command>` — run a shell command at startup (`sh -c`, inheriting the proxy's host env) and expose its captured stdout as the jaq string variable `$<name>`. Any failure (non-zero exit, empty output, missing tool) yields `null`. The captured value is treated as a secret and never logged.
- `--fs <jaq>` — a transformer (like `--env`) whose output object maps paths (relative to `$temp_file_root`) to string file contents, written into a private `0700` temp dir the proxy creates and cleans up on exit/SIGTERM/SIGINT. Path traversal is rejected.
- `$temp_file_root` — a jaq binding holding the path of that temp dir (empty when no `--fs` is present).

The `coding` and `pantheon` packages' bash MCP servers use these to restore Atlassian CLI (`acli`) functionality inside the sandbox without keyring access: the proxy self-sources the real API token from the host OS keyring (`secret-tool` on Linux, `security` on macOS) via `--load-exec`, selects the active profile (the one whose `cloud_id:account_id` matches `current_profile`), synthesizes a private `jira_config.yaml` containing that profile's real `cloud_id`/`account_id` but a fixed sentinel token, points `acli` at it via `ACLI_CONFIG_DIR`, and the MITM proxy replaces the `Authorization` header with the real credentials before forwarding — only for requests to `api.atlassian.com` or the active profile's exact site, so credentials are never forwarded to unrelated `*.atlassian.net` tenants. This works from just `acli jira auth login` on the host — no `ATLASSIAN_API_TOKEN`/`ATLASSIAN_EMAIL` environment variables and no manual keyring extraction are required. `--extra-read ~/.config/acli` is no longer needed. The injection no-ops cleanly when the user is not logged in (the keyring lookup yields `null`).

### Fixes

- harden default whitelist with least-privilege split (#735)
- make cleanup test deterministic on Windows (#760)
- show session compaction in transcript instead of stdout spinner (#778)

## 0.2.2 (2026-05-29)

### Features

- add --env sentinel env vars for bash tool calls (#672)
- automate models.yaml updates via LiteLLM registry (#678)

### Fixes

- rename .changesets to .changeset so knope consumes them (#688)
- don't request roots from clients lacking the capability (#692)
- ingest LiteLLM bare-keyed first-party models (#696)

## 0.2.1 (2026-05-27)

### Features

- add automatic background cleanup of inactive plans (#656)
- add shebang support for non-bash interpreters (#659)

### Fixes

- propagate errors for invalid jq expressions in package patches (#651)
- remove duplicate gemini-3.1-flash-lite entries in provider catalogs (#663)

## 0.2.0 (2026-05-25)

### Breaking Changes

- remove all default session related features and configurations (#477)

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
- markdown table rendering with widget architecture and render cache (#351) (#486)
- add package management system and runtime integration (#490)
- add word wrap and multi-line row support to markdown tables (#495)
- surface hook-blocked tool calls in TUI transcript (#356) (#496)
- open agent/session picker for bare .agent/.session commands (#508)
- add insert and re_replace tools (#511) (#514)
- add surgical editing and unified diff output (#516)
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
- track I/O task JoinHandle to detect unexpected subprocess exit (#71) (#488)
- sort session picker by last updated time instead of created time (#504)
- scroll history into view on first navigation press (#509)
- resolve duplicate exit_session call preventing resume hint (#522)
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
