---
harnx: minor
---
Remove hook config fields `type`, `event`, `matcher`, and `timeout`. Hooks now use a command-only model: the `command` field specifies a hook server binary (e.g., `harnx-claude-compatible-hook-server --event <E> --matcher <M> [--persistent] -- <child>` for generic hooks, or `harnx-proxy-auth ...` for native hooks that self-declare their event/matcher).
