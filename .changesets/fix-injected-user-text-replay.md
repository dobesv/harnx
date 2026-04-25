---
harnx: patch
---
Fix user messages typed during a tool round being duplicated on every subsequent loop iteration. The TUI's mid-loop pending-message injection sets `Input::injected_user_text`, which `begin_turn` writes to the session log. The agent loop now clears the field after each round so the same text isn't re-emitted on every following round, and the LLM sees the user's message once instead of seven or eight times.
