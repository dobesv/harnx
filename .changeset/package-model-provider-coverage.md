---
harnx: patch
pantheon: minor
coding: minor
---
Refresh package agent models by workload and provide Gemini, Claude, Codex,
OpenAI API, and non-Anthropic Bedrock fallbacks for every agent, including
compaction. Prefer Codex immediately before the equivalent OpenAI API model.

Use GPT-6 Astra at maximum effort for Oracle and Plato, with Claude Fable 5.1
as their Claude alternative. Move Atlas and general Gemini workers to Gemini
3.8 Flash, keep Opus 4.8 for Sisyphus and Daedalus, and use cheaper models for
routine work and compaction. No package agent selects newer Opus versions.

Package-qualified OpenAI-compatible clients now inherit shared provider model
metadata. Add the Bedrock GLM/MiniMax and direct Gemini 3.8 entries, and generate
Astra/Fable reasoning aliases with the required request settings. Preserve
Gemini function-call IDs through tool-result replay and configure Fable 5.1
to tolerate thinking invalidated by conversation compaction. Packages
require harnx 0.34.0 or a development build containing these changes.
