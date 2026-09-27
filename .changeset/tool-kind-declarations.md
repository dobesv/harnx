---
"harnx-core": patch
"harnx-toolset": patch
"harnx-runtime": patch
"harnx-fs-tools": patch
"harnx-bash-tools": patch
"harnx-grep-tools": patch
"harnx-fetch-tools": patch
"harnx-time-tools": patch
"harnx-plans-tools": patch
---

Add static tool kind declarations for better tool categorization (#2096)

Tools now declare their categorization (Read, Edit, Search, Execute, Fetch, etc.)
statically via `ToolSpec::with_kind()` at registration time. This kind is carried
through to `ToolEvent::Started` for presentation in UI surfaces.

Changes:
- `ToolSpec::with_kind()` and `ToolSpec::kind()` for declaring/retrieving kind in meta
- `ToolDeclaration.kind` field to carry the declared kind
- `ToolKind` implements `From<ToolProgressKind>` for clean conversion
- Emit declared kind in `ToolEvent::Started` instead of hardcoded `Other`
- Native toolsets declare appropriate kinds:
  - fs: Read/Edit/Search kind based on tool operation
  - bash: Execute kind for all tools
  - grep: Search kind
  - fetch: Fetch kind
  - plans: Read/Edit/Delete based on operation prefix
  - time: Other kind
