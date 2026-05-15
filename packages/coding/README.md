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

To use a different model, create `~/.config/harnx/agents/coder.md`:

```markdown
---
model: openai:gpt-4.1
---
```

Or use Claude Opus for harder problems:

```markdown
---
model: claude:claude-opus-4-7
---
```

## MCP servers used

| Namespace | Purpose |
|-----------|---------|
| `bash_*` | Shell execution |
| `fs_*` | File system read/write |
| `plans_*` | Plan/task management |
| `time_*` | Time utilities |
| `fetch_*` | HTTP fetch |
| `exa_*` | Web search |
| `context7_*` | Library documentation |
| `grep_*` | GitHub code search |
