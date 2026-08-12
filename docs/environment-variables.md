# Environment Variables

## Env file

Harnx can load environment variables from a `.env` file located in the data directory: `~/.local/share/harnx/.env`.

## Config-Related Envs

- **HARNX_MODEL**: The default model to use.
- **HARNX_TEMPERATURE**: The temperature setting for the model.
- **HARNX_TOP_P**: The top_p setting for the model.
- **HARNX_STREAM**: Whether to stream the output (boolean).
- **HARNX_SAVE**: Whether to save the conversation history (boolean).
- **HARNX_EDITOR**: The editor to use for editing messages or configuration.
- **HARNX_WRAP**: Whether to wrap the output text (boolean).
- **HARNX_WRAP_CODE**: Whether to wrap code blocks (boolean).
- **HARNX_SAVE_SESSION**: Whether to save the session (boolean).
- **HARNX_COMPRESS_THRESHOLD**: The threshold for compressing the session history.
- **HARNX_TOOL_USE**: Enable or disable tool use (boolean). Note: renamed from `AICHAT_FUNCTION_CALLING`.
- **HARNX_USE_TOOLS**: Specify which tools to use.
- **HARNX_RAG_EMBEDDING_MODEL**: The model used for embeddings in RAG.
- **HARNX_RAG_RERANKER_MODEL**: The model used for reranking in RAG.
- **HARNX_RAG_TOP_K**: The number of top results to retrieve.
- **HARNX_RAG_CHUNK_SIZE**: The size of chunks for document processing.
- **HARNX_RAG_CHUNK_OVERLAP**: The overlap between chunks.
- **HARNX_RAG_TEMPLATE**: The template for RAG prompts.
- **HARNX_HIGHLIGHT**: Whether to highlight the output (boolean).
- **HARNX_LIGHT_THEME**: Whether to use a light theme (boolean).
- **HARNX_SERVE_ADDR**: The address to serve the API on.
- **HARNX_USER_AGENT**: The user agent string for API requests.
- **HARNX_SAVE_SHELL_HISTORY**: Whether to save shell history (boolean).
- **HARNX_SYNC_MODELS_URL**: The URL to sync models from.

## Client-Related Envs

- **{client}_API_KEY**: API key for a specific client (e.g., `OPENAI_API_KEY`, `CLAUDE_API_KEY`).
- **HARNX_PLATFORM**: The platform to use.
- **HARNX_PATCH_{client}_CHAT_COMPLETIONS**: Patch for chat completions for a specific client.
- **HARNX_SHELL**: The shell to use for executing commands.

## Files/Dirs Envs

- **HARNX_CONFIG_DIR**: The directory for configuration files.
- **HARNX_ENV_FILE**: Path to the `.env` credentials file. Defaults to `~/.local/share/harnx/.env`.
- **HARNX_BASH_ENV_FILE**: Path to the `.env.bash` credentials file used by the bash toolset server. Defaults to `~/.local/share/harnx/.env.bash`.
- **HARNX_CONFIG_FILE**: The path to the configuration file.
- **HARNX_SESSIONS_DIR**: The directory for sessions.
- **HARNX_RAGS_DIR**: The directory for RAG data.
- **HARNX_FUNCTIONS_DIR**: The directory for functions.
- **HARNX_MESSAGES_FILE**: The path to the messages file.

## Agent-Related Envs

- **<AGENT_NAME>_FUNCTIONS_DIR**: The functions directory for a specific agent.
- **<AGENT_NAME>_DATA_DIR**: The data directory for a specific agent.
- **<AGENT_NAME>_CONFIG_FILE**: The configuration file for a specific agent.
- **Agent config env vars**: Environment variables for agent configuration.

## Local History Envs

- **HARNX_HISTORY_MAX_FILES**: Maximum number of files allowed in a single snapshot. Default: `10000`
- **HARNX_HISTORY_MAX_FILE_BYTES**: Maximum size in bytes for an individual file in a snapshot. Default: `10485760` (10 MiB)
- **HARNX_HISTORY_MAX_TOTAL_BYTES**: Maximum total size in bytes for all files in a single snapshot. Default: `104857600` (100 MiB)

## Logging Envs

- **HARNX_LOG_LEVEL**: The log level (e.g., `debug`, `info`).
- **HARNX_LOG_FILE**: The path to the log file.
- **HARNX_LLM_TRACE**: Path to a file that receives one JSON line per LLM
  HTTP request and per response chunk. Independent of `HARNX_LOG_LEVEL`.
  Each line is `{ts, kind, ...}` where `kind` is `request`, `response`, or
  `stream-event`. Use this to inspect exactly what the harness sent to the
  model and what it received — for example, when the model claims tool
  results were "replayed" or "cached" and you want to confirm whether the
  message history the harness built is responsible. The file is appended,
  not truncated, so set a fresh path per session.

## Tool filesystem allowlist envs

`harnx-fs-tools` and `harnx-bash-tools` accept the same path lists and batch toggles. `harnx-sandbox-run` accepts the four explicit path lists but doesn't support batches.

- **HARNX_TOOLS_ALLOW_READ**: Platform-separated read-only paths.
- **HARNX_TOOLS_ALLOW_WRITE**: Platform-separated read/write paths.
- **HARNX_TOOLS_ALLOW_EXEC**: Platform-separated read/execute paths.
- **HARNX_TOOLS_ALLOW_RWX**: Platform-separated read/write/execute paths.
- **HARNX_TOOLS_ALLOW_COMMON_DEFAULT**: Enable common system and temporary paths with `1`, `true`, `yes`, or `on`.
- **HARNX_TOOLS_ALLOW_DEV_TOOLS**: Enable development toolchains and caches.
- **HARNX_TOOLS_ALLOW_REPO_WORK**: Enable detected project paths and session working directory.
- **HARNX_TOOLS_ALLOW_ALL**: Request full filesystem access, subject to the `$HOME` guard.

No allow variables or CLI options means deny-all for filesystem and bash tool servers. See [Allowlist migration](migration-allowlist.md) for removed variables.

`HARNX_BASH_ENV_PASSTHROUGH` remains a comma-separated list of host environment variable names forwarded into bash or sandbox-run child processes. Example: `HARNX_BASH_ENV_PASSTHROUGH=GITHUB_TOKEN,SSH_AUTH_SOCK`.

## NATS Transport Envs

These are set for you in normal use. Tool and hook servers receive them from
whichever process launched them.

- `HARNX_NATS_URL` — NATS server or cluster URL.
- `HARNX_NATS_TOKEN` — token auth for that connection.
- `HARNX_NATS_TLS`, `HARNX_NATS_TLS_CERT`, `HARNX_NATS_TLS_KEY`, `HARNX_NATS_TLS_CA` — TLS settings, matching the keys in `nats_servers/<cluster>.yaml`.
- `HARNX_NATS_REPLICAS` — JetStream replica count for buckets harnx creates.
- `HARNX_SERVER_SCOPE` — the scope a tool or hook server registers under.

**Do not set `HARNX_SERVER_SCOPE` yourself unless you are deploying servers
independently.** It namespaces every NATS subject and registry key. A worker
only finds servers carrying the exact same value, and a mismatch fails silently
— the worker simply sees no tools. Setting it in a container's global
environment is worse: several harnx binaries treat its presence as "I was
launched by a worker, speak NATS", so a stdio-launched hook would stop
answering its stdio handshake.

## Generic Envs

- **HTTPS_PROXY / ALL_PROXY**: Proxy settings for network requests.
- **NO_COLOR**: Disable colored output.
- **EDITOR**: The default editor.
- **XDG_CONFIG_HOME**: The base directory for configuration files on Linux.
