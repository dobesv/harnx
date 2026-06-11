# harnx-mcp-hooks-proxy

`harnx-mcp-hooks-proxy` is an MCP proxy that runs `harnx` hooks around tool calls made to an underlying MCP server. It lets you wrap an existing MCP server with custom pre-tool and post-tool logic.

## Overview

Proxy sits between an MCP host, such as Claude Desktop, and child MCP server process. It intercepts `tools/call` requests and responses, dispatching configured hooks before and after child server handles call.

## Installation

To install `harnx-mcp-hooks-proxy` from `harnx` workspace:

```sh
cargo install --path crates/harnx-mcp-hooks-proxy
```

## Usage

Run proxy by declaring hook argv tokens directly, then terminate each hook with `;`. Use `--` to separate proxy flags from child command.

```sh
harnx-mcp-hooks-proxy \
  --pre-tool-use claude-command --matcher '^exec$' /path/to/pre-hook.sh --log /tmp/pre.log \; \
  --post-tool-use claude-command /path/to/post-hook.sh done \; \
  -- post-child-mcp-server --arg1
```

Each hook uses `find -exec` style syntax: one flag, followed by multiple argv tokens, terminated by `;`.

## CLI Options

| Option | Description |
| :--- | :--- |
| `--pre-tool-use <TYPE> [--async] [--matcher <REGEX>] <CMD> [ARGS...] ;` | Hook to run before tool call. |
| `--post-tool-use <TYPE> [--async] [--matcher <REGEX>] <CMD> [ARGS...] ;` | Hook to run after successful tool call. |
| `--post-tool-use-failure <TYPE> [--async] [--matcher <REGEX>] <CMD> [ARGS...] ;` | Hook to run after tool call failure. |
| `--` | Separator between proxy flags and child command to execute. |

## Hook Specification

Hook grammar per event flag:

```text
--pre-tool-use <TYPE> [--async] [--matcher <REGEX>] <CMD> [ARGS...] ;
--post-tool-use <TYPE> [--async] [--matcher <REGEX>] <CMD> [ARGS...] ;
--post-tool-use-failure <TYPE> [--async] [--matcher <REGEX>] <CMD> [ARGS...] ;
```

Rules:

- First token after event flag is hook type, such as `claude-command`.
- `--async` marks hook as asynchronous.
- `--matcher <REGEX>` limits hook to matching tool names.
- First non-option token after type and optional flags is command.
- Remaining tokens become command arguments.
- Hook ends at literal `;`. In many shells, write that as `\;` so shell passes terminator through unchanged.
- No inner quoting layer needed for hook command parsing. Pass command and each argument as separate argv tokens.

Examples:

```sh
harnx-mcp-hooks-proxy \
  --pre-tool-use claude-command /usr/local/bin/audit-hook --phase before \; \
  -- post-child-mcp-server
```

```sh
harnx-mcp-hooks-proxy \
  --post-tool-use claude-command --async --matcher '^exec$' python3 /opt/hooks/notify.py success webhook \; \
  -- child-mcp-server --stdio
```

Proxy shell-quotes hook command and args internally when building `HookConfig.command`, so tokens with spaces or quotes are preserved correctly without requiring you to pack whole hook into one quoted string.
