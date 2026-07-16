---
harnx: minor
---

feat(session): automatic session title generation

Sessions now get a short, LLM-generated title after the first exchange and
periodically as they grow (every `title_update_threshold` tokens, default
50,000). Configure the generator via `title_agent` (global in `config.yaml` or
per-agent in front matter); leave it unset to disable. Titles are stored as
append-only `Title` log entries, surfaced in local and remote (NATS) session
listings and the serve API, and can be set manually with `.set title <text>`
(which freezes automatic regeneration). Do not use a reasoning model as the
title agent.
