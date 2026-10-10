---
harnx: patch
---

Fix the TUI transcript freezing mid-turn after a sub-agent was cancelled, for example by an invocation deadline, when only finished sub-agent rows kept appearing. Each session's live output is now fenced only by `Cancel` entries in its own log, so a cancelled child no longer hides live output from its parent or its siblings, an interrupted parent no longer hides it from later children, and switching sessions no longer carries the previous session's fence.
