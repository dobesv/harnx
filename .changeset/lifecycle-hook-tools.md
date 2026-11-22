---
harnx: minor
---

Lifecycle event hooks can now declare an opt-in `tools:` allowlist to call tools from Bash and Python scripts. In Bash, allowlisted tools run as builtin commands with standard CLI flags; in Python, scripts call `call_tool(name, args)`. Tool middleware wraps hook tool calls, recoverable tool errors return to scripts as error results, and locally managed NATS workers automatically start tool servers needed only by hooks on session activation. Configuration parsing rejects unknown fields; remove `tools:` from hook entries before downgrading to an earlier harnx version.
