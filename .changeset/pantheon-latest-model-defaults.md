---
"pantheon": minor
---

chore(pantheon): update agent default models to latest versions

Refresh the default `model:` (and `model_fallbacks`) for Pantheon agents to the
latest model IDs:

- OpenAI `gpt-5.4`/`gpt-5.5` → `gpt-5.6-sol` (light/mid reasoning) or
  `gpt-5.6-terra` (heavy critics/orchestration).
- Anthropic `claude-sonnet-4-6` → `claude-sonnet-5`.
- Gemini flash tier `gemini-3-flash-preview`/`gemini-3.5-flash` →
  `gemini-3.6-flash` (GA).
- Gemini lite tier `gemini-3.1-flash-lite` → `gemini-3.5-flash-lite` (GA), used
  by the compaction agents.

Also adds `gemini-3.6-flash` and `gemini-3.5-flash-lite` as curated entries in
`crates/harnx/models.yaml` ahead of their publication in the LiteLLM registry.

Agents already on current models (`claude-opus-4-8`, `gemini-3.1-pro-preview`,
`bedrock:zai.glm-5`) are unchanged.
