---
harnx: patch
---
Fix duplicate tool_use emission in the Claude and Bedrock streaming parsers. When a response contained two tool_use blocks, the first one was emitted twice (once on `content_block_stop`, then again as the "missed stop event" fallback inside the next `content_block_start`), because `content_block_stop` was not clearing accumulator state. This showed up in session transcripts as duplicate tool_calls entries with identical ids and as orphan "tool response pending" placeholders in tool_results. Hardens `add_tool_calls` to dedupe by id as defense-in-depth, and adds regression tests across all streaming clients (Claude, Bedrock, OpenAI, Cohere, Vertex AI Gemini) asserting two tool calls in one response always emit as exactly two calls.
