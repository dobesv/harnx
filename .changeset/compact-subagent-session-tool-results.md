---
harnx: patch
---

Render `<agent>_session_send`, `session_load` and `session_cancel` results as one-line summaries in the TUI and CLI instead of a YAML dump. A dispatch-only send shows `dispatched <session_id>` (plus `(unconfirmed)` when the activation publication failed), a send with a wait shows the same status line as `session_wait`, load shows the prompt status, event count and a continuation hint, and cancel names the outcome. The model-facing results and the transcript detail view keep the full output.
