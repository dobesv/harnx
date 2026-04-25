---
harnx: minor
---
Unified the LLM agent loop across all front-ends (CLI, TUI, ACP server) into a single canonical implementation in `harnx-runtime::agent_loop`. The ACP server now gains stop hooks, async hooks, retry/fallback, embeddings, and `UserPromptSubmit` hook support for free. Fixes issue #305 where tool errors in ACP sub-agents ended the session instead of being fed back to the LLM.
