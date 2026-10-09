# AGENTS.md — Harnx

## Project Overview

Harnx is a modular command-line LLM agent harness written in **Rust**. It lets users build custom agents from the ground up with full control over prompts, tools, models, and sub-agents. It integrates with 20+ LLM providers (OpenAI, Claude, Gemini, Ollama, Bedrock, etc.) and supports MCP (Model Context Protocol) servers.

## Technology Stack

- **Language:** Rust (edition 2021, toolchain pinned in `rust-toolchain.toml` — rustup and CI both read this file automatically)
- **Async runtime:** Tokio (multi-threaded)
- **Test Runner:** Nextest
- **HTTP client:** reqwest (rustls-tls)
- **CLI framework:** clap (derive)
- **Serialization:** serde + serde_json + serde_yaml
- **TUI:** ratatui + crossterm
- **RAG:** hnsw_rs + bm25
- **MCP SDK:** rmcp
- **CI:** GitHub Actions (see `.github/workflows/ci.yaml`)
- **Release tooling:** [knope](https://knope.tech) (see `knope.toml`)
- **Dependency management:** Renovate (see `renovate.json`)

The workspace enables `serde_json/preserve_order`, so `serde_json::to_string` produces
JSON with keys in their original insertion order. When hashing JSON for deduplication or
comparison, call `Value::sort_all_objects()` first. Omitting this produces different
hashes for semantically-identical payloads with different key ordering.

## Repository Layout

```
├── Cargo.toml                  # [workspace] manifest — shared dep versions live here
├── crates/
│   ├── harnx/                  # Main CLI and TUI crate
│   │   ├── Cargo.toml
│   │   ├── assets/             # Bundled assets (syntax/theme .bin, HTML playgrounds)
│   │   ├── models.yaml         # Model catalog (providers, pricing, capabilities)
│   │   ├── tests/              # Integration tests
│   │   └── src/
│   │       ├── main.rs         # Entry point for the `harnx` binary
│   │       ├── lib.rs          # Library root — re-exports modules
│   │       ├── cli.rs          # CLI argument parsing (clap)
│   │       ├── serve.rs        # HTTP server mode
│   │       ├── tool.rs         # Built-in tool definitions
│   │       ├── mcp_safety.rs   # MCP tool safety classification
│   │       ├── config/         # Configuration, agent/session management
│   │       ├── render/         # Markdown + streaming output
│   │       ├── tui/            # Interactive TUI (ratatui)
│   │       ├── commands.rs     # Dot-command handlers (.help, .model, .session, …)
│   │       ├── rag/            # RAG pipeline
│   │       ├── mcp/            # MCP client/server integration
│   │       ├── hooks/          # Event hook system
│   │       ├── utils/          # Shared utilities
│   │       └── bin/            # Bins that share harnx library code (mcp-bash, mcp-fs)
│   ├── harnx-plans-tools/        # Toolset server: NATS-backed plan and todo management (standalone crate)
│   ├── harnx-attachment-tools/   # Toolset server: NATS-backed attachment and media management (standalone crate)
│   ├── harnx-blob-store/        # NATS-backed blob storage for attachments and plans (standalone crate)
│   ├── harnx-mcp-server/        # MCP server: exports harnx tools and agents-as-tools over stdio or Streamable HTTP (standalone crate)
│   ├── harnx-a2a-server/        # A2A server: exports harnx agents over JSON-RPC and SSE (standalone crate)
│   └── harnx-test-bins/        # Internal dev/test binaries (publish = false)
├── example_config/             # Example user configuration
├── docs/                       # User-facing documentation
├── scripts/                    # Shell completions and shell-integration scripts
├── Argcfile.sh                 # Developer helper commands (argc-based; install moved to xtask)
├── xtask/                      # Rust task runner (`cargo xtask install`) for local automation
├── .changeset/                 # Changeset files for release notes
├── knope.toml                  # Release automation config
├── renovate.json               # Dependency update bot config
└── .github/workflows/          # CI (ci.yaml) and release (release.yaml) workflows
```

## Verifying Changes

You MUST run the full verification pipeline before committing:

```sh
cargo build --workspace                                       # Compile the project, including sibling bins the e2e tests spawn
cargo fmt --all                                               # Auto-format code (rustup uses rust-toolchain.toml version — matches CI)
cargo clippy --workspace --all-targets -- -D warnings         # Lint — treat warnings as errors
cargo nextest run $(cargo xtask affected)                    # Run the tests of every package the change can affect, once
cargo nextest run --workspace -E 'test(=<name>) | ...' --stress-count=20  # Stress only the tests you added or changed
cs delta origin/HEAD                                          # Run CodeScene code quality analysis on current branch changes
python3 scripts/check_changesets.py                           # Check that every changeset will reach a changelog
```

On Linux, every test runs inside bubblewrap 0.5.0 or later, which needs
unprivileged user namespaces: install it first (`sudo apt install bubblewrap`)
and see "Test sandbox".

**Scope the test run to the affected packages.** `cargo xtask affected` diffs
the working tree, uncommitted and untracked files included, against its merge
base with `origin/HEAD` (or `--base <rev>`) and prints `-p` arguments for every
package whose tests the change can break, or `--workspace` when that is all of
them. It uses guppy's determinator, so a `Cargo.lock` bump selects only the
packages that build the bumped crate. If it prints nothing, no Rust test is
affected; don't run the bare `cargo nextest run` it would expand to, which
tests everything.

Cargo's graph cannot see tests that launch another workspace binary by path,
or read files outside their crate (`packages/`, `example_config/`, an
`include_str!` of a sibling crate's file). Those edges are listed in
`.config/affected.toml`. Adding a test that spawns a sibling binary or reads
such a file means adding its edge there.

**Stress only what you touched.** A full-workspace stress run costs five full
suites and surfaces flakes that predate your change, which then get ignored.
Stress the tests you added or modified instead, and prefer `--stress-count=20`
since a handful of tests is cheap to repeat. This matters most for broker-backed,
tmux/interrupt e2e and other timing-sensitive tests; a pure unit test needs no
stress run. Some flakes only appear under full-suite load, so when a new test
is timing-sensitive, also run it once alongside the rest of its crate.

CI uses the same selection on pull requests and runs the whole suite in the
merge queue and on `main`, so a missing edge is caught before merge rather than
after. CI skips the build and tests entirely when every changed file is on the
exclusion list in `.github/workflows/ci.yaml` (web, docs, changesets, READMEs,
release workflows), which the first path rule in `.config/affected.toml`
mirrors. A test that starts reading one of those paths must take it off both.

**Use `cargo nextest`, never `cargo test`.** Tests rely on nextest's per-test
process isolation; `cargo test` shares one process and produces spurious
failures. The tmux/interrupt e2e tests guard against this and will panic with a
redirect message if run under `cargo test` (via `harnx_core::require_nextest()`).

FD-redirection tests (e.g., `cli_event_sink.rs` `final_usage_is_standalone`)
use `dup2` to capture stdout/stderr. Under `cargo test`, libtest's own capture
intercepts writes before `dup2` sees them, yielding empty buffers and misleading
test failures.

**Do not skip any of these steps or you WILL miss problems**
**Do not ignore clippy warnings.** CI sets `RUSTFLAGS=--deny warnings` and runs `cargo clippy -- -D warnings`, so any warning will fail the build.
**CodeScene Health scores MUST NOT decrease as part of the change, only increase**

A clean `cs delta` doesn't guarantee CodeScene's PR check passes. The check
(`CodeScene Code Health Review (main)`) runs the server's analysis, which
scores some files differently and can report findings `cs` doesn't: it once
flagged Low Cohesion in a file where three helper functions had been added,
while `cs check` scored the same file as improved, and moving the helpers to
their own module cleared it. Read the PR check's result after pushing, and
when it fails on something `cs` can't reproduce, satisfy the PR check. A
check stuck in `queued` restarts with
`gh api -X POST repos/dobesv/harnx/check-runs/<id>/rerequest`.

### Test sandbox

On Linux and macOS, every test nextest runs goes through
`scripts/nextest-sandbox`, registered as the Cargo target runner in
`.cargo/config.toml`. Each test gets a private harnx and XDG home
(`HARNX_*_DIR`, `XDG_*_HOME` and `XDG_RUNTIME_DIR` point inside it; `HOME`
itself is unchanged) and only an allowlisted part of the ambient environment:
process basics, Cargo's and nextest's variables, toolchain variables, insta's
`INSTA_*` switches, `NATS_SERVER_BIN` and `CI`. Credentials, agent and
session sockets, proxies, tmux and git context, and every `HARNX_*` variable
are dropped. On Linux the test also runs under bubblewrap with its own
network namespace (loopback only), PID namespace and `/tmp`, and your real
harnx directories are covered by empty mounts, so nothing a test starts
outlives it and no test can reach your broker or another test's. The
top-level `/tmp` entry that holds the checkout, the target directory or
`HOME` is shared with the host. No DNS server is reachable inside, so a name
resolves only through `/etc/hosts` and NSS modules: RFC 6761 `*.localhost`
names need `myhostname` in `/etc/nsswitch.conf` (`libnss-myhostname` on
Debian and Ubuntu, which CI installs). On macOS the test
runs in its own process group, which is killed when the test ends, along with
any process whose command line names the test's private home, such as its
broker. A process that moved to its own group without naming the home, such
as harnx's local worker or a `ChildProcessManager` child, outlives the test.
macOS gets no masks, network namespace or private `/tmp`, and `HOME` is real
there too, so code that builds harnx paths from `HOME` instead of the
variables reaches your real directories.

The macOS sandbox is also Linux's light mode, `HARNX_TEST_SANDBOX=light`: a
private home, the filtered environment and a process group, without
bubblewrap. harnx's own sandbox (`harnx-sandbox-exec`, which runs the bash
tool's commands) marks what it runs with `HARNX_IN_SANDBOX=1`, and nothing
can create namespaces in there, so the runner uses light mode there by itself.
That is how an agent runs the suite from its bash tool; the tests of what only
bubblewrap gives skip there.

Linux needs bubblewrap 0.5.0 or later (`sudo apt install bubblewrap`), and
bubblewrap needs unprivileged user namespaces. Ubuntu 24.04 restricts them
through AppArmor by default. An AppArmor profile that allows `userns` for
`/usr/bin/bwrap` lifts that for bubblewrap alone;
`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` lifts it for
every program on the host, which CI does only on its throwaway runners.
Either is the machine owner's decision, so an agent asks rather than changing
it. Containers block the namespaces too; set `HARNX_TEST_SANDBOX=light`
there, or `off` to run unsandboxed. Without bubblewrap, every test fails with an
install hint. Where an AppArmor restriction, a sysctl or a seccomp filter
blocks the namespaces, every test fails with a message pointing here.
This sandbox is unrelated to the bash tool's `--no-sandbox` (see "Bridged tool
servers in tests").

`HARNX_TEST_SANDBOX=off` runs tests unsandboxed, which is the first thing to
try when a test fails only under the sandbox. The test then gets your real
environment, harnx directories and broker. On Linux, a test that needs the
outside network fails by design. A test killed by a signal shows as exit code
128+N (139 for SIGSEGV, 134 for SIGABRT) rather than as the signal. A test
that needs an ambient variable should set it itself; extend the allowlist in
the script only for variables every test legitimately needs, with a comment
saying why. `test_sandbox_canary::tests_run_inside_the_sandbox` in
`harnx-core` fails if tests stop running inside the sandbox; in light mode it
checks the private home and the environment only.

### Integration test layout

Each crate's integration tests build as one binary, `tests/it/main.rs`, with
every test file a module of it. Add a new file under `tests/it/` and declare
it there; shared helpers such as `common` are declared once in `main.rs` and
reached with `use crate::common;`. A new top-level `tests/foo.rs` still works,
but Cargo builds it as a separate binary that links the whole dependency graph
again, and linking 128 of those was most of a Windows CI build.

Test names carry the module, e.g. `tmux_e2e::retry_all_fail_shows_warnings_in_tui`,
so select tests with `test(/^tmux_e2e::/)` rather than `binary(tmux_e2e)`;
every integration binary is now named `it`. insta snapshots for these tests
live in `tests/it/snapshots/` with an `it__` prefix.

### Bridged tool servers in tests

Integration tests that need `bash_exec` register a bash tool server via `harnx-mcp-bridge`:

```yaml
command: harnx-mcp-bridge
args:
  - --name
  - bash
  - --
  - harnx-bash-tools
  - --mcp-stdio
  - --no-sandbox
  - --allow-read
  - <working_dir>
```

Use `--no-sandbox` when the test scope is bash behavior, not sandbox isolation. Hosts that block unprivileged user namespaces (some containers) fail `harnx-sandbox-exec` with exit 127 before the command runs. Precedent: `proxy_auth_hook_injects_env_vars_into_bash_exec` in `crates/harnx/tests/it/tmux_e2e.rs`; `write_with_wait_tool` in `crates/harnx/src/test_utils/interrupt.rs` bridges the time server the same way.

### Session transcript and PreToolUse hook mutation

### Hook command strings must use `shell_words::join`

Hook command strings that embed binary paths or JSON expressions MUST use
`shell_words::join` on all platforms, including Windows. Unquoted Windows paths
(`C:\Program Files\...\hook-server.exe`) split on backslashes and spaces when
passed through `shell_words::split` in the hook server, breaking execution.

Precedent: `crates/harnx-runtime/src/nats_worker/operator_tools/tests.rs:761` and
`crates/harnx-runtime/tests/it/nats_worker/tool_cli.rs:619`. Existing fixtures
use `shell_words::join([binary, "--event", "PreToolUse", "--jaq", expression])`
to preserve Windows paths with backslashes and JSON containing apostrophes.

### Session transcript and PreToolUse hook mutation

Persisted `ToolCalls` entries hold the arguments as received from the LLM, before
`PreToolUse` hooks run, except that `null`s for omitted optional parameters are
already gone (see "Tool parameter schemas"). `execute_tool_round_with_persistence`
(`crates/harnx-runtime/src/tool.rs`) appends calls before hooks apply
`mutated_tool_input`. To assert hook-injected env or args, read `ToolResults`
or process output instead.

### Test skips must probe capability, not timeout

A test that skips on timeout hides real failures. Guard skips with an explicit capability probe (checking `tmux -V`, verifying a sibling binary exists, or testing user namespace availability). A wait-for-X timeout is a test failure, not a skip condition.

A probe must not skip on its own mistakes either. `harnx-sandbox-run`'s integration tests skipped from the day they were written, because their probe passed flags the binary never accepted. When the probe's own invocation is rejected, such as by a usage error, fail the test; skip only when the capability is missing.

### Broker-backed tests and wall-clock margins

Tests that spawn a `nats-server` run in the `broker-e2e` / `heavy-e2e` groups
(`.config/nextest.toml`; local runs use `-local` copies with higher caps). On
a contended GitHub runner that whole block has been measured running 6 to 40
times its idle cost, comparing runs whose diffs did not touch it:
`cancellation::hierarchy::direct_child_cancellation_stops_only_its_worker_subtree`
went 0.53s to 21.2s, and the ubuntu job 184s to 483s. Tests whose duration is a
fixed sleep stayed flat, so the cause is starvation of real work rather than
clock skew. The trigger is not understood.

Size deadlines in these tests against the degraded cost, not the idle one. A
per-call timeout with a few hundred milliseconds of slack, or a turn backstop
set near the idle duration, reports a slow runner as a broken turn; those two
shapes caused 23 of 31 CI failures in one sample of 100 runs. The `ci` profile's
`slow-timeout = { period = "60s", terminate-after = 4 }` is what catches a real
hang, so an in-test backstop only has to beat 240s and say something more useful
than a SIGKILL would.

When one of these tests fails, check a passing run's timings for the same block
before blaming the test. If the whole block is slow, the margin is the bug.

### CLI binaries need an 8 MiB stack on Windows

Windows executables start with a 1 MiB stack. Deep debug-build async dispatch frames
(NATS poll loops, reservation handle state machines) overflow that in unoptimized builds.
`harnx` and `harnx-worker` both use `run_with_worker_stack`/`main_runtime::run` to spawn
a named 8 MiB OS thread, build the Tokio runtime there, and `block_on` a boxed async
future. The same bootstrap runs on every platform so Linux tests exercise the Windows
code path.

Precedent: `crates/harnx-worker/src/main.rs:134–166` and
`crates/harnx/src/main_runtime.rs`. New binaries that construct deep async state must
follow this pattern instead of `#[tokio::main]`.

Tests that reproduce the Windows stack limit on Linux use `pre_exec` to set
`RLIMIT_STACK` to 1 MiB (`crates/harnx/tests/it/tool_cli.rs`, Linux-only). Those
tests verify the same bootstrap path that Windows executes unchanged.

### Telling a flake from a regression

A failure that repeats on every attempt is not a flake. The `ci` profile retries
three times, so one broken test prints `TRY 1 FAIL` through `TRY 4 FAIL` and
reads in the log exactly like a timing flake. Three things separate them: did
every attempt fail, did the other two platform jobs pass (CI runs ubuntu, macos
and windows), and does the same test pass on `main`. Four failed attempts on one
platform while the others are green is a platform regression, and the retries
only make it slower to find. `gh run list --branch <branch> --workflow CI` also
shows whether an earlier head on the same branch was green, which brackets the
change that broke it.

When comparing against `main` to distinguish flake from regression, use
`origin/main`, not an intermediate branch commit. A branch that accumulated
multiple changes may have passed earlier commits while failing on later ones,
so a parent on the branch is not a stable baseline.

### Web/Frontend Verification

**Run all web/frontend commands from `web/`, never the repo root.** The root has no
`package.json`, so corepack cannot resolve the pnpm version and attempts to download
into a read-only cache, failing with `EROFS: read-only file system`. The `web/`
directory has `web/package.json` with the pnpm version pinned in `packageManager`.

```sh
cd web
pnpm exec tsc -b                                    # Typecheck (NOT tsc --noEmit — root tsconfig has files: [] so it always exits 0)
pnpm exec oxlint                                    # Lint
pnpm exec vitest run                                # Unit tests
pnpm test:e2e                                       # Playwright end-to-end tests
```

**Use `tsc -b`, not `tsc --noEmit`.** The root tsconfig sets `files: []` so
`--noEmit` is hollow and always exits 0; `-b` builds the actual project references.

## Commit Conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>
```

Common types:
- `feat` — New feature
- `fix` — Bug fix
- `docs` — Documentation only
- `style` — Formatting, whitespace (no logic changes)
- `refactor` — Code restructuring (no new features or fixes)
- `perf` — Performance improvement
- `test` — Adding or updating tests
- `chore` — Build, tooling, dependency updates

Examples from the project history:
```
feat: add harnx-plans-tools as a NATS-backed plan and todo toolset server
chore(deps): update rust crate syntect to v5.3.0
```

## Changeset Files

When making a user-visible change, create a changeset file in `.changeset/`:

```markdown
---
harnx: minor
---
Brief description of the change.
```

The YAML front matter specifies the version bump: `patch`, `minor`, or `major`.

The key on the left must be one of the packages knope versions:

- **`harnx`** — the entire Rust workspace. All `harnx-*` crates share one version
  (`version.workspace = true`), so use `harnx` even for a change scoped to a
  single crate like `harnx-proxy-auth` or `harnx-core`.
- **`pantheon`** — the `packages/pantheon` agent package.
- **`coding`** — the `packages/coding` agent package.

Keys and change types are bare words: `harnx: patch`, not `"harnx": patch`
(the quoting JavaScript's changesets tool uses) or `harnx: "patch"`. knope
(checked with 0.23.0) reports none of these mistakes:

- It reads only `.md` files directly inside `.changeset/`. A file in
  `.changesets/` or in a subdirectory is never read and never removed.
- It skips a quoted key and a key that is not one of the three packages, such
  as a crate name. A file with no usable key stays in `.changeset/` forever,
  and its change never reaches a changelog. If the file has a usable key too,
  knope consumes and deletes it, and the skipped key's entry is lost.
- It deletes a file whose change type is anything but `major`, `minor` or
  `patch` (`Patch`, `"patch"`, `patch # note`) without adding that key's
  entry.

knope does stop the release on a blank line or comment in the front matter, a
duplicate key, empty front matter, a byte-order mark or a missing front
matter block, but its error doesn't name the file.

`python3 scripts/check_changesets.py` (Python 3.11 or newer) rejects all of
these. CI runs it on every pull request, and the Prepare Release workflow runs
it before `knope release`.

knope renders a one-line description as a bullet. A description of several
lines becomes a `####` heading made from its first line, followed by the
remaining lines, so keep the first line a complete sentence and don't
hard-wrap it.

## GitHub Actions workflows that open pull requests

Open PRs with a token minted from the `harnx-release-bot` GitHub App
(`actions/create-github-app-token@v3` with `vars.RELEASE_BOT_APP_ID` and
`secrets.RELEASE_BOT_PRIVATE_KEY`), never the workflow's own `GITHUB_TOKEN`.
A PR opened with `GITHUB_TOKEN` gets its `pull_request` workflow runs parked as
"action required", so CI never starts and Mergify has nothing to queue until a
maintainer approves the run by hand. App-authored PRs run CI immediately.
`.github/workflows/update-models.yml` is the reference.

When attributing the bot's commits, its noreply address takes the bot *user*
id (`gh api /users/<app-slug>[bot] --jq .id`), not the installation id the
token action exposes.

### What the weekly models.yaml refresh does and does not maintain

`.github/workflows/update-models.yml` runs `scripts/update_models.py` against
the LiteLLM registry every Monday. Know what it does not maintain before
trusting a price or adding a model.

A model the registry does not list is kept verbatim, never refreshed, and
reported in the run summary as `provider <name>: preserving models not present
in LiteLLM: ...`. That line is the only signal that an entry's price is frozen,
so read it rather than skimming the diff. Fields in `HARNX_ONLY_FIELDS`, such
as `patches` and `require_max_tokens`, describe harnx's own request handling,
are never supplied upstream, and must stay in that list or a refresh resets
them. Any new field of that kind belongs there too.

LiteLLM records a capability only where it holds, so an omission is silence
rather than a denial. `CURATED_CAPABILITY_FLAGS` lets a curated `true` survive
a refresh that says nothing; a refresh can still switch one on. Without it,
Llama 4 on Bedrock quietly loses `supports_vision` and starts refusing images.

Vendor prefixes are deliberately not allowlisted. Screening Bedrock ids on
shape instead is what keeps each vendor AWS adds from being dropped until
someone edits the script.

The registry is unreliable for Bedrock in particular, and its errors run in
the dangerous direction — it put GLM 4.7 Flash's 4K output ceiling at 128K and
MiniMax M2.5's 196K context at 1M, either of which has harnx ask for more than
the model accepts. `BEDROCK_CARD_CORRECTIONS` pins such values against the AWS
model card; add an entry with the card name in a comment, and delete it once
upstream agrees. Verify a Bedrock model's limits against its card rather than
trusting a refresh, and see issue #2025 for reconciling the catalog against
`ListFoundationModels`.

## Key Patterns

- **Error handling:** Use `anyhow::Result` / `anyhow::bail!` throughout.
- **Async:** All I/O is async via Tokio. Use `async fn` and `.await`.
- **Client modules:** Provider clients live in `crates/harnx-client/src/` and follow the patterns in `macros.rs`. Config structs live in `crates/harnx-core/src/provider_config/`.
- **Configuration:** `config.yaml` holds global settings. Clients and MCP servers use individual YAML files; agents are Markdown files with YAML front matter in `agents/`.
- **Dual license:** MIT OR Apache-2.0. Preserve license headers where present.

## Tool Servers

Native toolset servers are named `harnx-<noun>-tools` (e.g. `harnx-fs-tools`, `harnx-bash-tools`, `harnx-time-tools`, `harnx-plans-tools`, `harnx-exa-tools`, `harnx-fetch-tools`, `harnx-attachment-tools`). They run `harnx_toolset_server::run_toolset_main(toolset)` and default to NATS mode. For Streamable HTTP MCP mode, pass `--mcp-http`; `--host` defaults to `0.0.0.0` and `--port` selects the listening port. Default HTTP ports are:

| Server | Port |
| --- | ---: |
| plans | 3000 |
| time | 3001 |
| bash | 3002 |
| fs | 3003 |
| grep | 3004 |
| exa | 3005 |
| fetch | 3006 |
| attachments | 3007 |

When launching behind `harnx-mcp-bridge` for stdio MCP compatibility, pass `--mcp-stdio` — without it, the server waits for NATS and the bridge handshake times out.

Binaries with `-mcp-` in the name are genuine MCP infrastructure (`harnx-mcp-bridge`, `harnx-mcp-remote`, `harnx-mcp-server`) or test fixtures (`harnx-mock-mcp`), not native toolsets.

### Adding a new native toolset server

The checklist below covers every integration point. Miss any and the release fails or the binary ships incomplete.

1. **New crate** — mirror `harnx-grep-tools` structure: `src/{lib,main,toolset,client,format}.rs`, `src/server/{mod,handler,model,params}.rs`. Implement `Toolset` (`name`, `default_mcp_http_port`, `tools`, `invoke`) and `ServerHandler` (rmcp) sharing the same handlers. `main.rs` calls `harnx_toolset_server::run_toolset_main`.

2. **Workspace Cargo.toml** — add to `[workspace] members`.

3. **release.yaml** — two spots: one release shard's `packages` list (choose the shard whose link time it best balances; see the comment above `shard:`), and the docker job's "Verify extracted binaries" `for` loop. The docker job downloads every Linux archive in the release, so it needs no per-binary pattern.

4. **docker/harnx.Dockerfile** — `COPY linux-${TARGETARCH}/<binary> /usr/local/bin/<binary>` line. The Dockerfile header lists the four release.yaml locations that must be kept in sync.

5. **Docs enumerating binaries** — `docs/healthz.md`, `docs/metrics.md`, `docs/environment-variables.md` enumerate tool servers in multiple places; all lists must be updated. `docs/configuration-guide.md` has a "native-servers" sentence listing examples. Illustrative mentions (time-tools examples in `docs/kubernetes-deployment.md`, etc.) do not need updates.

6. **`.gitattributes`** — if the crate ships golden `.txt` fixtures, add `text eol=lf`.

7. **Changeset** — if the change touches `packages/coding/**` or `packages/pantheon/**`, the changeset front matter must also have unquoted `coding:`/`pantheon:` keys (separate knope packages with their own CHANGELOGs).

8. **MCP HTTP port** — use the next free port in the sequence (e.g., 3006 after exa's 3005).

9. **CI.yaml** — no per-crate edit needed; CI builds every workspace bin and selects tests with `cargo xtask affected`. If another crate's tests launch the new binary, add a `[[test-edge]]` for it to `.config/affected.toml`.


### Static `ToolKind` declarations

Native toolsets declare their tool categorization (`Read`, `Edit`, `Search`, `Execute`, `Fetch`, etc.) statically at registration time. This kind flows through to `ToolEvent::Started` for UI presentation and is **not an authorization boundary**.

**How it works:**

1. `ToolSpec::with_kind(ToolProgressKind::Read)` stores kind in `meta["harnx:kind"]`
2. `nats_tool_provider.rs:registered_tool()` extracts `spec.kind()` and sets `ToolDeclaration.kind`
3. `ToolEvent::Started` resolves `decl.kind.unwrap_or(ToolKind::Other)` in `harnx-runtime/src/tool.rs:521` and `nats_session.rs:2044`

**When adding a native toolset:**

- Call `spec.with_kind(ToolProgressKind::<variant>)` on every tool spec
- Choose kind by tool semantics:
  - `Read` — read-only data access (fs: `read`, `ls`; plans: `get_plan`, `get_task`, `get_note`, plus `list_*` tools; k8s-sandbox: `status`; subagent: `session_load`)
  - `Edit` — file/content mutation (fs: `write`, `edit`, `insert`, `re_replace`, `rollback_file`; plans: `add_plan`, `update_plan`, `add_task`, `update_task`, `add_note`, `update_note`)
  - `Delete` — destructive removal (plans: `delete_plan`, `delete_task`, `delete_note`; k8s-sandbox: `release`; subagent: `session_cancel`)
  - `Search` — content or path search (grep, fs: `grep`, `find`; exa: `web_search_exa`)
  - `Execute` — shell/command execution (all bash tools; k8s-sandbox: `connect`; subagent: `session_prompt`)
  - `Fetch` — HTTP fetch (fetch tools; exa: `web_fetch_exa`)
  - `Think` — model reasoning
  - `SwitchMode` — mode changes
  - `Other` — uncategorized (time tools; subagent: `session_new`)
- Add a test asserting expected kinds (see `harnx-fs-tools/src/toolset.rs:all_fs_tools_declare_correct_kind`)

**Non-goal:** Kind is presentation-only. Access control, rate limiting, or policy gating must not rely on `ToolKind` — a malicious MCP server can return any kind it wants.

### SSRF / private-IP protection for URL-fetching tool servers

When a tool fetches attacker-influenced URLs, block connections to private, loopback, link-local, and other special-purpose IP addresses. The pattern in `harnx-fetch-tools/src/net.rs` covers the pitfalls:

1. **Resolver + literal check required** — `reqwest::dns::Resolve` is NOT called for IP-literal URLs (`http://127.0.0.1`, `http://2130706433`, `http://[::ffff:127.0.0.1]`). You need BOTH a guarded resolver (for DNS answers) AND a `check_url` that classifies literal hosts, applied to the initial URL and every redirect hop.

2. **Reject whole mixed answer** — the resolver must collect ALL addresses, then reject if ANY is disallowed (prevents DNS-rebinding split A/AAAA bypass). Return only vetted addresses. Empty answer = explicit error.

3. **Redirects** — `reqwest::redirect::Policy::custom` does NOT inherit the default limit. Wrap `Policy::limited(10)` and re-run `check_url` on each hop; block with `attempt.error(..)` (not `stop()` — stop returns the redirect as success).

4. **Proxy** — protected client must call `.no_proxy()`; reject `proxy` tool arg while protection is on (a proxy resolves the target itself, bypassing the guarded resolver).

5. **IP classification** — normalize IPv4-mapped IPv6 (`to_ipv4_mapped`) before classifying. `std::net::IpAddr::is_global` is unstable; use `ipnet` CIDR ranges instead. Block the full IANA special-purpose set (0/8, 10/8, 127/8, 169.254/16, 172.16/12, 192.168/16, 100.64/10, 192.0.0/24, 192.0.2/24, 198.18/15, 224/4, 240/4, broadcast; IPv6: ::/128, ::1/128, fc00::/7, fe80::/10, ff00::/8, plus documentation/benchmark ranges). `harnx-fetch-tools/src/net.rs` encodes the tables in `ipv4_blocked_ranges()` and `ipv6_blocked_ranges()`.

6. **Testing** — assert no connection is attempted to blocked addresses (bind a listener, assert it never accepts). Redirect tests must drive live HTTP through the production policy, not just call `check_url` on strings.

Reference implementation: `crates/harnx-fetch-tools/src/net.rs` (`GuardedResolver`, `check_url`, `redirect_policy`, IP tables) and `src/client.rs` (`FetchClient`).

### Reasoning-signature compatibility across providers (issue #1804)

Each provider has its own opaque reasoning-signature format:
- **Anthropic**: `thinking` block `signature` (also used by Bedrock Claude)
- **Gemini**: `functionCall` part `thoughtSignature`
- **OpenAI**: `reasoning` item `encrypted_content` (shared by codex)

These formats are **not interchangeable**. Replaying a signature to an incompatible provider
causes HTTP 400, which halts fallback (400 is non-retryable). The fix added
`reasoning_provenance: Option<ReasoningProvenance>` to `ToolCall`
(`harnx-core/src/tool.rs`), tagging each signature with its producing protocol and model.

Compatibility rules (verified in `ToolCall::compatible_signature`):
- **OpenAI**: fail-closed on model identity — `encrypted_content` is bound to both org and model.
  Same-protocol, different-model → incompatible.
- **Anthropic/Gemini**: protocol must match; model binding is not enforced.

Import handling when signature is incompatible or unknown (legacy sessions lack provenance):
- **Gemini**: first `functionCall` of each step must carry `thoughtSignature`. Use placeholder
  `"skip_thought_signature_validator"` for imported/unknown history (per Google docs).
- **Anthropic**: omit the `thinking` block entirely. Missing signature → 400.
- **OpenAI**: omit the `reasoning` input item. Safe because harnx uses `function_call` with
  `call_id`, not OpenAI-internal `id`/`fc_` which would trigger reasoning-pairing validation.

Architecture: provenance-aware normalization lives in each provider's `build_body` (request-local,
never mutates stored history). Each builder calls
`tool_call.compatible_signature(dest_protocol, dest_model)` and applies destination handling.

When modifying provider client code: tag provenance at capture (streaming/non-streaming extraction), check compatibility before replay, and allocate correlation IDs for imported anonymous tool calls (Gemini doesn't return IDs; use `ToolCallIdAllocator` in each `build_body`). Do NOT add a shared pre-pass — each provider knows its own
protocol and import-handling rules.

#### Replayed reasoning across the two Bedrock clients

Replaying the previous turn's reasoning is the default, and it is load-bearing:
a model handed tool results with no record of its own thinking reads the calls
as somebody else's and narrates a session boundary. That failure is why the
echo exists, so do not strip reasoning wholesale, per provider or globally.

Which Bedrock client you configure decides whether replay happens at all, and
the two differ in more than wire format:

- `type: bedrock` speaks Converse and signs with the AWS credential chain, so
  it is the one that works with rotating credentials such as IRSA in
  Kubernetes. It is the only path that replays reasoning.
- `type: openai-compatible` against `bedrock-runtime.../openai/v1`
  authenticates with a `BEDROCK_API_KEY` bearer token and cannot use the
  credential chain. `openai.rs` discards the stored thought when building a
  request, so nothing is replayed. Both shipped packages ship this variant.

On the Converse path the replay is gated on having captured a signature, and
that gate is what keeps a hostile model safe rather than any per-model
setting. A model that returns `reasoningContent` without a signature leaves
`thought_signature` empty, `compatible_signature` returns `None`, and the
block is omitted. Kimi K3 behaves exactly that way, verified on 2026-09-20 by
tracing `HARNX_LLM_TRACE`: across a two-turn tool-calling session with the
suppression removed, harnx sent no `reasoningContent` at all.

AWS's Kimi K3 card warns that Converse answers an `InternalServerException`
when a multi-turn request carries earlier reasoning. That did not reproduce —
replaying the block by hand through the Converse API was accepted with
`stopReason: end_turn`. Treat the warning as unconfirmed for this
configuration rather than disproven; it may need a signed block, a longer
reasoning span or another region.

A model that both emits a *signed* reasoning block and rejects the replay
would defeat the signature gate and need real per-model suppression. None is
known, so none is implemented — add it when a probe finds one, not before.

Read a Bedrock model card's "Usage Considerations and Limitations" before
adopting it, and probe a *two-turn tool-calling* exchange against the client
type you actually deploy. A single-turn smoke test cannot reproduce this class
of failure, and a probe on one client type says nothing about the other.


### Adding a Provider Client

Wire a new provider client via `register_client!` in `crates/harnx-client/src/lib.rs`:

```rust
register_client!(
    (myprovider, "myprovider", MyProviderConfig, MyProviderClient),
    // ...
);
```

This macro expands to the module declaration, config enum variant, and client registry. Then:

1. Add a config struct to `crates/harnx-core/src/provider_config/myprovider.rs` and export it in that dir's `mod.rs`. `register_client!` reads `models`, `model_catalog` and `system_prompt_prefix` off every config by field access, so all three must be present or the macro will not compile.
2. Add `ClientConfig::MyProviderConfig(_)` match arms in `lib.rs` (`effective_name`/`set_name`/`set_package`) and `crates/harnx-runtime/src/config/patches_split.rs` (`apply_client_patch`).
3. Implement `Client`:
   - Sync auth (API key): use `impl_client_trait!` macro (see `cohere.rs` for example).
   - Async per-request auth (OAuth token refresh): write a manual `impl Client` like `vertexai.rs` or `codex.rs`, running token prep at the top of each `*_inner` method.
4. For Responses API variants, key `model.endpoint()` to `"responses"` to reuse `openai_responses.rs` helpers.

Env-var field access uses `config_get_fn!` — the macro generates `${STEM}_${FIELD}` lookup where STEM is the client filename (e.g. `myprovider_api_key` → `MYPROVIDER_API_KEY`).

A client inherits catalog entries from the `models.yaml` block named by its
`model_catalog`. With that field unset the filename decides instead: the
client's own type, `openai` for `codex`, or for `openai-compatible` the first
provider whose name the filename stem starts with. That fallback predates the
field and is kept only so existing configs work — prefer `model_catalog`, and
note the filename rule is a prefix match, so it silently claims
`deepseek-proxy` for DeepSeek and leaves `aws-prod` with nothing. The
filename still drives env-var prefixes, `api_base` shortcuts and package patch
matching, which are separate lookups.

### Building a rustls ClientConfig

Never call `rustls::ClientConfig::builder()`. It resolves rustls'
*process-default* `CryptoProvider`, and this workspace compiles rustls with both
`ring` (via async-nats/tokio-rustls) and `aws-lc-rs` (via the AWS SDK's
hyper-rustls stack). Most binaries do not install a default, so that resolution
panics at runtime with "Could not automatically determine the process-level
CryptoProvider". Use `builder_with_provider(Arc::new(rustls::crypto::ring::
default_provider()))` instead.

This bites indirectly too. A dependency that builds its own config on your
behalf hits the same panic, which is why `NatsEndpoint::apply_tls_options`
(`crates/harnx-nats-common/src/connect.rs`) supplies a `tls_client_config` for
every TLS connection rather than letting async-nats construct one — including
a best-effort config for plaintext `nats://` endpoints, in case the server
demands a TLS upgrade in its INFO.

Feature unification makes this invisible to a narrow test. `harnx-nats-common`
alone resolves rustls with `ring` and cannot reproduce the ambiguity, so tests
covering it must live in a crate whose graph also pulls in the AWS SDK —
`harnx-runtime`, `harnx-worker` or `harnx`. See
`crates/harnx-runtime/tests/it/tls_client_config.rs`. Check with
`cargo tree -p <crate> -e features -i rustls`.

### Tool-call argument parsing

When a provider client parses tool-call arguments from LLM output, use this pattern:

```rust
let arguments: Value = if arguments_str.is_empty() {
    json!({})
} else {
    serde_json::from_str(arguments_str)
        .with_context(|| format!("Tool call '{name}' have non-JSON arguments '{arguments_str}'"))?
};
```

Empty string maps to `{}` (API omits arguments for no-arg calls). Non-empty malformed JSON
propagates as an error with context naming the tool and echoing the raw argument string.
This convention is consolidated across all provider parsers (`openai.rs`, `openai_responses.rs`,
`bedrock.rs`, `claude.rs`, `cohere.rs`).

### Tool parameter schemas

`ToolDeclaration.parameters` (`harnx_core::json_schema::JsonSchema`) holds the
schema exactly as the tool declared it. When a tool registers,
`JsonSchema::from_tool_schema` drops `$schema` and inlines local `$ref`s. A
`$ref` that recurses stays, along with its definitions. Keep this copy lossless:
an earlier typed struct stripped `null` from `type` lists and dropped `$ref`,
which sent `update_plan`'s `tasks` items to every model as `{}`. Convert at the
provider boundary instead:

- **OpenAI Responses** (`openai`, `codex`): `openai_strict::strict_parameters`
  converts each schema to strict mode. Every property becomes required, and
  each optional one that doesn't already accept `null` is made to.
  `additionalProperties: false` is set on every object, unsupported annotations
  are dropped, and `oneOf` becomes `anyOf`. A schema strict mode can't express
  is sent unchanged with `strict: false`. Free-form maps (`bash_exec`'s `env`,
  the fetch tools' `headers`), `{}`, `allOf` and `uniqueItems` all fall back
  this way. The reason is logged at debug level.
- **Gemini and Vertex** (`vertexai.rs`): the schema goes through
  `parametersJsonSchema` unchanged. The older `parameters` field only takes an
  OpenAPI-style subset.
- **Claude, Bedrock Converse and Chat Completions clients**: the schema is sent unchanged.

Never leave `strict` unset on a Responses tool. Responses then converts the
schema itself: it makes every property required without making it nullable,
and GPT models fill the optional ones with `""`, `0` or `[]`. A response
echoes `tools[].strict`, which shows whether a tool ran strict or fell back.
This was measured on 2026-10-01 against `gpt-6.1-sol` on both the API and the
Codex backend.

Strict mode has the model send `null` for every parameter it leaves out. In
`harnx-engine/src/chat_completions.rs`, `DeclaredSchemas::clean` removes a
`null` the model sent for an optional property whose schema does not accept
`null`, for every provider, before persistence and hooks. A tool that needs a
meaningful `null` must declare the property nullable (`Option<T>` in schemars
does). A new native toolset with a map-typed parameter loses strict mode for
that whole tool, so prefer a list of name/value objects when strictness
matters.

### Loop protection

Models, Gemini in particular, fall into loops. Two guards in
`harnx_core::loop_guard` catch them. `ToolRepeatGuard` catches a model that
repeats the same tool call with the same result hundreds of times. The output
guard, described below, catches a model that streams the same text over and
over.

`ToolRepeatGuard` counts identical calls within the current tool loop and
escalates:

- A call is identified by its name plus its arguments compared as JSON values,
  and it counts only when its result is also identical. Gemini sends the same
  arguments with the keys in different orders, so hashing the raw argument
  text misses its loops; comparing results leaves polls whose output changes
  alone.
- The 2nd to 4th identical call within 10 minutes runs and gets a `[harnx]`
  note in its result. The 5th is refused with an error saying when it may run
  again. A refusal ends the turn with a `RepetitionStop` instead when the
  model's previous response contained a refusal, or when it would be the same
  call's third refusal. A model sends a response's calls before it sees any
  of their results, so two refusals within one response do not end the turn
  (`ToolRepeatGuard::begin_batch`).
- The guard lives on `AgentLoopContext`, one per turn, and resets when a user
  or parent message arrives mid-loop (`input.injected_user_text()`) or the
  session is compacted (the length of `compressed_messages` changes). It never
  reads the log.
- A call refused in a round that also defers for human (HITL) approval is not
  persisted as refused, and the continuation starts with a fresh guard, so
  that call can run. That is acceptable because a person is in the loop.
- A stop persists as the turn's `Error` entry: a sentence plus a
  `harnx:repetition {...}` marker. Match the marker only with
  `harnx_core::loop_guard::parse_repetition_terminal`. To classify a failed
  turn, call `harnx_runtime::parse_worker_terminal`, which tries the budget
  marker and then that one. The sub-agent tool and the CLI one-shot turn the
  result into `TerminationKind::Repetition`.
- `loop_detection.tool_calls` (global or agent front matter) and
  `HARNX_LOOP_DETECTION=0` turn it off. The variable sets both global values,
  `tool_calls` and `output`, and an agent's front matter can override either,
  so an agent whose front matter sets `tool_calls: true` still has this guard
  on. `harnx dump session <agent> <id> --check-loop-detection` replays a stored
  session through the same guard, and also reports the saved replies the
  output guard (below) would have stopped.
- Tools that poll should return something that changes between calls
  (`time_wait` and `time_wait_until` return their start and end times;
  `bash_wait` reports total runtime), or the guard treats an unchanged poll as
  a repeat. Conversely, a result that always changes hides a loop from the
  guard: every `bash_exec` result for a command that ran embeds a fresh
  `execution_id` and log paths, so no two of them match and the guard never
  counts them, however often a model repeats the command. A `bash_exec` that
  fails before its command starts (an empty command, an invalid `env` key or an
  unusable `working_dir`) returns the same error each time with no id, and
  those repeats do count. An agent whose job is polling can turn the guard
  off in its front matter.
- Tests that drive a mock model through repeated tool calls meet the guard
  too: a fifth identical call with an identical result in one turn is
  refused. Give the mock tool a result that changes per call (the
  bounded-growth interruption test's `counter_ping` answers `pong 1`,
  `pong 2`, …), or
  turn the guard off in the test's config
  (`loop_detection.tool_calls = false`). A mock model that streams 2,000 or
  more characters of one repeated unit trips the output guard, described
  below, in the same way: vary the text, or set
  `loop_detection.output = false` in the test's config.

#### Output guard

- The output guard (`harnx_core::loop_guard::output_repeat`) watches the
  streamed answer and thinking separately. A channel whose last 2,000 or more
  characters are at least four back-to-back copies of one unit (up to 2,000
  characters) fails the stream with `RepetitiveOutput`. `SseHandler` runs it
  for every provider, and `call_chat_completions` checks non-streamed replies
  the same way. `run_chat_completion_streaming` must never return a stopped
  stream as a partial reply, as it does after other stream errors.
- Clients that read an OpenAI chat-completions stream, such as
  `openai-compatible` and `llama-server`, send reasoning through the answer
  channel inside `<think>` tags (`openai_transition_reasoning` in
  `crates/harnx-client/src/openai.rs`), so it shares the answer's detector. A
  reasoning loop on those providers is still caught, but it is reported as
  `answer`, both in the retry note's wording and as the stop's `source`.
- The retry layer (`harnx_engine::retry`) retries that model once,
  immediately, with `Input.transient_note`, a user message added to requests
  but never persisted, then tries the next fallback with the note and without
  a cooldown, and finally ends the turn with `RepetitionStop` (`source`
  `answer` or `thinking`). A model gets that one retry per call, however many
  backoff attempts it has. A cooldown would take the model away from every
  other turn over one request's loop. The text streamed before a stop stays in
  the live view; only the finished reply is saved.
- The usage of a reply the output guard stopped is missing from session token
  totals, token budgets, and the token and cost metrics (`record_llm_metrics`:
  `harnx_llm_tokens_total`, `harnx_llm_cost_dollars`), which take only a
  completed call's usage. All three undercount by the discarded replies.
- Only exact repetition counts: a loop whose copies differ slightly (an
  incrementing counter) is not caught, and neither is repetition inside
  tool-call arguments. `loop_detection.output` turns this guard off.

### Tool result templates and undefined behavior

Tool display templates are rendered by `make_template_env()` in
`crates/harnx-core/src/tool.rs`, which MUST use `UndefinedBehavior::Chainable`
(not `Lenient`). MiniJinja's `Lenient` mode tolerates printing an undefined
value, but it still raises "undefined value" when accessing an attribute or
index of an undefined intermediate — `default()` filters cannot rescue this.

The canonical result template `{{ result.content[0].text | default('') }}`
walks into `result.content`, which is absent on recoverable-error results
(`{"is_error": true, "error": ...}` — no `content` field). Under `Lenient`,
indexing into undefined raises before `default('')` applies (#1537).

When writing result templates:
- be null-safe for the error-result shape (`result.content` may be absent)
- `| default(...)` only works because Chainable makes missing intermediate
  paths evaluate to undefined; syntax errors and unknown filters still error

### Native toolset error mapping

Native `Toolset` implementations' `map_result` must map **all** `ErrorData` from handlers (both
`internal_error` and `invalid_params`) to `ToolInvokeError::Recoverable`. Reserve `Fatal` for:

- result-serialization failure in the `Ok` branch (`serde_json::to_value`)
- true transport/lifecycle death (e.g. MCP bridge `TransportClosed`)

**Why:** `Fatal` aborts the agent turn; `Recoverable` surfaces as `{"is_error": true, ...}` so the
session continues and the agent can retry. The canonical pattern is
`crates/harnx-fs-tools/src/toolset.rs:59-66`. When adding a native toolset, do not special-case
`ErrorCode::INTERNAL_ERROR` to `Fatal`.

### Partial results

A tool that has produced something worth keeping before it can fail, such as a
remote job id or the child session a sub-agent call started, records it with
`ToolInvocationContext::record_partial_result(value)`
(`harnx-toolset/src/partial_result.rs`). The value is at most
`PARTIAL_RESULT_MAX_BYTES` (4 KiB) of JSON. The latest value wins until the
call's reply is journaled, after which it is frozen. The NATS tool server keeps
it on the call's invocation journal row (`RecordedInvocation.partial_result`),
so it survives a worker restart. MCP transports hand the tool no store, and
recording is then a no-op.

When the call does not succeed, the runtime adds the value under
`partial_result` (`harnx-core/src/partial_result.rs`) to the output it writes:

- the engine's `{"is_error": true, ...}` for a recoverable error, including the
  transport failures and timeouts `NatsToolProvider` attaches it to;
- wind-up placeholders, and journaled failures that wind-up picks up;
- failed replays.

A successful call returns only its own result. A tool that wants the model to
see the same information on success puts it in that result itself.

Recording and reading are both best-effort. `NatsToolProvider` skips its read
of the row while NATS is disconnected and gives up after 5 s, so an unreadable
row costs the output its partial result, never the call its error.
`record_partial_result` returns a `Result` and the tool decides what a failure
means; the sub-agent call logs it and carries on.

Sub-agent calls record `{"session_id", "sub_agent"}` as soon as the child
session is bound. That is how the parent model learns the child's id when the
call fails, so nothing writes `SubAgentStarted` to the parent's transcript;
readers keep handling the entries old transcripts hold.

### MCP ServerHandler::call_tool error mapping

MCP server implementations (`ServerHandler::call_tool`) must return recoverable failures as
`Ok(CallToolResult::error(vec![ContentBlock::text(msg)]))` (`is_error: Some(true)`). This applies to:

- argument deserialization errors (`parse_arguments`) and input validation failures
- domain execution failures (missing resources, file I/O errors, failed text replacements, rate limits)

Reserve `Err(ErrorData)` exclusively for:

- unknown tool names (`ErrorData::invalid_params("unknown tool: ...")`)
- malformed envelopes rejected before handler execution
- broken transport, lifecycle, or session state

Do not return `method_not_found` (-32601) for unknown tool names in `tools/call`. That code is
reserved for unknown JSON-RPC methods. Unknown tool requests must return `invalid_params` (-32602).

Per SEP-1303 and the MCP specification, client agents use `is_error: true` results to see error text
and self-correct. Returning JSON-RPC error frames for domain or argument errors causes client SDKs to
abort the session.

### Session log entries and transcript protocol

Local broker failover keeps the authenticated endpoint stable and runs election
through `nats_local_server::LocalBroker` even while a frontend is idle. Create
production connections with `harnx_nats_common::connect::NatsEndpoint`; use the
operation-specific recovery helpers documented in `docs/nats-ha.md` under
“Recovery contract and shared implementation”. Retrying a handler whose side
effects are unknown is unsafe. Failover tests must retain existing clients and
in-flight work while removing the owner, rather than testing only fresh clients.

`SessionLogEntry` (`harnx-core/src/session.rs`) and `SessionEvent` (`harnx-core/src/event.rs`)
are different types with different change-cost:

- **`SessionLogEntry`** — durable transcript entries persisted to NATS. Adding a variant is a
  **transcript-protocol change**; canonical replay hard-rejects `Unknown`, so older workers
  cannot read transcripts with new variants. Deploy readers before writers. Precedents:
  `TurnEnd` (#1490), `Error` (#1545), `SubAgentStarted` (#1604).

- **`SessionEvent`** — advisory events emitted to live subscribers, not persisted. Adding an
  optional field (with `#[serde(default, skip_serializing_if)]`) is a safe additive change.
  Used when a live client needs data that isn't in the durable entry (e.g. `after_seq` field
  in `HandoffCommitted` added in #1803).

When extending handoff or session metadata, check which type carries the data. The durable
entry is the source of truth; the advisory event is a convenience for clients that haven't
reloaded the transcript.

Required match-site updates (3 compile-time exhaustive matches):
- `config/session.rs` — reconstruction into `Session.messages`
- `nats_session.rs` — `render_log_entry_to_sink` (usually a no-op arm with comment)
- `session_history.rs` — `entry_type` and `entry_searchable_text`

Wildcard matches elsewhere (`session_reconstruct.rs`, fence helpers, etc.) compile without
changes but should be audited for correctness.

Mid-tool entries (arriving between `ToolCalls` and `ToolResults`) must be queued in
`messages_queued_during_tool` during reconstruction so tool_use→tool_result adjacency is
preserved. See `SubAgentStarted` handling in `config/session.rs` for the pattern.

Session identity is `(agent, local_id)` within a cluster. Use `Session::storage_key()`,
`NatsSession::storage_key()`, or `SessionInitializer::session_key(local_id)` for all
broker storage, control, execution, and parent references. Internal protocol fields
named `session_id` carry this key; public metadata, tool results, hooks, and
confirmation requests retain the local ID.
See `docs/nats-ha.md` under “Session identity”. Do not pass a local ID to by-key APIs.

Client/control code can append to another session's log via
`NatsSessionLog::new(jetstream, storage_key)`. Worker/tool output must instead go
through the session log backend while holding the lease, so the append is fenced
on the tail the turn last observed and a `Cancel` that landed meanwhile stops it.

The worker appends the durable `HandoffCommitted` entry **before** emitting the advisory
`SessionEvent::HandoffCommitted` (see `agent_loop.rs:979-1001`). This guarantees a live
handoff's sequence is strictly greater than any attach boundary captured before the commit,
enabling clients to gate navigation on `after_seq > attached_seq`.

Worker appends go through `FencedSessionLogSink`, which carries the lease fence and
the expected tail. Stream-tail CAS is the authority: JetStream's finite dedup window
alone is not enough, so keep the same message ID and expected-last-sequence across a
retry and treat a conflict as a decision to re-read, never as a reason to append
again. A `Cancel` is an ordinary log entry any holder of the session can write,
including frontends that hold no lease. See `docs/nats-ha.md` under "Interruption".

### Reading KV state that was just written

async-nats creates every KV bucket with `allow_direct`, so `kv::Store::get`
and `entry` are direct gets, which a follower answers from whatever it has
applied. A key written moments earlier, by this process or by the one that
handed it the work, has to be read through `harnx_nats_common::leader_reads`,
inside `leader_reads::retry_transient` so a leader election isn't a failure.
`SessionMetadataStore` reads every key that way. See "Read a record that was
just written" in `docs/nats-ha.md`.

### Adding per-session data

Don't add a typed field to `SessionMetadata`. It denies unknown fields, so
every older reader fails on the records that carry the field, and during a
rolling deploy older workers stop running those sessions. Use an extension
namespace, which older readers keep as opaque JSON. Descriptive values that
agents or frontends set, such as a user identity, belong in session
properties (`dev.harnx.session_properties`): a row in `PROPERTY_DEFINITIONS`
(`nats_session_metadata/session_properties.rs`) brings its validation, its
line in `harnx_write_session_meta`'s description and whether sub-agent
sessions inherit it. See "Session properties" in `docs/nats-ha.md`.

### HTTP Test Clients and Proxy Environments

Integration tests for harnx-serve that make loopback HTTP requests **must disable ambient proxy configuration** to avoid malformed `Accept` headers. HTTP proxies may aggregate repeated `Accept` fields (e.g. `Accept: text/html` followed by `Accept: text/event-stream`) into a single comma-separated value, which breaks content negotiation.

In the test fixture client, use `.no_proxy()`:

```rust
let client = Client::builder()
    .no_proxy()
    .timeout(Duration::from_secs(60))
    .build()?;
```

Tests inherit `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` env vars from the test runner's environment. Sandboxing that strips these variables exposes the real header-handling behavior. The `Accept` multi-field parsing fix (`HeaderMap::get_all`) ensures repeated fields behave identically to comma-joined values.

### Session user identity

`user_id` is a read-only session property set only when canonical metadata is
first created. It is never overwritten on later prompts, handoffs into an
existing session, or reconnects. Concurrent creators race; the first successful
metadata write wins and stamps the identity.

**User identity is immutable**. Access rules may grant additional scopes via
group or role memberships, but these memberships never replace or extend the
session owner. The persisted `user_id` remains the sole owner for all session
access checks requiring ownership match (admin scope can bypass ownership). See
`crates/harnx-runtime/src/nats_session_metadata/session_properties.rs` where
`user_id` is defined with `read_only: true`, and tests in
`session_properties_tests.rs` guarding this invariance.

Shared request identity resolution lives in `harnx_runtime::identity`
(`identity.rs`), used by `harnx-a2a-server` and `harnx-serve`. It extracts from
configured HTTP headers or cookies and is fail-closed: an empty or malformed
matching source returns an error rather than falling back.

Precedence at metadata creation:

1. Explicit/inherited `user_id` in `SessionInitializer` (A2A caller identity,
   sub-agent inheritance, handoff).
2. Request identity resolved by `harnx-serve` from configured sources.
3. `user_id` in `nats_servers/<cluster>.yaml` for the destination cluster.
4. Global `user_id` in `config.yaml` / `HARNX_USER_ID`.

Blank explicit or inherited strings count as absent, so defaults apply. Handoffs
use `SessionProperties::inherited_user_id` to copy only the user identity, not
execution-context properties like `git_branch`.

Control commands in `harnx-serve` (cancel, compact) must not create metadata.
They check for an existing canonical record via `control_session()` before
opening a control handle; without metadata they return idle/not-found rather
than creating one. Creating it there would stamp only the defaults, and the
next prompt's request identity could never replace them.

See `docs/configuration-guide.md` under "Session User Identity" and
`crates/harnx-serve/README.md` under "Request Identity" for user-facing config.

Coordinated A2A admission stores active identity/snapshot in `sessions/{storage_key}/a2a/context` with an owner-checked CAS, then repairs `sessions/{storage_key}/a2a/index`. Admission reads the authority even when index repair lagged. Terminal records use create-only `a2a/archive/{uuid}` projections. Legacy `a2a/tasks/{uuid}` creation keeps index-first ordering and skippable creation intents. First-message reservations are shared create-only keys under `a2a/first-messages/{scope-hash}`, written after a scoped candidate lease but before session effects. Every emitted A2A update commits its snapshot/cursor and one pending event in that context. Independent leader-backed readers capture global watermark before authoritative snapshot, then read task events from watermark+1; no subscriber queue is shared. Retirement requires confirmed stop, archive, mapping and terminal publication. Exact remote cancellation intent lives in the same context document. Create-only `a2a.registry.{storage-hash}` discovery records precede work, and bootstrap starts a bounded leader-read background sweep; worker lease renewal never blocks A2A takeover. See `crates/harnx-a2a-server/src/runner/README.md` for scope, recovery and coordinated GC, and the crate README for upgrade/size limits.
Session-prefix keys are purged by `delete_remote_session_by_key` through
`purge_session_prefix`. Coordinated session deletion also removes global first-message reservations, recovery registrations, task event subjects and the scoped A2A lease. It refuses unsettled work and revision-purges authority before the log; purge tombstones fence stale writers. Checkpoint cleanup retains subject predecessors and never purges an active pending envelope. Stream policy, measured payload limits, permissions and drain rollout live in `docs/a2a-operations.md`. New per-session storage
should use the session prefix to reuse existing GC.

### Crate layering for NATS tool servers

Tool servers that need NATS object/KV storage must depend on `harnx-blob-store`,
which provides media put/get, plans KV, activity touch, and owner-scoped deletion.
This crate is intentionally isolated from `harnx-runtime` and `harnx-toolset-server`
to keep tool servers lightweight and avoid pulling in the full execution stack.

Verify layering with: `cargo tree -p harnx-blob-store` (must show no
`harnx-runtime` or `harnx-toolset-server`). Add new storage operations to
`harnx-blob-store` rather than duplicating NATS access in tool servers.

Caller session identity reaches native NATS toolsets via
`ToolInvocationContext.invoking_session: Option<SessionRef>`. The `SessionRef`
carries `agent: Option<String>` and `session_id: String` (6-char local id).
`harnx-toolset::SessionRef` and `harnx-core::cid_url::SessionRef` are duplicate
definitions to avoid circular dependencies; convert between them manually.
See `crates/harnx-toolset-server/src/invocation.rs` for construction.

### Canonical `cid:` URL scheme

Attachments and plans use canonical `cid:` URLs that identify resources by owning session and path or content digest:

- `cid:media:<agent>/<session-id>/<hash>`: Immutable media blob stored in the `harnx_attachments` JetStream object store. `<hash>` is the lowercase 64-character SHA-256 digest of the content. Cached permanently.
- `cid:plan:<agent>/<session-id>/<slug>`: Plan index document stored in the `harnx_plans` JetStream KV bucket. Resolves to rendered markdown containing the plan description, tasks table, and notes list.
- `cid:plan:<agent>/<session-id>/<slug>/tasks/<id>` and `.../notes/<id>`: Mutable plan task and note documents stored in `harnx_plans`.

URL components:
- `<agent>`: Percent-encoded agent name (`pantheon%2Fatlas`), matching Web UI route segments. Sessions without an agent use `_temp`.
- `<session-id>`: 6-character local session ID (`[A-Za-z0-9_-]+`, which may start with a hyphen).
- `<slug>` and `<id>`: URL-safe slug identifiers (`[a-z0-9-]+`).

Operational properties:
- **Capability URL**: When access rules are off, the URL is the capability (bearer token) — possession grants access without per-resource ACLs. When access rules are enabled, `/v1/cid/*` requires a valid session-authorization check against the caller identity (see `cid.rs` — authorization runs before blob resolution and ETag handling).
- **Activity renewal**: Any attachment or plan read or write touches the owning session's activity timestamp (`SessionActivity.last_activity_at`), debounced in-process to at most one write per hour. This resets the session retention clock while resources remain in active use.
- **Session deletion cascade**: Session deletion (`delete_owner`) purges both `media/<owner>/` objects in `harnx_attachments` and `plan/<owner>/` keys in `harnx_plans`.
- **Plans storage location**: Plans live exclusively in NATS JetStream KV (`harnx_plans`); previous filesystem storage under `.agent` is retired.


### Run-deadline cancellation is invocation-fenced

Worker deadline timeouts use `interrupt_invocation` (in `nats_session/interrupt.rs`),
binding the cancellation to the specific admission's prompt sequence. Unlike
session-level `interrupt_session`, which targets the current turn, invocation-fenced
cancellation checks the terminal boundary before appending a `Cancel`. If the
invocation already completed or an earlier cancellation won, the fenced call
returns without appending. Late timer callbacks cannot cancel a subsequent
independent run in the same session.

This fencing is distinct from lease loss or worker failover, which set a
nonterminal abort flag without publishing a cancellation.

When running turns across multiple sessions, each `NatsSession` needs its own
`AbortSignal`. `NatsSession::interrupt` calls `abort_signal.set_ctrlc()`, so a shared
abort flag poisons every later turn in unrelated sessions. Process-wide abort belongs
only on the supervisor. A session handle used for orphan reconciliation also cannot be
reused; its abort flag is now set, and a replacement turn must create a fresh handle.

### Changing a JetStream consumer's configuration

`Stream::get_or_create_consumer` returns an existing consumer unchanged, so a
configuration change made there never reaches a cluster that already has the
consumer. Use `create_consumer`, which updates it, and pass every field: an
update replaces the whole configuration, and async-nats leaves zero values
out, so an omitted field falls back to the server default. `backoff` has three
effects the activation consumers depend on (`activation_transport.rs`): NATS
replaces `ack_wait` with `backoff[0]`, a progress ack restarts the timer
against the current delivery's entry, and a NAK asking for `delay` on delivery
k waits `delay - backoff[0] + backoff[k-1]`. A message that reaches
`max_deliver` is never delivered again, even after the limit is raised, and
lowering the limit below a message's delivery count drops it without the
max-deliveries advisory. See "Delivery limits" in `docs/nats-ha.md`.

### Interrupt acceptance

Interruption is established by one durable `Cancel` entry in the session log and
by nothing else. Acceptance is that append landing, not a tool acknowledgement,
a lease disappearing or a process exiting. TUI, one-shot and followers return on
it; don't reintroduce a wait for tools to die before the editor comes back.

Tool protocol v5 carries `CancelAcceptance::{Accepted, AlreadyFinished, Rejected,
Unknown}` and nothing else. There is no cleanup status to report and no physical
shutdown to confirm — a cancel is best effort and its acknowledgement is for
logging. Cancels are idempotent, so resending one is always allowed and is how
wind-up and resume reach a call whose original invocation is gone. Don't add a
parallel cleanup flag, and don't read missing metadata as proof anything stopped.

Resource owners keep cleanup handles outside turn/reply futures. A dropped
`JoinHandle` detaches its task, and started `spawn_blocking` work can't be aborted.
MCP cancellation closes only the request waiter; never restart shared
infrastructure to cancel a call. See `docs/nats-ha.md` under "Interruption".

The TUI runs broker requests as spawned tasks and only checks their join
handles from its render loop (`pending_exit_cancel` in `harnx-tui/src/types.rs`).
That loop blocks in the terminal poll for up to one 80 ms tick and looks at a
pending request with `now_or_never`, so a bare future polled from the loop
advances one await per tick. An interrupt request awaits one JetStream round
trip per session log entry; driven from the loop, a 300-entry session's Ctrl+C
spent 22 seconds attaching and then timed out inside its two-second append
budget on every retry. Don't hand the loop a bare future to poll.

Reads on the interrupt path stay at one entry. `interrupt_session`, the
worker's hint check and the fenced-append tail lookups use
`NatsSessionLog::last_entry_async` and decide from the log's last entry; the
fenced append's conflict brings back anything newer. A whole-log read there
costs one JetStream request per entry and put long sessions past the two-second
append budget on remote clusters. The known cost is a stray `Cancel` when an
idle session's last entry is a mutation or control entry.

### One-shot timeout/abort select loop purity

The CLI one-shot `run_turn_select_loop` (`crates/harnx/src/oneshot_nats.rs:188-234`)
uses a pure-race pattern: each `tokio::select!` arm returns a `TurnLoopOutcome`
variant without side effects. The `session.interrupt()` call that sets
`abort_signal.set_ctrlc()` runs *after* the select completes, in the
`run_turn` match block (`oneshot_nats.rs:275-292`). This prevents a race where
a biased re-poll could see the signal set by an in-arm interrupt and select
the wrong branch (#1743). When adding arms or modifying this loop, keep them
pure signal checks — no `.await` calls, no shared-state mutations inside select
arms.

Build the workspace before cross-process tests after changing tool or hook wire
types. Those tests launch workspace sidecars as well as linked test code; a stale
hook or tool binary can fail decoding even when a per-crate build succeeds.

### TUI transcript items are TUI-local

`TranscriptItem` (`harnx-tui/src/types.rs`) derives only `Clone + Debug` — it is **not** serialized to
NATS. Adding a field or variant is a local TUI change, not a transcript-protocol change. Contrast
with `SessionLogEntry` variants (previous section), which are protocol-versioned.

New `TranscriptItem::ToolCall` fields (`start_anchor`, `final_elapsed_ms`, `id`) support tool-call
timer display and completion correlation. These are TUI-local; no protocol change.

### The config lock and blocking NATS calls

Never hold a `GlobalConfig` guard across a NATS round trip. Persistence
reaches NATS from synchronous code through `block_in_place` plus
`Handle::block_on`, and the reply is read only while some Tokio worker polls
the I/O driver. The multi-thread runtime lets one parked worker poll it at a
time. If that worker wakes, runs a task that then blocks on the config lock,
and the lock holder is waiting for a NATS reply, nothing reads the reply.
Both sides stay parked at zero CPU, `/healthz` stops answering and the
broker eventually drops the connections. Staging workers hung this way when
a title write held the per-session write guard while
`wait_for_post_turn_maintenance` blocked on `config.read()`.

Take what you need under a short guard, drop it, do the round trip, then
take the guard again to apply the result. `session::record_title` and
`CompactionLog` (`config/session.rs`) re-check that the session id still
matches before applying. `persist_active_session_override` and
`Config::switch_model` don't, and rely on front-end commands running one at a
time. Keep the sequence free of `.await` when a dropped future must not
leave it half done, as compaction does. A loop that polls session state uses
`try_read` and treats a held lock as still busy
(`Config::session_maintenance_pending`).

Two safeguards cover what is left:

- `ConfigLock` is a `YieldingRwLock` (`config/lock.rs`). A contended
  acquisition on a multi-thread runtime waits inside `block_in_place`, so the
  worker hands its core on and the driver keeps being polled. It is a backstop,
  not permission to hold the lock across I/O.
- The agent loop's transcript appends (`before_chat_completion`,
  `append_session_tool_calls`, `prepare_session_tool_results`,
  `prepare_after_chat_completion`) still append under `config.write()`. They
  depend on it to keep the in-memory transcript in log order, and they rely
  on the yielding lock to stay deadlock-free.

`config::lock::tests::contended_waiter_keeps_the_io_driver_polled`
reproduces the hang: with a plain `parking_lot::RwLock` it deadlocks on every
run. `config/tests/lock_free_persistence.rs` checks each persistence path
with a sink that fails if the lock is still held.

### Spawning long-lived child processes

Spawn any child that must not outlive harnx through
`harnx_core::child_process::ChildProcessManager`, not `Command::spawn` directly.

`kill_on_drop(true)` alone is not process-exit cleanup. It fires only when the
`Child` value is dropped, so a handle reachable from a `static` — a process-wide
registry, a cache, a `OnceLock` — is never killed at all: Rust does not run
destructors on statics at process exit, and the child reparents to PID 1. The
llama-server registry in `crates/harnx-client/src/llama_server/process.rs` relied
on this and stranded a live `llama-server` on every exit, which accumulated into
thousands of orphaned mock servers across test runs.

`ChildProcessManager` has the kernel enforce it instead (`setpgid` +
`PR_SET_PDEATHSIG`), which also covers panic, abort, and SIGKILL. It spawns from
one stable OS thread because Linux binds `PR_SET_PDEATHSIG` to the *thread* that
forked: spawning straight from a Tokio worker lets `block_in_place` hand that
worker to the blocking pool, whose idle threads retire and take healthy children
down with them.

`PR_SET_PDEATHSIG` is Linux-only and has no portable equivalent, so on macOS and
Windows a child held by a `static` still outlives its parent — the manager only
puts it in its own process group there. Gate tests that assert parent-death on
`target_os = "linux"`, as `harnx-core`'s own child-process tests do. Anything
that must be cleaned up off Linux needs an explicit shutdown path instead.

Keep `kill_on_drop(true)` as well — it retires the child promptly when its
manager is dropped while the process keeps running.

Test helpers that spawn a broker need the same guarantee for a narrower reason:
a failing assertion unwinds past the helper's own `kill`/`wait`, and
`std::process::Child` does not reap on drop. The stranded `nats-server` then
competes with every later broker test in that nextest run, and `retries = 3`
turns one flake into several. `spawn_test_nats` returns `TestNatsServer`
(`crates/harnx-runtime/src/nats_worker/tests.rs`) and the integration harness
returns `NatsServerHandle` (`crates/harnx-runtime/tests/it/common/mod.rs`); both
reap in `Drop`. Keep new broker helpers in that shape.

### NATS routing role is per-binary, not derived from env

`Config.nats_routing` (`NatsRouting::{Default,Cluster(name),FrontendLocal}`) controls
whether a process resolves the reserved `__local__` cluster locally or rejects it.
The role MUST be set per-binary at bootstrap, never inside shared `Config::init`/
`init_headless`:

- **Front-ends** (`harnx` CLI, `harnx-serve`): call `apply_frontend_nats_routing()`
  after `Config::init`, which reads `HARNX_NATS_SERVER` and sets the role to
  `Cluster(name)` when present, or `FrontendLocal` when unset. Under `FrontendLocal`,
  `resolve_nats_server(__local__)` resolves to the auto-managed, file-lock-elected
  local broker and ignores operator `HARNX_NATS_URL`/`HARNX_NATS_TOKEN`.
- **Workers/tool servers**: `init_headless` without the call, keeping
  `NatsRouting::Default` so their injected `HARNX_NATS_URL/TOKEN` handoff reaches
  `resolve_nats_server(__local__)`.

Deriving the role from `HARNX_NATS_SERVER` inside shared init would break
workers: a worker must read its parent's injected `HARNX_NATS_URL/TOKEN`
endpoint for `__local__`, but it can inherit an operator's `HARNX_NATS_SERVER`
from the container environment. Because the worker never calls
`apply_frontend_nats_routing()`, it stays `Default` regardless of that env var.
The seam is `resolve_nats_server()` at `nats_split.rs:234`:
`NatsRouting::Default` → reads env handoff; `NatsRouting::Cluster` → bails;
`NatsRouting::FrontendLocal` → resolves auto-managed local broker.

### NATS/GC tests: CI coverage and isolation

CI runs integration tests against an isolated `nats-server` per test via
`spawn_nats_server` in `crates/harnx-runtime/tests/it/common/mod.rs`. The CI
workflow installs `nats-server` on all platforms (`.github/workflows/ci.yaml`);
tests that skip when the binary is absent still pass, but real coverage requires
the installed binary.

In-module `#[cfg(test)]` tests that gate on `HARNX_NATS_TEST_URL` (unset in CI)
**do not run in CI** — they skip when that env var is missing. The test
sandbox drops it, so they skip locally too: running them takes
`HARNX_TEST_SANDBOX=off` as well as the URL. Those tests also
share one physical server and the global `SESSION_METADATA_BUCKET` when run
locally, so they contaminate each other's state. New NATS/GC tests that must run
in CI belong in `crates/harnx-runtime/tests/it/`, use `spawn_nats_server` for
per-test isolation, and assert on specific session IDs rather than global
bucket stats. Precedent: `tests/it/worker_remote_session_cleanup.rs`.

A TUI test that spawns the local broker or worker isolates them with
`TestEnvironment` (`crates/harnx-tui/src/test_utils/environment.rs`) under
`ENV_LOCK`. Under nextest on Linux and macOS the test sandbox already gives
each test its own harnx data directory, and with it its own broker, but
Windows and `HARNX_TEST_SANDBOX=off` runs have no sandbox. There a test
without `TestEnvironment` shares the user's broker directory with every other
test process in the run, including the persisted broker port, and a port
still held by a broker another process just stopped fails every spawn attempt.

A test can start a broker without meaning to. Test configs keep `Default`
routing and get no broker handoff, so any `__local__` lookup starts or joins
the local broker in the test's harnx data directory. Under the sandbox that is
a private one, and the test pays for starting it. In an unsandboxed run it is
your real one: the test joins your broker, or starts it and waits for
nats-server to recover its whole JetStream store (3s for a 4 GB store), then
stops it on exit for the next test process to start again. That is why
`Tui::init` skips its unread lookup under `cfg(test)` and tool rounds skip
discovery without `HARNX_NATS_URL`. To check a test, run it with
`HARNX_TEST_SANDBOX=off` and `HARNX_DATA_DIR` set to an empty directory, and
see whether `nats/v1` appears there.

A single broker can stand in for a replica that lags the stream leader, which no
real cluster produces on demand. A `mappings` block in the server config diverts
`$JS.API.DIRECT.GET.<stream>` (and `.>`) to an empty stream, which answers a
genuine 404 the way a lagging follower does, and `$JS.API.CONSUMER.CREATE.<stream>`
(and `.>`) to a subject nothing serves. `STREAM.MSG.GET` and `STREAM.INFO`, which
only the leader answers while the stream has one, still reach the real stream.
`crates/harnx-toolset-server/tests/it/stale_replica.rs` does this for the
invocation journal, with a probe that fails the test if the diversion stops
applying. `crates/harnx-runtime/tests/it/nats_session_metadata_stale_replica.rs`
does the same for the session metadata bucket, starting its broker with
`common::spawn_configured_nats_server`. A leaderless stream, where every
replica answers `STREAM.INFO`, is simulated the same way: `journal_listing.rs`
maps that request to a responder that answers as such a replica does.

To assert what JetStream work an operation costs, subscribe to the API subject on
the test broker: requests to `$JS.API.STREAM.CREATE.<stream>` (opening a KV
bucket) and `$JS.API.STREAM.INFO.<stream>` (a key listing, with its
`subjects_filter` in the payload) reach an ordinary subscriber in the same
account. Make a round trip on the subscriber's connection, such as
`query_account`, after subscribing and again before counting: `flush` only
writes the client's buffer, so without it the subscription may not be
registered when the operation starts, and a request may still be on its way.
A test that subscribed and flushed missed its one listing about once in twenty
stress runs. `journal_layout.rs` checks each journal lookup lists only its own
round this way, and `nats_tool_provider/tool_round.rs` that a provider opens
the journal once.

## CLI Flag Constraints

The root `Cli.file: Vec<String>` has `#[clap(short, long, global = true, hide = true)]` at
`crates/harnx/src/cli.rs:42`. This reserves `-f` globally for `--file`. New format flags (e.g.
`--format`) MUST NOT define a short form — use long-only. See `sessionFormat` in
`harnx-runtime/src/config/session_format.rs` and the `--format` flag in `harnx/src/cli.rs`
for the precedent (PR #1448).

## Durable vs Advisory Event Contract

`SessionLogEntry` (`harnx-core/src/session.rs`) is durable and persisted to NATS JetStream.
`AgentEvent` (`harnx-core/src/event.rs`) is advisory and emitted to the lossy fan-out subject
`sessions.{id}.events`. The durable entry is authoritative; advisories are best-effort previews.

### Session status vs the `/events` wake-up channel

The SSE `/events` endpoint (`session_routes.rs:173`) carries only lossy wake-ups
(`session-updated {after_seq}`, `read-updated`), NOT the authoritative run lifecycle.
Run status flows through the AG-UI run stream:

- **RUN_STARTED/RUN_FINISHED/RUN_ERROR**: emitted by the assistant-ui POST to
  `/v1/agents/{agent}/sessions/{session}` (`ag_ui.rs:1219`). The server derives
  `RUN_FINISHED` from durable completion (`TurnEnd` or `Cancel`); `RUN_ERROR`
  signals worker loss or task failure.
- **turn_interrupted / hitl_pending_approval**: reconstructed from the durable
  log and sent as AG-UI `CUSTOM` events during promptless attach (`ag_ui.rs:1640`).

The Web UI NO LONGER polls JSON-RPC `session/get` for status. Sub-agent rows
derive `running`/`done`/`failed`/`cancelled`/`awaiting_approval` from AG-UI events
on the child stream (`SubAgentSessionNotes.tsx:81`); foreground interruption uses
local optimistic state cleared by the run terminal. When status appears stale,
reload re-hydrates from the authoritative log.

### Follow mode MUST emit durable entries only

`SessionEventStream::attach()` (`nats_event_sink.rs:386`) subscribes to advisories first, then
loads durable history. Live clients call `refresh_history()` on wake to poll for newly committed
entries — this is REQUIRED because some durable entries (e.g. `TurnEnd`) have NO advisory event.

When implementing `--follow` or live-tail rendering (CLI or TUI):

1. Replay `stream.history()` for initial output.
2. In the follow loop, `tokio::select!{ next() | timeout | ctrl_c() }`.
3. On wake, record `old_len = stream.history().len()`.
4. Call `stream.refresh_history().await` and emit `history()[old_len..]` as `SessionLogEntry`. On transient failure, log and continue polling — reads fail fast; the caller owns retry cadence (see `CompletionPoller` in `nats_session/completion.rs`).
5. NEVER serialize `AdvisoryEnvelope.event` directly — it's a preview, not durable state.

`--follow` is read-only observation; it does not interrupt or cancel the running session.

### Entry rendering for text output

`replay_entries_to_sink()` (`nats_session.rs:1473`) renders `SessionLogEntry` tuples through a
frontend's `AgentEventSink`. Callers must first apply log mutations (edits/rewinds) if they need
an effective snapshot — the helper renders entries in the supplied order without resolution.
Control entries (`TurnEnd`, `HandoffCommitted`, `HitlApproval*`) are silent
in text rendering because their state hydrates separately from human transcript output.
Legacy `SubAgentStarted` entries are silent too and hydrate no frontend state;
they still render a runtime note into model context.

`MessageContent::to_text()` (`harnx-core/src/message.rs`) extracts text only and drops image
parts — it's for LLM-facing contexts. Human-readable transcripts (CLI dump/`--follow`, TUI
history, REST `/history`, session-history search) must use `to_transcript_text()` instead,
which renders `ImageUrl` parts as `[image attachment: <cid>]` markers. This ensures image-only
messages produce non-empty output. Render sites: `nats_session.rs:render_message_entry`,
`harnx-tui/src/lifecycle.rs:messages_to_transcript_items_for_cluster`,
`harnx-serve/src/lib.rs:history_message_content`, and
`harnx-runtime/src/session_history.rs:entry_searchable_text`.

### Metadata rendering requires session reconstruction

Text-format session metadata (`.info session` or `harnx info session`) uses `session::render()`
(`config/session.rs:500`) which requires a reconstructed `Session` with model resolution and
token counts. The helper `load_session_for_render()` (`session_format.rs:88`) loads KV metadata,
resolves the model via `overrides.model` or the named agent's config, reconstructs transcript-
derived fields (`turns`, `tokens`), and calls `update_tokens()`. Do NOT pass the raw
`SessionMetadata` KV record to `session::render()` — it lacks resolved model and transcript state.

## Usage Accounting Semantics

`ModelEvent::Final.usage` (`harnx-core/src/event.rs:66-71`) is a **display-only per-turn total** that
sums every model completion in that turn's tool loop. Session cumulative totals and status-bar
metrics use a **separate mechanism**: `record_completion_usage` in `config/mod.rs:1082-1085` writes to
`Session.completion_usage` per model call. These mechanisms are independent. Anyone modifying usage
display must keep them separate or they'll double-count.

Per-tool-call display usage in `ToolEvent::Update.usage` is also display-only and non-cumulative; it
replaces on each update and is stored in ACP `_meta.harnx:usage` (`harnx-acp-server/src/event_map.rs:254`,
`event_map.rs:354-357` for subagent projection).

## Tool Progress Patch Semantics

`ToolUpdatePatch` and `ToolDisplayState` (`harnx-core/src/tool.rs:270-368`) implement pure merge:

- `None`/omitted = unchanged
- `Some(vec![])` for collections = clear
- Collections **replace** (never append)
- Usage snapshots **replace** (never sum)
- Terminal status (`Completed`, `Failed`) cannot be set by patches; runtime owns that truth

Surfaces (TUI/Web/CLI) and the ACP mapper must apply these semantics. Tests enforce terminal-status
guard (`display_state_terminal_status_ignored` in `tool.rs:1443`).

### `title` vs `markdown` in `ToolEvent::Update`

`title` is a concise activity label; `markdown` is rendered body content. Historical lesson `5960f7d0a`
(PR #418) split them after overloading caused confusion. Don't merge them.

### `call_tool_with_progress` is opt-in

`ToolProvider::call_tool_with_progress` (`harnx-core/src/tool.rs:243-253`) delegates to `call_tool_with_id`
by default. Provider decorators/wrappers that forward only legacy methods silently swallow updates.
Engine dispatch must call the `_with_progress` variant to enable progress.

### Tool-call ID assignment

Tool calls receive stable UUID IDs via `ensure_tool_call_ids` (`harnx-engine/src/tool.rs:201-207`) before
session transcript persistence and provider dispatch. The runtime calls the same helper. Empty or missing
IDs are replaced with fresh UUIDs; existing non-empty IDs are preserved. Legacy orphan repair assigns
IDs before cloning calls so recovery position matching remains valid.

### Progress emission and sink capture

`RuntimeToolProgress` (`harnx-engine/src/progress.rs`) coalesces rapid updates with a 250ms budget. First
meaningful update emits immediately; subsequent updates merge into pending state and flush after the
interval or synchronously in `finalize()`. Abort and terminal states reject later updates. The runtime
captures `current_agent_event_sink()` when building `emit_tool_update_fn` (`harnx-runtime/src/tool.rs:279-282`);
tools emitting from `tokio::spawn` see the originating turn sink, not a stale task-local or global fallback.

CLI sink rate-limits streamed tool-update notices (`TOOL_UPDATE_THROTTLE_MS = 2000` in `cli_event_sink.rs`)
and deduplicates on title. A notice prints only when the title changes and at least 2 seconds have passed
since the prior emission for that call. Entries initialize on `ToolEvent::Started`, clean up on terminal
events (`Completed`, `Failed`, `Blocked`) or `clear_tool_timers()` at turn end. The throttle is per-call,
not global, so concurrent tools emit independently.

### Native toolset progress handle

`ToolInvocationContext.progress` (`harnx-toolset/src/lib.rs:196`) is a cloneable `ToolProgressHandle`
passed to `Toolset::invoke_with_context`. Defaults to a no-op sink when the request lacks the
`harnx:tool_progress` capability. Tools call `progress.update(patch)` to emit live state; the handle
is call-bound and safe to clone into spawned tasks. Implementations that forward only `Toolset::invoke`
will not receive the handle.

The server-side `ProgressPublisher` (`harnx-toolset-server/src/progress.rs`) coalesces on a 250ms
interval, mirroring the engine's `RuntimeToolProgress`. First update publishes promptly; subsequent
updates merge pending state; `finish()` yields the final bounded snapshot.

### Wire capability and producer bounds

Clients enable tool progress by including `CAPABILITY_TOOL_PROGRESS = "harnx:tool_progress"`
(`harnx-toolset/src/progress.rs:10`) in `ToolRequest.capabilities`. Absent capability: no NATS
progress messages publish, no `final_progress` snapshot attached.

Producer-side bounds are UTF-8-safe and enforced before publication
(`ToolProgressPatch::bounded()`):

- `TOOL_PROGRESS_MAX_STRING_BYTES = 4 KiB` — title, path-like fields
- `TOOL_PROGRESS_MAX_MARKDOWN_BYTES = 64 KiB` — rendered markdown body
- `TOOL_PROGRESS_MAX_LOCATIONS = 64` — location blocks
- `TOOL_PROGRESS_MAX_CONTENT_BLOCKS = 64` — structured content blocks
- `TOOL_PROGRESS_MAX_CONTENT_BYTES = 64 KiB` — aggregate serialized content
- `TOOL_PROGRESS_MAX_IMAGE_BYTES = 16 KiB` — single image data block

Terminal status values (`Completed`, `Failed`) are removed before publication; runtime owns terminal
truth. Tests verify: `terminal_status_is_removed_before_publication` (`progress.rs:390-396`).

### Final progress snapshot on ToolReply

`ToolReply.final_progress` (`harnx-toolset/src/lib.rs:381-382`) carries the last bounded snapshot
outside `result`, so it never enters model-facing tool output. Journal, reply cache, and replay
preserve the field. Fast completion (e.g., cache hit) still carries the snapshot because the
publisher flushes before awaiting the reply.

### Subagent progress projection into tool updates

`TurnEvent::SubAgentProgress` projects into `ToolCallUpdate` for ACP and Web clients, bridging
the legacy poll-based reporter with the shared tool-call rendering path. Key invariants:

1. **Correlation key** — `SubAgentProgress.invocation_id` maps to the parent tool call's
   `tool_call_id`, not the child session. A child session can be prompted more than once,
   so `invocation_id` is the stable correlation key.

2. **ACP status mapping** — `SubAgentProgressStatus` has six variants; ACP `ToolCallStatus`
   has four. Rich internal states (`Cancelling`, `Unconfirmed`) and child `Done` all map to
   `InProgress`. The parent tool call remains in progress until `ToolEvent::Completed` arrives.
   Only `Cancelled` and `Failed` map to `Failed`.

3. **Title format** — Projected title includes agent name, child title (if present), and compact
   usage: `@ {agent} — {child_title} — ({in}→{out})`. Zero usage omits the arrow suffix.

4. **Structured usage** — ACP has no native usage field; usage is placed in namespaced
   `_meta.harnx:usage` (`harnx-acp-server/src/event_map.rs:354-357`). Web also includes usage
   in the `tool_update` payload.

5. **Dual emission** — Web sink emits both legacy `sub_agent_progress` and projected `tool_update`
   custom SSE events, allowing incremental client migration. ACP emits only the projected
   `ToolCallUpdate`.

6. **Reporter cadence** — `SubagentProgressReporter` publishes a running snapshot as
   soon as it starts, then one per metric change and one per 10-second heartbeat
   (`SUBAGENT_PROGRESS_HEARTBEAT` in `subagent_toolset.rs`). The projection layer
   does not add throttling.

Implementation: `subagent_progress_to_update` in `event_map.rs:320-360` (ACP), and
`emit_subagent_progress_update` in `ag_ui.rs:578-610` (Web). Tests enforce status mapping
(`subagent_progress_preserves_internal_states`, `subagent_progress_done_keeps_parent_tool_in_progress`
in `event_map_tests.rs`; `subagent_progress_projected_status_mapping` in `ag_ui_subagent_tests.rs`).

### Terminal Agent Status Semantics

The TUI emits OSC 9999 (Orca) and OSC 9;4 (kitty/JetBrains) sequences to signal agent state to
compatible terminals. Implementation in `crates/harnx-tui/src/terminal_status.rs` with process-global
`TERMINAL_STATUS` state (`LazyLock<TerminalStatusState>`).

Key invariants (verified by tests in `terminal_status.rs`):

1. **Sticky-failure rule** — `Error` and `Interrupted` states are sticky: they cannot be downgraded to
   `Done` by the shared turn-end path. Only a new `Working` status resets and allows progression to
   `Done`. This prevents a successful completion from overwriting a failure the user should see.

2. **Wire protocol constraint** — orcatui rejects JSON `"failed"` in OSC 9999 payloads. Error state
   uses `"interrupted"` for compatibility: `{"state":"interrupted"}`. ConEmu progress (OSC 9;4) uses
   state=2 (red bar).

3. **Cancellation ordering** — when settling an interrupted prompt (`cancellation.rs`), `llm_busy` must
   be set to `false` **before** calling `cancel_tool_confirm()`. If reversed, `cancel_tool_confirm()`
   sees `llm_busy == true` with an active modal and emits a transient `Working`, producing a flicker.
   The emission at prompt-interrupted must be `Interrupted`, not `Working`.

4. **Modal resolve emission** — resolving a tool confirmation modal emits `Working` only when both:
   - `was_confirm_modal`: a `ConfirmToolUse` modal was actually open
   - `llm_busy`: the LLM is still processing in the tool loop

   If the modal was dismissed or `llm_busy` is false (e.g., cancellation already cleared it), no
   `Working` emission occurs from the modal-close path.

5. **Teardown and editor suspend** — `force_clear()` emits `Clear` but preserves `last` in the state
   so `restore()` can re-emit the active status when resuming from `$EDITOR`. This allows a transient
   clear during external-editor suspend without losing the semantic state.

Configuration: `terminal_status: bool` in `config.yaml` (default `true`) or `HARNX_TERMINAL_STATUS=0`.
Auto-disabled when stdout is not a TTY, `TERM=dumb`, or `CI` is set. User-facing docs in
`docs/configuration-guide.md` under "Terminal Status".

### TUI Markdown link extraction and transcript order

Markdown links in tool bodies and results are extracted into `TranscriptItem::MarkdownLink` rows for keyboard accessibility. Key invariants:

1. **Call-owned links sit between body and result** — `detail_view.rs:323-330` skips `MarkdownLink` items when pairing a `ToolCall` with its result. Updates must not assume `index+1` is the result.

2. **Reply+link blocks are atomic and owned** — `subagent_sessions.rs::insert_reply_before_status_row` splices `ToolResultMarkdown` followed by its `MarkdownLink` rows in a single operation. Re-delivery replaces only rows tagged with the same session key and invocation in `subagent_reply_owner`. Text equality isn't ownership: an adjacent ordinary result can have identical text.

3. **Live/history parity via shared constructor** — `tool_transcript.rs::tool_completed_to_transcript_items` extracts from the untruncated Markdown template when provided, otherwise falls back to full output. Main/child live events and history rebuild (`lifecycle.rs`) delegate to this helper. Preview regression fixtures must put links outside both the terminal-sized head and the non-TTY default tail (75 lines); placing them at the end doesn't prove truncation in CI.

4. **`ToolResultMarkdown` is non-navigable by design** — `types.rs:1003-1007` excludes it from `is_navigable()`. Keyboard focus goes to the preceding `ToolCall`; link rows provide URL visibility and selection.

5. **Intermediate assistant text closes at tool boundaries** — `assistant_transcript.rs` projects links from the complete streamed Markdown before tool rows, thoughts, source changes and turn end. `Started`/`Blocked` end the model response and retire its final-replacement target. Notices, thoughts and child source changes close aggregation but preserve the parent's canonical `Final` target. `streamed_text_idx` tracks the displayed source separately from `main_streamed_text_idx`. Refresh only the contiguous owned link block and rebase focus, selection anchor and tracked stream indices; surviving URLs retain their targets when a canonical final changes link order. Direct cancellation and task-error handlers must close the stream before resetting trackers; event preprocessing doesn't cover those calls. Actual `ThoughtText` stays plain text, with no link extraction.

See tests in `markdown_link_accessibility_tests.rs`, `intermediate_assistant_links.rs` and `subagent_session_tests.rs`.


### TUI tool-call row in-place updates

Live tool progress updates (`ToolEvent::Update`) mutate the active `TranscriptItem::ToolCall` row in-place instead of appending detached `StatusLine` items. Shared reducer logic in `crates/harnx-tui/src/tool_render.rs` (`apply_tool_event_update`, `complete_tool_call`, `fail_tool_call`) handles both main transcript (`agent_events.rs`) and subagent child transcripts (`subagent_transcript.rs`).

Key invariants (verified by `test_inplace_tool_call_update_sequence`, `test_tool_update_fallback_late_update_after_completed_ignored`, and related tests in `tool_live_updates_tests.rs`):

1. **Late update rejection** — `apply_tool_update` checks `final_elapsed_ms.is_some()` and returns `false` if set. Updates after completion/failed state are ignored, preventing stale late arrivals from corrupting a frozen row.

2. **Terminal status guard** — `apply_tool_update` rejects `ToolStatus::Completed` and `ToolStatus::Failed` in the patch. Only `complete_tool_call` and `fail_tool_call` can set terminal status, ensuring timer freeze, cache invalidation, and result item attachment happen atomically.

3. **Fallback synthesis uses `"tool"` sentinel** — when `apply_tool_event_update` finds no matching running row, it synthesizes a minimal row with `tool_name: "tool"`. This prevents duplicate title rendering (P-DUPTITLE): the renderer suppresses the title suffix when `tool_name == title`.

4. **Render cache bypass for running tools** — running rows (`final_elapsed_ms.is_none()`) bypass `rendered_cache` on every render pass to show ticking timer and spinner frame updates. Only completed rows (`!is_running`) populate the cache.

### Web `tool_update` SSE event and client reducer

The web client (`harnx-serve`) emits `tool_update` custom SSE events for `ToolEvent::Update`, carrying `{ tool_call_id, markdown?, status?, title?, kind?, locations?, usage? }`. Only present fields are included; omitted fields mean "no change".

The client-side reducer (`web/src/toolUpdates.ts`) applies patch semantics matching the Rust side:

- Omitted/undefined fields leave current state unchanged
- `locations: []` explicitly clears locations (empty array is meaningful)
- Terminal status (`Completed`, `Failed`) from live updates is ignored — UI status comes from the tool result, not from patches

The `ToolCallCard` component uses live fields when present: `title` overrides `toolName`, `markdown` takes precedence over `toolSummary`, `kind` maps to icons, and `locations` display as `file:line` pairs. Presentation logic (`web/src/toolCallPresentation.ts`) enforces precedence: error/action-required icons and border colors never get masked by live `kind` or `status`.

### Tool confirmation modal ordering and delivery

When a `PreToolUse` hook returns `permissionDecision: "ask"`, the TUI modal queues an optional user message via durable JetStream append before sending the approval reply. Worker reloads the session log at the tool seam, ensuring the agent sees `tool call → tool result (real or blocked) → queued message`.

Key invariants (verified by `denied_zero_execution_round_injects_queued_messages_once_after_blocked_result` in `crates/harnx-runtime/tests/it/nats_tool_confirmation.rs`):

1. **Order-barrier** — frontend awaits JetStream PubAck before replying. Worker receives decision only after message is durable.
2. **Origin capture** — modal state captures `(session_id, cluster)` at open; enqueue targets that origin, not the currently-active session.
3. **Idempotent dedup** — stable `submission_id` (UUID generated at modal open) reused on retry; duplicate JetStream appends are safe.
4. **Fail-closed paths** — append failure keeps modal open with draft; route closure, dismissal, and Ctrl+C all resolve false without enqueue.

The 2-second keyboard-idle gate on approval (`TOOL_CONFIRM_IDLE_GATE` in `harnx-tui/src/tool_confirmation.rs`) resets on every keypress and paste into the message textarea. Ctrl+J is a newline fallback when Shift+Enter is unavailable.

### TUI printable-character keybindings with SHIFT-tolerant matching

Crossterm may report shifted printable characters (`<`, `>`, `G`) with `KeyModifiers::SHIFT` set on
some terminals. Binding these characters with strict `KeyModifiers::NONE` causes the match arm to
silently never fire.

Pattern for shift-sensitive char bindings:
```rust
(KeyCode::Char('g' | '<') | KeyCode::Home, KeyModifiers::NONE | KeyModifiers::SHIFT) => { ... }
```

Accept `NONE | SHIFT` on char arms (not CONTROL/ALT combinations). Home/End keycodes don't need
SHIFT tolerance — they're not char keys. See AgentPicker in `input_modal.rs` for the `||` guard variant,
and jump-key handlers in `detail_view.rs`/`input.rs`/`subagent_sessions.rs` for the or-pattern form.


### Terminal Agent Status Semantics

The TUI emits OSC 9999 (Orca) and OSC 9;4 (kitty/JetBrains) sequences to signal agent state to
compatible terminals. Implementation in `crates/harnx-tui/src/terminal_status.rs` with process-global
`TERMINAL_STATUS` state (`LazyLock<TerminalStatusState>`).

Key invariants (verified by tests in `terminal_status.rs`):

1. **Sticky-failure rule** — `Error` and `Interrupted` states are sticky: they cannot be downgraded to
   `Done` by the shared turn-end path. Only a new `Working` status resets and allows progression to
   `Done`. This prevents a successful completion from overwriting a failure the user should see.

2. **Wire protocol constraint** — orcatui rejects JSON `"failed"` in OSC 9999 payloads. Error state
   uses `"interrupted"` for compatibility: `{"state":"interrupted"}`. ConEmu progress (OSC 9;4) uses
   state=2 (red bar).

3. **Cancellation ordering** — when settling an interrupted prompt (`cancellation.rs`), `llm_busy` must
   be set to `false` **before** calling `cancel_tool_confirm()`. If reversed, `cancel_tool_confirm()`
   sees `llm_busy == true` with an active modal and emits a transient `Working`, producing a flicker.
   The emission at prompt-interrupted must be `Interrupted`, not `Working`.

4. **Modal resolve emission** — resolving a tool confirmation modal emits `Working` only when both:
   - `was_confirm_modal`: a `ConfirmToolUse` modal was actually open
   - `llm_busy`: the LLM is still processing in the tool loop

   If the modal was dismissed or `llm_busy` is false (e.g., cancellation already cleared it), no
   `Working` emission occurs from the modal-close path.

5. **Teardown and editor suspend** — `force_clear()` emits `Clear` but preserves `last` in the state
   so `restore()` can re-emit the active status when resuming from `$EDITOR`. This allows a transient
   clear during external-editor suspend without losing the semantic state.

Configuration: `terminal_status: bool` in `config.yaml` (default `true`) or `HARNX_TERMINAL_STATUS=0`.
Auto-disabled when stdout is not a TTY, `TERM=dumb`, or `CI` is set. User-facing docs in
`docs/configuration-guide.md` under "Terminal Status".
## Issue/task tracker

### Session Unread State

Session-level unread state tracks sessions requiring user attention. Key endpoints:

- **TUI**: In the session picker, press `'u'` or `'U'` to toggle unread on the selected session.
- **Web**: SSE `/v1/agents/{agent}/sessions/{session}/events` emits `event: read-updated` when read-state changes, triggering session list refresh.
- **JSON-RPC**: `session/mark_read` and `session/mark_unread` methods control state.

Implementation details in [`docs/nats-ha.md#session-unread-state`](docs/nats-ha.md#session-unread-state).


GitHub Issues is the issue/task tracker for this project.
