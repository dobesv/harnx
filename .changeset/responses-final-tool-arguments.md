---
harnx: patch
---
OpenAI and Codex tool calls now take their arguments from the complete string the Responses API sends when each call finishes, instead of the streamed fragments, and harnx logs a warning if the two ever differ.
