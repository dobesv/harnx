---
harnx: minor
---
Session management improvements: shorter 6-char base64url session IDs, allow switching agents/sessions directly without needing `.exit` first, "New session" as the first option in the session picker instead of pressing ESC, and require both an agent and session to be selected before any chat activity (CLI auto-selects session; TUI shows pickers on startup). Removes the `.exit session` and `.exit agent` dot-commands.
