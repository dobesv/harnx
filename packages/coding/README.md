# coding

A capable, self-contained coding assistant for harnx. Ideal for solo coding
sessions when you want a single smart agent rather than the full Pantheon
multi-agent orchestration overhead.

## What's included

| Agent | Model | Role |
|-------|-------|------|
| `coder` | claude-sonnet-4-6 | Main coding assistant |
| `compact-coder` | gpt-4.1-mini | Context compaction for long sessions |

## Quick start

Install from GHCR (replace `v0.1.0` with the current release):

```sh
harnx-pkg add ghcr.io/dobesv/harnx-packages/coding v0.3.4
```

Set your API keys in `~/.local/share/harnx/.env`:

```sh
CLAUDE_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...    # for fallback and compaction
```

Run the coder:

```sh
harnx coder
```

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
  - 'if .name == "coder" then .model = "openai:gpt-4.1" end'
  - 'if .name == "compact-coder" then .model = "gemini:gemini-2.5-flash" end'
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
