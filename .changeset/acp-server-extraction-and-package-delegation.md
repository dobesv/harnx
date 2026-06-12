---
harnx: major
---
Move ACP support out of the `harnx` binary and fix same-package pantheon delegation.

- **Breaking:** The `harnx --acp <agent>` flag has been removed (no backward compatibility). Serving an agent over ACP is now done with the standalone `harnx-acp-server <agent>` binary. Internally generated ACP subagent servers are auto-registered to spawn `harnx-acp-server <agent>` instead of `harnx --acp <agent>`; the server binary is resolved as a sibling of the running `harnx` executable, falling back to `harnx-acp-server` on `PATH` (#550).
- **Fix:** Same-package pantheon delegation no longer drops the package prefix on spawn. Auto-registered ACP servers now keep the package-qualified spawn target (e.g. `pantheon/aristarchus`) in their `args`, independent of the per-agent display-name rewrite that exposes same-package peers under their bare name. Package-loaded `acp_servers/*.yaml` whose `args` reference the bare server stem are normalized to the qualified `<package>/<name>` target. This fixes delegation between two agents in the same package once same-named top-level agents are removed (#804).
