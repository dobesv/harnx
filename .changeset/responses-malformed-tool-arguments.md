---
harnx: patch
---
The OpenAI Responses parser now surfaces an error when a streamed tool call has non-empty but malformed JSON arguments, instead of silently replacing them with an empty object `{}`.

Both the non-streaming (`openai_extract_responses`) and streaming (`responses_finalize_tool_call`) paths previously did `serde_json::from_str(..).unwrap_or_else(|_| json!({}))`, so a truncated or invalid arguments buffer produced a tool call with no arguments rather than reporting the failure. They now propagate the parse error with the tool name and raw arguments, matching the chat parser. An empty arguments string still defaults to `{}`.
