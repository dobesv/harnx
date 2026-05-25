---
harnx-core: minor
harnx-mcp-bash: minor
---
Add shebang support to the bash MCP server. Scripts starting with `#!` (e.g., Python, Node, Ruby) are automatically written to temporary files and executed via the detected interpreter. Includes dynamic Markdown code-fence highlighting and automatic sandbox allowlisting for absolute interpreter paths.
