---
harnx: minor
pantheon: patch
---

Add reserved output-control parameters (`head_lines`, `tail_lines`, `max_output_bytes`) to command templates and disable per-line clipping when `max_output_bytes` is explicit on bash paths.

- Command templates automatically expose `head_lines`, `tail_lines`, and `max_output_bytes` alongside `timeout_secs`. Declaring any of these names in a template's `parameters` map fails at load time.
- Passing `max_output_bytes` explicitly disables per-line middle clipping across `bash_exec`, `bash_spawn` wait and status outputs, `read_exec_log`, command templates, and the in-process `bash` script tool.
- Command-template truncation hints omit `read_exec_log` and reference only `max_output_bytes`, `head_lines`, and `tail_lines`.
- Templates can declare optional `output:` defaults for `head_lines`, `tail_lines`, and `max_output_bytes`. Non-negative integers only; unknown keys and invalid defaults fail loading. Caller arguments override each field independently. A template-set byte limit also disables per-line clipping.
- Ship `gh_issue_view` and `jira_view` with a default byte limit of 65536.

Before upgrading, rename or remove template parameters named `head_lines`, `tail_lines`, or `max_output_bytes`. Templates supplied through explicit `--tool`/`--tools-dir` configs fail at startup if they declare these names; auto-discovered package templates with colliding parameters are skipped with a warning.
