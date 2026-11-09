---
harnx: patch
---

Map LiteLLM's current `gemini` provider key in `scripts/update_models.py` (keeping the legacy `google_ai` alias), so the weekly catalog refresh ingests Gemini pricing again instead of silently keeping stale entries. Refresh the Gemini block of `models.yaml` from the live registry: cached-token calls now have `cache_read_price` for the whole Gemini 2.5+ family, and stale input/output prices are corrected (e.g. `gemini-2.5-pro` was listed at $0, `gemini-3.6-flash` at the wrong rate). Curated entries and request patches are preserved.

Skip chat entries whose `supported_output_modalities` excludes text — LiteLLM tags the audio-only Lyria music models and `gemini-2.5-pro-preview-tts` as mode `chat`, but the shipped chat-completions transport parses text output only, so they never become selectable chat models.

Add `harnx_llm_unpriced_calls_total` so a call with nonzero tokens that cannot be priced (missing input, output, or cache price) is counted instead of vanishing from all spend metrics. `Model::cost_usd` still refuses to estimate missing costs.
