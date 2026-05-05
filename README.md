# Harnx: Your agents, your way

Harnx is a modular command-line LLM agent harness that lets you build your own agents
from the ground up, giving you total control over the prompt, tools, model, and sub-agents.

It uses and implements standard protocols, meaning that many of the components are also
usable with other harnesses.

## Install

Harnx ships as three binaries, each installable independently:

| Binary | What it does | Release size |
|---|---|---|
| `harnx` | Full CLI — TUI + Cmd + HTTP (`--serve`) + ACP (`--acp=<agent>`) | ~18 MB |
| `harnx-serve` | HTTP-only server, no TUI deps | ~10 MB |
| `harnx-acp-server` | ACP-only headless agent over stdio, no TUI deps | ~11 MB |

Install whichever you need. Most users want just `harnx`; headless server
deployments can skip the TUI deps by picking `harnx-serve` or
`harnx-acp-server` directly.

### Install from Git (Rust developers)

```sh
cargo install --git https://github.com/dobesv/harnx harnx
cargo install --git https://github.com/dobesv/harnx harnx-serve
cargo install --git https://github.com/dobesv/harnx harnx-acp-server
```

The package name after `--git <url>` picks which workspace member to install.
Add `--tag v0.30.0` to pin a specific release, or `--branch monorepo` to
track an in-progress branch.

### Install from a local checkout

Clone the repo, then:

```sh
# Install all three at once via the project's argc task runner:
argc install

# ...or pick one:
argc install harnx
argc install harnx-serve
argc install harnx-acp-server

# Raw cargo also works:
cargo install --path crates/harnx
cargo install --path crates/harnx-serve
cargo install --path crates/harnx-acp-server
```

The `argc install` helper accepts `--debug` (build unoptimized for faster
compile), `--force` (overwrite existing bins), and `--locked` (use the
committed `Cargo.lock` for a reproducible install).

### Pre-built Binaries

Download pre-built archives for macOS, Linux, and Windows from
[GitHub Releases](https://github.com/dobesv/harnx/releases). Each release
publishes a separate archive per binary per target (e.g.
`harnx-0.30.0-x86_64-unknown-linux-musl.tar.gz`). Extract and add to `$PATH`.

## Features

### Multi-Providers

Integrate seamlessly with over 20 leading LLM providers through a unified interface. Supported 
providers include OpenAI, Claude, Gemini (Google AI Studio), Ollama, Groq, Azure-OpenAI, 
VertexAI, Bedrock, Github Models, Mistral, Deepseek, AI21, XAI Grok, Cohere, Perplexity, 
Cloudflare, OpenRouter, Ernie, Qianwen, Moonshot, ZhipuAI, MiniMax, Deepinfra, VoyageAI, 
any OpenAI-Compatible API provider.

### TUI Mode

Experience an interactive chat TUI with features like tab autocompletion of dot-commands, multi-line 
input support, history, and attachments.

### CLI Mode

Explore powerful command-line functionalities with Harnx's CMD mode.

### Multi-Form Input

Accept diverse input forms such as stdin, local files and directories, and remote URLs, allowing flexibility in data handling.

| Input             | CMD                                  | TUI                              |
| ----------------- | ------------------------------------ | -------------------------------- |
| CMD               | `harnx hello`                       |                                  |
| STDIN             | `cat data.txt \| harnx`             |                                  |
| Last Reply        |                                      | `.file %%`                       |
| Local files       | `harnx -f image.png -f data.txt`    | `.file image.png data.txt`       |
| Local directories | `harnx -f dir/`                     | `.file dir/`                     |
| Remote URLs       | `harnx -f https://example.com`      | `.file https://example.com`      |
| External commands | ```harnx -f '`git diff`'```         | ```.file `git diff` ```          |
| Combine Inputs    | `harnx -f dir/ -f data.txt explain` | `.file dir/ data.txt -- explain` |

### Agents

Customize agents to tailor LLM behavior, enhancing interaction efficiency and boosting productivity.

> An agent is a Markdown file combining a system prompt with model configuration, tools, variables, and documents.

### Session

Maintain context-aware conversations through sessions, ensuring continuity in interactions.

> The left side uses a session, while the right side does not use a session.

### Macro

Streamline repetitive tasks by combining a series of dot-commands into a custom macro.

### RAG

Integrate external documents into your LLM conversations for more accurate and contextually relevant responses.

### Tool Use

Tool use supercharges LLMs by connecting them to external tools and data sources. This unlocks a 
world of possibilities, enabling LLMs to go beyond their core capabilities and tackle a wider 
range of tasks.

#### AI Tools & MCP

Integrate external tools to automate tasks, retrieve information, and perform actions directly 
within your workflow.

#### Bundled MCP servers

Harnx ships with several built-in MCP servers ready to enable in your config. See 
`example_config/mcp_servers/` for ready-to-use templates.

*   **`harnx-mcp-fs`** — Filesystem access (`read`, `write`, `edit`, `ls`, `grep`, `find`, `rollback_file`)
    *   Path validation against allowed roots; smart output truncation; binary detection.
    *   [Local history snapshots](docs/local-history-guide.md) before and after every mutation.
*   **`harnx-mcp-bash`** — Bash command execution (`exec`, `spawn`, `wait`, `terminate`, `read_exec_log`)
    *   **Filesystem sandboxing via [birdcage](https://crates.io/crates/birdcage)** (Linux/macOS) — write access limited to roots by default; per-call `inputs`/`outputs` overrides.
    *   Process group management (kill-on-drop) and background `spawn` + `wait` pattern.
    *   Path validation and history snapshots around mutating commands.
*   **`harnx-mcp-time`** — Time and timezone utilities (`get_current_time`, `convert_time`, `wait`).
*   **`harnx-mcp-plans`** — File-based plan/task/note management (`list_plans`, `add_task`, `get_task`, etc.)
    *   YAML front-matter markdown storage for plans, tasks, and notes with rich metadata.

#### AI Agents (CLI version of OpenAI GPTs)

AI Agent = Instructions (Prompt) + Tools (Function Callings) + Documents (RAG).

![harnx-agent](https://github.com/user-attachments/assets/0b7e687d-e642-4e8a-b1c1-d2d9b2da2b6b)

### Local Server Capabilities

Harnx includes a lightweight built-in HTTP server for easy deployment.

```
$ harnx --serve
Chat Completions API: http://127.0.0.1:8000/v1/chat/completions
Embeddings API:       http://127.0.0.1:8000/v1/embeddings
Rerank API:           http://127.0.0.1:8000/v1/rerank
LLM Playground:       http://127.0.0.1:8000/playground
LLM Arena:            http://127.0.0.1:8000/arena?num=2
```

#### Proxy LLM APIs

The LLM Arena is a web-based platform where you can compare different LLMs side-by-side. 

Test with curl:

```sh
curl -X POST -H "Content-Type: application/json" -d '{
  "model":"claude:claude-3-5-sonnet-20240620",
  "messages":[{"role":"user","content":"hello"}], 
  "stream":true
}' http://127.0.0.1:8000/v1/chat/completions
```

#### LLM Playground

A web application to interact with supported LLMs directly from your browser.

![harnx-llm-playground](https://github.com/user-attachments/assets/aab1e124-1274-4452-b703-ef15cda55439)

#### LLM Arena

A web platform to compare different LLMs side-by-side.

![harnx-llm-arena](https://github.com/user-attachments/assets/edabba53-a1ef-4817-9153-38542ffbfec6)

## Custom Themes

Harnx supports custom dark and light themes, which highlight response text and code blocks.

![harnx-themes](https://github.com/dobesv/harnx/assets/4012553/29fa8b79-031e-405d-9caa-70d24fa0acf8)

## Documentation

- [TUI & Dot-Commands Guide](docs/tui-guide.md)
- [Command-Line Guide](docs/command-line-guide.md)
- [Agent Guide](docs/agent-guide.md)
- [Macro Guide](docs/macro-guide.md)
- [RAG Guide](docs/rag-guide.md)
- [Environment Variables](docs/environment-variables.md)
- [Configuration Guide](docs/configuration-guide.md)
- [Custom Theme](docs/custom-theme.md)
- [FAQ](docs/faq.md)

## Contributing

### Conventional Commits
We use [Conventional Commits](https://www.conventionalcommits.org/) to automate our release process and changelog generation. Please follow the convention for all your commit messages.

Common types:
- `feat`: A new feature
- `fix`: A bug fix
- `docs`: Documentation only changes
- `style`: Changes that do not affect the meaning of the code (white-space, formatting, etc)
- `refactor`: A code change that neither fixes a bug nor adds a feature
- `perf`: A code change that improves performance
- `test`: Adding missing tests or correcting existing tests
- `chore`: Changes to the build process or auxiliary tools and libraries such as documentation generation

### Changeset Files
When you make a change that should be included in the changelog, please create a "changeset" file in the `.changesets/` directory. These are Markdown files that describe the change.

Example `.changesets/new-feature.md`:
```markdown
---
harnx: minor
---
Added a new feature to the CLI.
```

The YAML front matter specifies the package and the type of version bump (`patch`, `minor`, or `major`).

### Releasing
To trigger a new release:
1. Ensure all changesets and conventional commits are merged into `main`.
2. Install [knope](https://knope.tech/installation/).
3. Run `knope release` locally (or via a GitHub Action if configured).
4. Knope will:
   - Calculate the new version based on changesets and conventional commits.
   - Update `Cargo.toml`.
   - Update `CHANGELOG.md`.
   - Create a git tag.
   - Push the tag to GitHub, which triggers the release workflow.

## License

Copyright (c) 2023-2025 harnx-developers.

Harnx is made available under the terms of either the MIT License or the 
Apache License 2.0, at your option.

See the LICENSE-APACHE and LICENSE-MIT files for license details.

## Lineage

Harnx began as an independently continued derivative of [aichat](https://github.com/sigoden/aichat).

