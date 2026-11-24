---
harnx: minor
---
`help <tool>` in a `bash` script now lists the fields of a tool's `-o json` result under `FIELDS (-o json)`, for the file, `bash`, command-template and fetch tools and, through `harnx-mcp-bridge` and `harnx-mcp-remote`, for any MCP tool that declares an `outputSchema` (its top-level fields).
