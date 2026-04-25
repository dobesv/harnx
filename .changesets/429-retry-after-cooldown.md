---
harnx: minor
---
When a 429 response includes a `Retry-After` hint, the retry loop now compares
it against the total remaining backoff budget for that model:

- If `retry_after` is **shorter** than the remaining budget, the loop waits for
  exactly `retry_after` before the next attempt (instead of the exponential
  backoff delay).
- If `retry_after` is **longer** than the remaining budget, the loop exits
  immediately and lets the outer fallback logic place the model on cooldown for
  the server-specified duration — no unnecessary retries are made.
