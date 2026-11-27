---
harnx: patch
---

`grep_query` returns its whole search report as structured content to `bash` scripts: the default call, `-o text` and `-o json` all print one compact JSON line that pipes into `jq`, and Python calls such as `tools.grep_grep_query(query="...")` return a dict (the runtime prefixes the server name onto the tool). The tool listing declares `harnx:script_output` and `harnx:output_schema` covering both result shapes (hits with `results_by_repository`, and the no-results `results` array), so `help grep_grep_query` shows an `OUTPUT` section and a `FIELDS (-o json)` list. Direct model calls keep the same 2-space pretty-printed JSON, byte for byte, and errors keep their text form in every format. Both the MCP adapter (served by `--mcp-stdio` and `--mcp-http`) and the direct MCP handler honor the `harnx:structured_result` request `_meta` and list the same capability metadata. (issue #3000)
