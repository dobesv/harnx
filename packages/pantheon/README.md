# pantheon

The Pantheon compound-engineering agent suite for harnx. Provides a complete
multi-agent system for software engineering tasks: an orchestrator that breaks
work into plans, specialist workers that execute tasks, verifiers that check
results, and a multi-agent code review pipeline.

## What's included

### Orchestrators
| Agent | Role |
|-------|------|
| `sisyphus` | Main assistant — persistent task executor. Breaks tasks into plans, delegates to specialists, runs quality gates. |
| `daedalus` | Strategic planner — interviews users, researches codebases, produces implementation plans, then hands off to Atlas. |
| `atlas` | Plan execution orchestrator — takes Daedalus plans and drives them to completion via specialist delegation. |

### Specialist Workers
| Agent | Model | Best For |
|-------|-------|----------|
| `hephaestus` | gpt-5.4 | Large refactors, migrations, deep implementation |
| `iris` | gemini-3.1-pro-preview | UI, frontend, visual engineering |
| `apollo` | gemini-3.1-pro-preview | Creative solutions, novel UX |
| `athena` | zai.glm-5 (Bedrock) | Complex multi-file features, agent of last resort |
| `hermes` | zai.glm-5 (Bedrock) | Quick fixes, one-liners, config tweaks |
| `hestia` | zai.glm-5 (Bedrock) | Maintenance, dependency updates, linting |
| `plato` | gpt-5.4 | Architecture, data modeling, complex algorithms |
| `peitho` | gemini-3-flash-preview | Documentation, READMEs, release notes |

### Research & Quality
| Agent | Model | Role |
|-------|-------|------|
| `pytheas` | gemini-3-flash-preview | Reconnaissance — fast codebase + GitHub/issue context lookup |
| `zosimus` | gpt-5.5 | Deep investigation — bug reproduction, hypothesis validation |
| `librarian` | gemini-3.1-pro-preview | External knowledge — web search, docs, GitHub |
| `oracle` | zai.glm-5 (Bedrock) | Architectural decisions and consultation |
| `argus` | gemini-3-flash-preview | Independent verification — PASS/FAIL with evidence |
| `mnemosyne` | zai.glm-5 (Bedrock) | Knowledge compounding — writes `docs/solutions/` entries |
| `clio` | gemini-3-flash-preview | Git operations — squash, rebase, push |

### Code Review Pipeline (Aristarchus)
`aristarchus` orchestrates a full multi-agent PR review:
- **9 Muse specialists**: Calliope (quality), Euterpe (conventions), Thalia (testing), Melpomene (security), Polyhymnia (privacy), Erato (UI/a11y), Terpsichore (refactoring), Urania (architecture), Nemesis (reliability), Tyche (deployment)
- **3 Judges**: Minos, Rhadamanthus, Aeacus — independently second-pass every finding with consensus voting
- Produces structured reports with blocker/suggestion/highlight findings and inline PR comments

### Compaction Agents
`compact-dev`, `compact-researcher`, `compact-planner`, `compact-reviewer`,
`compact-argus`, `compact-mnemosyne`, `compact-reliability`, `compact-deploy` —
lightweight context-compression agents (gemini-3.1-flash-lite-preview) used to keep
long conversations within token limits.

---

## Quick start

Install from GHCR (replace `v0.1.0` with the current release):

```sh
harnx-pkg add ghcr.io/dobesv/harnx-packages/pantheon v0.1.0
```

Then set your API keys in `~/.config/harnx/.env`:

```sh
CLAUDE_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
GEMINI_API_KEY=AIza...
BEDROCK_API_KEY=...   # for the zai.glm-5 agents
```

Run Sisyphus:

```sh
harnx sisyphus
```

Or start the Daedalus planning pipeline for a new feature:

```sh
harnx daedalus
```

---

## Client configs included

The package ships four client configs:

| File | Provider | Used by |
|------|----------|---------|
| `clients/claude.yaml` | Anthropic Claude API | sisyphus, daedalus, atlas |
| `clients/openai.yaml` | OpenAI API | hephaestus, plato, aristarchus, zosimus, … |
| `clients/gemini.yaml` | Google Gemini API | argus, clio, pytheas, compaction agents, … |
| `clients/bedrock.yaml` | AWS Bedrock | hermes, hestia, athena, metis, oracle, mnemosyne, nemesis, rhadamanthus, tyche, urania |

API keys are loaded from `~/.config/harnx/.env` (recommended) or from the
environment. Variable names: `CLAUDE_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `BEDROCK_API_KEY`.

---

## Overriding models

The package ships with sensible public-release defaults. To override agent
settings without editing package files (which are overwritten on update), use a
**patch file** placed next to the installed package directory:

```
~/.config/harnx/packages/pantheon.patch.yaml
```

### Patch file format

```yaml
agents:
  ".*":                      # regex matched against bare agent name
    model: claude:claude-opus-4-7   # override model for every agent
  "hephaestus":              # match only hephaestus
    model: openai:o3
  "zosimus":
    model: openai:o3
```

Keys in the patch take precedence over the package's values. The regex is
matched against the bare agent name (e.g. `hephaestus`), not the qualified
`pantheon/hephaestus` form.

### Use your own private/preview models

If you have access to preview models, override per-agent or use `".*"` to
replace all at once:

```yaml
# ~/.config/harnx/packages/pantheon.patch.yaml
agents:
  ".*":
    model: openai:gpt-5.4   # replace every agent's model
```

### Using the Bedrock client for `zai.glm-5` agents

Several agents (hermes, hestia, athena, metis, oracle, mnemosyne, nemesis,
rhadamanthus, tyche, urania) use `bedrock:zai.glm-5`. The Bedrock client config
is included in the package (`clients/bedrock.yaml`) — no manual setup needed.

Set your Bedrock API key in `~/.config/harnx/.env`:

```sh
BEDROCK_API_KEY=...
```

If you use a different AWS region, override `api_base` via the patch file:

```yaml
# ~/.config/harnx/packages/pantheon.patch.yaml
clients:
  "bedrock":
    api_base: https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1
```

If you don't have Bedrock access, override those agents to a different model:

```yaml
# ~/.config/harnx/packages/pantheon.patch.yaml
agents:
  "hermes|hestia|athena|metis|oracle|mnemosyne|nemesis|rhadamanthus|tyche|urania":
    model: openai:gpt-4.1-mini
```

---

## Shared prompt fragments

The `agents/shared/` directory contains reusable prompt fragments included by
agents via the `variables` system. These cover:

- Agent identity/instructions for each specialist
- Tool usage guides (ast-grep search, ast-grep rewrite)
- Cross-cutting protocols (output style, repo documentation discovery, quality
  review gate, Clio delegation, Jira/GitHub lookup)

You can override individual shared files by creating matching files in your
local `~/.config/harnx/agents/shared/` directory.

---

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
| `plans.yaml` | `plans_*` | Plan/task/note management, stored in `.agent/plans/` relative to the working directory. |
| `time.yaml` | `time_*` | Current time and wait/sleep utilities. |

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
# ~/.config/harnx/packages/pantheon.patch.yaml
mcp_servers:
  "bash":
    # Append extra args without replacing the package's existing args.
    # --extra-exec / --extra-rwx extend sandbox filesystem access.
    # --env VAR_NAME passes a variable from the host into sandboxed commands.
    args_append:
      - --extra-exec
      - /opt/company-tools/bin
      - --extra-rwx
      - ~/.codescene
      - --env
      - CS_ACCESS_TOKEN
    # Replace the roots list entirely:
    roots:
      - "."
      - /opt/company-sdk
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
| `env` | Set environment variables on the server process (key/value pairs). Mainly useful for servers that read config from their environment (e.g. API keys for `exa`). |
| `roots` | Replace the roots list entirely |

### Optional / advanced

`tmux_*` and `trigger_agent` tools in `sisyphus.md` are harnx built-ins for
TUI/tmux workflow features — no separate server config needed.
