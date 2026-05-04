---
harnx: minor
---
Separate agent configuration from runtime data by implementing XDG-compliant directory structure. Agent `.md` files now live in `agents/` within the config directory, while sessions, logs, and other runtime data are redirected to dedicated data and state directories.
