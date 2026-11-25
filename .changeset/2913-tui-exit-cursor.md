---
harnx: patch
---

Fix the TUI leaving the cursor at the top-left of the restored screen on exit, which made the session breakdown and "Resume this session by running" messages overwrite the pre-TUI screen content. The cursor position is now captured before the alternate screen is entered and restored explicitly after leaving it, clearing from there down; when that position is unknown (non-TTY, unresponsive terminal) or the terminal was resized during the session, exit output starts on a fresh line below all prior content. The panic cleanup path also re-shows the cursor.
