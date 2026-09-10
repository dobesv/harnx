# coding

A capable, self-contained coding assistant for harnx. Ideal for solo coding
sessions when you want a single smart agent rather than the full Pantheon
multi-agent orchestration overhead.

## What's included

| Agent | Model | Role |
|-------|-------|------|
| `coder` | claude-sonnet-5 | Main coding assistant |
| `compact-coder` | gemini-3.5-flash-lite | Context compaction for long sessions |

## Quick start

Install from GHCR (replace `v0.1.0` with the current release):

```sh
harnx-pkg add ghcr.io/dobesv/harnx-packages/coding v0.3.4
```

Use harnx 0.34.0 or a development build containing the package model updates.
Configure any one of Gemini, Claude, Codex, OpenAI API, or Bedrock; both agents
have fallbacks for every provider. For a ChatGPT subscription, run `codex login`.
For API-key access, set the applicable key in `~/.local/share/harnx/.env`:

```sh
CLAUDE_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
GEMINI_API_KEY=AIza...
BEDROCK_API_KEY=...     # Amazon Bedrock API key
```

Run the coder:

```sh
harnx coder
```

## Model fallbacks

Both chains prefer Codex subscription access immediately before the equivalent
OpenAI API model:

| Agent | Ordered model chain |
|-------|---------------------|
| `coder` | Sonnet 5 → Codex Terra → OpenAI Terra → Gemini 3.8 Flash → Bedrock GLM 5 |
| `compact-coder` | Gemini 3.5 Flash-Lite → Codex Luna → OpenAI Luna → Sonnet 5 → Bedrock GLM 4.7 Flash |

Terra balances everyday coding cost and quality; Luna and Flash-Lite keep
compaction inexpensive. Sonnet 5 is the Claude compaction fallback because its
1M context window can summarize long coding sessions. Opus overrides should
remain pinned to 4.8; newer Opus versions are deliberately excluded.

The five `clients/*.yaml` files inherit model metadata from harnx's shared
catalog. The Bedrock client uses the OpenAI-compatible endpoint in `us-east-1`
and an API key, not the AWS SigV4 credential chain. Region, model entitlement,
context limits, and subscription quotas still apply. Fallback handles missing
credentials, authentication errors, and exhausted retries; request errors such
as HTTP 400/404 stop the turn.

For the dated selection rationale and provider sources, see the
[Pantheon model policy](https://github.com/dobesv/harnx/blob/main/packages/pantheon/MODELS.md).

## What the coder can do

- Read and write files in your local repo
- Run shell commands (tests, linters, builds, git)
- Search the web and official library docs
- Search GitHub for code examples
- Track multi-step tasks with local plans

## Overriding the model

To change the model without editing package files (which are overwritten on
update), use a patch file placed next to the installed package directory:

```
~/.config/harnx/packages/coding.patch.yaml
```

```yaml
agents:
  - 'if .name == "coder" then .model = "codex:gpt-5.6-terra" end'
  - 'if .name == "compact-coder" then .model = "gemini:gemini-3.5-flash-lite" end'
```

To use Claude Opus for harder problems:

```yaml
agents:
  - 'if .name == "coder" then .model = "claude:claude-opus-4-8" end'
```

## Tool servers

The package includes ready-to-use tool server configs in `tool_servers/`. Bundled native toolset servers run directly, while external stdio servers use a bridge adapter. All servers are automatically active when the package is installed — you don't need to copy or symlink anything.

> **Don't edit files inside the package directory.** They will be overwritten
> when you run `harnx-pkg update`. To customise a server, create a file with
> the same name in `~/.config/harnx/tool_servers/` — your top-level config
> takes precedence over the package's copy.

### Bundled tool servers (under tool_servers/)

| Server | Namespace | Requires | Notes |
|--------|-----------|----------|-------|
| `bash.yaml` | `bash_*` | None (bundled binary) | Shell execution. Opts into common system, development-tool, and repository allow batches plus explicit app paths. Includes a native PreToolUse hook (`harnx-proxy-auth`) for GitHub/Atlassian credential injection. |
| `fs.yaml` | `fs_*` | None (bundled binary) | Filesystem read/write. Opts into repository and development-tool allow batches. |
| `plans.yaml` | `plans_*` | None (bundled binary) | Plan/task tracking, stored in `.agent/plans/` relative to the working directory. |
| `time.yaml` | `time_*` | None (bundled binary) | Current time and wait utilities. |
| `fetch.yaml` | `fetch_*` | Node.js / npx | Fetches URLs as markdown or text. No API key. |
| `exa.yaml` | `exa_*` | Node.js / npx | Web search via Exa. Requires `EXA_API_KEY`. |
| `context7.yaml` | `context7_*` | Node.js / npx | Library docs lookup. No API key. |
| `grep.yaml` | `grep_*` | None (bundled binary) | GitHub code search via grep.app. No API key. |

Add your Exa key to `~/.local/share/harnx/.env`:

```sh
EXA_API_KEY=...
```

Get a key at [exa.ai](https://exa.ai).

### Customising tool server config

Since package files are read-only, use the patch file to customise tool servers:

```yaml
# ~/.config/harnx/packages/coding.patch.yaml
tool_servers:
  # Each entry is a jq expression; .name is the server name.
  # The expression receives the full server config as JSON and returns it modified.

  # Append a custom executable path to the bash server:
  - 'if .name == "bash" then .args += ["--allow-exec", "/opt/company-tools/bin"] end'

  # Disable a server you don't want:
  - 'if .name == "exa" then .enabled = false end'
```

Available fields you can set per server with jq:

| Field | Effect |
|-------|--------|
| `.enabled` | Enable or disable the server (`true`/`false`) |
| `.args` | Replace the args list entirely |
| `.args += [...]` | Append args after the existing args |
| `.env.KEY = "value"` | Set an environment variable on the server process |
