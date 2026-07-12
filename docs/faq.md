# FAQ

## How to log or debug?

Set the `HARNX_LOG_LEVEL` environment variable to `debug`:

```sh
HARNX_LOG_LEVEL=debug harnx
```

Then check the log file at `<harnx-config-dir>/harnx.log`.

## How to enable web search?

There are two ways to enable web search:

### 1. Models with built-in web search

Some models have built-in web search capabilities (e.g., Perplexity, OpenRouter online models). Enable this via a model patch in your config.

### 2. Web search tool use

Use the `web_search` tool to give your LLM web search capabilities through tool use.

## Why does my MCP server say its API key is missing (even though it's in `.env`)?

If an MCP server — e.g. the Exa web-search server run via `npx` — reports a missing or empty API key even though you set it in `~/.local/share/harnx/.env`, the cause is almost always the **sandbox**. When `npx`/`node` is wrapped by a [harnx sandbox](sandbox-run.md), the server launches with a scrubbed environment, so the key never reaches it.

Forward the specific variable through the sandbox from the server's config:

```yaml
# mcp_servers/exa.yaml
env:
  HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY
```

Two related gotchas:

- **`env:` values are not `$VAR`-expanded.** `EXA_API_KEY: "$EXA_API_KEY"` sends the literal string `$EXA_API_KEY`, not its value — use `HARNX_BASH_ENV_PASSTHROUGH` (above) to forward the real value instead.
- **The error text tells you which problem you have.** "API key must be provided" means an *empty* value reached the server (stripped, or never set); "Invalid API key" means a *wrong* value reached it (e.g. the un-expanded literal `$EXA_API_KEY`).

## Why do sub-agents fail with "missing credentials" only in the web UI (`harnx-serve`)?

If credentials work in the TUI but fail in the web UI, the `harnx-serve` process likely lacks the necessary environment variables or cannot resolve your `.env` file. See the [Sub-Agent Credentials Troubleshooting Guide](harnx-serve-subagent-credentials.md) for a detailed fix.

## Why compress sessions?

The Chat API is stateless, so the full conversation history is sent with every request. This means history grows rapidly, causing two problems:

1. **Increased latency and cost.** Larger payloads take longer to process and consume more tokens.
2. **May exceed LLM capacity.** Models have a maximum context window. Long conversations can hit that limit.

Harnx addresses this with automatic session compression. When consumed tokens exceed the `compress_threshold`, Harnx compresses the conversation history automatically.

## Why don't LLMs call tools even though they support tool use?

Several things can prevent tool calls from working:

1. **The LLM may only support non-streaming tool use.** Some models can't handle tool calls in streaming mode. Try using `-S` or `.set stream false` to disable streaming.

2. **Missing `functions.json`.** The tool definitions file may not exist. Rebuild your tools in the `llm-functions` directory.

3. **Input not related to available tools.** The LLM won't call tools if your prompt doesn't relate to any of the registered tool functions.

## What is an agent?

An agent is a Markdown file that combines a system prompt with model configuration, tools, variables, documents, and more. Agents are stored at `<harnx-config-dir>/agents/<name>.md` and use YAML front-matter for configuration.

A simple agent might only have a system prompt. A more advanced agent can include:

- **Model and parameter overrides** (model, temperature, top_p)
- **MCP tools** for function calling
- **Variables** for dynamic prompt templates
- **Conversation starters** for guided interactions
- **Documents** for RAG (retrieval-augmented generation)

See the [Agent Guide](agent-guide.md) for full details.
