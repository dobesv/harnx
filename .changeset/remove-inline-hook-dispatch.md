---
harnx: minor
---

Remove inline runtime hook dispatch so hooks run fully over NATS. Delete the `harnx-mcp-hooks-proxy` crate and launch bash proxy-auth injection as a co-located NATS hook.
