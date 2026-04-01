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

## What is the difference between agents and roles?

**Roles** are a prompt library. A role consists of instructions (a system prompt) and optional model configuration.

**Agents** are like OpenAI GPTs. An agent is a superset of a role. In fact, a role is just an agent that only has instructions.

Agents add these capabilities on top of roles:

- **Variables** for dynamic prompt templates
- **Conversation starters** for guided interactions
- **Documents** for RAG (retrieval-augmented generation)
- **AI tools** for function calling
