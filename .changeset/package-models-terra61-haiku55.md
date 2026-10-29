---
harnx: patch
pantheon: minor
coding: minor
---
Refresh agent fallbacks with GPT-6.1 Sol for coding and judgment, GPT-6.1 Terra for writing, visual work and research, and Claude Haiku 5.5 for small tasks and compaction. Preserve compatible Terra request settings and Haiku adaptive thinking through catalog refreshes.

Upgrade GLM 5 slots to Bedrock GLM 5.3 with explicit high reasoning effort, and reasoning-heavy Gemini fallbacks to Gemini 4 Argon. Atlas tries Argon after both Sol routes. Route native Gemini streaming requests correctly and keep Argon within the supported per-request output and unified context limits.

Prefer Luna for all compactors and Hestia, replace the compactors' 4K-output Bedrock GLM fallback with Luna, and add MiniMax M3.1 Flash as Hestia's direct fallback at low effort. Offer optional direct Z.AI routing while retaining Bedrock. Preserve MiniMax and GLM reasoning during tool continuation. Try another model on specific model availability errors or incompatible image/context input, without cooling down models solely for request incompatibility; malformed requests still stop the chain.

Collect the Codex backend's required event stream for non-streaming callers so subscription-based compaction also works when streaming is disabled.
