# VHS tape for the harnx agent demo.
#
# Render with:   ./demos/render.sh agent
# (which starts harnx-mock-llm, points HARNX_CONFIG_DIR at demos/config, runs
# `vhs demos/agent.tape`, then cleans up.)
#
# To render this tape standalone (mock-llm already running on :3829):
#   HARNX_CONFIG_DIR=demos/config vhs demos/agent.tape

Output demos/out/agent.gif

# render.sh points HOME at an empty tmpdir so the user's shell init (jetbrains
# hooks, git-aware prompts, etc.) stays out of the recording.
Set Shell    bash
Set FontSize 16
Set Width    1100
Set Height   520
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
Type "harnx -a demo-agent --session demo --empty-session"
Enter
Sleep 3s
Show

# First turn.
Type "How many Rust files are in this workspace?"
Sleep 400ms
Enter
Sleep 5s

# Second turn — uses the next scripted mock-llm response.
Type "Which crate is the largest?"
Sleep 400ms
Enter
Sleep 6s

# Ctrl+D exits the TUI; Ctrl+C only aborts in-flight operations.
Ctrl+D
Sleep 500ms
