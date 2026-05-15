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
lightweight context-compression agents (gemini-2.5-flash-lite) used to keep
long conversations within token limits.

---

## Quick start

Install from GHCR:

```sh
harnx pkg install ghcr.io/dobesv/harnx-packages/pantheon:latest
```

Then set your API keys in `~/.config/harnx/.env`:

```sh
CLAUDE_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
GEMINI_API_KEY=AIza...
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

The package ships three generic client configs:

| File | Provider | Used by |
|------|----------|---------|
| `clients/claude.yaml` | Anthropic Claude API | sisyphus, daedalus, atlas |
| `clients/openai.yaml` | OpenAI API | hephaestus, plato, aristarchus, zosimus, … |
| `clients/gemini.yaml` | Google Gemini API | argus, clio, pytheas, compaction agents, … |

API keys are loaded from `~/.config/harnx/.env` (recommended) or from the
environment. The variable names are `CLAUDE_API_KEY`, `OPENAI_API_KEY`, and
`GEMINI_API_KEY`.

---

## Overriding models

The package ships with sensible public-release defaults. You can override any
agent's model without editing the package files by creating a local override
file in your harnx config directory.

### Override a single agent's model

Create `~/.config/harnx/agents/<agent-name>.md` with just the frontmatter
fields you want to change. Harnx merges local agent files with package agents,
with the local file taking precedence for any keys it defines.

Example — use Claude Opus for Hephaestus instead of GPT-4.1:

```markdown
---
model: claude:claude-opus-4-7
---
```

Save as `~/.config/harnx/agents/hephaestus.md`.

### Use your own private/preview models

If you have access to preview models (e.g. newer Gemini or GPT snapshots), you
can override agent models to point to those. Example for all Gemini agents:

```sh
# ~/.config/harnx/agents/sisyphus.md
---
model: claude:claude-sonnet-4-6    # your preferred private model
---
```

### Using the Bedrock client for `zai.glm-5` agents

Several agents (hermes, hestia, athena, metis, oracle, mnemosyne, nemesis,
rhadamanthus, tyche, urania) use `bedrock:zai.glm-5` via AWS Bedrock. To use
them, add `~/.config/harnx/clients/bedrock.yaml`:

```yaml
type: openai-compatible
name: bedrock
api_base: https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1
models:
  - name: zai.glm-5
    type: chat
    max_input_tokens: 200000
    max_output_tokens: 128000
```

AWS credentials are picked up from the standard environment (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_REGION`) or from `~/.aws/credentials`.

If you don't have Bedrock access, override those agents to use any OpenAI-compatible
model, e.g. `openai:gpt-4.1-mini`.

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

## MCP servers used

Agents in this package use the following MCP tool namespaces. Install the
corresponding MCP servers in your harnx config to use all features:

| Namespace | Server | Purpose |
|-----------|--------|---------|
| `bash_*` | `harnx-mcp-bash` (bundled) | Shell execution |
| `fs_*` | `harnx-mcp-fs` (bundled) | File system read/write |
| `plans_*` | `harnx-mcp-plans` (bundled) | Plan/task management |
| `time_*` | `harnx-mcp-time` (bundled) | Time utilities |
| `fetch_*` | `mcp-fetch` | HTTP fetch |
| `exa_*` | `mcp-exa` | Web search (Exa API) |
| `context7_*` | `mcp-context7` | Library documentation |
| `grep_*` | `mcp-grep` | GitHub code search |
