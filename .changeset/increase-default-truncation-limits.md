---
harnx: minor
---

Raise default tool-output truncation to 128 KiB and 1000 lines and stop clipping long lines by default.

- Use a shared default of 131,072 bytes and a 250-head/750-tail line window for fs read, grep, bash tools, templates, the bash script tool, fetch, attachment text reads, and MCP bridge previews. Long lines are no longer clipped per line by default.
- Keep TUI and CLI display truncation unchanged.
- Set sub-agent `session_load` and `session_wait` defaults to 131,072 bytes and 250/750 lines per event; raise their byte ceiling to 307,200.
- Set attachment text `byte_limit` default to 128 KiB.
- Set MCP bridge default result size to 131,072 bytes; `--max-result-bytes` and `HARNX_MCP_MAX_RESULT_BYTES` still override it.
- Set fetch `max_length` default to 131,072 characters.
