---
harnx: minor
---
Add OpenAI `/v1/responses` support so gpt-5.6 reasoning models work with function tools and `reasoning_effort` (blocked on `/v1/chat/completions`). New reasoning-level model aliases `gpt-5.6-sol:high|max` and `gpt-5.6-terra:high|max` route to `/v1/responses` via a new `endpoint` model field, with cross-turn reasoning replay (`reasoning.encrypted_content` via `thought_signature`), `store: false` default overridable through a new `patches.responses` client-config key.
