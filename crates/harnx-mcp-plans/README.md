# harnx-mcp-plans

`harnx-mcp-plans` is an MCP server providing file-based management for plans, tasks, and notes. It stores data as YAML-frontmatter markdown files, making the storage human-readable and compatible with standard version control systems.

## Overview

The server organizes information into plans. Each plan is a directory containing:
- `plan.md`: The main plan description and metadata.
- `tasks/*.md`: Individual task files.
- `notes/*.md`: Individual note files.

## Usage

Run the server as a subprocess in your MCP host (e.g., Claude Desktop).

### CLI Options

| Option | Short | Environment Variable | Default | Description |
| :--- | :--- | :--- | :--- | :--- |
| `--dir <path>` | `-d` | `AGENT_PLANS_PATH` | `.agent/plans` | Path to the plans directory. |
| `--retention-days <N>` | `-r` | `AGENT_PLANS_RETENTION_DAYS` | `14` | Retention period in days. |

### Retention & Cleanup

The server automatically cleans up inactive plans to manage disk space.

- **Behavior**: On startup and every 24 hours thereafter, the server scans the plans directory. It deletes any plan directory where no file activity (`plan.md`, `tasks/*.md`, or `notes/*.md`) has occurred for longer than the retention period.
- **Disabling Cleanup**: Set `--retention-days 0` or `AGENT_PLANS_RETENTION_DAYS=0` to disable the automatic cleanup process.

## Tools

The server provides a comprehensive set of tools for managing the lifecycle of plans and their components:

- **Plans**: `list_plans`, `add_plan`, `get_plan`, `update_plan`, `delete_plan`
- **Tasks**: `list_tasks`, `add_task`, `get_task`, `update_task`, `append_task`, `delete_task`
- **Notes**: `list_notes`, `add_note`, `get_note`, `delete_note`
