# harnx-mcp-time

`harnx-mcp-time` is an MCP server that provides time and timezone utilities to LLM agents. It allows agents to check the current time, convert between timezones, and perform timed waits.

## Overview

The server implements several tools for handling temporal data, using the Model Context Protocol (MCP) over stdio. It automatically detects the local timezone of the host system but allows overrides in tool calls.

## Installation

To install `harnx-mcp-time` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-mcp-time
```

## Tools

The server exposes the following tools:

- `get_current_time`: Get the current time in a specific IANA timezone.
- `convert_time`: Convert, offset, and reformat timestamps (ISO, Unix, or Epoch Millis).
- `wait`: Pause execution for a specified number of seconds (max 3600).
- `wait_until`: Pause execution until a specific target time (HH:MM or full ISO).
