# Harnx: All-in-one LLM CLI Tool

[![CI](https://github.com/dobesv/harnx/actions/workflows/ci.yaml/badge.svg)](https://github.com/dobesv/harnx/actions/workflows/ci.yaml)
[![Discord](https://img.shields.io/discord/1226737085453701222?label=Discord)](https://discord.gg/mr3ZZUB9hG)

Harnx is an all-in-one LLM CLI tool featuring Shell Assistant, CMD & REPL Mode, RAG, AI Tools & Agents, and More.

> **Lineage:** Harnx began as an independently continued derivative of [aichat](https://github.com/sigoden/aichat). This repository keeps that lineage explicit while moving forward as its own project.

## Install

### Install from Git

- **Rust Developers:** `cargo install --git https://github.com/dobesv/harnx harnx`
- **From Source:** `cargo install --path .`

### Pre-built Binaries

Download pre-built binaries for macOS, Linux, and Windows from [GitHub Releases](https://github.com/dobesv/harnx/releases), extract them, and add the `harnx` binary to your `$PATH`.

## Features

### Multi-Providers

Integrate seamlessly with over 20 leading LLM providers through a unified interface. Supported providers include OpenAI, Claude, Gemini (Google AI Studio), Ollama, Groq, Azure-OpenAI, VertexAI, Bedrock, Github Models, Mistral, Deepseek, AI21, XAI Grok, Cohere, Perplexity, Cloudflare, OpenRouter, Ernie, Qianwen, Moonshot, ZhipuAI, MiniMax, Deepinfra, VoyageAI, any OpenAI-Compatible API provider.

### CMD Mode

Explore powerful command-line functionalities with Harnx's CMD mode.

![harnx-cmd](https://github.com/user-attachments/assets/6c58c549-1564-43cf-b772-e1c9fe91d19c)

### REPL Mode

Experience an interactive Chat-REPL with features like tab autocompletion, multi-line input support, history search, configurable keybindings, and custom REPL prompts.

![harnx-repl](https://github.com/user-attachments/assets/218fab08-cdae-4c3b-bcf8-39b6651f1362)

### Shell Assistant

Elevate your command-line efficiency. Describe your tasks in natural language, and let Harnx transform them into precise shell commands. Harnx intelligently adjusts to your OS and shell environment.

![harnx-execute](https://github.com/user-attachments/assets/0c77e901-0da2-4151-aefc-a2af96bbb004)

### Multi-Form Input

Accept diverse input forms such as stdin, local files and directories, and remote URLs, allowing flexibility in data handling.

| Input             | CMD                                  | REPL                             |
| ----------------- | ------------------------------------ | -------------------------------- |
| CMD               | `harnx hello`                       |                                  |
| STDIN             | `cat data.txt \| harnx`             |                                  |
| Last Reply        |                                      | `.file %%`                       |
| Local files       | `harnx -f image.png -f data.txt`    | `.file image.png data.txt`       |
| Local directories | `harnx -f dir/`                     | `.file dir/`                     |
| Remote URLs       | `harnx -f https://example.com`      | `.file https://example.com`      |
| External commands | ```harnx -f '`git diff`'```         | ```.file `git diff` ```          |
| Combine Inputs    | `harnx -f dir/ -f data.txt explain` | `.file dir/ data.txt -- explain` |

### Role

Customize roles to tailor LLM behavior, enhancing interaction efficiency and boosting productivity.

![harnx-role](https://github.com/user-attachments/assets/023df6d2-409c-40bd-ac93-4174fd72f030)

> The role consists of a prompt and model configuration.

### Session

Maintain context-aware conversations through sessions, ensuring continuity in interactions.

![harnx-session](https://github.com/user-attachments/assets/56583566-0f43-435f-95b3-730ae55df031)

> The left side uses a session, while the right side does not use a session.

### Macro

Streamline repetitive tasks by combining a series of REPL commands into a custom macro.

![harnx-macro](https://github.com/user-attachments/assets/23c2a08f-5bd7-4bf3-817c-c484aa74a651)

### RAG

Integrate external documents into your LLM conversations for more accurate and contextually relevant responses.

![harnx-rag](https://github.com/user-attachments/assets/359f0cb8-ee37-432f-a89f-96a2ebab01f6)

### Tool Use

Tool use supercharges LLMs by connecting them to external tools and data sources. This unlocks a world of possibilities, enabling LLMs to go beyond their core capabilities and tackle a wider range of tasks.

We have created a new repository [https://github.com/sigoden/llm-functions](https://github.com/sigoden/llm-functions) to help you make the most of this feature.

#### AI Tools & MCP

Integrate external tools to automate tasks, retrieve information, and perform actions directly within your workflow.

![harnx-tool](https://github.com/user-attachments/assets/7459a111-7258-4ef0-a2dd-624d0f1b4f92)

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

- [Chat-REPL Guide](https://github.com/dobesv/harnx/wiki/Chat-REPL-Guide)
- [Command-Line Guide](https://github.com/dobesv/harnx/wiki/Command-Line-Guide)
- [Role Guide](https://github.com/dobesv/harnx/wiki/Role-Guide)
- [Macro Guide](https://github.com/dobesv/harnx/wiki/Macro-Guide)
- [RAG Guide](https://github.com/dobesv/harnx/wiki/RAG-Guide)
- [Environment Variables](https://github.com/dobesv/harnx/wiki/Environment-Variables)
- [Configuration Guide](https://github.com/dobesv/harnx/wiki/Configuration-Guide)
- [Custom Theme](https://github.com/dobesv/harnx/wiki/Custom-Theme)
- [Custom REPL Prompt](https://github.com/dobesv/harnx/wiki/Custom-REPL-Prompt)
- [FAQ](https://github.com/dobesv/harnx/wiki/FAQ)

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

Harnx is made available under the terms of either the MIT License or the Apache License 2.0, at your option.

See the LICENSE-APACHE and LICENSE-MIT files for license details.
