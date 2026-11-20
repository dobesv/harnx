---
harnx: minor
---
Answer middleware A2UI interactions from ACP and the CLI. ACP renders pending A2UI surfaces with the shared plain-text fallback and offers one option per declared button action (inputs are answered with their declared defaults, since ACP permission replies carry only an option ID). The new `harnx interact list` and `harnx interact answer` commands show a session's pending interactions with their exact identity and submit answers — action, structured data, optional note — through the same durable acceptance path as the web and TUI frontends, reporting accepted, already-applied, stale, conflicting, and invalid outcomes without journaling invalid replies.
