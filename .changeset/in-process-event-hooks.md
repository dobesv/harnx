---
harnx: minor
---

Redefine `hooks:` as in-process lifecycle event hooks: a map of `SessionStart`, `UserPromptSubmit`, `Stop`, and `StopFailure` entries carrying inline Bash/Python scripts, with `add_context`, `block`, `rewrite_prompt`, and `resume` operations, `on_error` policies, per-event name/override/disable resolution across global, tool-server/package, and agent levels, and Stop resume bounded by `max_resume`. Legacy `command:` hook entries fail config load with migration guidance; use middleware for tool events. Hook-added context now reaches the next model call mid-turn and on NATS HA worker turns.
