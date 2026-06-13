---
harnx: minor
---
Client names are now derived from the YAML filename stem instead of a `name:` field in the file contents.

- A client defined in `clients/<name>.yaml` is named `<name>` (extension stripped, verbatim — no lowercasing). For package clients the name is `<package>/<stem>`.
- A `name:` field inside a client spec is now ignored (silently skipped); the filename is the sole source of the client name. There is no migration — rename the file if you relied on a differing `name:` field. This mirrors how MCP/ACP server specs are already named (#823).
- The provider-default fallback (e.g. defaulting an unnamed client to `openai`) has been removed; dynamic clients created from a `provider:model` selection are still named after their provider.
