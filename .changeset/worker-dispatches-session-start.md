---
harnx: minor
---

Fire `SessionStart` hooks from the worker and drop `SessionEnd` support.

`SessionStart` was dispatched by the CLI/TUI frontend, which never has a NATS
hook provider, so the event went nowhere and only logged "NATS hook provider
unavailable". The worker now fires it once per session, on the activation that
creates the session, where the hook servers it launched are reachable. Any
`additionalContext` a `SessionStart` hook returns is injected into the first
turn.

`SessionEnd` is removed. Only the frontend knows a session ended, and worker
activations happen per turn, so there was no place to fire it correctly. Hooks
registered for `SessionEnd` no longer match any event.
