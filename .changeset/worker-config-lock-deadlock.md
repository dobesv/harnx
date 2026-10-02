---
harnx: patch
---
Fix workers that stopped serving `/healthz` and processing sessions until restarted. A session title write held the session's config lock across a NATS metadata update while the end-of-turn maintenance wait blocked a Tokio worker on the same lock, which left nothing polling for the NATS reply. Titles, `.set` overrides, `.model` and compaction now write to NATS without holding the config lock, polling loops no longer block on it, and a contended config lock now waits without tying up a Tokio worker.
