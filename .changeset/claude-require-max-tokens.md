---
"harnx": patch
---

fix(models): require max_tokens for modern Claude Sonnet/Haiku models

`claude-sonnet-5` (and other 4-5-generation-or-newer Sonnet/Haiku base models)
were emitted without `require_max_tokens: true`, so requests omitted
`max_tokens` and the Anthropic Messages API rejected them with
`max_tokens: Field required (400)`.

`scripts/update_models.py` now derives `require_max_tokens` by model name via a
new `claude_requires_max_tokens()` helper (Sonnet/Haiku >= 4-5 and any bare
major >= 5), so current and future releases get the flag automatically. Opus
models remain governed by the existing adaptive-thinking logic. Regenerated
`crates/harnx/models.yaml` accordingly.
