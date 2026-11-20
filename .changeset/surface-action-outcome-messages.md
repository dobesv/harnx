---
harnx: patch
---

Surface-action rejection messages in the CLI, TUI and web UI now say when a direct tool may already have run and tell users to check session state before retrying. The CLI keeps a defensive message for a `pending` outcome, though the current submit API handles pending acknowledgements internally.
