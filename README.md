# Harnx: Your agents, your way

Harnx is a modular command-line LLM agent harness that lets you build your own agents
from the ground up, giving you total control over the prompt, tools, model, and sub-agents.

It uses and implements standard protocols, meaning that many of the components are also
usable with other harnesses.

## Install

Harnx ships as a family of independently installable binaries. Each release
publishes a separate archive per binary per target, so you can install only
what you need:

| Binary                  | What it does                                                                    | Docs                                             |
|-------------------------|---------------------------------------------------------------------------------|--------------------------------------------------|
| `harnx`                 | Terminal interface, TUI or non-interactive                                      | [Command-Line Guide](docs/command-line-guide.md) |
| `harnx-serve`           | HTTP-only server, no TUI deps                                                   | [README](crates/harnx-serve/README.md)           |
| `harnx-pkg`             | Package manager for harnx agent configurations                                  | [Package System](docs/packages.md)               |
| `harnx-mcp-bash`        | MCP server exposing bash/subprocess execution with safety guards                | [Bash MCP Server](docs/bash-mcp-server.md)       |
| `harnx-fs-tools`        | Toolset server exposing filesystem operations with safety guards                 | [README](crates/harnx-fs-tools/README.md)       |
| `harnx-mcp-plans`       | MCP server exposing file-based plan/task/note management                        | [README](crates/harnx-mcp-plans/README.md)       |
| `harnx-mcp-plans-github` | MCP server exposing GitHub Issues-backed plan/task/note management              | [README](crates/harnx-mcp-plans-github/README.md)|
| `harnx-mcp-time`        | MCP server exposing time and timezone utilities                                 | [README](crates/harnx-mcp-time/README.md)        |
| `harnx-sandbox-run`     | Run commands inside the birdcage sandbox with hook support                      | [Sandbox Run](docs/sandbox-run.md)               |
| `harnx-sandbox-exec`    | Low-level birdcage sandbox wrapper with explicit path allow-lists               | [README](crates/harnx-sandbox-common/README.md)  |
| `harnx-aws-creds`       | Bearer-protected helper that serves AWS credentials to tools                    | [AWS Creds](docs/aws-creds.md)                   |
| `harnx-k8s-creds`       | Persistent hook that injects scoped Kubernetes credentials into sandboxed tools | [README](crates/harnx-k8s-creds/README.md)       |
| `harnx-proxy-auth`      | TLS-intercepting auth proxy that injects credentials and runs hooks             | [README](crates/harnx-proxy-auth/README.md)      |

Install whichever you need. Most users want just `harnx`; headless server
deployments can skip the TUI deps by picking `harnx-serve` directly. MCP server
binaries (`harnx-mcp-*`) are needed only when configured as external servers.

### Install using asdf

```sh
asdf plugin add harnx https://github.com/dobesv/asdf-plugins.git
asdf install harnx latest
asdf set -u harnx latest
```

### Install from Git (Rust developers)

```sh
cargo install --git https://github.com/dobesv/harnx harnx
cargo install --git https://github.com/dobesv/harnx harnx-serve
cargo install --git https://github.com/dobesv/harnx harnx-sandbox-run
```

The package name after `--git <url>` picks which workspace member to install.
Add `--tag v0.30.0` to pin a specific release, or `--branch monorepo` to
track an in-progress branch.

### Install from a local checkout

Clone the repo, then:

```sh
# Install all harnx binaries at once via cargo xtask:
cargo xtask install

# ...or pick one or more:
cargo xtask install harnx

# Raw cargo also works:
cargo install --path crates/harnx
cargo install --path crates/harnx-serve
```

The `cargo xtask install` helper accepts `--debug` (build unoptimized for
faster compile) and an optional list of bin names to restrict the install. It
always builds with the committed `Cargo.lock` for a reproducible result and
overwrites any existing bins.

It also builds the web UI (via `pnpm install` + `pnpm build` in `web/`) and
copies the compiled assets into the default directory `harnx serve` loads from
(`<data_dir>/web-assets`, i.e. `~/.local/share/harnx/web-assets` on Linux). This
requires [pnpm](https://pnpm.io/) on your `PATH`. Pass `--skip-web` to install
only the Rust binaries and skip the web UI.

### Pre-built Binaries

Download pre-built archives for macOS, Linux, and Windows from
[GitHub Releases](https://github.com/dobesv/harnx/releases). Each release
publishes a separate archive per binary per target (e.g.
`harnx-0.30.0-x86_64-unknown-linux-musl.tar.gz`). Extract and add to `$PATH`.

## Features

### Multi-Providers

Integrate seamlessly with over 20 leading LLM providers through a unified interface. Supported 
providers include OpenAI, Claude, Gemini (Google AI Studio), Ollama, llama-server, Groq, Azure-OpenAI, 
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

*   **`harnx-fs-tools`** — Filesystem toolset (`read`, `write`, `edit`, `insert`, `re_replace`, `ls`, `grep`, `find`, `rollback_file`)
    *   Path validation against allowed roots; smart output truncation; binary detection.
    *   [Local history snapshots](docs/local-history-guide.md) before and after every mutation.
*   **`harnx-mcp-bash`** — Bash command execution (`exec`, `spawn`, `wait`, `terminate`, `read_exec_log`)
    *   **Filesystem sandboxing via [birdcage](https://crates.io/crates/birdcage)** (Linux/macOS) — commands run with read+write+exec access to the project roots plus default system/cache allow-lists.
    *   Process group management (kill-on-drop) and background `spawn` + `wait` pattern.
    *   Path validation and history snapshots around mutating commands.
*   **`harnx-mcp-time`** — Time and timezone utilities (`get_current_time`, `convert_time`, `wait`).
*   **`harnx-mcp-plans`** — File-based plan/task/note management (`list_plans`, `add_task`, `get_task`, etc.)
    *   YAML front-matter markdown storage for plans, tasks, and notes with rich metadata.

#### AI Agents (CLI version of OpenAI GPTs)

AI Agent = Instructions (Prompt) + Tools (Function Callings) + Documents (RAG).

<img width="1100" height="760" alt="Image" src="https://github.com/user-attachments/assets/a1cd8de9-b630-40d3-83d1-3ddc53a1bded" />


### Local LLM via llama-server

Harnx can spawn and manage local `llama-server` (from the [llama.cpp](https://github.com/ggml-org/llama.cpp) project) child processes. It communicates over high-performance **Unix domain sockets**, serving OpenAI-compatible chat completions. This allows you to run local GGUF models with streaming and tool use support without heavy in-process builds or external TCP services.

- **Installation**: Install `llama-server` via [GitHub Releases](https://github.com/ggml-org/llama.cpp/releases) or `brew install llama.cpp`.
- **Per-Model Config**: Each model entry in `models[]` specifies its own GGUF source and tuning knobs (`ctx_size`, `n_gpu_layers`, `threads`). One configuration file can serve multiple distinct models.
- **Model Sources (Precedence)**:
  1. `model_path`: Path to a local `.gguf` file (passes `-m`).
  2. `hf_repo`: HuggingFace repo spec (passes `-hf`, e.g. `ggml-org/gemma-3-1b-it-GGUF`).
  3. **Name as Source**: If both above are missing, the model `name` is used as the HuggingFace repo spec.
- **HuggingFace Auto-Download**: When using an `hf_repo` (or a name-based HF spec), `llama-server` automatically downloads the GGUF to the standard HuggingFace cache on first use. No manual download is required.
  - *Note*: Private or gated repos require the `HF_TOKEN` environment variable.
  - *Note*: Auto-download requires a `llama-server` binary built with OpenSSL (included in official releases and `brew` installations).
  - *Note*: First-run downloads can be large; initial readiness may take several minutes.
- **Multi-Process Lifecycle**: Harnx lazily spawns up to one `llama-server` process per distinct runtime configuration. A configuration's identity includes the model source (local path or HF repo) **plus** its tuning parameters (`ctx_size`, `n_gpu_layers`, `threads`, `extra_args`, `socket_path`), so two models that differ only in tuning — even with the same HF repo spec — run as separate processes and are not shared. Only models you actually use will run. Each process holds its own model in RAM/VRAM. All processes are reaped when Harnx exits.
- **Capability Matrix**: chat ✅, streaming ✅, tool calls ✅ (model-dependent), embeddings ❌, rerank ❌, vision ❌.
- **Binary Discovery**: Harnx searches for the `llama-server` binary in this order:
  1. Config `binary_path`
  2. `HARNX_LLAMA_SERVER_BIN` environment variable
  3. System `PATH`
- **Socket Path**: Defaults to `~/.local/share/harnx/llama-server-<pid>-<hash>.sock` (override with the per-model `socket_path` field).
- **Platform**: Unix-only (uses AF_UNIX sockets); not supported on Windows.

See `example_config/clients/llama-server.yaml` for a configuration template.
### Local Server Capabilities

Use standalone `harnx-serve` binary for HTTP deployment:

```sh
$ harnx-serve --addr 127.0.0.1:8000
Embeddings API:       http://127.0.0.1:8000/v1/embeddings
Rerank API:           http://127.0.0.1:8000/v1/rerank
```

Pass `--model <MODEL>`, `--dry-run`, or one or more `--mcp-root <PATH>` flags as needed.
For interactive agent sessions, use AG-UI control plane exposed by `harnx-serve` under `/v1/agents`. See [crates/harnx-serve/README.md](crates/harnx-serve/README.md).

## Custom Themes

Harnx supports custom dark and light themes, which highlight response text and code blocks.

<p align="center">
  <img width="49%" alt="harnx-themes Dracula (dark)" src="https://github.com/user-attachments/assets/f0755154-a406-416f-8e4f-8462ed9d0d01" />
  <img width="49%" alt="harnx-themes Alucard (light)" src="https://github.com/user-attachments/assets/d0d91b12-ec6b-40b3-82c8-1bb83ab97d1f" />
  <br>
  <em>Dracula (dark) / Alucard (light)</em>
</p>

<!--
  To re-render these GIFs (needs VHS in an environment that allows headless Chromium):
    ./demos/render.sh themes-dark && ./demos/render.sh themes-light
  They land in the gitignored demos/out/. Upload both to GitHub user-attachments
  (drag-and-drop in a PR/issue) and replace the src URLs above.
-->

## Documentation

- [TUI & Dot-Commands Guide](docs/tui-guide.md)
- [Command-Line Guide](docs/command-line-guide.md)
- [Agent Guide](docs/agent-guide.md)
- [Macro Guide](docs/macro-guide.md)
- [RAG Guide](docs/rag-guide.md)
- [Environment Variables](docs/environment-variables.md)
- [NATS HA Deployment Guide](docs/nats-ha.md)
- [Configuration Guide](docs/configuration-guide.md)
- [Tool Confirmation Guide](docs/tool-confirmation-guide.md)
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
When you make a change that should be included in the changelog, please create a "changeset" file in the `.changeset/` directory. These are Markdown files that describe the change.

Example `.changeset/new-feature.md`:
```markdown
---
harnx: minor
---
Added a new feature to the CLI.
```

The YAML front matter specifies the package and the type of version bump (`patch`, `minor`, or `major`).

The package key must be one of the three that knope versions: `harnx`, `pantheon`, or `coding`. Use `harnx` for any change to the Rust workspace — all `harnx-*` crates share a single version, so individual crate names (e.g. `harnx-core`) are **not** valid keys and will cause `knope release` to error.

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

