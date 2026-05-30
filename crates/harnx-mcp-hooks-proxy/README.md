# harnx-mcp-hooks-proxy

`harnx-mcp-hooks-proxy` is an MCP proxy that runs `harnx` hooks around tool calls made to an underlying MCP server. It allows users to wrap any existing MCP server with custom pre-tool and post-tool logic.

## Overview

The proxy sits between an MCP host (like Claude Desktop) and a child MCP server process. It intercepts `tools/call` requests and responses, dispatching them to configured hooks before and after the child server handles the call.

## Installation

To install `harnx-mcp-hooks-proxy` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-mcp-hooks-proxy
```

## Usage

Run the proxy by specifying hooks and the child command to wrap, separated by `--`.

```sh
harnx-mcp-hooks-proxy \
  --pre-tool-use "claude-command my-pre-hook" \
  --post-tool-use "claude-command my-post-hook" \
  -- child-mcp-server --arg1
```

## CLI Options

| Option | Description |
| :--- | :--- |
| `--pre-tool-use <SPEC>` | Hook to run before a tool call. |
| `--post-tool-use <SPEC>` | Hook to run after a successful tool call. |
| `--post-tool-use-failure <SPEC>` | Hook to run after a tool call failure. |
| `--` | Separator between proxy flags and the child command to execute. |

### Hook Specification

A `<SPEC>` follows the format: `claude-command [--async] [--matcher <REGEX>] <COMMAND>`

- `--async`: Run the hook asynchronously (do not wait for completion).
- `--matcher <REGEX>`: Only run the hook if the tool name matches the regex.
