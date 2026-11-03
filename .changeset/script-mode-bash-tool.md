---
harnx: minor
---

Enable in-process script execution for the synthesized `bash` tool in script mode.

When `bash_tools` resolves to inner tools, the synthesized `bash` tool runs scripts in an isolated BashKit subshell with an in-process virtual filesystem (`/tmp`), no host filesystem access, and no network access. Inner tools are exposed as commands with kebab-case options mapped from their JSON schemas, plus `--json` input overrides and `-o json|text` output formatting. Includes built-in `tools [pattern]` catalog discovery and `<tool> --help`. Inner tool calls invoke hooks and enforce a concurrency cap (default 8). Tools requiring interactive approval fail fast with exit code 3 (`needs approval: <tool>; call it directly instead of from bash`).

If a worker restarts while a script runs, the call is reported as interrupted and is never automatically re-run.

Note: while `bash_tools` is active, the bare tool name `bash` is replaced by script mode; qualified names like `<server>_bash` continue to work. Mixed-version clusters: older workers silently ignore a frontmatter `bash_tools` field (frontmatter parsing is lenient), but they cannot load persisted session overrides carrying `bash_tools` (session metadata uses strict field checking). Upgrade all workers before setting `bash_tools` overrides.
