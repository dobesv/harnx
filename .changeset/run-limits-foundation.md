---
harnx: minor
---
Add finite numeric run-limit configuration with a 24-hour fallback, checked deadline resolution, and immutable run/invocation metadata storage. Omitted, null, zero and negative values inherit policy; positive values choose finite allowances. Replay reuses saved admission times and deadlines; worker enforcement is separate.
