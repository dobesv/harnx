---
# Agent-specific configuration
# Location: `<harnx-config-dir>/agents/<agent-name>.md`

model: openai:gpt-4o             # Specify the LLM to use
temperature: null                # Set default temperature parameter, range (0, 1)
top_p: null                      # Set default top-p parameter
use_tools:                       # Which MCP tools to allow
  - Bash
  - Glob
  - Grep
  - ListDirectory
  - Read
  - Write
description: ""                  # Short description of the agent
version: ""                      # Agent version string
agent_default_session: null      # Set a session to use when starting the agent
instructions: null               # Override the instructions below

variables:                       # Agent variables (prompted on first use)
  - name: project_dir
    description: The project directory
    default: "."
  - name: system_context
    description: Shared system prompt loaded from file
    path: shared/system-prompt.md

conversation_starters:           # Suggested starting prompts
  - What can you help me with?
  - Let's debug this issue

documents:                       # RAG document paths (relative to config dir)
  - docs/guide.md

# hooks:                         # Per-agent hooks (merged with global)
#   max_resume: 3
#   entries:
#   - event: Stop
#     type: claude-command
#     command: "/path/to/agent-hook.sh"
---

You are a helpful AI assistant. Your task is to help the user with their questions and tasks.
