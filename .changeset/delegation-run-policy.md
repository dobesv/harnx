---
harnx: minor
---
Delegation tools use worker-owned wall-clock deadlines. Independent CLI token budgets and accounting remain.

Delegation tools publish target policy resolved by the worker, including package patches. Omitted, null, zero or negative `timeout_secs` inherits target policy; a positive integer chooses a finite local allowance, and inherited deadlines can shorten it. Worker timeout results include scope, frozen deadline and run/invocation IDs, bounded public progress and artifact references, and scope-specific continuation hints. The finite global fallback is 86,400 seconds (24 hours); strings and deadline-disable values are not supported.

External MCP requests receive a durable outer run scope before tool dispatch. Exported agent calls inherit that scope while keeping worker target policy authoritative; connection reservations and cached tool catalogs do not renew or share execution deadlines.
