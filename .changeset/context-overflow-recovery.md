---
harnx: patch
---
Recover from provider context-window errors by compacting session history and retrying the failed request once, including the first request of a turn. Keep newly submitted input verbatim during recovery, preserve request-only patches, and report when a request still fails after compaction. Recognize context overflow in HTTP and streaming errors from Gemini, OpenAI-compatible providers, Anthropic, and Bedrock.
