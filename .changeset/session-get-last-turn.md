---
harnx: patch
---
`session/get` now reports how the session's last turn ended in `last_turn`: `completed`, `error` with the worker's message, `timed_out` with its deadline, or `interrupted`. A client that reconnects after a run ended can tell a failed turn from a finished one, which `state` and `history_snapshot` don't show.
