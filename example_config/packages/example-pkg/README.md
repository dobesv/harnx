# example-pkg

A demonstration harnx package showing the expected on-disk layout for installed packages.

## Files

```
packages/example-pkg/
  manifest.yaml          # Written by harnx-pkg at install time
  package.yaml           # Optional metadata provided by the package itself
  agents/
    example-pkg-agent.md # Agent markdown with YAML frontmatter
  README.md              # This file (optional)
```

## Namespacing

After installation:
- Agent `example-pkg-agent` is exposed as `example-pkg/example-pkg-agent`
- ACP tool: `example-pkg__example-pkg-agent_session_prompt`
- MCP tools (if any): `example-pkg__<server>_<tool>`
