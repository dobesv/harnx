---
harnx: minor
---

Bash command templates gained a `diff:` setting with the same values as the `diff` option of `exec` and `spawn` (`none`, `summary`, `files`, `full`), pinning what an invocation reports about what it changed; omitted keeps the existing default. `diff: none` takes no execution-history snapshot and reports no diff — the plan publisher declares it so nothing copies worktree bytes into the repository's local Git object store before its scan. `diff: none` cannot combine with `protocol:`.

Declared `sandbox.env` ambient passthrough now also reaches scripts when a template runs locally, not just under the sandbox. A sandboxed launch keeps the existing name-only transfer: the local `harnx-sandbox-exec` helper receives the names (`--env NAME`) and passes the values through itself, so ambient credentials never appear in the helper's argv.
