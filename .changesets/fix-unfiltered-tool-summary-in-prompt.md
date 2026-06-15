---
harnx: patch
---
Stopped appending a numbered tool summary to the agent system prompt. The summary was rendered from the agent's full, unfiltered tool set (`get_all_tools()` across every MCP server and package), so an agent in one package would have tools from other packages — e.g. `coding__*` tools listed for a `pantheon` agent — described in its prompt even though those tools were never offered to it. The list was also redundant: the model already receives every available tool as a structured definition via the API `tools` field, correctly filtered by `use_tools`.
