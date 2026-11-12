---
harnx: patch
---
A session lease taken after its key was released, reclaimed, deleted or expired now carries its TTL from the start. It used to be written without one until the holder's first renewal, about 10 seconds later, so a worker that died in that window held the session forever: other workers waited on a TTL that never came, and session GC treated the session as active. Releasing, reclaiming or deleting a lease also now leaves a purge marker that expires after an hour, where it used to leave a marker that stayed in `harnx_leases` for good. Markers left by earlier builds go when their key is leased again or their session is deleted.
