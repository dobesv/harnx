# Configuration Guide

## Configuration File

On first run, Harnx creates a configuration file automatically.

The config file path is `<user-config-dir>/harnx/config.yaml`. The exact location depends on your operating system:

| OS      | Path                                                    |
| ------- | ------------------------------------------------------- |
| Windows | `C:\Users\Alice\AppData\Roaming\harnx\config.yaml`     |
| macOS   | `/Users/Alice/Library/Application Support/harnx/config.yaml` |
| Linux   | `/home/alice/.config/harnx/config.yaml`                 |

To find the config file path on your system:

```sh
harnx --info | grep config_file
```

In REPL mode, you can open the config file for editing with:

```
.edit config
```

## LLM

- **model**: The LLM model to use. Can be set to a specific model or a client name.
- **temperature**: Controls randomness. Lower values make output more focused and deterministic.
- **top_p**: Controls diversity via nucleus sampling.

Setting `model` to a client name (e.g., `openai`) uses that client's default model.

## Behavior

- **stream**: Whether to stream responses. (`true`/`false`)
- **save**: Whether to save messages. (`true`/`false`)
- **keybindings**: Keybinding style for REPL mode. (`emacs`/`vi`)
- **editor**: External editor command for the `.edit` REPL command.
- **wrap**: Wrap text at a specified number of characters, or `no` to disable.
- **wrap_code**: Whether to wrap code blocks. (`true`/`false`)

## Tool Use

Harnx supports tool use (renamed from "Function calling").

- **tool_use**: Enable or disable tool use globally. (`true`/`false`)
- **toolsets**: Define named groups of tools for use with `use_tools` or `-t/--tool`.
- **use_tools**: Specify which tools to make available.

Visit [https://github.com/sigoden/llm-functions](https://github.com/sigoden/llm-functions) for setup instructions and available tools.

## Default Session

These fields accept a session spec that automatically loads a role or session when entering a mode. The spec format is `role:<name>`, `session:<name>`, or `<session>:<role>` (load a session and apply a role if the session is empty).

- **repl_default_session**: Session spec applied when entering REPL mode (e.g. `role:code`, `session:default`, `temp:code`).
- **cmd_default_session**: Session spec applied when entering CMD mode.
- **agent_default_session**: Session identifier used when starting an agent (e.g. `temp`, `default`).

## Session

- **save_session**: Whether to save session history. (`true`/`false`)
- **compress_threshold**: Token count threshold that triggers conversation compression.
- **summarize_prompt**: Prompt used when summarizing conversation for compression.
- **summary_prompt**: Prompt used when generating a session summary.

## RAG

- **rag_embedding_model**: Model used for generating embeddings.
- **rag_reranker_model**: Model used for reranking retrieved documents.
- **rag_top_k**: Number of top results to retrieve.
- **rag_chunk_size**: Size of text chunks for document splitting.
- **rag_chunk_overlap**: Overlap between consecutive chunks.
- **rag_template**: Template for formatting RAG context in prompts.
- **document_loaders**: Configuration for loading different document types.

See the [RAG Guide](rag-guide.md) for detailed setup instructions.

## Appearance

- **highlight**: Whether to enable syntax highlighting. (`true`/`false`)
- **light_theme**: Whether to use the light theme. (`true`/`false`). Note: this is `light_theme`, not `theme`.
- **left_prompt**: Custom left prompt for REPL mode.
- **right_prompt**: Custom right prompt for REPL mode.

See [Custom REPL Prompt](custom-repl-prompt.md) for prompt customization details.

## Misc

- **serve_addr**: Address and port for the built-in HTTP server.
- **user_agent**: Custom User-Agent header for HTTP requests.
- **save_shell_history**: Whether to save shell command history. (`true`/`false`)
- **sync_models_url**: URL to sync available models from.

## Agent-Specific Configuration

Each agent can have its own configuration file at:

```
<harnx-config-dir>/agents/<agent-name>/config.yaml
```

Agent config files support the following properties:

- **model**: Override the default model for this agent.
- **temperature**: Override the default temperature for this agent.
- **top_p**: Override the default top_p for this agent.
- **use_tools**: Specify which tools this agent can use.
- **agent_default_session**: Session to use when starting this agent.
- **instructions**: System prompt / instructions for the agent.
- **variables**: Variables that can be interpolated into the agent's prompts.

## Clients

Harnx supports many LLM providers. Each client is configured in the `clients` section of the config file.

### General Client Configuration

Every client supports these common options:

```yaml
clients:
  - type: <provider>        # Provider type (e.g., openai, claude, gemini)
    api_key: <key>           # API key (can also use env vars)
    models:
      - name: <model-name>
        max_input_tokens: 128000
        max_output_tokens: 8192
```

You can add chat models, embedding models, and reranker models to any client:

```yaml
clients:
  - type: openai
    api_key: <key>
    models:
      - name: gpt-4o
        type: chat
        max_input_tokens: 128000
        max_output_tokens: 16384
      - name: text-embedding-3-large
        type: embedding
        max_input_tokens: 8191
        default_chunk_size: 3000
      - name: custom-reranker
        type: reranker
        max_input_tokens: 8191
```

### Proxy

Set a proxy for a specific client:

```yaml
clients:
  - type: openai
    api_key: <key>
    proxy: socks5://127.0.0.1:1080
```

### Patching API Requests

You can patch the URL, headers, and body of API requests sent to any client. This is useful for custom endpoints, authentication schemes, or provider-specific parameters.

```yaml
clients:
  - type: openai
    patch:
      chat_completions:
        url: https://custom-endpoint.example.com/v1/chat/completions
        headers:
          X-Custom-Header: custom-value
        body:
          custom_param: custom_value
```

### Provider Examples

#### Gemini Safety Settings
Gemini safety settings example using patch:

```yaml
clients:
  - type: gemini
    api_key: xxxxxxxxxxxxxxxxxxxxxxxx
    patch:
      chat_completions:
        body:
          safetySettings:
            - category: HARM_CATEGORY_HARASSMENT
              threshold: BLOCK_NONE
            - category: HARM_CATEGORY_HATE_SPEECH
              threshold: BLOCK_NONE
            - category: HARM_CATEGORY_SEXUALLY_EXPLICIT
              threshold: BLOCK_NONE
            - category: HARM_CATEGORY_DANGEROUS_CONTENT
              threshold: BLOCK_NONE
```

#### DeepSeek Beta API
Deepseek beta API example using patch:

```yaml
clients:
  - type: openai-compatible
    name: deepseek
    api_base: https://api.deepseek.com
    api_key: sk-xxxxxxxxxxxxxxxxxxxxxxxx
    patch:
      chat_completions:
        headers:
          X-Beta: "true"
```

#### OpenAI-Compatible Providers

Any provider that implements the OpenAI API format can be configured:

```yaml
clients:
  - type: openai-compatible
    name: my-provider
    api_base: https://api.my-provider.com/v1
    api_key: <key>
    models:
      - name: my-model
        max_input_tokens: 32000
        max_output_tokens: 4096
```
