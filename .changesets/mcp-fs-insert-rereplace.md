---
harnx: minor
---
Add `insert` and `re_replace` tools to `harnx-mcp-fs`.

- `insert`: insert text at a line position (0 = prepend, N = after line N, supports optional column for mid-line insertion)
- `re_replace`: regex find-and-replace using fancy_regex syntax with capture group back-references
