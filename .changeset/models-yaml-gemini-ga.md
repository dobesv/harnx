---
"harnx": patch
---

chore(models): add gemini-3.6-flash and gemini-3.5-flash-lite to the catalog

Add the two Gemini models that reached GA on 2026-07-21 as curated entries in
`crates/harnx/models.yaml`, ahead of their appearance in the upstream LiteLLM
registry. Pricing is carried forward from the prior flash / flash-lite tier and
will be overwritten by LiteLLM data on the next `scripts/update_models.py` sync
(the sync preserves curated models not yet in the registry).
