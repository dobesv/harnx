# NATS service permission tests

`nats-permissions.conf` is an executable **sandbox** profile for NATS 2.11.6.
It has known passwords and an anonymous admin for test setup; don't deploy it as
production authentication. Operational setup and rollout are in
[`docs/a2a-operations.md`](../../../../../docs/a2a-operations.md).

`permissions.rs` checks provisioning/CAS/leader APIs, event conditional publication,
worker transcript/lease APIs and subject purge permissions. Negative probes require
an observed NATS permission violation, not a successful socket write.
`multi_replica/operations` starts an actual worker daemon and independent backend
clients with this profile, completes one real turn, closes a missing admission
without a prompt, and calls production session GC through the worker account.

| Role | Allowed resource families | Denied probes |
| --- | --- | --- |
| Backend | STREAM CREATE/UPDATE/INFO/MSG.GET/PURGE, metadata/lease KV APIs and watches, `a2a.tasks.>`, transcript append/read, cluster activation publication, metadata/read invalidations | STREAM DELETE, worker activation consumer creation, unrelated subject |
| Worker | Activation pull consumers/ACK, transcript lifecycle, KV metadata/leases, task-event INFO/PURGE for GC, journal/blob resource APIs | `a2a.tasks.>` publication, unrelated subject |

KV slash-containing keys occupy a single dot-delimited NATS subject token.
`$KV.harnx_leases.sessions.*.a2a.lock` doesn't match the actual
`$KV.harnx_leases.sessions/{storage}/a2a/lock`. Broad KV-token service access
can't separate worker and A2A lease paths; code enforces that separation.
This is not a per-user task ACL. Principal/export access stays in AccessRules.

Task readers use leader `STREAM.MSG.GET` and no task-event consumers.
KV watches and activation dispatch have their own existing consumers.
Tests assert task-event `consumer_count == 0`.

New tool/hook profiles need their own permission probes. These tests cover
text-turn backend/worker provisioning and coordinated session cleanup, not every
installed tool subject.
