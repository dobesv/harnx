---
harnx: patch
---

Fix automatic session-title generation when running a package agent (e.g. `pantheon/sisyphus`).

A globally-configured `title_agent` was resolved relative to the active agent's package, so a top-level `title-agent` was looked up as `<package>/title-agent` and never found — title generation was silently disabled. Global title agents now resolve at the top level, while an agent's own `title_agent` frontmatter still resolves package-relative.

Also surface title-generation failures instead of failing silently: a new `TitleGenerationFailed` event is shown in the TUI, CLI, server (AG-UI), and web client, carrying the full error chain. Background title-agent output is isolated from the main transcript. Adds a `.title` command to view the current title and guard state and `.title generate` to (re)generate on demand, shows the title in `.info session`, and adds logging to the title-generation path.
