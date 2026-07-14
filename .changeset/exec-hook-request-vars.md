---
harnx: minor
---

feat(proxy-auth): send resolved `vars` to executable hooks on each request

Executable (`--hook <path>` / inline shebang) hooks now receive a `vars` object
on every JSONL request containing the resolved, non-secret context that jq hooks
reference as jaq variables — the `fake_*` sentinels and `temp_file_root`. Real
secrets are deliberately excluded (a hook already inherits proxy-auth's process
environment, so putting them in the payload would only widen the logging
surface).

This lets a hook write files into proxy-auth's own per-instance temp dir
(`--fs`'s `$temp_file_root`) — unique per proxy and auto-deleted on exit — and
agree with a sibling `--env` on the path, instead of guessing a shared location.
`example_config/jira-auth-hook.py` uses `vars.temp_file_root` to place its
synthetic acli config exactly where `--env` points `ACLI_CONFIG_DIR`, fixing
`acli` auth in the sandbox (the previous `\($temp_file_root)/harnx-fs-acli`
rendered as `/harnx-fs-acli` because `$temp_file_root` is empty without `--fs`).
The hook also gained verbose per-request tracing (method + host + path +
injection decision) when `HARNX_JIRA_LOG_FILE` is set.
