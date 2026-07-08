---
"harnx": minor
---

Add AG-UI tool approval HITL interrupt/resume flow. Tool rounds that need approval now finish with interrupt metadata on `RUN_FINISHED`, sessions expose pending interrupts on reconnect, and clients can resume with approve/deny decisions without re-asking model.
