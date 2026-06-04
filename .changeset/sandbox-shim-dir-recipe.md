---
harnx: patch
---

Rewrite the "AI Agent Wrapper Scripts" section of `docs/sandbox-run.md` to use a PATH-prepended shim directory (`${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin`). The shims are named after the real commands (`claude`, `gemini`, `node`/`yarn`/`npm`/`npx`/`pnpm`), each stripping its own directory from `PATH` before exec'ing the real tool inside a tailored birdcage sandbox, using the project-root pseudo-variables. Replaces the old `claude-sb`/`gemini-sb` recipe (#575).
