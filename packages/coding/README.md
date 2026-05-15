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

Install from GHCR:

```sh
harnx pkg install ghcr.io/dobesv/harnx-packages/coding:latest
```

Set your API keys in `~/.config/harnx/.env`:

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
  "coder":
    model: openai:gpt-4.1          # lighter model for everyday tasks
  "compact-coder":
    model: gemini:gemini-2.5-flash  # or any other cheap compaction model
```

To use Claude Opus for harder problems:

```yaml
agents:
  "coder":
    model: claude:claude-opus-4-7
```

## MCP servers

The package includes ready-to-use MCP server configs in `mcp_servers/`. These
are automatically active when the package is installed — you don't need to copy
or symlink anything.

> **Don't edit files inside the package directory.** They will be overwritten
> when you run `harnx-pkg update`. To customise a server, create a file with
> the same name in `~/.config/harnx/mcp_servers/` — your top-level config
> takes precedence over the package's copy.

### Bundled with harnx (no extra install)

| Server | Namespace | Notes |
|--------|-----------|-------|
| `bash.yaml` | `bash_*` | Shell execution. Rooted at `.` (working directory). Common toolchain exec paths (asdf, bun, cargo, nvm, pyenv, rustup, yarn, etc.) are pre-configured — non-existent paths are silently ignored. |
| `fs.yaml` | `fs_*` | Filesystem read/write. Rooted at `.` (working directory) to avoid exposing credentials in `~`. |
| `plans.yaml` | `plans_*` | Plan/task tracking, stored in `.agent/plans/` relative to the working directory. |
| `time.yaml` | `time_*` | Current time and wait utilities. |

### External servers (require install)

| Server | Namespace | Requires | Notes |
|--------|-----------|----------|-------|
| `fetch.yaml` | `fetch_*` | Node.js / npx | Fetches URLs as markdown or text. No API key. |
| `exa.yaml` | `exa_*` | Node.js / npx | Web search via Exa. Requires `EXA_API_KEY`. |
| `context7.yaml` | `context7_*` | Node.js / npx | Library docs lookup. No API key. |
| `grep.yaml` | `grep_*` | uv / uvx | GitHub code search via grep.app. No API key. |

Add your Exa key to `~/.config/harnx/.env`:

```sh
EXA_API_KEY=...
```

Get a key at [exa.ai](https://exa.ai).

### Customising MCP server config

Since package files are read-only, use the patch file to customise MCP servers:

```yaml
# ~/.config/harnx/packages/coding.patch.yaml
mcp_servers:
  "bash":
    # Append extra args without replacing the package's existing args:
    args_append:
      - --extra-exec
      - /opt/company-tools/bin
    # Replace the roots list entirely:
    roots:
      - ~/projects/myapp
  "exa":
    # Disable a server you don't want:
    enabled: false
```

Available patch keys per server:

| Key | Effect |
|-----|--------|
| `enabled` | Enable or disable the server |
| `args` | Replace the args list entirely |
| `args_append` | Append args after the package's existing args |
| `env` | Merge env vars (patch keys win; others preserved) |
| `roots` | Replace the roots list entirely |
