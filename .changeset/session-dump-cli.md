---
harnx: minor
---
Split session inspection into action-entity CLI commands and TUI parity:
- `harnx info session <agent> <id> [--format text|yaml|json]` now displays session metadata only (behavior change).
- `harnx dump session <agent> <id> [--format text|yaml|json] [--follow]` dumps the session transcript (with JSONL for json format, and live streaming via `--follow`).
- Renamed `harnx session delete` to `harnx delete session` and `--list-sessions` to `harnx list sessions`.
- Added `.dump session` and updated `.info session` in the TUI overlay.
