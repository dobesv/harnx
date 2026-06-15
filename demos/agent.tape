# VHS tape for the harnx agent demo — a scripted, end-to-end coding session.
#
# Render with:   ./demos/render.sh agent
# (which starts harnx-mock-llm, points HARNX_CONFIG_DIR at demos/config, runs
# `vhs demos/agent.tape`, then cleans up.)
#
# To render this tape standalone (mock-llm already running on :3829):
#   HARNX_CONFIG_DIR=demos/config vhs demos/agent.tape
#
# Everything is faked: the mock LLM (demos/scripts/agent-flow.yaml) emits the
# tool calls, and harnx-mock-mcp (demos/scripts/agent-tools.yaml) returns canned
# read/edit/test results. No real tools run. harnx renders its real tool-call
# and tool-result UI, so it reads like an authentic read -> edit -> test loop.

Output demos/out/agent.gif

# render.sh points HOME at an empty tmpdir so the user's shell init (jetbrains
# hooks, git-aware prompts, etc.) stays out of the recording.
Set Shell    bash
Set FontSize 16
Set Width    1100
Set Height   760
Set Theme    "Catppuccin Mocha"
Set TypingSpeed 55ms
Set PlaybackSpeed 1.0

# Enable syntax highlighting so the diff and test output in the tool results are
# colored. (Do NOT set NO_COLOR — any non-empty value disables color in crossterm;
# render.sh already unsets it.) COLORTERM advertises truecolor for RGB theme colors.
Env HARNX_HIGHLIGHT "true"
Env COLORTERM "truecolor"

# Launch harnx into the TUI. HARNX_CONFIG_DIR is exported by render.sh; if
# you're invoking vhs directly, export it yourself first.
#
# Hide/Show keeps the bare-prompt frame from appearing in the GIF — recording
# only resumes once harnx has painted its first screen.
Hide
# --session demo bypasses the "Select Session" picker that would otherwise
# eat the first keystrokes; --empty-session forces a fresh history each run.
Type "harnx -a code-agent --session demo --empty-session"
Enter
Sleep 3s
Show

# One prompt kicks off the whole multi-step session: the agent reads the file,
# applies an edit (shown as a diff), runs the test, and reports back — each step
# driven by the next scripted mock-llm turn + mock-mcp tool result.
Type "Add a truncate_middle helper to the text utils, with a test."
Sleep 400ms
Enter

# Read + apply-edit rounds play out (each faked tool call is paced by
# response_delay_ms in agent-tools.yaml); snapshot the diff.
Sleep 4s
Screenshot demos/out/agent-edit.png

# Test run + summary land; snapshot the finished session.
Sleep 4s
Screenshot demos/out/agent-done.png

# Ctrl+D exits the TUI; Ctrl+C only aborts in-flight operations.
Sleep 1s
Ctrl+D
Sleep 500ms
