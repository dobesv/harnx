# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- add GitHub auth proxy hook (`harnx-proxy-auth`): persistent hook binary that acts as an HTTPS MITM proxy, injecting configurable auth headers for matching URLs into `bash_exec`/`bash_spawn` tool environments (closes #531)

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
