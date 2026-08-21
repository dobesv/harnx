---
harnx: patch
---
Stop the NATS worker from re-feeding a turn's own user messages back into itself. The header-insert migration re-maps a headerless session's leading user block onto the migration's log seq, which sits above the turn's seed cursor, so the mid-round injection callback read the prompt the turn was already answering as a new message. Injected text was also persisted as a fresh user log entry, so every injection guaranteed another one on the next tool round and left a leftover message the end-of-turn drain ran as another turn — the TUI kept spinning and queued replies never started a loop. Worker turns no longer send their prompt to the model twice either.
