---
harnx: minor
---
Update built-in models list to add new models and remove deprecated ones.

New models added:
- OpenAI: `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.4-nano`, `gpt-4.1-mini`, `gpt-4.1-nano`
- Anthropic: `claude-opus-4-7` (new flagship, 1M context, 128k output) across claude/vertexai/bedrock/openrouter providers
- Google Gemini: `gemini-3.1-pro-preview`, `gemini-3.1-flash-lite-preview` (replacing deprecated `gemini-3-pro-preview`)
- xAI: `grok-4.20-0309-reasoning`, `grok-4.20-0309-non-reasoning`

Removed/fixed:
- Removed deprecated `gpt-4-turbo` and `gpt-3.5-turbo` from the openai provider
- Replaced `gemini-3-pro-preview` (shut down March 9, 2026) with `gemini-3.1-pro-preview`
- Removed stale `google` provider block at end of file (had deprecated gemini-1.5 models with placeholder pricing)
- Fixed `max_output_tokens` for `claude-opus-4-6` and related models to correctly reflect 128k limit
