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
│   ├── harnx-plans-tools/        # MCP server: file-based plan and todo management (standalone crate)
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
cargo build --workspace                                       # Compile the project
cargo fmt --all                                               # Auto-format code (rustup uses rust-toolchain.toml version — matches CI)
cargo clippy --workspace --all-targets -- -D warnings         # Lint — treat warnings as errors
cargo nextest run --workspace --stress-count=5                # Run all tests, repeat several times to catch flaky tests
cs delta origin/HEAD                                          # Run CodeScene code quality analysis on current branch changes                                          
```

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

### Broker-backed tests and wall-clock margins

Tests that spawn a `nats-server` run in the `broker-e2e` / `heavy-e2e` groups
(`.config/nextest.toml`). On a contended GitHub runner that whole block has been
measured running 6 to 40 times its idle cost, comparing runs whose diffs did not
touch it: `cancellation::hierarchy::direct_child_cancellation_stops_only_its_worker_subtree`
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
feat: add harnx-plans-tools as a file-based plan and todo management MCP server
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

The key on the left must be one of the three packages knope versions:

- `harnx` — the entire Rust workspace. All `harnx-*` crates share one version
  (`version.workspace = true`), so use `harnx` even for a change scoped to a
  single crate like `harnx-proxy-auth` or `harnx-core`.
- `pantheon` — the `packages/pantheon` agent package.
- `coding` — the `packages/coding` agent package.

Individual crate names are **not** valid keys; `knope release` will error on
them.

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

Native toolset servers are named `harnx-<noun>-tools` (e.g. `harnx-fs-tools`, `harnx-bash-tools`, `harnx-time-tools`, `harnx-plans-tools`, `harnx-exa-tools`, `harnx-fetch-tools`). They run `harnx_toolset_server::run_toolset_main(toolset)` and default to NATS mode. For Streamable HTTP MCP mode, pass `--mcp-http`; `--host` defaults to `0.0.0.0` and `--port` selects the listening port. Default HTTP ports are:

| Server | Port |
| --- | ---: |
| plans | 3000 |
| time | 3001 |
| bash | 3002 |
| fs | 3003 |
| grep | 3004 |
| exa | 3005 |
| fetch | 3006 |

When launching behind `harnx-mcp-bridge` for stdio MCP compatibility, pass `--mcp-stdio` — without it, the server waits for NATS and the bridge handshake times out.

Binaries with `-mcp-` in the name are genuine MCP infrastructure (`harnx-mcp-bridge`, `harnx-mcp-remote`) or test fixtures (`harnx-mock-mcp`), not native toolsets.

### Adding a new native toolset server

The checklist below covers every integration point. Miss any and the release fails or the binary ships incomplete.

1. **New crate** — mirror `harnx-grep-tools` structure: `src/{lib,main,toolset,client,format}.rs`, `src/server/{mod,handler,model,params}.rs`. Implement `Toolset` (`name`, `default_mcp_http_port`, `tools`, `invoke`) and `ServerHandler` (rmcp) sharing the same handlers. `main.rs` calls `harnx_toolset_server::run_toolset_main`.

2. **Workspace Cargo.toml** — add to `[workspace] members`.

3. **release.yaml** — five spots: build `-p` list, `archive_specs`, x86_64 verify pattern, aarch64 verify pattern, dist bin `for` loop.

4. **docker/harnx.Dockerfile** — `COPY linux-${TARGETARCH}/<binary> /usr/local/bin/<binary>` line. The Dockerfile header lists the four release.yaml locations that must be kept in sync.

5. **Docs enumerating binaries** — `docs/healthz.md`, `docs/metrics.md`, `docs/environment-variables.md` enumerate tool servers in multiple places; all lists must be updated. `docs/configuration-guide.md` has a "native-servers" sentence listing examples. Illustrative mentions (time-tools examples in `docs/kubernetes-deployment.md`, etc.) do not need updates.

6. **`.gitattributes`** — if the crate ships golden `.txt` fixtures, add `text eol=lf`.

7. **Changeset** — if the change touches `packages/coding/**` or `packages/pantheon/**`, the changeset front-matter must include `"coding"`/`"pantheon"` scopes (separate knope packages with their own CHANGELOGs).

8. **MCP HTTP port** — use the next free port in the sequence (e.g., 3006 after exa's 3005).

9. **CI.yaml** — no per-crate edit needed; CI uses `cargo build --workspace` and `cargo nextest run --all`.

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
`crates/harnx-runtime/tests/tls_client_config.rs`. Check with
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
A sub-agent start in the parent log is written through the invoking tool's
handle on that same lease.

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
returns `NatsServerHandle` (`crates/harnx-runtime/tests/common/mod.rs`); both
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

CI runs integration tests against an isolated `nats-server` per test file via
`spawn_nats_server` in `crates/harnx-runtime/tests/common/mod.rs`. The CI
workflow installs `nats-server` on all platforms (`.github/workflows/ci.yaml`);
tests that skip when the binary is absent still pass, but real coverage requires
the installed binary.

In-module `#[cfg(test)]` tests that gate on `HARNX_NATS_TEST_URL` (unset in CI)
**do not run in CI** — they skip when that env var is missing. Those tests also
share one physical server and the global `SESSION_METADATA_BUCKET` when run
locally, so they contaminate each other's state. New NATS/GC tests that must run
in CI belong in `crates/harnx-runtime/tests/`, use `spawn_nats_server` for
per-test isolation, and assert on specific session IDs rather than global
bucket stats. Precedent: `tests/worker_remote_session_cleanup.rs`.

A TUI test that spawns the local broker or worker isolates them with
`TestEnvironment` (`crates/harnx-tui/src/test_utils/environment.rs`) under
`ENV_LOCK`. Without it the test shares the user's broker directory with every
other test process in the run, including the persisted broker port, and a port
still held by a broker another process just stopped fails every spawn attempt.

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
Control entries (`TurnEnd`, `HandoffCommitted`, `HitlApproval*`, `SubAgentStarted`) are silent
in text rendering because their state hydrates separately from human transcript output.

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
replaces on each update and is stored in ACP `_meta.harnx:usage` (`harnx-acp-server/src/event_map.rs:254`).

## Tool Progress Patch Semantics

`ToolUpdatePatch` and `ToolDisplayState` (`harnx-core/src/tool.rs:258-356`) implement pure merge:

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

`ToolProvider::call_tool_with_progress` (`harnx-core/src/tool.rs:232-242`) delegates to `call_tool_with_id`
by default. Provider decorators/wrappers that forward only legacy methods silently swallow updates.
Engine dispatch must call the `_with_progress` variant to enable progress.

### Tool-call ID assignment

Tool calls receive stable UUID IDs via `ensure_tool_call_ids` (`harnx-engine/src/tool.rs:182-188`) before
session transcript persistence and provider dispatch. The runtime calls the same helper. Empty or missing
IDs are replaced with fresh UUIDs; existing non-empty IDs are preserved. Legacy orphan repair assigns
IDs before cloning calls so recovery position matching remains valid.

### Progress emission and sink capture

`RuntimeToolProgress` (`harnx-engine/src/progress.rs`) coalesces rapid updates with a 250ms budget. First
meaningful update emits immediately; subsequent updates merge into pending state and flush after the
interval or synchronously in `finalize()`. Abort and terminal states reject later updates. The runtime
captures `current_agent_event_sink()` when building `emit_tool_update_fn` (`harnx-runtime/src/tool.rs:279-282`);
tools emitting from `tokio::spawn` see the originating turn sink, not a stale task-local or global fallback.

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

### TUI tool-call row in-place updates

Live tool progress updates (`ToolEvent::Update`) mutate the active `TranscriptItem::ToolCall` row in-place instead of appending detached `StatusLine` items. Shared reducer logic in `crates/harnx-tui/src/tool_render.rs` (`apply_tool_event_update`, `complete_tool_call`, `fail_tool_call`) handles both main transcript (`input.rs`) and subagent child transcripts (`subagent_transcript.rs`).

Key invariants (verified by `test_inplace_tool_call_update_sequence`, `test_tool_update_fallback_late_update_after_completed_ignored`, and related tests in `tool_live_updates_tests.rs`):

1. **Late update rejection** — `apply_tool_update` checks `final_elapsed_ms.is_some()` and returns `false` if set. Updates after completion/failed state are ignored, preventing stale late arrivals from corrupting a frozen row.

2. **Terminal status guard** — `apply_tool_update` rejects `ToolStatus::Completed` and `ToolStatus::Failed` in the patch. Only `complete_tool_call` and `fail_tool_call` can set terminal status, ensuring timer freeze, cache invalidation, and result item attachment happen atomically.

3. **Fallback synthesis uses `"tool"` sentinel** — when `apply_tool_event_update` finds no matching running row, it synthesizes a minimal row with `tool_name: "tool"`. This prevents duplicate title rendering (P-DUPTITLE): the renderer suppresses the title suffix when `tool_name == title`.

4. **Render cache bypass for running tools** — running rows (`final_elapsed_ms.is_none()`) bypass `rendered_cache` on every render pass to show ticking timer and spinner frame updates. Only completed rows (`!is_running`) populate the cache.

### Tool confirmation modal ordering and delivery

When a `PreToolUse` hook returns `permissionDecision: "ask"`, the TUI modal queues an optional user message via durable JetStream append before sending the approval reply. Worker reloads the session log at the tool seam, ensuring the agent sees `tool call → tool result (real or blocked) → queued message`.

Key invariants (verified by `denied_zero_execution_round_injects_queued_messages_once_after_blocked_result` in `crates/harnx-runtime/tests/nats_tool_confirmation.rs`):

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
SHIFT tolerance — they're not char keys. See AgentPicker in `input.rs` for the `||` guard variant,
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
