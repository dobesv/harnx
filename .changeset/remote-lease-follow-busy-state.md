---
harnx: patch
---

Fix promptless AG-UI run showing idle when remote worker is active

When a Web UI client opens a promptless `/run` against a session whose local `SessionActor` is `Idle` but a remote NATS worker holds the lease, the AG-UI endpoint now follows the remote worker's advisory stream instead of terminating immediately with a synthetic `RUN_FINISHED`. This prevents the Web UI from showing an idle (Send button, no spinner) state while a remote worker is actively processing a turn.

The remote-follow path:
- Emits `RUN_STARTED` (exactly one)
- Hydrates history snapshot (MessagesSnapshot)
- Attaches to the session's NATS advisory stream and translates events to AG-UI frames
- Terminates with `RUN_FINISHED` when a matching durable `TurnEnd` is observed; sustained lease absence is handled separately as crash detection
- Handles worker crash (lease absent for sustained interval with no `TurnEnd`) by forcing finish
- Handles race condition (turn ended between lease sample and stream attach) by checking for existing `TurnEnd` and finishing immediately

