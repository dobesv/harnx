---
harnx: minor
---
Start tool servers declared by active middleware helper tools on local worker session activation. When the worker daemon manages tool servers locally, `configured_worker_services` selects servers matching `tools:` selectors across global middleware, enabled tool-server files, and agent front matter. Helper servers start without listing them in the agent's `use_tools`, are reference-counted, and linger across turns.
