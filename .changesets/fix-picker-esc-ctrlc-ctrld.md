---
harnx: patch
---

Fix ESC, Ctrl+C, and Ctrl+D doing nothing in agent/session pickers (#467).

- **Ctrl+D** in any picker now exits the process immediately.
- **Ctrl+C** in any picker now exits the process (no in-flight prompt to abort).
- **ESC on AgentPicker** with no agent active (startup) now exits the process.
- **ESC on SessionPicker** when reached via `harnx` → agent picker → session picker (`origin_agent=None, origin_session=None`) now goes back to the AgentPicker.
- **ESC on SessionPicker** when reached via `harnx -a <agent>` with no prior session (`origin_agent=Some, origin_session=None`) now exits the process.
- The existing mid-switch cancel behaviour (ESC restores origin agent+session when `origin_session` is present) is unchanged.
