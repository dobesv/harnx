# Environment Variables

## Env file

Harnx can load environment variables from a `.env` file located in the configuration directory: `<harnx-config-dir>/.env`.

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
- **HARNX_ENV_FILE**: The path to the environment file.
- **HARNX_CONFIG_FILE**: The path to the configuration file.
- **HARNX_ROLES_DIR**: The directory for roles.
- **HARNX_SESSIONS_DIR**: The directory for sessions.
- **HARNX_RAGS_DIR**: The directory for RAG data.
- **HARNX_FUNCTIONS_DIR**: The directory for functions.
- **HARNX_MESSAGES_FILE**: The path to the messages file.

## Agent-Related Envs

- **<AGENT_NAME>_FUNCTIONS_DIR**: The functions directory for a specific agent.
- **<AGENT_NAME>_DATA_DIR**: The data directory for a specific agent.
- **<AGENT_NAME>_CONFIG_FILE**: The configuration file for a specific agent.
- **Agent config env vars**: Environment variables for agent configuration.

## Logging Envs

- **HARNX_LOG_LEVEL**: The log level (e.g., `debug`, `info`).
- **HARNX_LOG_FILE**: The path to the log file.

## Generic Envs

- **HTTPS_PROXY / ALL_PROXY**: Proxy settings for network requests.
- **NO_COLOR**: Disable colored output.
- **EDITOR**: The default editor.
- **XDG_CONFIG_HOME**: The base directory for configuration files on Linux.
