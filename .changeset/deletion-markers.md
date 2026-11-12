---
harnx: patch
---
Deleting a session, a plan document or an A2A registration no longer leaves markers that stay in NATS forever. Purges in `harnx_sessions` and `harnx_plans` now leave markers that expire after an hour; opening the session metadata store or the plans bucket allows the per-message TTLs this needs on a bucket an older build created, and until something from this build has opened it, deletions leave permanent markers as before. Deleting a session's attachments now leaves nothing in `harnx_attachments`, where the object store used to keep a metadata entry marked deleted for each object. Markers left by earlier builds stay until their bucket is recreated.
