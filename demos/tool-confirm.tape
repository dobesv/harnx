# VHS tape for the harnx tool confirmation demo.
#
# Render with:   ./demos/render.sh tool-confirm
# (which starts harnx-mock-llm, points HARNX_CONFIG_DIR at demos/config, runs
# `vhs demos/tool-confirm.tape`, then cleans up.)
#
# To render this tape standalone (mock-llm already running on :3829):
#   HARNX_CONFIG_DIR=demos/config vhs demos/tool-confirm.tape

Output demos/out/tool-confirm.gif

# render.sh points HOME at an empty tmpdir so the user's shell init (jetbrains
# hooks, git-aware prompts, etc.) stays out of the recording.
Set Shell    bash
Set FontSize 16
Set Width    1100
Set Height   600
Set Theme    "Catppuccin Mocha"
Set TypingSpeed 60ms
Set PlaybackSpeed 1.0

# Launch harnx into the TUI. HARNX_CONFIG_DIR is exported by render.sh; if
# you're invoking vhs directly, export it yourself first.
#
# Hide/Show keeps the bare-prompt frame from appearing in the GIF — recording
# only resumes once harnx has painted its first screen.
Hide
# --session demo bypasses the "Select Session" picker that would otherwise
# eat the first keystrokes; --empty-session forces a fresh history each run.
Type "harnx -a tool-confirm-agent --session demo --empty-session"
Enter
Sleep 3s
Show

# First turn: ask about the time. The mock LLM will call time_get_current_time,
# which triggers the PreToolUse "ask" hook and shows a confirmation prompt.
Type "What time is it in UTC?"
Sleep 400ms
Enter
Sleep 4s

# The confirmation prompt appears. Approve the tool call by typing 'y'.
Type "y"
Enter
Sleep 5s

# Second turn: ask about New York time. Another tool call + confirmation prompt.
Type "What about in New York?"
Sleep 400ms
Enter
Sleep 4s

# This time deny the tool call (default is No, so just press Enter).
Enter
Sleep 4s

# Ctrl+D exits the TUI.
Ctrl+D
Sleep 500ms
