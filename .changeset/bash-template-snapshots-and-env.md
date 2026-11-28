---
harnx: minor
---

Bash command templates gained a `snapshots: none` opt-out: the invocation takes no execution-history snapshot, classifies nothing, and reports no diff. The plan publisher declares it so a host-side pre-scan snapshot cannot copy flagged worktree bytes into the repository's local Git object store before the publisher's scanner refuses them; `snapshots: none` cannot combine with `protocol:`.

Declared `sandbox.env` ambient passthrough now resolves into the child environment for local launches. A sandboxed launch keeps the existing name-only `--env NAME` transfer to `harnx-sandbox-exec` and never materializes ambient values in the helper's argv, where host observers could read credentials.
