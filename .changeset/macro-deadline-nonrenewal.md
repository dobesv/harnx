---
harnx: patch
---
Keep the latest inherited run scope when nested macros return, so later steps can't escape a shorter nested deadline. Reject expired macro steps before polling a ready dot-command. Add real NATS coverage for macro root persistence, nested continuation lineage, frozen configuration, finite default behavior and timeout aborts.
