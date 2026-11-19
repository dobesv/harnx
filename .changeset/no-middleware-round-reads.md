---
harnx: patch
---
Avoid reloading session history on new NATS tool rounds with no middleware. Probe the log tail and checkpoint ownership instead, while preserving durable recovery after middleware removal.
