---
harnx: minor
---

Run configured hooks fully over NATS. Adds a generic `harnx-claude-compatible-hook-server` that runs `claude-command` and `claude-command-persistent` hooks over NATS, a native NATS hook mode for `harnx-proxy-auth`, a `hooks:` field on tool-server configs, and a worker-side supervisor that launches configured hooks scoped by where they're defined (global, tool-server, or agent). NATS now dispatches lifecycle, prompt, stop, and tool-use events that have runtime call sites; `InstructionsLoaded` and `CwdChanged` are supported by the protocol but aren't fired by the runtime yet. PreToolUse context injection and Ask approvals work over NATS. The inline dispatch path remains as a fallback for now.
