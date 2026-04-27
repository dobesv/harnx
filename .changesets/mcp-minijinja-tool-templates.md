---
harnx: minor
---
MCP tool call and result rendering now supports MiniJinja templates.

MCP servers can provide display templates via `_meta.call_template` and
`_meta.result_template` in their tool definitions. The `mcp_servers/xxx.yaml`
config can override these with `tool_templates.<tool_name>.call_template` /
`result_template` (higher precedence).

Template context:
- Call template: `{{ args }}` — full JSON arguments object; access fields with `{{ args.<key> }}`
- Result template: `{{ result }}` — full MCP result JSON object (fields: `content`, `isError`)

All 4 built-in MCP servers (bash, fs, time, todo) now include example templates
for their tools.
