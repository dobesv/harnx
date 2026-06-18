---
harnx: minor
---
Add upload-by-reference attachment encoding for the Gemini and Anthropic providers. Historical image attachments stored as `cid:` references are now uploaded once to the provider Files API (Gemini File API / Anthropic Files API) and reused across turns via an in-memory cache (keyed by content id, with expiry where the provider sets one), instead of re-inlining base64 every turn. Falls back to base64 inline content when upload is unsupported or fails. OpenAI remains base64-only because the Chat Completions API cannot reference uploaded images by file id. Backends without a Files API (Vertex, Bedrock, Ollama, etc.) continue to use base64. No change to the on-disk transcript format.
