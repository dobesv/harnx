# VHS tape for the harnx theme demo (dark mode — Dracula).
#
# Render with:   ./demos/render.sh themes-dark
# render.sh exports HARNX_CONFIG_DIR=demos/config (which holds dark.tmTheme =
# Dracula) and starts harnx-mock-llm with demos/scripts/themes-dark-flow.yaml.
# Output goes to the gitignored demos/out/ (same as agent.tape); upload the
# rendered GIF to GitHub user-attachments and link it from README.md.

Output demos/out/themes-dark.gif

Set Shell    bash
Set FontSize 16
Set Width    1100
Set Height   760
Set Theme    "Catppuccin Mocha"
Set TypingSpeed 60ms
Set PlaybackSpeed 1.0

# Force dark mode so render_options() loads demos/config/dark.tmTheme (Dracula).
Env HARNX_THEME dark
# Enable syntax highlighting in the recording. HARNX_HIGHLIGHT forces harnx's own
# highlighting on; COLORTERM advertises truecolor so RGB theme colors survive.
#
# Do NOT set NO_COLOR here. Per the no-color.org convention that crossterm (our
# terminal backend) follows, NO_COLOR disables color when set to ANY non-empty
# value — even "false" or "0". Setting `Env NO_COLOR "false"` therefore strips
# every color escape at the backend, so harnx loads the theme and builds colored
# spans but the recording renders flat, unhighlighted text. render.sh already
# `unset`s NO_COLOR; leaving it unset is what actually enables color.
Env HARNX_HIGHLIGHT "true"
Env COLORTERM "truecolor"
# Render the response in one shot (no token streaming). Streamed assistant text
# is split per-line into separate transcript entries, so a fenced code block
# never reaches the markdown renderer as one document and is left unhighlighted
# (tracked separately). Using the final, non-streamed response keeps this
# theme/highlighting showcase correct until that streaming path is fixed.
Env HARNX_STREAM "false"

Hide
Type "harnx -a demo-agent --session demo --empty-session"
Enter
Sleep 3s
Show

# Ask for a rich code sample; the mock streams a fenced Rust block that the
# TUI highlights with the Dracula theme once the turn finalizes.
Type "Can you show me a rich Rust code sample?"
Sleep 500ms
Enter
Sleep 10s

# Intermediate snapshot: the highlighted code block. Useful as a static
# screenshot and for debugging whether syntax highlighting actually applied.
Screenshot demos/out/themes-dark-code.png

Type ".info theme"
Sleep 500ms
Enter
Sleep 5s

# Intermediate snapshot: the resolved theme diagnostic.
Screenshot demos/out/themes-dark-info.png

Ctrl+D
Sleep 500ms
