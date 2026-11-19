---
harnx: patch
---

Answer direct-child approvals from parent rows in the web UI. Legacy approvals use a new exact-generation RPC, so an old row can't answer a newer request that reuses the same tool-call ID. Confirmation interactions use the child's interaction-response endpoint. Rows show recorded answers for conflicts and link to the child session when a request or server can't be answered from the parent page.
