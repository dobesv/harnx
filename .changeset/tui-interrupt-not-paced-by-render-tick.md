---
harnx: patch
---
Fix TUI Ctrl+C reporting "timed out appending the interrupt" on long sessions. The interrupt request now runs as its own task instead of being advanced one broker round trip per render tick, so it is no longer paced by the render loop. Broker read time still grows with the session's length.
