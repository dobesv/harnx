---
harnx: patch
---
Keep AG-UI lifecycle events balanced when attaching to active local or remote sessions. Open text, tool, step, and thinking segments now close before run terminals, including after local broadcast lag and remote poll-based completion. Fixes #1043, #1837, and #1830.
