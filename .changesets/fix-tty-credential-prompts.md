---
harnx: patch
---
Inject non-interactive safe defaults into the bash child process environment to
prevent programs from corrupting the terminal display, hanging on interactive
pagers, or emitting ANSI escape sequences into tool output.

Programs like `git` open `/dev/tty` directly for credential prompts, bypassing
`stdin(Stdio::null())`. Interactive pagers (`less`, `more`) hang forever waiting
for keystrokes. Tools that detect a color-capable `$TERM` emit ANSI escapes that
clutter text sent to the model.

The fix adds a new lowest-precedence layer in `build_child_env()`. Each key is
seeded with the **host-environment value** when present, otherwise the fallback.
Higher-precedence layers (`.env.bash`, `extra_env_passthrough`, `env_overrides`)
can override any of these further.

Defaults injected (fallback used only when the host env does not set the key):

- **Credential/prompt suppression:** `GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS=true`,
  `SSH_ASKPASS=true`, `SSH_ASKPASS_REQUIRE=force`, `DEBIAN_FRONTEND=noninteractive`
- **Pager suppression:** `PAGER=cat`, `GIT_PAGER=cat`, `MANPAGER=cat`,
  `SYSTEMD_PAGER=cat`, `GH_PAGER=cat`
- **ANSI color suppression:** `TERM=dumb`, `NO_COLOR=1`, `CLICOLOR=0`,
  `FORCE_COLOR=0`
