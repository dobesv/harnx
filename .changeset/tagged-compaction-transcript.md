---
harnx: patch
pantheon: patch
coding: patch
---
Compaction summaries no longer report an agent's instructions to its delegates as the user's own requests. The summarizer now receives the transcript as speaker-tagged elements (`<user>`, `<assistant>`, `<tool_call>`, `<tool_result>`, `<runtime_note>`) instead of headings, our own tag names inside message text are escaped, and a `<kept_after_summary>` block shows which messages, including the user's latest prompt, are replayed verbatim after the summary. The packaged compaction prompts and the built-in summarizer prompt explain the tags.
