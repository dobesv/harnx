# VHS tape for the harnx theme demo.
#
# Render with:   ./demos/render.sh themes

Output demos/out/themes.gif

Set Shell    bash
Set FontSize 16
Set Width    1100
Set Height   600
Set Theme    "Catppuccin Mocha"
Set TypingSpeed 60ms
Set PlaybackSpeed 1.0

Hide
Type "harnx -a demo-agent --session demo --empty-session"
Enter
Sleep 3s
Show

# First turn.
Type "Can you show me a simple Rust function?"
Sleep 400ms
Enter
Sleep 7s

# Check the theme
Type ".info theme"
Sleep 400ms
Enter
Sleep 4s

# Ctrl+D exits the TUI
Ctrl+D
Sleep 500ms
