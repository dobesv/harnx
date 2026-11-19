---
harnx: patch
---

Persist trusted handoff selections with completed tool rounds and recover handoffs after worker crashes without calling the model again. Retries reuse the target session and prompt, and write one handoff commit per round. Middleware denials, error results and switch-shaped JSON without a result reference can't activate a handoff.
