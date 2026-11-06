---
harnx: patch
---

Fix `harnx info agent <name>` dropping tool-server tools (bash_exec, fs_*, plans_*, …) from `use_tools`: explicit selectors now stay verbatim in the rendered dump even when no local tool declaration exists, since those tools are only declared by workers at runtime. Toolsets still expand, output is deduplicated in selector order, and glob selectors (including `*`) are kept alongside the names they match locally instead of silently shrinking to builtins. Also route `.info tools` through the worker like `.list tools`, so interactive sessions list the agent's actual tools rather than synthetic handoff declarations; without an active session the command now reports an error like the other operator tool commands.
