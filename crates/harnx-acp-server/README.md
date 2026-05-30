# harnx-acp-server

`harnx-acp-server` is a standalone ACP agent binary for headless deployments. It speaks the Agent Client Protocol (ACP) over stdio, allowing `harnx` agents to be integrated into host coordinators such as Zed or Superpowers without requiring the full `harnx` TUI.

## Overview

The server implements the `HarnxAgent` interface, binding the ACP protocol to the `harnx-runtime`. It handles session management, prompt execution, and tool calls while providing real-time event updates to the host.

## Installation

To install `harnx-acp-server` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-acp-server
```

## CLI Options

| Option | Short | Description |
| :--- | :--- | :--- |
| `<AGENT>` | | **Required**. The name of the agent to serve (must exist in `agents/`). |
| `--model <MODEL>` | `-m` | Select a specific LLM model to use. |
| `--dry-run` | | Echo prompts instead of sending them to the LLM. |
| `--mcp-root <PATH>` | | Add one or more MCP roots (comma-separated). |
